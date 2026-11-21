# The bindings

openbyd's client is written once, in Rust. Python has its own binding (`crates/openbyd-py`). Every other language sits on one shared layer and a typed layer generated for it:

| Language | Package | Loads | Typed layer |
|---|---|---|---|
| JavaScript / TypeScript | [`node/`](node) (npm [`openbyd`](https://www.npmjs.com/package/openbyd)) | `crates/openbyd-node`, a Node add-on | `node/src/generated` |
| Java / Kotlin | [`jvm/`](jvm) (Maven `openbyd`) | `crates/openbyd-capi`, a C library, through JNA | `jvm/src/generated` |
| C# / .NET | [`dotnet/`](dotnet) (NuGet `OpenByd`) | `crates/openbyd-capi`, through P/Invoke | `dotnet/OpenByd/Generated` |

```
openbyd-client            the client (Rust)
      │
openbyd-host              the client by method name, JSON in and out
      ├── openbyd-node    Node add-on            ── node/   TypeScript
      └── openbyd-capi    C library (openbyd.h)  ├─ jvm/    Java
                                                 └─ dotnet/ C#
```

## The shared layer: `openbyd-host`

`crates/openbyd-host` offers pyBYD's `BydClient` and `BydCar` by method name. Arguments are a JSON object keyed by pyBYD's parameter names; a result is JSON, and a model is exactly pyBYD's `model_dump(mode="json")`. Nothing language-specific lives there, so a binding is thin and the three behave alike.

- **Functions** that need no client (`openbyd_host::call`): `version`, `encode_envelope`, `decode_envelope`, `parse_model`, `parity_call`, `configure`, `runtime`, `has_testing`.
- **A client** (`openbyd_host::Client`) is made from a config (an object of `BydConfig`'s fields) and options, and is closed until its `start` method is called. Methods that answer at once (`SYNC_METHODS`: `start`, `close`, `export_session`, `car.state`, ...) and methods that wait on the network (`ASYNC_METHODS`: `login`, `get_vehicle_realtime`, `car.lock.lock`, ...) are separate entry points.
- **A car** is a number: `get_car` returns `{"car": 1, "vin": ...}`, and a car's methods (prefixed `car.`) take that number as `car`.
- **Events** go to the binding as a name and a JSON object: `vehicle_info`, `mqtt_event`, `command_ack`, `command_lifecycle`, `mqtt_connect`, `request` (sent only when the client's `events` option names them) and `state_changed` (when `get_car` was given `state_events`).
- **Questions** are events whose payload has a `request` number: `gate_policy`, `pin_resolver` and `relogin`, asked only when the matching option is `"host"`. The binding answers with the sync method `resolve`: `{"request": n, "value": ...}` or `{"request": n, "error": {"class", "message", ...}}`. `gate_policy` and `pin_resolver` wait at most 30 seconds and then take pyBYD's default; `relogin` waits as long as it takes.
- **Errors** are JSON objects: `class` is the Python exception class pyBYD raises, `bases` its parents nearest first, then `message`, `code`, `endpoint`, `status_code`, `error_type` and `loc`. A binding maps `class` to its own error class and tries `bases` in turn when it has none of that name.
- **Floats JSON cannot hold** are the strings `"NaN"`, `"Infinity"` and `"-Infinity"`, in results only where the field is a float. A typed binding reads them back as numbers.
- **Tests**: with the cargo feature `testing`, `openbyd_host::testing::call` runs the fake BYD server and the MQTT test broker by call name (`server.start`, `server.reply`, `broker.push`, ...), so each binding tests itself end to end without wrapping them.

`crates/openbyd-capi/include/openbyd.h` is the whole C interface: five functions and two callback types.

## The typed layer: `spec.py` and `generate.py`

[`spec.py`](spec.py) describes the surface once: every method with its parameters, options and result, the car's features, the events, the error classes and the shapes openbyd adds. The shapes of pyBYD's models are not written there. They are read from the pyBYD installed, through pydantic's JSON schema, so the published types follow upstream.

```sh
python bindings/generate.py            # write every generated file
python bindings/generate.py --check    # fail if any is out of date (CI runs this)
python bindings/generate.py node       # one package: node, jvm or dotnet
```

It needs pyBYD at the commit in `parity/UPSTREAM_COMMIT`, as the parity tests do. [`surface.json`](surface.json) is the surface as data: `openbyd-host`'s tests compare it with the methods the core dispatches, and the packages' tests check every recorded pyBYD model dump against it.

Each emitter (`gen/typescript.py`, `gen/java.py`, `gen/csharp.py`) writes into directories that hold nothing else, so a file it stops writing is removed. Hand-written code in each package covers what a generator should not: loading the native library, the client's constructor and lifecycle, callbacks, and `get_car`.

## Conventions every binding follows

- **Names** are pyBYD's in the language's own case: `get_vehicle_realtime` is `getVehicleRealtime` in TypeScript and Java and `GetVehicleRealtimeAsync` in C#. Model, enum and error class names are pyBYD's (Java and C# say `Exception` where pyBYD says `Error`).
- **Results keep pyBYD's JSON.** TypeScript returns it as it is, so fields are snake_case. Java records and C# records name their members in their own case and map them to the snake_case JSON.
- **What a caller passes** (the config, options, parameter objects) is in the language's own case. `StandbySchedule` also accepts an optional `zone` (an IANA time zone name such as `Europe/Madrid`) so a daily preconditioning plan follows that zone's wall clock across daylight-saving changes; `zone` is ignored on other schedules.
- **Errors** are pyBYD's hierarchy under one root, `OpenBydError`, which carries `kind`, the Python class name. Python built-ins pyBYD lets through (`ValueError`, `TypeError`, a reply that is not JSON) are `DataError`; pydantic's `ValidationError` is `ValidationError`, a `DataError`.
- **Callbacks never run on the core's threads**, and one that throws does not affect the client.
- **Nothing is lost quietly**: a NaN argument, an unknown option value or a wrong type is an error.

## Adding a method

1. Add it to `openbyd-host` (`src/client.rs`): an arm in `call` or `call_sync`, and its name in `ASYNC_METHODS` or `SYNC_METHODS`.
2. Describe it in `spec.py`.
3. Run `python bindings/generate.py`, then each package's tests.

`cargo test -p openbyd-host --all-features` fails when `surface.json` and the core's method lists disagree.

## Releasing

`.github/workflows/release-bindings.yml` builds the Node add-on and the C library for every platform (Windows x64, Linux x64 and arm64 for glibc and musl, macOS arm64 and x64), packs the three packages with them, installs each packed package into a project of its own on every platform (`bindings/smoke`), and only then publishes: to npm (the package `openbyd` and one `@openbyd/<platform>` per add-on), Maven Central and NuGet.

A tag `v<version>` publishes all three, alongside the Python package. Run by hand, the workflow publishes to the registries ticked, or to none: then it is a rehearsal, and the packages are an artifact of the run. A pull request that changes the workflow or what it runs rehearses it too.

A package's version is the workspace's (`Cargo.toml`); `generate.py` fails when one says otherwise, and the workflow refuses a tag that names another version.

## Building and testing a package

Each package's tests need its native library built with the feature `testing`:

```sh
# Node
cd bindings/node && npm install
npm run build:native -- --debug --testing && npm test

# JVM and .NET share the C library
cargo build -p openbyd-capi --features testing
cd bindings/jvm && mvn test
cd bindings/dotnet && dotnet test
```
