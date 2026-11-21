using System.Collections.Concurrent;
using System.Diagnostics;
using System.Runtime.CompilerServices;
using System.Text.Json;
using System.Text.Json.Nodes;
using OpenByd.Models;
using OpenByd.Testing;
using Xunit.Abstractions;
using static OpenByd.Tests.Names;

namespace OpenByd.Tests;

/// <summary>The client end to end against the fake server: reads, failures, every option, sessions.</summary>
[Collection(Sockets.Name)]
public sealed class ClientTests(ITestOutputHelper output)
{
    private static readonly JsonNode Done = World.Json("""{"controlState": 1, "requestSerial": "S1"}""");

    [Fact]
    public void TheLibraryUnderTestHasTheFakeServer()
    {
        Assert.True(TestSupport.IsAvailable, "build it with: cargo build -p openbyd-capi --features testing");
    }

    [Fact]
    public async Task ReadsReturnPyBydsDumpsTyped()
    {
        using var world = new World();
        var client = world.Client(config: world.Config(
            countryCode: "DE", language: "de", device: new DeviceProfile { Model = "Pixel" }));
        Assert.Null(client.ExportSession());
        Assert.False(client.CommandsEnabled);
        Assert.False(client.MqttIsRunning);
        await client.LoginAsync();

        var vehicle = Assert.Single(await client.GetVehiclesAsync());
        Assert.Equal(Vin, vehicle.Vin);
        Assert.Equal(EnergyType.Ev, vehicle.EnergyType);
        Assert.Equal("SEAL", vehicle.ModelName);
        Assert.Equal(["vin", "energyType", "modelName"], vehicle.Raw.Keys);
        Assert.Equal("0", vehicle.Raw["energyType"].GetString());

        var login = world.Server.Requests(Login)[0];
        var outer = Assert.NotNull(login.Outer);
        Assert.Equal("DE", outer.GetProperty("countryCode").GetString());
        Assert.Equal("de", outer.GetProperty("language").GetString());
        Assert.Equal("Pixel", outer.GetProperty("model").GetString());

        Assert.Equal(AcSwitch.Off, (await client.GetHvacStatusAsync(Vin)).AcSwitch);
        Assert.True((await client.GetVehicleCapabilitiesAsync(Vin)).Lock);
        Assert.Equal(Vin, (await client.GetVehicleCapabilitiesAsync(Vin, forceRefresh: true)).Vin);
        var configs = await client.GetLatestConfigsAsync([Vin]);
        Assert.Equal([Vin], configs.Keys);
        Assert.NotNull(await client.GetLatestConfigAsync(Vin));

        world.Server.Reply(Homepage, Reply.Data(World.Json("""{"elecPercent": 80, "chargingState": 0}""")));
        var homepage = await client.GetChargingHomepageAsync(Vin);
        Assert.Equal(80, homepage.Status.Soc);
        Assert.NotNull(homepage.Schedule.Vin);

        var session = await client.EnsureSessionAsync();
        Assert.NotEmpty(session.SignToken);
        Assert.True(session.Ttl > 0);
        Assert.Equal("SEAL", client.VehicleMetadata(Vin)?.ModelName);
        Assert.Null(client.VehicleMetadata("TESTVIN0000000009"));
        var diagnostics = client.GetCommandAckDiagnostics();
        Assert.Equal((0L, 0L), (diagnostics.Pending, diagnostics.Matched));
    }

    [Fact]
    public async Task FailuresArePyBydsExceptionClassesWithTheirFields()
    {
        using var world = new World();
        var client = world.Client();
        world.Server.Reply(Homepage, Reply.Code("1001"));
        var unsupported = await Assert.ThrowsAsync<BydEndpointNotSupportedException>(
            () => client.GetChargingStatusAsync(Vin));
        Assert.True(unsupported is BydApiException and BydException and OpenBydException);
        Assert.Equal("BydEndpointNotSupportedError", unsupported.Kind);
        Assert.Equal("1001", unsupported.Code);
        Assert.Equal(Homepage, unsupported.Endpoint);

        world.Server.Reply(Hvac, Reply.Status(503, "down"));
        var transport = await Assert.ThrowsAsync<BydTransportException>(() => client.GetHvacStatusAsync(Vin));
        Assert.Equal(503, transport.StatusCode);
        Assert.Equal(Hvac, transport.Endpoint);

        // What pydantic refuses in pyBYD is a ValidationException, with where and why.
        var invalid = await Assert.ThrowsAsync<ValidationException>(
            () => client.SetSeatClimateAsync(Vin, new SeatClimateParams { MainHeat = 9 }));
        Assert.True(invalid is DataException);
        Assert.Equal("ValidationError", invalid.Kind);
        Assert.Equal(["main_heat"], invalid.Loc);
        Assert.NotEmpty(invalid.ErrorType);

        var category = await Assert.ThrowsAsync<DataException>(
            () => client.GetNotificationsAsync((NotificationCategory)9));
        Assert.Equal("ValueError", category.Kind);
    }

