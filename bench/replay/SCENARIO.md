# Replay scenarios

A scenario describes what a set of accounts' cars did over a span of time, in enough detail for the fake BYD server and the test broker to answer a client as BYD would have: the state of each car over time, the moments BYD answered an error, and the moments a push arrived. It holds no identity and no location.

Scenarios come from two places: `bench/replay/generate.py` makes synthetic ones (tests and CI use only these), and `bench/replay/export_prod.py` builds one from a production `ingest_events` table, anonymised before it leaves the database pod. Exported scenarios are never committed; see [Privacy](#privacy).

## File format

A scenario is a UTF-8 text file of JSON objects, one per line (`.jsonl`). The first line is the header; every other line has a `type`. Lines are ordered: header, then all `account` lines, then all `car` lines, then the timeline lines (`state`, `error`, `push`) sorted by `t`, ties in file order.

Times are whole seconds since the scenario's start (`t`, an integer, `0 <= t <= duration_s`).

### Header

```json
{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 86400, "accounts": 2, "cars": 3, "seed": "demo", "stats": {}}
```

| Key | Meaning |
|---|---|
| `version` | `1`. A reader refuses any other version. |
| `source` | `"synthetic"` or `"export"`. |
| `start` | The instant `t = 0` stands for, UTC, to the second. For an export it is the real time plus the export's random offset. |
| `duration_s` | Length of the scenario. |
| `accounts`, `cars` | Counts, for a quick check against the lines that follow. |
| `seed` | Optional, synthetic only: the generator's seed, written like a model key. |
| `stats` | Optional. Counts (events by type, what an export dropped): one flat object whose keys are lowercase words joined by underscores and whose values are non-negative whole numbers. |

### Account

```json
{"type": "account", "account": "acct-0001", "cars": ["TESTVIN0000000001", "TESTVIN0000000002"]}
```

`account` is `acct-` and a number. It is the whole identity: the login name is `<account>@example.com` and the password is `secret`, by convention, so no e-mail address appears in a scenario file.

### Car

```json
{"type": "car", "vin": "TESTVIN0000000001", "account": "acct-0001", "model": "seal", "energy": "ev"}
```

| Key | Meaning |
|---|---|
| `vin` | `TESTVIN` and ten digits. The letter `I` never occurs in a real VIN, so a synthetic one can never be mistaken for one. |
| `model` | A catalogue key (`^[a-z0-9_-]{1,40}$`) or `"unknown"`. It picks the reply templates. |
| `energy` | `"ev"` or `"hybrid"`. |

A car has no position. The replay server gives each car a made-up anchor (from the car's index in the file) and moves it around that anchor as its odometer advances, so a parked car stays where it is and a driving car moves at the speed its odometer says.

### State

The complete state of one car from `t` until that car's next `state` or `error` line.

```json
{"type": "state", "vin": "TESTVIN0000000001", "t": 3600, "online": true, "soc": 71.0, "odometer_km": 20431.4, "speed_kmh": 62.0, "vehicle_on": true, "plugged": false, "charging": false, "connect_state": 0, "charge_power_kw": null, "minutes_to_full": null, "hvac_on": false, "fuel_percent": null, "gps": true}
```

| Key | Type | Meaning |
|---|---|---|
| `online` | bool | BYD can reach the car. `false` is a sleeping car: BYD answers its realtime read with placeholder zeros, and the other fields here are the last known values. |
| `soc` | number or null | Battery, percent. |
| `odometer_km` | number or null | Odometer. |
| `speed_kmh` | number or null | Speed. |
| `vehicle_on` | bool or null | Switched on (ready to drive). |
| `plugged` | bool | A charger is connected. |
| `charging` | bool | Charging now. |
| `connect_state` | int or null | BYD's charger connection state, as given. |
| `charge_power_kw` | number or null | Charge power at the battery. |
| `minutes_to_full` | int or null | Remaining charge time. |
| `hvac_on` | bool or null | Climate running. |
| `fuel_percent` | number or null | Tank, hybrids only. |
| `gps` | bool | Whether a location read succeeds. |

Every key is present on every `state` line. Values are bounded: percentages 0 to 100, odometer 0 to 10,000,000, speed 0 to 400, charge power 0 to 1,000, minutes to full 0 to 10,080, `connect_state` -1 to 255. A car with no `state` line at or before the current time is answered as asleep with a zero odometer.

### Error

BYD answers this car's reads with an error from `t` until the car's next `state` line or next error other than 1005. A push does not end it. A car that recovers always gets a `state` line at that moment, even when nothing about it changed.

```json
{"type": "error", "vin": "TESTVIN0000000001", "t": 5400, "code": "1008"}
```

| `code` | What the server does |
|---|---|
| `"1008"` | Busy: every car read for this VIN answers 1008. Account reads (the vehicle list) still succeed. |
| `"6002"` | Car unreachable: the realtime read answers 6002. |
| `"1001"` | Not supported: the realtime read answers 1001. |
| `"1005"` | Session expiry, a single event: at `t` every session of the car's account is expired once. The next request answers 1005, a new login works, and the car's previous state applies again. |
| any other digits | The realtime read answers that code. |

### Push

A message BYD pushed over MQTT at `t`. The broker publishes it, encrypted with the key of the account's newest session, to whoever is subscribed for that account. An account with no connection at `t` misses it.

```json
{"type": "push", "vin": "TESTVIN0000000001", "t": 3605, "event": "vehicleInfo", "online": true, "soc": 71.0, "odometer_km": 20431.9, "speed_kmh": 60.0, "vehicle_on": true, "charge_power_kw": null, "fuel_percent": null}
{"type": "push", "vin": "TESTVIN0000000001", "t": 3700, "event": "remoteControl"}
{"type": "push", "vin": "TESTVIN0000000001", "t": 9000, "event": "smartCharge", "plugged": true, "charging": true, "connect_state": 1, "soc": 54.0}
```

`event` is `vehicleInfo`, `remoteControl`, `smartCharge` or `energyConsumption`. A `vehicleInfo` push carries any of the `state` keys shown above (a key that is absent was not in the push); `smartCharge` carries any of `plugged`, `charging`, `connect_state` and `soc` (again, an absent key was not in the push; a key that is present has the type of the `state` key of the same name); the other two carry nothing and are sent from a template.

## Replay time

The replay clock maps wall time to scenario time: `t = start_at + (wall - wall_at_start) * speed`. It is shared by the server and the broker. Before it is started it stands at `start_at`; after `duration_s` the last state of each car holds and no more pushes are sent.

Timestamps inside replies (the car's last report time, the times of pushes) are `epoch + t`, where `epoch` is the header's `start` (repeatable: the same request at the same `t` gets the same reply) or, for a client that compares BYD's times with its own clock, the wall time at which the replay started minus `start_at`.

## Privacy

An exported scenario is built inside the production pod and is anonymous when it leaves it:

- Accounts and VINs are numbered in an order shuffled with a random salt that exists only in the pod's memory for the length of the export. Nothing in the file maps back.
- No coordinate is read out of the pod. A `gps` key is a yes or no.
- Every time is the real time plus one random offset per export, rounded to the second, and written as seconds since the shifted start.
- Only the keys in this document are written; a value must have the type given here or it is dropped. Free text (error messages, names, plates, places), e-mail addresses and tokens are never written. A model is written only if the backend took it from its catalogue (by VIN or by BYD's own model name), never when it was derived from text a customer typed.
- `bench/replay/privacy.py` checks the finished text and fails the export if it finds anything shaped like a real VIN (17 characters of the VIN alphabet), a database id (a UUID), an e-mail address, or two numbers with more than three decimals next to each other (a coordinate pair). The export script runs the same check in the pod on every line before it writes it and stops at the first failure; the local step that receives the stream deletes what it received unless the export finished cleanly and the whole file passes the check again.

Exported files live only under `E:\Projects\openbyd-replay-data\`. They are not committed, uploaded, or pasted anywhere.
