# openbyd for Node.js

An unofficial client for the BYD overseas app API: [pyBYD](https://github.com/jkaberg/pyBYD)'s `BydClient` and `BydCar` for TypeScript and JavaScript, on openbyd's Rust core. Every request it builds and every reply it parses is compared with pyBYD in the repository's tests.

This project is not affiliated with, endorsed by or supported by BYD. It has not yet been run against BYD's live service; see the [status](../../README.md#status) of the whole project.

```sh
npm install openbyd
```

For Node.js 20 and later, on Windows (x64), Linux (x64 and arm64, glibc and musl) and macOS (arm64 and x64).

## Use it

```ts
import { Client } from "openbyd";

const client = new Client({ username: "you@example.com", password: "...", controlPin: "123456" });
try {
  await client.login();
  for (const vehicle of await client.getVehicles()) {
    const realtime = await client.getVehicleRealtime(vehicle.vin);
    console.log(vehicle.vin, realtime.elec_percent);
  }
} finally {
  client.close();
}
```

A car, with its state and its commands by feature:

```ts
const car = await client.getCar(vin, { onStateChanged: (vin, state) => console.log(state.realtime?.elec_percent) });
await client.verifyCommandAccess(vin);
await car.updateRealtime();
if (car.lock.available) await car.lock.lock();
await car.hvac.start({ temperature: 21, duration: 20 });
```

Methods are pyBYD's, in camelCase: `get_vehicle_realtime` is `getVehicleRealtime`. One that waits on the network returns a promise; one that answers at once (`exportSession()`, `commandsEnabled`, `car.state`) returns its value. Optional arguments are an options object: `getVehicleRealtime(vin, { pollAttempts: 3 })`. Preconditioning plans use `getStandbyPlans`, `saveStandbyPlan`, `createStandbyPlan`, `setDefaultStandbyPlan`, `deleteStandbyPlan` and `runStandbyPlan`; on a daily `StandbySchedule`, set `zone` to an IANA time zone name (such as `"Europe/Madrid"`) so the next run follows that zone's wall clock across daylight-saving changes.

## Types

Everything is typed, and the types are generated from pyBYD's own models, so they follow upstream.

- **What you get back is pyBYD's JSON, untouched**: a model is exactly `model_dump(mode="json")`, so its fields keep pyBYD's snake_case names (`realtime.elec_percent`) and are read-only. A model's `raw` is the reply it was parsed from.
- **What you pass is camelCase**, like the rest of your code: the config (`controlPin`), options (`pollAttempts`) and parameter objects (`startClimate(vin, { temperature: 21, timeSpan: 2 })`).
- **Enums are their values**: `vehicle.energy_type` is `-1 | 0 | 1 | 2`, and `EnergyType.EV` is `0`. An argument that is not one of an enum's values does not compile.
- **A float JSON cannot hold** (NaN, an infinity) comes back as that number, only in fields that are floats.

The repository's tests check every model dump recorded from pyBYD (more than 11,000) against these types.

`parseModel("GpsInfo", raw)` parses a raw payload (an MQTT event's data, say) as one of pyBYD's models. `encodeEnvelope` and `decodeEnvelope` are the envelope cipher.

## Errors

Failures are pyBYD's exception classes, under one root:

```
OpenBydError                      .kind: the Python class name
├── BydError
│   ├── BydCryptoError ── BangcleError ── BangcleTableLoadError
│   ├── BydTransportError         .statusCode, .endpoint
│   └── BydApiError               .code, .endpoint
│       ├── BydAuthenticationError ── BydSessionExpiredError
│       ├── BydRemoteControlError
│       ├── BydControlPasswordError
│       ├── BydEndpointNotSupportedError
│       ├── BydDataUnavailableError
│       ├── BydRateLimitError
│       └── BydServiceBusyError
├── DataError                     a value could not be used (ValueError, TypeError, ...)
│   └── ValidationError           .errorType, .loc
└── CancelledError                a wait for a push was cut short
```

```ts
try {
  await client.lock(vin);
} catch (error) {
  if (error instanceof BydControlPasswordError) console.log("wrong PIN", error.code);
  else throw error;
}
```

## Callbacks

Give them when the client is made. They run on JavaScript's thread.

```ts
const client = new Client({
  username: "you@example.com",
  password: "...",
  onVehicleInfo: (vin, data) => console.log(vin, data.elec_percent),
  onCommandAck: (ack) => console.log(ack.request_serial, ack.success),
  gatePolicy: (vin, verdict) => verdict.supported || verdict.reason === "function_no_missing",
  relogin: async () => loadSessionFromSomewhere(),
});
```

- `onVehicleInfo`, `onMqttEvent`, `onCommandAck`, `onCommandLifecycle`, `onMqttConnect`, `requestObserver`: pyBYD's callbacks, and one per request sent.
- `gatePolicy`: `"strict"` (pyBYD's), `"allow_missing_data"`, or a function deciding from the command gate's verdict.
- `pinResolver`: replaces pyBYD's rule for the `commandPwd` sent.
- `relogin`: `"password"` (pyBYD's), `"never"`, or a function returning a saved session.
- `commandAckTtlSeconds`, `commandRetries`.

A callback that throws does not affect the client. The failure goes to `onCallbackError(name, error)`, or is emitted as a process warning when there is none.

## Lifetime

A client costs a few small allocations: every client in the process shares one connection pool and one Rust runtime.

`close()` closes the cars, stops MQTT and makes later calls throw `BydError`; `start()` opens it again. `using client = new Client(...)` closes it at the end of the block.

A client with push callbacks (or a car with `onStateChanged`) keeps the process alive until it is closed, as an open socket would: it is waiting for pushes. A client without them does not.

Sessions can be saved and resumed without a password login: `client.exportSession()` gives the tokens, `Client.fromSession(tokens, options)` resumes them.

## The native add-on

The client is compiled code. Each platform's add-on is a package of its own, `openbyd-<platform>` (`win32-x64-msvc`, `linux-x64-gnu`, `linux-arm64-musl`, `darwin-arm64`, ...), which `openbyd` lists as an optional dependency: npm installs the one that fits the machine. An install with `--omit=optional` or `--no-optional` leaves it out, and the package then fails to load, saying which one is missing.

On another platform, or to run your own build, put the add-on at `native/openbyd.<platform>.node` inside the package ([Build and test](#build-and-test)); that file is looked for first.

## Testing your own code

`openbyd/testing` has a fake BYD server that speaks the real wire protocol and an MQTT test broker. They need an add-on built with `--testing`; `hasTesting` tells whether the one loaded has them.

```ts
import { FakeServer } from "openbyd/testing";

using server = new FakeServer([{ username: "user@example.com", password: "secret" }]);
server.reply("/control/getStatusNow", { data: { acSwitch: 1 } });
const client = new Client({ username: "user@example.com", password: "secret", baseUrl: server.url, mqttEnabled: false });
```

## Build and test

You need a Rust toolchain and Node.js 22 or later (the tests use its test runner).

```sh
cd bindings/node
npm install
npm run build:native -- --debug --testing   # the add-on, with the fake server; leave out --debug for a release build
npm test                                    # compiles the package and runs its tests
```

`src/generated` is written by `python bindings/generate.py` from `bindings/spec.py` and pyBYD's models; see [the bindings' README](../README.md).