    [Fact]
    public async Task ANumberJsonCannotHoldIsRefusedNotSent()
    {
        using var world = new World();
        var client = world.Client();
        foreach (var bad in new[] { double.NaN, double.PositiveInfinity, double.NegativeInfinity })
        {
            var argument = await Assert.ThrowsAsync<DataException>(
                () => client.GetVehicleRealtimeAsync(Vin, pollInterval: bad));
            Assert.Equal("ValueError", argument.Kind);
            Assert.Contains("poll_interval", argument.Message);
            var field = await Assert.ThrowsAsync<DataException>(
                () => client.StartClimateAsync(Vin, new ClimateStartParams { Temperature = bad }));
            Assert.Equal("ValueError", field.Kind);
            Assert.Throws<DataException>(() => new Client(new BydConfig
            {
                Username = User, Password = Password, SessionTtl = bad,
            }));
        }
        Assert.Empty(world.Server.Requests());
    }

    [Fact]
    public void ABadConfigIsRefusedWhenTheClientIsMade()
    {
        var config = new BydConfig { Username = User, Password = Password };
        Assert.Throws<ValidationException>(() => new Client(new BydConfig { Username = User, Password = null! }));
        Assert.Throws<DataException>(() => new Client(config, new ClientOptions { CommandRetries = 0 }));
        Assert.Throws<DataException>(
            () => new Client(config, new ClientOptions { CommandAckTtlSeconds = double.PositiveInfinity }));
        Assert.Throws<ArgumentNullException>(() => new Client(null!));
        new Client(config, new ClientOptions { GatePolicy = GatePolicy.Strict, Relogin = ReloginPolicy.Password }).Dispose();
    }

    [Fact]
    public async Task AClientFailsAfterCloseAndWorksAgainAfterStart()
    {
        using var world = new World();
        var client = world.Client();
        Assert.False(client.Closed);
        Assert.Equal(AcSwitch.Off, (await client.GetHvacStatusAsync(Vin)).AcSwitch);
        client.Close();
        Assert.True(client.Closed);
        var closed = await Assert.ThrowsAnyAsync<BydException>(() => client.GetHvacStatusAsync(Vin));
        Assert.Contains("not initialized", closed.Message);
        client.Start();
        Assert.False(client.Closed);
        Assert.Equal(AcSwitch.Off, (await client.GetHvacStatusAsync(Vin)).AcSwitch);
        using (var scoped = world.Client())
        {
            await scoped.LoginAsync();
        }
    }

