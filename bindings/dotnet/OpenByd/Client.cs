using System.Text.Json;
using OpenByd.Models;

namespace OpenByd;

/// <summary>
/// The async client for the BYD API: pyBYD's <c>BydClient</c> with a Rust core.
/// </summary>
/// <remarks>
/// <para>
/// Methods are pyBYD's, in PascalCase; the ones that wait on the network end in
/// <c>Async</c>. What they return are pyBYD's models as records. Failures are pyBYD's
/// exception classes under <see cref="OpenBydException"/>
/// (<see cref="BydApiException"/> and its subclasses, with <c>Code</c> and
/// <c>Endpoint</c>); a method that returns a task reports every failure through the task.
/// </para>
/// <para>
/// Every client in a process shares one HTTP connection pool and one Rust runtime, so a
/// client is cheap: thousands can be created. All members are safe to use from several
/// threads at once.
/// </para>
/// <para>
/// Dispose the client when done with it. One that is dropped instead is collected like any
/// object, unless it listens for pushes (<see cref="ClientOptions.OnVehicleInfo"/>,
/// <see cref="ClientOptions.OnMqttEvent"/>, <see cref="ClientOptions.OnCommandAck"/>,
/// <see cref="ClientOptions.OnCommandLifecycle"/>, <see cref="ClientOptions.OnMqttConnect"/>,
/// or a car's state listener): that one stays alive, connected, until it is closed.
/// </para>
/// </remarks>
/// <example>
/// <code>
/// using var client = new Client(new BydConfig { Username = "you@example.com", Password = "..." });
/// await client.LoginAsync();
/// foreach (var vehicle in await client.GetVehiclesAsync())
/// {
///     var realtime = await client.GetVehicleRealtimeAsync(vehicle.Vin);
///     Console.WriteLine($"{vehicle.Vin} {realtime.ElecPercent}");
/// }
/// </code>
/// </example>
public sealed partial class Client : IDisposable
{
    private readonly ClientCore _core;
    private readonly Dictionary<long, Car> _cars = [];
    private volatile bool _closed;

    /// <summary>
    /// A client for an account, ready to use.
    /// </summary>
    /// <param name="config">pyBYD's config. A bad value throws <see cref="ValidationException"/>.</param>
    /// <param name="options">The callbacks and openbyd's own settings.</param>
    /// <exception cref="ValidationException">A value of the config does not validate.</exception>
    /// <exception cref="DataException">An option has a value that cannot be used.</exception>
    public Client(BydConfig config, ClientOptions? options = null)
        : this(config, options ?? new ClientOptions(), options?.Relogin)
    {
    }

    private Client(BydConfig config, ClientOptions options, ReloginPolicy? relogin)
    {
        ArgumentNullException.ThrowIfNull(config);
        _core = new ClientCore(config, options, relogin);
        try
        {
            _core.Start();
        }
        catch
        {
            _core.Dispose();
            throw;
        }
    }

    /// <summary>
    /// A client that resumes a session from <see cref="ExportSession"/>, without a password
    /// login.
    /// </summary>
    /// <remarks>
    /// With an empty <see cref="BydConfig.Password"/> it never logs in by itself
    /// (<see cref="ClientOptions.Relogin"/> defaults to <see cref="ReloginPolicy.Never"/>):
    /// when the session expires, calls fail with <see cref="BydSessionExpiredException"/>
    /// until the application imports a new one.
    /// </remarks>
    /// <param name="session">The saved session.</param>
    /// <param name="config">pyBYD's config; the password may be empty.</param>
    /// <param name="options">The callbacks and openbyd's own settings.</param>
    public static async Task<Client> FromSessionAsync(
        SessionTokens session, BydConfig config, ClientOptions? options = null)
    {
        ArgumentNullException.ThrowIfNull(config);
        var relogin = options?.Relogin
            ?? (string.IsNullOrEmpty(config.Password) ? ReloginPolicy.Never : ReloginPolicy.Password);
        var client = new Client(config, options ?? new ClientOptions(), relogin);
        try
        {
            await client.ImportSessionAsync(session).ConfigureAwait(false);
        }
        catch
        {
            client.Dispose();
            throw;
        }
        return client;
    }

