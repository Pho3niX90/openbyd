# The replay driver protocol

`bench/replay_detect.py` (the deterministic detection replay) does not run a worker freely. It steps one through a scenario: it owns the replay server and its manual clock, and tells a **driver** what happens at each moment. A driver is a variant's own code behind a small command loop. This document is what a driver has to provide; a new variant (a Rust worker, for instance) is one more row in the variants table once it has one.

The backend's Python driver is `backend/scripts/replay_driver.py` in the BYD Connect repository. `bench/replay/reference_driver.py` is a small stand-in that the tests of this repository use; it is not a variant and detects nothing worth reporting.

## What is replaced and what is not

The orchestrator replaces two things only: **when** the worker polls and receives pushes (the schedule comes from the scenario) and **the clock** (it is virtual). Everything from "bytes arrived from BYD" onward must be the variant's real code: decrypting and parsing replies and pushes, mapping them, merging, detecting drives and charges, and storing them. A driver that re-implements any of that is measuring a copy.

## One process per account

Accounts do not affect each other, so each account is replayed alone: its own replay server (holding only that account's part of the scenario), its own driver process, from `t = 0` to the end. Several accounts run side by side in separate processes. All driver processes of a run write to the same database.

The driver is started with the variant's command and environment, plus:

| Variable | Value |
|---|---|
| `BYD_BASE_URL_OVERRIDE` | The replay server of this account, `http://127.0.0.1:<port>`. The only BYD the driver may talk to. |
| `SSL_CERT_FILE` | The test broker's certificate (PEM file), for the MQTT connection. |
| `DATABASE_URL`, `DATABASE_SSL` | The run's database, already migrated and seeded with the scenario's accounts and cars (for a variant that keeps its detections in the database; another gets a placeholder). |
| `REDIS_URL` | `memory://`: session state lives in the driver process, so that key expiry can follow the virtual clock. |
| `CREDENTIAL_ENCRYPTION_KEY`, `JWT_SECRET` | The run's secrets (the seeding used the same ones). |
| `REPLAY_DRIVER_TIMEOUT_S` | Seconds of real time a command may take before the driver should give up and answer `"ok": false`. It is a little less than the orchestrator's own limit, after which the orchestrator stops the driver and fails the run. |

The login of an account is `<account>@example.com` with the password `secret` (`SCENARIO.md`).

## Messages

The orchestrator writes one JSON object per line to the driver's standard input. The driver answers each with exactly one line on standard output, `DRIVER <json>`, in order. Other lines on standard output are ignored (a launcher may print its own); logs go to standard error. Every command has `op` and `seq`; every answer repeats `seq` and has `ok`. An answer with `"ok": false` carries `"error": "<text>"` and ends the run as failed.

The driver does nothing between commands: no polling of its own, no timer that fires by itself. Anything it wants to do later (a debounce, a retry) it keeps as an internal timer in virtual time and runs when a command's time has passed it.

### `open`

```json
{"op": "open", "seq": 0, "protocol": 1, "account": "acct-0001", "vins": ["TESTVIN0000000001"], "epoch_unix_ms": 1767571200000, "duration_s": 21600}
```

The virtual wall clock is `epoch_unix_ms / 1000 + t`; it starts at `t = 0`. The driver starts the variant as its worker starts for this account: logs in, and connects and subscribes to MQTT. It answers when that is complete (the broker has acknowledged the subscription):

```json
{"seq": 0, "ok": true, "driver": "bydconnect-python", "settings": {"native_cipher": true, "shadow_sample_rate": 1.0}, "mqtt": true}
```

`settings` says what is in effect, so the orchestrator can check that a variant's switches took (the `expect` of its row). `mqtt` is whether the subscription stands.

### `step`

```json
{"op": "step", "seq": 17, "t": 3605, "pushes": 2, "poll": true}
```

Before it sends a step, the orchestrator has set the replay server's clock to `t`, which published the scenario's pushes due at `t`; `pushes` is how many of them the broker delivered to this account's connection. Steps come in rising order of `t`, each `t` once. The driver, in this order:

1. runs its internal timers due at or before `t`, in due order, each with the clock at its own due time (they must not need BYD: the server is already at `t`);
2. sets the clock to `t`;
3. waits until `pushes` MQTT messages have arrived and gone all the way through the variant's push handling (or been dropped by the variant's own code), in the order they arrived. A message that arrives earlier is held until this point, so that it is never handled with an earlier time on the clock;
4. if `poll`: makes one forced poll of the account's cars, as the variant's worker polls, and waits for everything that poll started (a new login after an expired session, the MQTT connection that follows it, its subscription).

