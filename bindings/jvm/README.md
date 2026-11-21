# openbyd for Java and Kotlin

An unofficial client for the BYD overseas app API: [pyBYD](https://github.com/jkaberg/pyBYD)'s client with a Rust core, for the JVM. The Rust core is [openbyd](../../README.md); this package loads it as a C library through JNA and puts a typed layer on top.

This project is not affiliated with, endorsed by or supported by BYD.

## Install

Java 17 or later.

```xml
<dependency>
  <groupId>io.github.pho3nix90</groupId>
  <artifactId>openbyd</artifactId>
  <version>0.1.3</version>
</dependency>
```

The jar is on Maven Central and carries openbyd's native library for Windows (x64), Linux (x64 and arm64, glibc) and macOS (arm64 and x64). On another platform, such as Alpine, see [The native library](#the-native-library).

## Use it from Java

```java
import io.github.pho3nix90.openbyd.Client;
import io.github.pho3nix90.openbyd.model.BydConfig;
import io.github.pho3nix90.openbyd.model.Vehicle;
import io.github.pho3nix90.openbyd.model.VehicleRealtimeData;

BydConfig config = new BydConfig("you@example.com", "...");
try (Client client = new Client(config)) {
    client.login().get();
    for (Vehicle vehicle : client.getVehicles().get()) {
        VehicleRealtimeData realtime = client.getVehicleRealtime(vehicle.vin()).get();
        System.out.println(vehicle.vin() + " " + realtime.elecPercent());
    }
}
```

## Use it from Kotlin

```kotlin
import io.github.pho3nix90.openbyd.Client
import io.github.pho3nix90.openbyd.model.BydConfig

Client(BydConfig("you@example.com", "...")).use { client ->
    client.login().get()
    for (vehicle in client.getVehicles().get()) {
        val realtime = client.getVehicleRealtime(vehicle.vin).get()
        println("${vehicle.vin} ${realtime.elecPercent}")
    }
}
```

Every type is null-marked ([JSpecify](https://jspecify.dev)), so Kotlin knows which values may be missing: `vehicle.vin` is a `String`, `realtime.elecPercent` a `Double?`. In a coroutine, await a future with `await()` from `kotlinx-coroutines-jdk8`.

## How it behaves

**Methods** are pyBYD's, in camelCase: `get_vehicle_realtime` is `getVehicleRealtime`. One that waits on the network returns a `CompletableFuture`; one that answers at once (`exportSession()`, `commandsEnabled()`, `car.state()`) returns its value. Optional arguments are an options object with fluent setters, and a method that has them also comes without:

```java
client.getVehicleRealtime(vin);
client.getVehicleRealtime(vin, new PollOptions().pollAttempts(3).mqttTimeout(5));
client.startClimate(vin, new ClimateStartParams().temperature(21).timeSpan(2));
```

What is not set is not sent, so the core applies pyBYD's default. Preconditioning plans use `getStandbyPlans`, `saveStandbyPlan`, `createStandbyPlan`, `setDefaultStandbyPlan`, `deleteStandbyPlan` and `runStandbyPlan`; on a daily `StandbySchedule`, call `.zone("Europe/Madrid")` (an IANA time zone name) so the next run follows that zone's wall clock across daylight-saving changes.

**Results** are pyBYD's models as records, exactly what `model_dump(mode="json")` gives: the same fields, in camelCase, with pyBYD's enums as Java enums (`EnergyType.EV`, whose `value()` is pyBYD's). An integer is a `long`, a float a `double`, a date or time the string pyBYD writes, and `raw` the reply as BYD sent it. A float JSON cannot hold (NaN, an infinity) is that `double`.

**Errors** are pyBYD's exception classes under `OpenBydException`, a `RuntimeException`, named `Exception` where pyBYD says `Error`: `BydApiException` and its subclasses carry `code()` and `endpoint()`, `BydTransportException` carries `statusCode()`. `kind()` is the name of the Python class pyBYD raises. A Python built-in pyBYD lets through (`ValueError`, `TypeError`) is a `DataException`, and pydantic's `ValidationError` is a `ValidationException`. A future is completed with the exception (`get()` wraps it in an `ExecutionException`); a method that answers at once throws it.

```java
try {
    client.lock(vin).get();
} catch (ExecutionException failed) {
    if (failed.getCause() instanceof BydApiException error) {
        System.err.println(error.code() + " at " + error.endpoint());
    }
}
```

A number JSON cannot hold (NaN, an infinity) is refused as an argument with a `DataException`, not sent as something else.

**A car**, pyBYD's `BydCar`, comes from `client.getCar(vin)`. It holds the vehicle's merged state and offers its commands by feature:

```java
Car car = client.getCar(vin).get();
car.updateRealtime().get();
if (car.lock().available()) {
    car.lock().lock().get();
}
car.hvac().start(new ClimateOptions().temperature(21).duration(20)).get();
VehicleSnapshot state = car.state();
```

**Callbacks** are set on `ClientOptions`:

```java
ClientOptions options = new ClientOptions()
        .onVehicleInfo((carVin, data) -> System.out.println(carVin + " " + data.elecPercent()))
        .requestObserver(record -> metrics.record(record.endpoint(), record.duration()))
        .gatePolicy(GatePolicy.ALLOW_MISSING_DATA)
        .onCallbackError((callback, error) -> log.warn("{} failed", callback, error));
Client client = new Client(config, options);
```

Each client runs its callbacks on one daemon thread of its own, in the order things happened, never on a thread of the core. A callback that throws is reported to `onCallbackError` (by default logged as a warning to the `System.Logger` named `io.github.pho3nix90.openbyd`) and does not affect the client. The three callbacks the core waits for decide something:

- `gatePolicy`: whether a command is sent, from the command gate's verdict. `GatePolicy.STRICT` (the default), `GatePolicy.ALLOW_MISSING_DATA`, or a function. If it throws, the verdict stands.
- `pinResolver`: the `commandPwd` to send, in place of pyBYD's rule. If it throws, pyBYD's rule is used.
- `relogin`: what to do when a new session is needed. `ReloginPolicy.PASSWORD` (the default), `ReloginPolicy.NEVER`, or a function returning a `CompletionStage<SessionTokens>` (`ReloginPolicy.blocking(...)` wraps one that returns the session itself). What it throws is what the call that needed the session fails with.

What is chained to a future (`thenApply`, `whenComplete`) runs on the executor futures use by default, the common pool.

**Sessions**: `client.exportSession()` gives the session as an application stores it, and `Client.fromSession(session, config)` resumes it without a password login. With an empty password in the config, the client never logs in by itself.

**Closing**: `close()` closes the cars, stops MQTT and frees the native client. Later calls fail with `BydException`, and calls still in flight with `CancelledException`. A closed client stays closed. A client that is dropped without being closed is closed when it is collected, unless it listens for pushes or has a call in flight; both keep it alive.

Every client in a process shares one HTTP connection pool and one Rust runtime, so a client is cheap. `OpenByd.configureRuntime(threads)` sets the runtime's worker threads (2 by default) before the first client is made.

`OpenByd` also has what needs no client: `version()`, the envelope cipher (`encodeEnvelope`, `decodeEnvelope`) and `parseModel(GpsInfo.class, raw)`, which parses a raw payload as one of pyBYD's reply models.

## The native library

The package loads `openbyd_c` (`openbyd_c.dll`, `libopenbyd_c.so`, `libopenbyd_c.dylib`) through [JNA](https://github.com/java-native-access/jna), which looks for it:

1. in the directories of the system property `jna.library.path`: `java -Djna.library.path=/path/to/openbyd/target/release ...`;
2. where the system keeps its libraries;
3. in the jar, under JNA's prefix for the platform: `win32-x86-64/`, `linux-x86-64/`, `linux-aarch64/`, `darwin-aarch64/` or `darwin-x86-64/`. The released jar carries the library for each of those, so on them nothing else is needed.

The Linux libraries in the jar need glibc 2.17 or later. JNA has no prefix for musl, so on Alpine build the library there and name its directory in `jna.library.path`.

Build the library from the repository's root:

```sh
cargo build --release -p openbyd-capi    # target/release/
```

On JDK 24 and later, add `--enable-native-access=ALL-UNNAMED` to the `java` command, or the JDK warns that JNA loads native code.

## Testing your own code

`io.github.pho3nix90.openbyd.testing` has a fake BYD server and an MQTT test broker that speak the real wire protocol on 127.0.0.1. They need a native library built with the cargo feature `testing`; `FakeServer.isAvailable()` tells whether the one loaded has it.

```java
try (FakeServer server = new FakeServer(new Account("user@example.com", "secret"))) {
    server.reply("/control/getStatusNow", Reply.data(Map.of("acSwitch", 1)));
    BydConfig config = new BydConfig("user@example.com", "secret").baseUrl(server.url());
    try (Client client = new Client(config)) {
        client.getHvacStatus("TESTVIN0000000001").get();
    }
}
```

## Building and testing this package

The typed layer in `src/generated` is written by the generator from `bindings/spec.py` and the pyBYD installed; do not edit it. From the repository's root:

```sh
python bindings/generate.py jvm        # write the generated sources
python bindings/generate.py --check    # fail if any is out of date
```

The tests need the native library with the feature `testing`, and Maven 3.8 or later with a JDK from 17 on:

```sh
cargo build -p openbyd-capi --features testing    # target/debug/
cd bindings/jvm
mvn test
```

`mvn test` finds the library in `../../target/debug`; name another directory with `-Dopenbyd.native.dir=...`. The tests run the client end to end against the fake server and the test broker, run every recorded pyBYD vector through the library, and read every recorded model dump into its generated record and write it back. One test is written in Kotlin.

`mvn -Prelease package` also builds the sources and Javadoc jars.
