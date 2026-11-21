using System.Collections.Concurrent;
using System.Diagnostics;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Threading.Channels;
using OpenByd.Models;

namespace OpenByd;

/// <summary>Runs the core's methods by name. The generated methods are written on it.</summary>
internal interface IInvoker
{
    /// <summary>The arguments of a call, holding what every call of this invoker carries.</summary>
    Args NewArgs();

    /// <summary>Runs a method that waits on the network.</summary>
    Task<T> CallAsync<T>(string method, Args args, Reader<T> read);

    /// <summary>Runs a method that waits on the network and returns nothing.</summary>
    Task CallAsync(string method, Args args);

    /// <summary>Runs a method that answers at once.</summary>
    T CallSync<T>(string method, Args args, Reader<T> read);

    /// <summary>Runs a method that answers at once and returns nothing.</summary>
    void CallSync(string method, Args args);
}

/// <summary>
/// A client's connection to the core: calls out, events in.
/// </summary>
/// <remarks>
/// <para>
/// The native client holds a weak handle to this object, so a client nobody refers to is
/// collected and its native client freed with it. Two things keep one alive regardless:
/// a call in flight (<see cref="InFlight"/>), whose task must complete, and listening for
/// pushes (<see cref="Listening"/>), until it is closed.
/// </para>
/// <para>
/// The core calls back on its own threads. Both callbacks copy what they were given and
/// return: a call's outcome completes its task on the thread pool, and an event joins the
/// queue one reader drains, so callbacks run in order and never on the core's threads.
/// </para>
/// </remarks>
internal sealed class ClientCore : IInvoker
{
    private sealed record Pending(ClientCore Core, TaskCompletionSource<Outcome> Completion);

    private readonly record struct Outcome(bool Ok, byte[] Json);

    private readonly record struct HostEvent(string Name, byte[] Payload);

    /// <summary>Every call in flight, of every client, by its number.</summary>
    private static readonly ConcurrentDictionary<ulong, Pending> InFlight = new();

    /// <summary>The clients that listen for pushes.</summary>
    private static readonly ConcurrentDictionary<ClientCore, bool> Listening = new();

    private static ulong s_lastCall;

    private readonly ClientOptions _options;
    private readonly ReloginPolicy? _relogin;
    private readonly ClientHandle _handle;
    private readonly bool _listens;
    private readonly Channel<HostEvent> _events =
        Channel.CreateUnbounded<HostEvent>(new UnboundedChannelOptions { SingleReader = true });
    private readonly ConcurrentDictionary<string, Action<string, VehicleSnapshot>> _stateListeners = new();
    private int _inFlight;
    private int _disposed;

    /// <summary>Makes the native client, closed until <see cref="Start"/>.</summary>
    internal unsafe ClientCore(BydConfig config, ClientOptions options, ReloginPolicy? relogin)
    {
        _options = options;
        _relogin = relogin;
        // Only the events somebody listens to cross from the core.
        var events = new List<string>();
        if (options.OnVehicleInfo is not null)
        {
            events.Add("vehicle_info");
        }
        if (options.OnMqttEvent is not null)
        {
            events.Add("mqtt_event");
        }
        if (options.OnCommandAck is not null)
        {
            events.Add("command_ack");
        }
        if (options.OnCommandLifecycle is not null)
        {
            events.Add("command_lifecycle");
        }
        if (options.OnMqttConnect is not null)
        {
            events.Add("mqtt_connect");
        }
        _listens = events.Count > 0;
        if (options.RequestObserver is not null)
        {
            events.Add("request");
        }
        var hostOptions = new Args()
            .Add<IReadOnlyList<string>>("events", events, static (writer, names) => Wire.WriteList(writer, names, Wire.WriteStr))
            .Add("command_ack_ttl_seconds", options.CommandAckTtlSeconds)
            .Add("command_retries", (long?)options.CommandRetries)
            .Add("gate_policy", options.GatePolicy?.WireName)
            .Add("pin_resolver", options.PinResolver is null ? null : "host")
            .Add("relogin", relogin?.WireName)
            .Add("test_tls", options.TestTls ? true : (bool?)null);
        var configJson = Wire.Encode(config, BydConfigJson.WriteValue);

        var target = GCHandle.Alloc(this, GCHandleType.Weak);
        nint client;
        byte* error = null;
        try
        {
            fixed (byte* configText = configJson)
            fixed (byte* optionsText = hostOptions.Finish())
            {
                client = Native.openbyd_client_new(
                    configText, optionsText, &OnEvent, &OnDone, (void*)GCHandle.ToIntPtr(target), &error);
            }
        }
        catch
        {
            target.Free();
            throw;
        }
        if (client == 0)
        {
            target.Free();
            throw error is null ? new BydException("the client could not be made") : Wire.TakeError(error);
        }
        _handle = new ClientHandle(this, client, target);
        _ = Task.Run(DrainAsync);
    }

