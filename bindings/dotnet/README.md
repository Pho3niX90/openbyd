# OpenByd for .NET

An unofficial client for the BYD overseas app API: [pyBYD](https://github.com/jkaberg/pyBYD)'s `BydClient` and `BydCar` for C#, on openbyd's Rust core. Every request it builds and every reply it parses is compared with pyBYD in the repository's tests.

This project is not affiliated with, endorsed by or supported by BYD. It has not yet been run against BYD's live service; see the [status](../../README.md#status) of the whole project.

The package is `OpenByd`, for .NET 8 and later. It is not on NuGet yet: build it from this repository ([below](#build-and-test)).

## Use it

```csharp
using OpenByd;

using var client = new Client(new BydConfig { Username = "you@example.com", Password = "..." });
await client.LoginAsync();
foreach (var vehicle in await client.GetVehiclesAsync())
{
    var realtime = await client.GetVehicleRealtimeAsync(vehicle.Vin);
    Console.WriteLine($"{vehicle.Vin} {realtime.ElecPercent}");
}
```

A car, with its state and its commands by feature:

```csharp
var car = await client.GetCarAsync(vin, onStateChanged: (vin, state) => Console.WriteLine(state.Realtime?.ElecPercent));
await client.VerifyCommandAccessAsync(vin);
await car.UpdateRealtimeAsync();
if (car.Lock.Available)
{
    await car.Lock.LockAsync();
}
await car.Hvac.StartAsync(temperature: 21, duration: 20);
```

Methods and arguments are pyBYD's, in PascalCase: `get_vehicle_realtime` is `GetVehicleRealtimeAsync`, and the ones that answer at once have no `Async` (`ExportSession`, `car.State`). An optional argument left out, or null, takes pyBYD's default.

## Results

Results are pyBYD's models as records in `OpenByd.Models`, with pyBYD's enums: `realtime.ElecPercent`, `vehicle.EnergyType == EnergyType.Ev`. A model's `Raw` is the reply it was parsed from.

Serialised with `System.Text.Json`, a model gives exactly pyBYD's `model_dump(mode="json")`, under pyBYD's snake_case names, and reads back from it. Reading is strict: an unknown or missing field, a null where the type has none or a value an enum does not have is a `JsonException`. A float JSON cannot hold is written as the string `"NaN"`, `"Infinity"` or `"-Infinity"`.

`OpenBydCore` has what needs no client: `ParseModel<GpsInfo>(json)` parses a raw payload as one of pyBYD's models, `EncodeEnvelope` and `DecodeEnvelope` are the envelope cipher, `Version` is the core's version.

## Errors

Failures are pyBYD's exception classes, with `Exception` where pyBYD says `Error`, under `OpenBydException`:

```csharp
try
{
    await client.LockAsync(vin);
}
catch (BydControlPasswordException error)
{
    Console.WriteLine($"{error.Code} at {error.Endpoint}");
}
catch (BydApiException error) when (error.Code == "6024")
{
    // A previous command is still in progress.
}
```

`BydApiException` and its subclasses carry `Code` and `Endpoint`; `BydTransportException` carries `StatusCode` and `Endpoint`. A value that cannot be used (pyBYD's `ValueError` or `TypeError`) is a `DataException`, and what pydantic refuses is a `ValidationException` with `Loc` and `ErrorType`. `Kind` is always the name of the Python class pyBYD raises. A number that is NaN or infinite is refused with `DataException` instead of sent.

A method that returns a task reports every failure through the task.

## Callbacks

`ClientOptions` holds the callbacks (`OnVehicleInfo`, `OnMqttEvent`, `OnCommandAck`, `OnCommandLifecycle`, `OnMqttConnect`, `RequestObserver`) and openbyd's settings:

```csharp
using var client = new Client(config, new ClientOptions
{
    OnVehicleInfo = (vin, data) => Console.WriteLine($"{vin} {data.ElecPercent}"),
    GatePolicy = GatePolicy.AllowMissingData,
    Relogin = ReloginPolicy.Custom(LoadSessionAsync),
    OnCallbackError = (callback, error) => logger.LogWarning(error, "{Callback} threw", callback),
});
```

Callbacks never run on the core's threads. Each client hands its events, in order, to one reader on the thread pool, so a callback that blocks holds back the ones behind it. A callback that throws is reported to `OnCallbackError` (by default as a warning to `System.Diagnostics.Trace`) and does not affect the client; a relogin function is the exception, since what it throws is the failure of the call that needed the session.

## Lifetime

Every client in a process shares one HTTP connection pool and one Rust runtime, so a client is cheap. All of its members can be used from several threads at once.

`Dispose` closes a client for good: later calls throw `ObjectDisposedException`, and calls still in flight fail with `CancelledException`. `Close` and `Start` are pyBYD's `async_close` and `async_start`: a closed client fails its calls with `BydException` until it is started again.

A client that is dropped without being disposed is collected like any object, once its calls in flight have finished. One that listens for pushes (any callback above except `RequestObserver`, or a car's `onStateChanged`) is waiting for them, and stays alive and connected until it is closed.

`Client.FromSessionAsync(session, config)` resumes a session saved from `ExportSession()` without a password login.

## The native library

The package calls openbyd's C library, `openbyd_c` (`crates/openbyd-capi`), through P/Invoke.

- **A packed package** carries it for each platform under `runtimes/<rid>/native/`, where .NET finds it by itself. Whatever is in `OpenByd/runtimes/` when `dotnet pack` runs is packed there; nothing is checked in.
- **In this repository** the library is copied next to whatever is built on the project, from the directory the MSBuild property `OpenBydNativeDir` names. By default that is the repository's `target/debug`, where `cargo build -p openbyd-capi` puts it. Build with `-p:OpenBydNativeDir=<directory>` to take it from elsewhere, such as `target/release`.

## Testing your own code

`OpenByd.Testing` has a fake BYD server that speaks the real wire protocol:

```csharp
using var server = new FakeServer(new Account { Username = "user@example.com", Password = "secret" });
server.Reply("/control/getStatusNow", Reply.Data(new JsonObject { ["acSwitch"] = 1 }));
using var client = new Client(new BydConfig
{
    Username = "user@example.com", Password = "secret", BaseUrl = server.Url, MqttEnabled = false,
});
```

It needs a C library built with the cargo feature `testing`, which a released library is not; `TestSupport.IsAvailable` tells which one is loaded.

## Build and test

```sh
cargo build -p openbyd-capi --features testing   # the C library, with the fake server
cd bindings/dotnet
dotnet test
```

The tests run the client against the fake server and an MQTT test broker, run every recorded pyBYD vector through the C library, and read every recorded pyBYD model dump into its record and back.

The files in `OpenByd/Generated` are written by `python bindings/generate.py dotnet` from `bindings/spec.py` and the pyBYD installed; see [the bindings](../README.md).