    [Fact]
    public async Task ADisposedClientRefusesCallsAndEndsTheOnesInFlight()
    {
        using var world = new World();
        world.Server.Reply(Hvac, Reply.Data(World.Json("""{"acSwitch": 1}""")).WithDelay(30));
        var client = world.Client();
        await client.LoginAsync();
        var inFlight = client.GetHvacStatusAsync(Vin);
        await World.Until("the request", () => world.Server.Requests(Hvac).Count == 1);
        client.Dispose();
        var cancelled = await Assert.ThrowsAsync<CancelledException>(() => inFlight.WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.Equal("CancelledError", cancelled.Kind);

        await Assert.ThrowsAsync<ObjectDisposedException>(() => client.GetHvacStatusAsync(Vin));
        Assert.Throws<ObjectDisposedException>(() => client.ExportSession());
        Assert.Throws<ObjectDisposedException>(() => client.Start());
        Assert.True(client.Closed);
        client.Dispose();
    }

    [Fact]
    public async Task AClientCanBeDisposedFromACallback()
    {
        using var world = new World();
        Client? client = null;
        var disposed = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        client = world.Client(new ClientOptions
        {
            TestTls = true,
            RequestObserver = _ =>
            {
                var onThreadPool = Thread.CurrentThread.IsThreadPoolThread;
                client!.Dispose();
                disposed.TrySetResult(onThreadPool);
            },
        });
        // The call may finish or be cut short by the disposal; it must not hang.
        var login = client.LoginAsync();
        Assert.True(await disposed.Task.WaitAsync(TimeSpan.FromSeconds(10)), "callbacks run on the thread pool");
        var outcome = await Task.WhenAny(login, Task.Delay(TimeSpan.FromSeconds(10)));
        Assert.Same(login, outcome);
        Assert.True(login.IsCompletedSuccessfully || login.Exception?.InnerException is CancelledException);
    }

    [Fact]
    public async Task DisposingAmidCallsNeitherHangsNorCrashes()
    {
        using var world = new World();
        world.Server.Reply(Hvac, Reply.Data(World.Json("""{"acSwitch": 1}""")).WithDelay(0.005));
        var outcomes = new List<Task<string>>();
        for (var round = 0; round < 16; round++)
        {
            var client = world.Client();
            // Early enough to cut some calls short, late enough to let others finish.
            var delay = round * 40;
            for (var call = 0; call < 6; call++)
            {
                outcomes.Add(Task.Run(async () =>
                {
                    try
                    {
                        await client.GetHvacStatusAsync(Vin);
                        // A call that answers at once, on a client that may be going away.
                        _ = client.CommandsEnabled;
                        return "answered";
                    }
                    catch (CancelledException)
                    {
                        return "cancelled";
                    }
                    catch (ObjectDisposedException)
                    {
                        return "disposed";
                    }
                }));
            }
            outcomes.Add(Task.Run(async () =>
            {
                await Task.Delay(delay);
                client.Dispose();
                return "disposed";
            }));
        }
        // Every call ends one of the three ways; none is left waiting, none fails otherwise.
        var ended = await Task.WhenAll(outcomes).WaitAsync(TimeSpan.FromSeconds(60));
        Assert.Equal(16 * 7, ended.Length);
        output.WriteLine(string.Join(", ", ended.GroupBy(outcome => outcome).Select(group => $"{group.Count()} {group.Key}")));
    }

    private static async Task<object> LockWith(World world, ClientOptions options, bool missing = false)
    {
        world.Server.Reply(LatestConfig, Reply.Data(new JsonObject
        {
            [Vin] = missing ? World.Functions() : World.Functions("1005"),
        }));
        world.Server.Reply(Remote, Reply.Data(Done));
        var client = world.Client(options);
        await client.VerifyCommandAccessAsync(Vin);
        Assert.True(client.CommandsEnabled);
        try
        {
            return await client.LockAsync(Vin);
        }
        catch (OpenBydException error)
        {
            return error;
        }
    }

    [Fact]
    public async Task TheGatePolicyDecidesWhetherACommandIsSent()
    {
        using var world = new World();
        var refused = Assert.IsType<BydEndpointNotSupportedException>(
            await LockWith(world, new ClientOptions(), missing: true));
        Assert.Equal("command_gate_blocked", refused.Code);
        Assert.Empty(world.Server.Requests(Remote));

        var allowed = Assert.IsType<RemoteControlResult>(
            await LockWith(world, new ClientOptions { GatePolicy = GatePolicy.AllowMissingData }, missing: true));
        Assert.True(allowed.Success);

        var verdicts = new ConcurrentQueue<(string Vin, CommandGateVerdict Verdict)>();
        var decided = Assert.IsType<RemoteControlResult>(await LockWith(
            world,
            new ClientOptions
            {
                GatePolicy = GatePolicy.Custom((vin, verdict) =>
                {
                    verdicts.Enqueue((vin, verdict));
                    return verdict.Reason == "function_no_missing";
                }),
            },
            missing: true));
        Assert.True(decided.Success);
        var asked = Assert.Single(verdicts);
        Assert.Equal(Vin, asked.Vin);
        Assert.False(asked.Verdict.Supported);
        Assert.Equal(RemoteCommand.Lock, asked.Verdict.Command);

        // A policy that throws falls back to the verdict, and the failure is reported.
        var reported = new ConcurrentQueue<(string Callback, Exception Error)>();
        var broken = await LockWith(
            world,
            new ClientOptions
            {
                GatePolicy = GatePolicy.Custom((_, _) => throw new InvalidOperationException("policy bug")),
                OnCallbackError = (callback, error) => reported.Enqueue((callback, error)),
            },
            missing: true);
        Assert.IsType<BydEndpointNotSupportedException>(broken);
        var failure = Assert.Single(reported);
        Assert.Equal("GatePolicy", failure.Callback);
        Assert.Equal("policy bug", failure.Error.Message);
    }

    [Fact]
    public async Task ThePinResolverReplacesPyBydsRule()
    {
        using var world = new World();
        const string digest = "0123456789abcdef0123456789abcdef";
        var calls = new ConcurrentQueue<(string? CommandPwd, string? ControlPin)>();
        await LockWith(world, new ClientOptions
        {
            PinResolver = (commandPwd, controlPin) =>
            {
                calls.Enqueue((commandPwd, controlPin));
                return digest;
            },
        });
        Assert.Contains(((string?)null, (string?)Pin), calls);
        Assert.Equal(digest, SentCommandPwd(world));

        // An answer that is no string falls back to pyBYD's rule: MD5, upper case.
        world.Server.ClearRequests();
        await LockWith(world, new ClientOptions { PinResolver = (_, _) => null! });
        Assert.Equal("E10ADC3949BA59ABBE56E057F20F883E", SentCommandPwd(world));

        // So does a resolver that throws, and the failure is reported.
        world.Server.ClearRequests();
        var reported = new ConcurrentQueue<string>();
        await LockWith(world, new ClientOptions
        {
            PinResolver = (_, _) => throw new InvalidOperationException("resolver bug"),
            OnCallbackError = (callback, _) => reported.Enqueue(callback),
        });
        Assert.Equal("E10ADC3949BA59ABBE56E057F20F883E", SentCommandPwd(world));
        Assert.Contains("PinResolver", reported);
    }

    private static string? SentCommandPwd(World world) =>
        world.Server.Requests(Remote)[0].Inner.GetProperty("commandPwd").GetString();

    [Fact]
    public async Task ACommandIsRetriedAsManyTimesAsAsked()
    {
        using var world = new World();
        world.Server.Reply(LatestConfig, Reply.Data(new JsonObject { [Vin] = World.Functions("1005") }));
        world.Server.Reply(Remote, Reply.Data(World.Json("""{"controlState": 2, "requestSerial": "S1"}""")));
        var client = world.Client(new ClientOptions { CommandRetries = 1 });
        await client.VerifyCommandAccessAsync(Vin);
        var failed = await Assert.ThrowsAnyAsync<BydApiException>(() => client.LockAsync(Vin));
        Assert.Equal("2", failed.Code);
        Assert.Single(world.Server.Requests(Remote));

        world.Server.Script(Remote, Reply.Code("5005"));
        await Assert.ThrowsAsync<BydControlPasswordException>(() => client.LockAsync(Vin, commandPwd: "654321"));
    }

    [Fact]
    public async Task TheRequestObserverHearsEveryRequest()
    {
        using var world = new World();
        var records = new ConcurrentQueue<RequestRecord>();
        world.Server.Reply(Homepage, Reply.Code("1001"));
        var client = world.Client(new ClientOptions { RequestObserver = records.Enqueue });
        await client.LoginAsync();
        await client.GetHvacStatusAsync(Vin);
        await Assert.ThrowsAsync<BydEndpointNotSupportedException>(() => client.GetChargingStatusAsync(Vin));
        // Callbacks run on their own reader, so the last one may come after the call returned.
        await World.Until("three records", () => records.Count == 3);
        Assert.Equal(
            new (string, string, string?)[] { (Login, "reply", "0"), (Hvac, "reply", "0"), (Homepage, "reply", "1001") },
            records.Select(record => (record.Endpoint, record.Outcome, record.Code)));
        Assert.All(records, record => Assert.True(record.Duration >= 0));

        // An observer that throws is reported and does not affect the client.
        var reported = new ConcurrentQueue<string>();
        var noisy = world.Client(new ClientOptions
        {
            RequestObserver = _ => throw new InvalidOperationException("observer bug"),
            OnCallbackError = (callback, _) => reported.Enqueue(callback),
        });
        Assert.Equal(AcSwitch.Off, (await noisy.GetHvacStatusAsync(Vin)).AcSwitch);
        await World.Until("the report", () => reported.Contains("RequestObserver"));
    }

    private sealed class Captured : TraceListener
    {
        public ConcurrentQueue<string> Messages { get; } = new();

        public override void Write(string? message) => Messages.Enqueue(message ?? "");

        public override void WriteLine(string? message) => Messages.Enqueue(message ?? "");
    }

    [Fact]
    public async Task ACallbacksFailureIsATraceWarningByDefault()
    {
        using var world = new World();
        var captured = new Captured();
        Trace.Listeners.Add(captured);
        try
        {
            var client = world.Client(new ClientOptions
            {
                RequestObserver = _ => throw new InvalidOperationException("observer bug"),
            });
            await client.LoginAsync();
            await World.Until("the warning", () => captured.Messages.Any(
                message => message.Contains("RequestObserver") && message.Contains("observer bug")));
        }
        finally
        {
            Trace.Listeners.Remove(captured);
        }
    }

    [Fact]
    public async Task SessionsAreExportedRestoredAndRenewed()
    {
        using var world = new World();
        var first = world.Client();
        await first.LoginAsync();
        var saved = World.Session(first);
        Assert.True(saved.TtlRemaining > 0);
        Assert.DoesNotContain(saved.SignToken, saved.ToString());
        Assert.DoesNotContain(saved.EncryToken, saved.ToString());
        Assert.Contains(saved.UserId, saved.ToString());

        var withoutPassword = world.Config(password: "");
        var options = new ClientOptions { TestTls = true };
        using (var restored = await Client.FromSessionAsync(saved, withoutPassword, options))
        {
            Assert.Equal(saved.SignToken, restored.ExportSession()?.SignToken);
            Assert.Equal(AcSwitch.Off, (await restored.GetHvacStatusAsync(Vin)).AcSwitch);
            Assert.Single(world.Server.Requests(Login));

            world.Server.ExpireSessions(User);
            var expired = await Assert.ThrowsAsync<BydSessionExpiredException>(() => restored.GetHvacStatusAsync(Vin));
            Assert.Equal("relogin_disabled", expired.Code);
            restored.InvalidateSession();
        }
        await Assert.ThrowsAsync<DataException>(
            () => Client.FromSessionAsync(saved with { UserId = "" }, withoutPassword, options));
        // A session's lifetime is a float of a model: the core refuses the one JSON cannot hold.
        var endless = await Assert.ThrowsAsync<DataException>(
            () => first.ImportSessionAsync(saved with { TtlRemaining = double.NaN }));
        Assert.Equal("ValueError", endless.Kind);

        // A relogin function hands over a new session when one is needed.
        var asked = 0;
        var renewing = world.Client(new ClientOptions
        {
            TestTls = true,
            Relogin = ReloginPolicy.Custom(async () =>
            {
                Interlocked.Increment(ref asked);
                var helper = world.Client();
                await helper.LoginAsync();
                return World.Session(helper);
            }),
        });
        Assert.Equal(AcSwitch.Off, (await renewing.GetHvacStatusAsync(Vin)).AcSwitch);
        Assert.Equal(1, asked);

        // What it throws is what the call throws.
        var refusing = world.Client(new ClientOptions
        {
            Relogin = ReloginPolicy.Custom(
                () => throw new BydAuthenticationException("no", code: "1002", endpoint: "x")),
        });
        var refused = await Assert.ThrowsAsync<BydAuthenticationException>(() => refusing.GetHvacStatusAsync(Vin));
        Assert.Equal(("no", "1002", "x"), (refused.Message, refused.Code, refused.Endpoint));

        var broken = world.Client(new ClientOptions
        {
            Relogin = ReloginPolicy.Custom(() => throw new InvalidOperationException("bug")),
        });
        var failure = await Assert.ThrowsAnyAsync<BydException>(() => broken.GetHvacStatusAsync(Vin));
        Assert.Contains("InvalidOperationException: bug", failure.Message);

        var nonsense = world.Client(new ClientOptions
        {
            Relogin = ReloginPolicy.Custom(() => Task.FromResult<SessionTokens>(null!)),
        });
        var none = await Assert.ThrowsAnyAsync<OpenBydException>(() => nonsense.GetHvacStatusAsync(Vin));
        Assert.Contains("relogin returned no session", none.Message);
    }

    [Fact]
    public async Task SeededMetadataSavesTheVehicleList()
    {
        using var world = new World();
        world.Server.Reply(Realtime, Reply.Data(World.Json("""{"elecPercent": 50, "time": 1700000000}""")));
        var client = world.Client();
        client.SeedVehicleMetadata(Vin, EnergyType.Hybrid, modelName: "SONG");
        Assert.Equal(EnergyType.Hybrid, client.VehicleMetadata(Vin)?.EnergyType);
        var realtime = await client.GetVehicleRealtimeAsync(Vin, pollAttempts: 0, mqttTimeout: 0);
        Assert.Equal(50, realtime.ElecPercent);
        Assert.Empty(world.Server.Requests(Vehicles));
        try
        {
            Assert.Empty(await client.GetNotificationsAsync(NotificationCategory.System));
        }
        catch (OpenBydException)
        {
            // The fake server has no inbox; that the category crosses is the point.
        }
    }

    [Fact]
    public async Task ManyClientsAreCheapAndRunAtOnce()
    {
        using var world = new World();
        world.Server.Reply(Hvac, Reply.Data(World.Json("""{"acSwitch": 1}""")).WithDelay(0.02));
        var clients = Enumerable.Range(0, 100).Select(_ => world.Client()).ToList();
        var statuses = await Task.WhenAll(clients.Select(client => client.GetHvacStatusAsync(Vin)));
        Assert.All(statuses, status => Assert.Equal(AcSwitch.Off, status.AcSwitch));
        Assert.Equal(100, world.Server.Requests(Login).Count);
    }

    [Fact]
    public async Task ACallInFlightKeepsItsClientAndADroppedClientIsCollected()
    {
        using var world = new World();
        world.Server.Reply(Hvac, Reply.Data(World.Json("""{"acSwitch": 1}""")).WithDelay(0.3));
        var (dropped, inFlight) = StartAndDrop(world);
        await World.Until("the request", () => world.Server.Requests(Hvac).Count == 1);
        Collect();
        // Nobody refers to the client, but its call still completes.
        Assert.Equal(AcSwitch.Off, (await inFlight).AcSwitch);
        await World.Until("the collection", () =>
        {
            Collect();
            return !dropped.TryGetTarget(out _);
        });

        // A client that listens for pushes is waiting for them: it stays until it is closed.
        var listening = Listen(world);
        Collect();
        Assert.True(listening.TryGetTarget(out var kept));
        kept.Close();
        kept = null;
        await World.Until("the collection of the closed client", () =>
        {
            Collect();
            return !listening.TryGetTarget(out _);
        });
    }

    /// <summary>
    /// Starts a call on a client nobody keeps. What is returned tells when the client's
    /// connection to the core is collected: an object only its options refer to.
    /// </summary>
    [MethodImpl(MethodImplOptions.NoInlining)]
    private static (WeakReference<object> Core, Task<HvacStatus> Call) StartAndDrop(World world)
    {
        var marker = new object();
        // Not through the world, which would keep the client.
        var client = new Client(world.Config(), new ClientOptions { RequestObserver = _ => GC.KeepAlive(marker) });
        return (new WeakReference<object>(marker), client.GetHvacStatusAsync(Vin));
    }

    /// <summary>A client that listens for pushes, which only its own callback refers to.</summary>
    [MethodImpl(MethodImplOptions.NoInlining)]
    private static WeakReference<Client> Listen(World world)
    {
        Client? client = null;
        client = new Client(world.Config(), new ClientOptions { OnMqttConnect = () => GC.KeepAlive(client) });
        return new WeakReference<Client>(client);
    }

    private static void Collect()
    {
        GC.Collect();
        GC.WaitForPendingFinalizers();
        GC.Collect();
    }
}