    // -- calls ---------------------------------------------------------------

    public Args NewArgs() => new();

    public async Task<T> CallAsync<T>(string method, Args args, Reader<T> read)
    {
        var outcome = await Begin(method, args).ConfigureAwait(false);
        return outcome.Ok ? Wire.Decode(outcome.Json, read) : throw Errors.FromJson(outcome.Json);
    }

    public async Task CallAsync(string method, Args args)
    {
        var outcome = await Begin(method, args).ConfigureAwait(false);
        if (!outcome.Ok)
        {
            throw Errors.FromJson(outcome.Json);
        }
    }

    public unsafe T CallSync<T>(string method, Args args, Reader<T> read)
    {
        ThrowIfDisposed();
        byte* text;
        int ok;
        fixed (byte* json = args.Finish())
        {
            text = Native.openbyd_client_call_sync(_handle, method, json, out ok);
        }
        return Wire.Take(text, ok, read);
    }

    public unsafe void CallSync(string method, Args args)
    {
        ThrowIfDisposed();
        byte* text;
        int ok;
        fixed (byte* json = args.Finish())
        {
            text = Native.openbyd_client_call_sync(_handle, method, json, out ok);
        }
        Wire.Take(text, ok);
    }

    private void ThrowIfDisposed() =>
        ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, typeof(Client));

    /// <summary>Starts a call; its outcome arrives through <see cref="OnDone"/>.</summary>
    private unsafe Task<Outcome> Begin(string method, Args args)
    {
        ThrowIfDisposed();
        var completion = new TaskCompletionSource<Outcome>(TaskCreationOptions.RunContinuationsAsynchronously);
        var call = Interlocked.Increment(ref s_lastCall);
        Interlocked.Increment(ref _inFlight);
        InFlight[call] = new Pending(this, completion);
        try
        {
            fixed (byte* json = args.Finish())
            {
                Native.openbyd_client_call(_handle, call, method, json);
            }
        }
        catch
        {
            if (InFlight.TryRemove(call, out _))
            {
                Interlocked.Decrement(ref _inFlight);
            }
            throw;
        }
        return completion.Task;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    private static unsafe void OnDone(void* userData, ulong call, int ok, byte* json)
    {
        // Nothing may be thrown into the core.
        Pending? pending = null;
        try
        {
            if (!InFlight.TryRemove(call, out pending))
            {
                return;
            }
            Interlocked.Decrement(ref pending.Core._inFlight);
            var text = MemoryMarshal.CreateReadOnlySpanFromNullTerminated(json).ToArray();
            pending.Completion.TrySetResult(new Outcome(ok != 0, text));
        }
        catch (Exception error)
        {
            pending?.Completion.TrySetException(error);
        }
    }

    // -- lifecycle -----------------------------------------------------------

    /// <summary>Opens the client (pyBYD's <c>async_start</c>).</summary>
    internal void Start()
    {
        CallSync("start", NewArgs());
        if (_listens)
        {
            Hold();
        }
    }

    /// <summary>Closes the cars and the client (pyBYD's <c>async_close</c>); it can be started again.</summary>
    internal void Close()
    {
        _stateListeners.Clear();
        Listening.TryRemove(this, out _);
        CallSync("close", NewArgs());
    }

    /// <summary>Keeps the client alive until it is closed: it is waiting for pushes.</summary>
    internal void Hold() => Listening.TryAdd(this, true);

    /// <summary>Hears every change of the state of the car with this VIN.</summary>
    internal void ListenToState(string vin, Action<string, VehicleSnapshot> listener)
    {
        _stateListeners[vin] = listener;
        Hold();
    }

    /// <summary>Frees the native client. Calls still in flight fail with <see cref="CancelledException"/>.</summary>
    internal void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0)
        {
            return;
        }
        _stateListeners.Clear();
        Listening.TryRemove(this, out _);
        _handle.Dispose();
    }

    /// <summary>
    /// The native client is gone, and no callback will run again: what its calls in flight
    /// would have reported is lost, so their tasks end here.
    /// </summary>
    internal void Released()
    {
        try
        {
            _events.Writer.TryComplete();
            if (Volatile.Read(ref _inFlight) == 0)
            {
                return;
            }
            foreach (var (call, pending) in InFlight)
            {
                if (ReferenceEquals(pending.Core, this) && InFlight.TryRemove(call, out var abandoned))
                {
                    abandoned.Completion.TrySetException(new CancelledException("the client was disposed"));
                }
            }
        }
        catch (Exception)
        {
            // This runs while a handle is released, where nothing may be thrown.
        }
    }

    // -- events --------------------------------------------------------------

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    private static unsafe void OnEvent(void* userData, byte* name, byte* payload)
    {
        // Nothing may be thrown into the core, and nothing here may wait.
        try
        {
            if (GCHandle.FromIntPtr((nint)userData).Target is ClientCore core)
            {
                core._events.Writer.TryWrite(new HostEvent(
                    Marshal.PtrToStringUTF8((nint)name) ?? "",
                    MemoryMarshal.CreateReadOnlySpanFromNullTerminated(payload).ToArray()));
            }
        }
        catch (Exception)
        {
            // An event that cannot be queued is dropped.
        }
    }

    /// <summary>The one reader of the client's events: callbacks run here, in order.</summary>
    private async Task DrainAsync()
    {
        await foreach (var hostEvent in _events.Reader.ReadAllAsync().ConfigureAwait(false))
        {
            if (Volatile.Read(ref _disposed) != 0)
            {
                continue;
            }
            var callback = CallbackOf(hostEvent.Name);
            if (callback is null)
            {
                // An event of a newer core.
                continue;
            }
            try
            {
                Dispatch(hostEvent);
            }
            catch (Exception error)
            {
                Report(callback, error);
            }
        }
    }

    /// <summary>The option that hears an event, as <see cref="ClientOptions.OnCallbackError"/> names it.</summary>
    private static string? CallbackOf(string name) => name switch
    {
        "vehicle_info" => nameof(ClientOptions.OnVehicleInfo),
        "mqtt_event" => nameof(ClientOptions.OnMqttEvent),
        "command_ack" => nameof(ClientOptions.OnCommandAck),
        "command_lifecycle" => nameof(ClientOptions.OnCommandLifecycle),
        "mqtt_connect" => nameof(ClientOptions.OnMqttConnect),
        "request" => nameof(ClientOptions.RequestObserver),
        "state_changed" => "OnStateChanged",
        "gate_policy" => nameof(ClientOptions.GatePolicy),
        "pin_resolver" => nameof(ClientOptions.PinResolver),
        "relogin" => nameof(ClientOptions.Relogin),
        _ => null,
    };

    private void Dispatch(HostEvent hostEvent)
    {
        var reader = new Utf8JsonReader(hostEvent.Payload);
        reader.Read();
        Wire.BeginObject(ref reader, "an event's payload");
        switch (hostEvent.Name)
        {
            case "vehicle_info":
            {
                string vin = "";
                VehicleRealtimeData? data = null;
                while (Wire.NextName(ref reader, out var field))
                {
                    switch (field)
                    {
                        case "vin":
                            vin = Wire.Str(ref reader);
                            break;
                        case "data":
                            data = VehicleRealtimeDataJson.ReadValue(ref reader);
                            break;
                        default:
                            reader.Skip();
                            break;
                    }
                }
                _options.OnVehicleInfo?.Invoke(vin, data!);
                break;
            }
            case "mqtt_event":
            {
                string name = "";
                string vin = "";
                IReadOnlyDictionary<string, JsonElement>? data = null;
                while (Wire.NextName(ref reader, out var field))
                {
                    switch (field)
                    {
                        case "event":
                            name = Wire.Str(ref reader);
                            break;
                        case "vin":
                            vin = Wire.Str(ref reader);
                            break;
                        case "data":
                            data = Wire.Object(ref reader);
                            break;
                        default:
                            reader.Skip();
                            break;
                    }
                }
                _options.OnMqttEvent?.Invoke(name, vin, data!);
                break;
            }
            case "command_ack":
                _options.OnCommandAck?.Invoke(Only(ref reader, "ack", CommandAckEventJson.ReadValue));
                break;
            case "command_lifecycle":
                _options.OnCommandLifecycle?.Invoke(Only(ref reader, "event", CommandLifecycleEventJson.ReadValue));
                break;
            case "mqtt_connect":
                _options.OnMqttConnect?.Invoke();
                break;
            case "request":
                _options.RequestObserver?.Invoke(Only(ref reader, "record", RequestRecordJson.ReadValue));
                break;
            case "state_changed":
            {
                string vin = "";
                VehicleSnapshot? state = null;
                while (Wire.NextName(ref reader, out var field))
                {
                    switch (field)
                    {
                        case "vin":
                            vin = Wire.Str(ref reader);
                            break;
                        case "state":
                            state = VehicleSnapshotJson.ReadValue(ref reader);
                            break;
                        default:
                            reader.Skip();
                            break;
                    }
                }
                if (_stateListeners.TryGetValue(vin, out var listener))
                {
                    listener(vin, state!);
                }
                break;
            }
            case "gate_policy":
                AnswerGatePolicy(ref reader);
                break;
            case "pin_resolver":
                AnswerPinResolver(ref reader);
                break;
            case "relogin":
            {
                long request = 0;
                while (Wire.NextName(ref reader, out var field))
                {
                    if (field == "request")
                    {
                        request = Wire.Int(ref reader);
                    }
                    else
                    {
                        reader.Skip();
                    }
                }
                if (_relogin?.Handler is { } handler)
                {
                    // Not awaited here: the events behind it are not held up by a login.
                    _ = Task.Run(() => AnswerReloginAsync(request, handler));
                }
                break;
            }
        }
    }

    /// <summary>The one field of a payload that has one.</summary>
    private static T Only<T>(ref Utf8JsonReader reader, string name, Reader<T> read)
        where T : class
    {
        T? value = null;
        while (Wire.NextName(ref reader, out var field))
        {
            if (field == name)
            {
                value = read(ref reader);
            }
            else
            {
                reader.Skip();
            }
        }
        return value ?? throw new JsonException($"the event has no {name}");
    }

    private void AnswerGatePolicy(ref Utf8JsonReader reader)
    {
        long request = 0;
        string vin = "";
        CommandGateVerdict? verdict = null;
        while (Wire.NextName(ref reader, out var field))
        {
            switch (field)
            {
                case "request":
                    request = Wire.Int(ref reader);
                    break;
                case "vin":
                    vin = Wire.Str(ref reader);
                    break;
                case "verdict":
                    verdict = CommandGateVerdictJson.ReadValue(ref reader);
                    break;
                default:
                    reader.Skip();
                    break;
            }
        }
        if (_options.GatePolicy?.Decide is not { } decide)
        {
            return;
        }
        try
        {
            Resolve(NewArgs().Add("request", request).Add("value", decide(vin, verdict!)));
        }
        catch (Exception error)
        {
            // Answered with the error, the core lets the verdict stand.
            Report(nameof(ClientOptions.GatePolicy), error);
            Resolve(NewArgs().Add("request", request).Add("error", error, Errors.Write));
        }
    }

    private void AnswerPinResolver(ref Utf8JsonReader reader)
    {
        long request = 0;
        string? commandPwd = null;
        string? controlPin = null;
        while (Wire.NextName(ref reader, out var field))
        {
            switch (field)
            {
                case "request":
                    request = Wire.Int(ref reader);
                    break;
                case "command_pwd":
                    commandPwd = Wire.Opt(ref reader, Wire.Str);
                    break;
                case "control_pin":
                    controlPin = Wire.Opt(ref reader, Wire.Str);
                    break;
                default:
                    reader.Skip();
                    break;
            }
        }
        if (_options.PinResolver is not { } resolver)
        {
            return;
        }
        try
        {
            Resolve(NewArgs().Add("request", request).Add("value", resolver(commandPwd, controlPin)));
        }
        catch (Exception error)
        {
            // Answered with the error, the core applies pyBYD's rule.
            Report(nameof(ClientOptions.PinResolver), error);
            Resolve(NewArgs().Add("request", request).Add("error", error, Errors.Write));
        }
    }

    private async Task AnswerReloginAsync(long request, Func<Task<SessionTokens>> handler)
    {
        try
        {
            var session = await handler().ConfigureAwait(false);
            Resolve(NewArgs().Add("request", request).Add("value", session, SessionTokensJson.WriteValue));
        }
        catch (Exception error)
        {
            // Not reported: it is the failure of the call that needed the session.
            Resolve(NewArgs().Add("request", request).Add("error", error, Errors.Write));
        }
    }

    /// <summary>Hands the core the answer to a question it asked.</summary>
    private void Resolve(Args answer)
    {
        try
        {
            CallSync("resolve", answer, Wire.Bool);
        }
        catch (Exception)
        {
            // The client is gone, and the question with it.
        }
    }

    private void Report(string callback, Exception error)
    {
        if (_options.OnCallbackError is not { } handler)
        {
            Trace.TraceWarning($"OpenByd: the {callback} callback threw: {error}");
            return;
        }
        try
        {
            handler(callback, error);
        }
        catch (Exception)
        {
            // A failing error handler has nowhere to report to.
        }
    }
}

/// <summary>The invoker of one car: every call names the car's number.</summary>
internal sealed class CarInvoker(ClientCore core, long car) : IInvoker
{
    public Args NewArgs() => new Args().Add("car", car);

    public Task<T> CallAsync<T>(string method, Args args, Reader<T> read) => core.CallAsync(method, args, read);

    public Task CallAsync(string method, Args args) => core.CallAsync(method, args);

    public T CallSync<T>(string method, Args args, Reader<T> read) => core.CallSync(method, args, read);

    public void CallSync(string method, Args args) => core.CallSync(method, args);
}