The wait in 3 is a barrier, not a pause: the driver knows a message is done because its handling finished, never because some time went by. A timeout exists only to fail the run.

```json
{"seq": 17, "ok": true, "mqtt": true}
```

### `finish`

```json
{"op": "finish", "seq": 9041, "t": 21600}
```

`t` is the end of the replayed window: the `duration_s` given in `open`. It may equal the last step's `t`. The driver runs the timers due at or before `t` (a timer due later never runs), sets the clock, and reports what is still open, per car, and its counters:

```json
{"seq": 9041, "ok": true, "open": {"TESTVIN0000000001": {"drive": {"start": "2026-01-05T05:58:20Z", "start_odo": 20431.4}, "charge": null}}, "counters": {"polls": 311, "pushes_applied": 5210, "logins": 3}}
```

`open` holds every car of the account; `drive` and `charge` are `null` or an object of the variant's own fields. `counters` is a flat object of whole numbers; it is summed over accounts and published, never compared.

A driver whose detections are not read from the database also reports them here, as `"detections": {"drives": [...], "charges": [...]}` (each item an object with `vin` and the variant's fields).

**What is compared is the JSON itself.** Detections and open sessions are compared key by key and value by value, with no conversion: `1` and `1.0` differ, `"2026-01-05T00:00:00Z"` and `"2026-01-05T00:00:00+00:00"` differ, a missing key and `null` differ. So they must not hold anything random or anything read from a real clock, and two variants can only agree if they write the same keys in the same form. The backend's Python driver sets that form: closed drives and charges are the columns of the backend's `cloud_drives` and `cloud_charges` tables (all but the row id, the car's foreign key and the geocoded addresses: `SKIP_COLUMNS` in `bench/replay_detect.py`), read with the backend's models, numbers as JSON numbers of the column's type and times as `YYYY-MM-DDTHH:MM:SS[.ffffff]Z`; open sessions are the backend's open drive and open charge objects as `clean_open` in `scripts/replay_driver.py` reports them. A variant that is to be compared with the Python worker either writes the same tables or reports the same keys in the same form.

After the answer the orchestrator closes standard input, and the driver exits with status 0.

## What makes a driver deterministic

Two runs of one variant on one scenario must give identical detections, to the last digit. The orchestrator asserts it. A driver gets there by:

- **One clock.** Every clock the variant's code reads (wall and monotonic) is the virtual one. Only the plumbing keeps real time: the event loop, socket and MQTT timeouts.
- **No real waiting as logic.** A sleep in the variant's code (between two reads of a result, before a retry) returns at once or becomes an internal timer; it never decides anything by how long it really took.
- **No free-running tasks.** A command's answer is sent only when everything it caused has finished.
- **One MQTT connection per account at a time**, so that the broker's count of delivered pushes is the count the driver waits for.
- **No outage the scenario does not hold.** Hundreds of thousands of short loopback connections can run the machine out of ephemeral ports; Windows then fails a connect (`WinError 52`) and the variant handles it as BYD being unreachable, which changes its detections. The scenario's own errors are replies, never failed connects. A Python driver gets a retry of such failures from the launcher (`--retry-local-connects`, counted as `connect_retries` in the guard report; a connect that fails every try ends the run as failed). A native driver must retry them the same way: nothing was sent, so the replay server never sees the attempt.
- **Stored values from the clock above.** Every stored column is compared, so none may be filled by the database server's own `now()` (that is real time) and nothing may be decided from such a value. The backend has none on these paths today; one added later would show as a failed repeat check.

## What a new variant has to provide

1. A command that speaks this protocol (for a native worker: a mode of its binary), reading the environment above.
2. Its detections: either rows in the backend's `cloud_drives` and `cloud_charges` tables (read by the orchestrator with the backend's models), or `detections` in its `finish` answer.
3. A row in `VARIANTS` of `bench/replay_detect.py`: key, label, command, environment, and what `settings` must say.
4. Containment: a Python driver runs under `bench/replay/launcher.py` (the loopback-only guard). A native binary must be contained another way; its only BYD address is `BYD_BASE_URL_OVERRIDE`.