    private IInvoker Invoker => _core;

    /// <summary>Whether <see cref="Close"/> was called and <see cref="Start"/> was not called since.</summary>
    public bool Closed => _closed;

    /// <summary>
    /// Opens the client again after <see cref="Close"/> (pyBYD's <c>async_start</c>). A new
    /// client is open already.
    /// </summary>
    public void Start()
    {
        _core.Start();
        _closed = false;
    }

    /// <summary>
    /// Closes the cars, stops MQTT and makes later calls fail with <see cref="BydException"/>
    /// (pyBYD's <c>async_close</c>), until <see cref="Start"/>. Calls already in flight
    /// finish; one waiting for an MQTT push stops waiting and polls over HTTP instead, as when
    /// the wait times out.
    /// </summary>
    public void Close()
    {
        ForgetCars();
        _core.Close();
        _closed = true;
    }

    /// <summary>
    /// Closes the client for good and frees its native client. Later calls throw
    /// <see cref="ObjectDisposedException"/>; calls still in flight fail with
    /// <see cref="CancelledException"/>. Safe to call more than once, and from a callback.
    /// </summary>
    public void Dispose()
    {
        _closed = true;
        ForgetCars();
        _core.Dispose();
    }

    private void ForgetCars()
    {
        lock (_cars)
        {
            _cars.Clear();
        }
    }

    /// <summary>
    /// The <see cref="Car"/> of a vehicle, built once and kept.
    /// </summary>
    /// <remarks>
    /// As pyBYD: a kept car is returned as it is, whatever the arguments. Otherwise the
    /// vehicle list is fetched unless <paramref name="vehicle"/> is given, then the
    /// capabilities.
    /// </remarks>
    /// <param name="vin">The vehicle identification number.</param>
    /// <param name="vehicle">The vehicle's raw payload; the vehicle list is fetched when null.</param>
    /// <param name="projectionTtl">How long a command's projections stay active, in seconds. Default <c>30</c>.</param>
    /// <param name="onStateChanged">
    /// Called after every change of the car's state, with the VIN and the new state. It runs
    /// like the callbacks of <see cref="ClientOptions"/>, and keeps the client alive until it
    /// is closed.
    /// </param>
    public async Task<Car> GetCarAsync(
        string vin,
        IReadOnlyDictionary<string, JsonElement>? vehicle = null,
        double? projectionTtl = null,
        Action<string, VehicleSnapshot>? onStateChanged = null)
    {
        var args = _core.NewArgs()
            .Add("vin", vin)
            .Add("vehicle", vehicle, Wire.WriteObject)
            .Add("projection_ttl", projectionTtl)
            .Add("state_events", onStateChanged is not null);
        var (number, builtVin) = await _core.CallAsync(args: args, method: "get_car", read: ReadCar).ConfigureAwait(false);
        lock (_cars)
        {
            if (!_cars.TryGetValue(number, out var car))
            {
                car = new Car(new CarInvoker(_core, number), builtVin);
                _cars[number] = car;
                if (onStateChanged is not null)
                {
                    _core.ListenToState(builtVin, onStateChanged);
                }
            }
            return car;
        }
    }

    private static (long Number, string Vin) ReadCar(ref Utf8JsonReader reader)
    {
        long number = 0;
        string vin = "";
        Wire.BeginObject(ref reader, "a car");
        while (Wire.NextName(ref reader, out var field))
        {
            switch (field)
            {
                case "car":
                    number = Wire.Int(ref reader);
                    break;
                case "vin":
                    vin = Wire.Str(ref reader);
                    break;
                default:
                    reader.Skip();
                    break;
            }
        }
        return (number, vin);
    }
}
