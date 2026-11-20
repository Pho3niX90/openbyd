# Paired car and cloud export

`export_pairs.py` pulls one car's in-car-app telemetry (`car_telemetry`) and the same
car's BYD cloud events (`ingest_events`) out of the BYD Connect production database,
anonymised, so they can be lined up by time on a local machine. It is the data-gathering
step for checking openbyd's and pyBYD's reading of BYD cloud data against what the car
itself reported.

It reuses the building blocks of `bench/replay/export_prod.py` (leak patterns, the
read-only short-transaction query runner, plan projection, the output-directory guard,
the error handling) by importing them. It does not copy them.

## Running it

Only the lead runs it against production, after review. Every command runs from the
repository root.

```sh
# Query plans only (node type, relation, index, estimated rows):
python -m bench.mapping.export_pairs fetch --explain

# Counts and plans, no row payloads read:
python -m bench.mapping.export_pairs fetch --dry-run

# The export:
python -m bench.mapping.export_pairs fetch --out-dir E:\Projects\openbyd-replay-data\seal
```

Options (all modes): `--car-days` (default 29, 1-29), `--cloud-days` (default 13, 1-13),
`--until` (ISO with a timezone; default now minus 10 minutes, on a minute boundary),
`--page-rows` (default 2000, 1-5000), `--pause-ms` (default 1000, 200-60000),
`--max-minutes` (default 15). `--out-dir` is needed only for an export.

`fetch` composes one stdin script from `bench/replay/export_prod.py` and
`bench/mapping/export_pairs.py` (`build_pod_script`: a prelude registers the first as
module `bench.replay.export_prod`, then the second, then calls its `main()`), and runs:

```sh
kubectl -n bydconnect exec -i deploy/api -- nice -n 19 python - <options>
```

The pod has no openbyd checkout, so the script carries both files. The composition is
deterministic, and each module is preceded by a comment with its SHA-256.

kubectl gets a timeout of `(--max-minutes + 2) * 60` seconds on top of the pod's own
alarm. On timeout it is killed, the scratch file is removed, and the run fails with a
fixed message.

For an export, the pod's stdout goes to a scratch file inside the output directory. The
meta line is moved to the top, every line is checked again, the line counts are compared
with the meta statistics, each stream's times are checked against its own window (see
below), and only then is the file renamed to
`pairs-<shifted start date>-<car days>d.jsonl`. An existing file is never overwritten.
On any failure the scratch file is removed. For `--dry-run` and `--explain` the single
JSON line is checked and printed. Nothing is written.

The output directory must be under `E:\Projects\openbyd-replay-data` and not in a git
work tree (`export_prod._export_directory`).

## The rules it enforces

1. **Read only.** Each query runs in its own transaction, which opens with
   `SET TRANSACTION READ ONLY` and statement (30 s), idle-in-transaction (10 s) and lock
   (2 s) timeouts. Only `SELECT` runs, plus `EXPLAIN (FORMAT JSON)` of those SELECTs
   (never `ANALYZE`). Nothing that runs migrations is imported.
2. **Four tables only.** `car_telemetry`, `ingest_events`, and from `cars` and
   `car_app_devices` only the columns the car needs.
3. **One car.** It reads the distinct `car_id` values in `car_telemetry` over the car
   window, one UTC day per query, at most two per day (`LIMIT 2`, no pagination). Zero
   cars fails. Two ids in one day, or a day whose id differs from the one already found,
   stops it at once (no further day is read) and fails with the fixed message
   `more than one car has car-app telemetry in the window; ask the owner which car to export`.
   Nothing is looked up about either car on that path. A car without a `cars` row, with a
   deleted one, or without a VIN also fails.
4. **Ground truth only.** The export requires the car's device row to say `car_app`. A
   `simulator` car fails with `the car with telemetry is a simulator`, and a car with no or
   another device row fails with `the car with telemetry has no car_app device row`. The
   dry run still reports the source in `device_sources`.
5. **Bounded and gentle.** `car_telemetry` is read one UTC day per query (so each query
   prunes to one daily partition) and `ingest_events` one UTC day per query, filtered on
   `user_id`, `vin` and `received_at` (so the composite index is used). Pages use a keyset
   cursor and `LIMIT`. Between queries there is a pause, and a hard SIGALRM deadline ends
   the process. The first error stops the run. Each transaction is closed before its rows
   are processed or written. The pod process runs under `nice -n 19`.
6. **Plan gate.** `ingest_events` holds every customer's events, so this car's rows must
   come through the `ix_ingest_events_user_vin_received` index. Every `ingest_events`
   statement (each row page, including resumed pages, and each dry-run count) is preceded
   by `EXPLAIN (FORMAT JSON)` (never `ANALYZE`) of the same text with the same parameters
   (the actual day window and keyset cursor), in its own short read-only transaction. It
   runs only if that plan passes. The rule fails closed:
   - every relation in the plan is `ingest_events` (or none);
   - every node on `ingest_events` is an `Index Scan`, `Index Only Scan` or
     `Bitmap Heap Scan`;
   - every index named while reading it (an `Index Scan` or `Index Only Scan`, or any
     `Bitmap Index Scan`) is exactly `ix_ingest_events_user_vin_received`.

   So a sequential scan, another index such as `ix_ingest_events_received_at`, or a child
   or partition relation it does not recognise stops the run with
   `refusing to export: a query plan does not read ingest_events through this car's index`.
   Car lines and earlier cloud pages may already be in the pod stream by then. The pod
   exits non-zero, so `fetch` removes the scratch file and keeps nothing.
7. **Dry run and explain read no payloads.** The dry run selects the car id, the car's
   metadata, `COUNT(*)` per day and `EXPLAIN` plans. Plans keep only node type, relation,
   index and estimated rows. A daily partition's name carries a real date, so the pod
   prints it as `car_telemetry_partition`, inside index names too. The local check accepts
   only the four table names and that string as relations, refuses `car_telemetry_p`
   followed by 8 digits anywhere in an index name, and runs the leak scan on node types
   and index names.
8. **Privacy, checked in the pod before a line is written and again locally.**
   - The VIN is `TESTVIN0000000001`. The car id, user id, VIN and event ids *are* selected,
     because the pod needs them to identify the car, filter its events and paginate. They
     stay in the pod's memory and are never written. The pod name (`actor`) is never
     selected. No names, plates, e-mails, tokens or free text are written.
   - `detail` is written only when it is a 1-5 digit code or exactly one of the fixed
     strings the backend writes (`session_expired`, `received`, `min_gap`, `busy`, `lock`,
     `not_selected`, `car_unlinked`, `missing_vin`, `exception`, `upstream`). The SQL
     returns nothing else (`detail IN (...)`), and any other detail is dropped and counted.
   - Dropped keys are counted by name only when the name is one the backend writes (car
     `data`: `motorRpm`, `motorRpmFront`, `motorRpmRear`, `slopeRaw`, `acceleratorPct`,
     `brakePct`; cloud payload: `fuelPercent`, `last50kmFuel`, `placeKind`,
     `lastStationaryPollAt`, `openDriveStartAt`). Every other name is counted as `other`, and
     the check accepts no other key. `app_version` is written only as
     `^[0-9]{1,3}(\.[0-9]{1,3}){1,3}(-debug)?$`.
   - **No coordinate leaves the pod.** Latitude, longitude, `altitudeM`, `headingDeg` and
     `slopeRaw` are never written. The pod works out distances itself (see `gps_*` below).
     Nothing is measured from a position of exactly (0, 0), on either side, because the
     distance would place the other position on a known circle. A car fix at (0, 0) gives
     `has_fix: false` and is counted as invalid. The check refuses a cloud line that has
     `gps_zero: true` together with any distance, age or matched-fix key.
   - **Time shift.** One random offset (`secrets`, uniform within +/- 30 days, whole
     seconds) is chosen in the pod and never written. Times are written as seconds since
     the shifted window start (`meta.start`). Real spacing is kept.
   - Every number is finite and has at most 3 decimals.
   - **The line check.** It fails on: any number with more than 3 decimals anywhere in a
     line (so no coordinate can pass); a VIN-shaped token other than the synthetic VIN, a
     UUID or an e-mail anywhere; invalid JSON, `NaN`/`Infinity` or duplicate keys; a key not
     in the line type's whitelist; a string outside its enum or pattern; a number outside its
     range; or a boolean where a number belongs (or the reverse). On failure nothing more is
     written, and the process exits non-zero with only `<rule> on <line type> line`.
9. **Errors.** Errors are printed without data: the exporter's own messages, or
   `mapping export failed: <exception class>`. Locally, kubectl's stderr is repeated only
   when a line is exactly one of those messages (a class name that fails the leak scan is
   not repeated). Anything else becomes `kubectl export failed (exit N)`. A malformed
   stored value (an unparsable or overflowing timestamp, a coordinate too large for a
   float, a JSON document the parser cannot read) is counted in the statistics and never
   aborts a run. The two JSON columns are selected as text (`CAST(data AS text)`,
   `CAST(payload AS text)`) and parsed in the pod, so an undecodable document is counted
   in `invalid_values` as `data` or `payload`, and its row is still written with what the
   typed columns give.

### Residual

The time offset protects the file when it is read on its own. It does not hide the real
clock from someone who knows when the export ran, or who sees the file's creation time:
the windows end at `--until`, so the last rows lie close to the moment of the run. Speed
over time and leg distances also stay in the file, so a trip's shape can be partly
reconstructed. For these reasons the file stays under `E:\Projects\openbyd-replay-data`
and is never committed or uploaded.

## The line format

JSON lines, compact, ASCII. In the pod stream the `meta` line comes last. In the kept
file it comes first. Then all `car` lines, oldest first, then all `cloud` lines, oldest
first. A key whose value is absent from the source row is left out.

### `car`: one per `car_telemetry` row

| key | value |
|---|---|
| `type`, `t` | `"car"`; seconds since `meta.start` (3 decimals). Required. |
| `cadence` | `moving` \| `stationary` \| `charging` \| `event` |
| `speed_kph` 0-400, `soc_pct` 0-100, `twelve_volt` 0-20, `odometer_km` 0-2,000,000 | numbers |
| `power_level` | `off` \| `acc` \| `on` \| `ok` |
| `gear` | `P` \| `R` \| `N` \| `D` \| `M` \| `S` |
| `doors` | object with any of `fl fr rl rr hood trunk`: `open` \| `closed` |
| `lock` | `locked` \| `unlocked` |
| `alarm` | `off` \| `armed` \| `alarming`; `alarm_raw` integer 0-255 |
| `twelve_volt_level` | `low` \| `normal` |
| `average_speed_kph` 0-400, `outside_temp_c` -60-80, `remaining_kwh` 0-300, `lifetime_kwh` 0-10,000,000 | numbers |
| `tyre_kpa` 0-1000, `tyre_temp_c` -40-150 | objects with any of `fl fr rl rr` |
| `charging_state_raw`, `charging_gun_raw` (integers 0-255), `charging_type_raw`, `charger_state_raw` (integers 0-254) | raw BYD values |
| `charging_power_kw` 0-1000, `charge_remaining_min` integer 0-6000, `charged_kwh` 0-300, `motor_power_kw` -500-1000 | numbers |
| `has_fix` | the row has a `location`: `true` when it holds a valid position |
| `fix_accuracy_m` 0-10,000, `fix_age_s` 0-3600, `fix_speed_kph` 0-400 | from the location (GPS ground speed) |

Any other `data` key is dropped and counted in `meta.stats.dropped_car_keys`: by name for
the fixed backend names above, as `other` otherwise. A location's `altitudeM` and
`headingDeg` are dropped without being counted.

### `cloud`: one per `ingest_events` row of the car (polls, pushes, errors, skips)

| key | value |
|---|---|
| `type`, `t`, `source`, `kind` | `"cloud"`; seconds since `meta.start`; `poll` \| `mqtt`; `ok` \| `error` \| `skip` \| `vehicleInfo` \| `smartCharge` \| `energyConsumption` \| `remoteControl` \| `other`. Required. |
| `detail` | a 1-5 digit code or one of the fixed strings listed above |
| `last_updated_t` | the payload's `lastUpdated` on the same clock as `t` (may be negative) |
| `odometer_km`, `soc`, `speed_kph`, `charge_power_kw`, `minutes_to_full`, `last50km_energy_kwh` | numbers |
| `online`, `vehicle_on`, `asleep`, `vehicle_unreachable`, `hvac_on` | booleans |
| `energy_distribution` | object with any of `drive electric hvac other` |
| `charging` | object with any of `is_charging is_connected connect_state soc minutes_to_full` |
| `gps_present` | the payload has `gps`: `true` for a valid position, `false` for an invalid one |
| `gps_fresh` | the payload's `gpsFresh` |
| `gps_zero` | the cloud position is exactly 0,0 (then nothing is measured from it) |
| `gps_dist_m` | whole metres from the cloud position to the car fix nearest in time (fix time = row time minus `ageS`), only if that fix is within 600 s |
| `gps_dt_s` | cloud received time minus that car fix's time, 1 decimal |
| `gps_car_accuracy_m`, `gps_car_speed_kph` | that fix's accuracy, and its car row's `speed_kph` |
| `gps_step_m` | whole metres from the previous cloud position of the same source |
| `gps_last_dist_m` | whole metres from the cloud position to the last non-zero car fix taken at or before the cloud row's received time, however old (a parked car has no fix within 600 s) |
| `gps_last_age_s` | that fix's age at the cloud row's time, 1 decimal, 0 to 30 days |

None of the `gps_dist_m`, `gps_dt_s`, `gps_car_*`, `gps_step_m` or `gps_last_*` keys
appears when the cloud position is absent, invalid or exactly (0, 0). The `gps_last_*`
keys are also absent when no car fix precedes the row.

On the cloud side, **null and absent differ**: a payload key present with `null` is
written as `null`, and a key the payload does not have is left out. This holds inside
`charging` and `energy_distribution` too, and for `gps` (`gps_present: null`), `gpsFresh`,
`climate` (`hvac_on: null`) and `lastUpdated`. Integers stay integers and booleans stay
booleans. A value of the wrong type or out of range is dropped and counted in
`meta.stats.invalid_values`. Its names come from a fixed set (`INVALID_NAMES`): every key
a line can carry, `<object>_<field>` for each field of a nested object (such as
`charging_soc` or `doors_fl`), and `data`, `payload`, `has_fix`, `gps_present`,
`gps_fresh`, `hvac_on`, `last_updated_t` and `first_seen_t`. The check accepts no other
name. Errors and skips carry no payload: the SQL returns it only
for `poll`/`ok` and `mqtt` rows. Payload keys that are not listed (`placeKind`,
`fuelPercent`, `lastStationaryPollAt`, ...) are dropped and counted in
`meta.stats.dropped_cloud_keys`, by name only for the fixed names above.

### `meta`: one, last in the pod stream, first in the kept file

`type`, `version` (1), `start` (the shifted window start, `YYYY-MM-DDTHH:MM:SSZ`; the
window is the longer of the two and ends at `--until`), `car_days`, `cloud_days`, `vin`
(`TESTVIN0000000001`), `model` (`cars.model_key` when its source is `vin` or `name` and
it matches `[a-z0-9_-]{1,40}`; otherwise `unknown`), `energy` (`ev` | `hybrid`),
`device_source` (always `car_app`), `app_version` (only if it matches
`^[0-9]{1,3}(\.[0-9]{1,3}){1,3}(-debug)?$`), `first_seen_t`
(`car_app_devices.first_seen_at` on the `t` clock, may be negative), and `stats`:

```json
{"car_rows": 0, "cloud_rows": {"poll_ok": 0, "poll_error": 0, "poll_skip": 0, "poll_other": 0,
  "mqtt_vehicleInfo": 0, "mqtt_remoteControl": 0, "mqtt_smartCharge": 0, "mqtt_energyConsumption": 0,
  "mqtt_other": 0, "other": 0},
 "dropped_car_keys": {}, "dropped_cloud_keys": {}, "invalid_values": {},
 "dropped_details": 0, "unparsable_last_updated": 0, "cloud_fixes_matched": 0, "cloud_fixes_unmatched": 0}
```

`cloud_rows.other` counts rows whose source is neither `poll` nor `mqtt`. They are not
written.

The local check also enforces each stream's own window from the meta line. With
`max_days = max(car_days, cloud_days)`, car lines need
`(max_days - car_days) * 86400 <= t <= max_days * 86400`, and cloud lines need the same
with `cloud_days`. Otherwise the check fails with `out_of_range on <type> line`.

### Dry run

One line: `cars`, `device_sources` (`car_app`, `simulator`, `other`, `none`), `car_rows`,
`car_days_with_rows`, `cloud_rows` (the buckets above), `cloud_days_with_rows`, and
`plans`, which has, for each of `identify`, `metadata`, `car_rows`, `car_rows_resumed`,
`car_count`, `cloud_rows`, `cloud_rows_resumed` and `cloud_count`, the nodes of that
query's first-page or resumed-page plan. The line also has `ingest_events_plan_ok`, which
is always `true` in a dry run that printed. The dry run's own counts read `ingest_events`,
so when a printed plan of an `ingest_events` statement fails the gate's rule, the dry run
stops with the same message as the export before counting anything, and each count
statement is gated on its own plan as well. `--explain` prints `plans` alone and never
refuses, so run it first; dated partition names in relations and index names are
replaced by `car_telemetry_partition`.

How many statements a run issues (each in its own transaction, with a pause before each
one but the first). `D` is the number of UTC days the car window touches: 30 for the
default 29 days, or 29 when `--until` falls exactly on midnight. `C` is the same for the
cloud window: 14, or 13. With one page per day:

- `--explain`: `D` identify + 1 metadata + 8 EXPLAIN = 39.
- `--dry-run`: `D` identify + 1 metadata + 8 EXPLAIN + `D` car counts + `C` x (EXPLAIN +
  cloud count) = 97.
- the export: `D` identify + 1 metadata + `D` car pages + `C` x (EXPLAIN + cloud page) =
  89, plus one statement for each extra car page and two (EXPLAIN + page) for each extra
  cloud page of 2000 rows.

## Tests

`python/tests/test_mapping_export.py` uses only fabricated rows and a fake database. It
also runs the composed pod script end to end in a subprocess, with fake `app.config`,
`app.db` and `sqlalchemy` modules.

```sh
python -m pytest python/tests/test_mapping_export.py -p no:cacheprovider
```

## Comparing

`compare.py` lines the car's own readings (the ground truth) up with what the cloud said
at the same moments and reports where the cloud side is wrong, imprecise, late or missing.
Standard library only, deterministic, local.

```sh
python -m bench.mapping.compare <pairs.jsonl> --out-dir <dir>
```

It writes `<dir>/results.json` (every number) and `<dir>/report.md` (tables). Both hold
aggregates only: counts, quantiles (min, p5, p25, median, p75, p95, max), means, confusion
matrices, code tables and fitted slopes. No input row, no `t` of a single event, no
per-event listing; transition lags are listed as bare numbers only when there are fewer
than 10. Every table gives n (paired samples), the number of distinct days (`t // 86400`)
they span and the car-state split.

How it pairs:

- **Car timeline.** Car rows sorted by `t`; exact duplicates dropped, and a row identical
  to the previous one apart from `t` and at most 1.5 s later is a resend and dropped. A
  row holds for 8 s (moving), 90 s (stationary, charging, no cadence) or 660 s (event; the
  first real export had moving rows up to ~6.5 s apart and parked beats often ~600 s
  apart); after that the car is `unknown`, unless the gap is inferred (below). An event row does not carry speed, odometer, kWh, tyres,
  temperatures or charging power, so at such a moment those come from the newest full row
  if it is still fresh. A fresh full row without a key means the car had no value.
- **Car state.** `driving` (cadence moving, speed >= 1 km/h), `charging`
  (`charging_type_raw` 2; not when unplugged or type 1; else `charging_power_kw` > 0),
  `plugged_idle` (gun 2 or charger state 1), `on_stationary` (power level acc/on/ok or
  cadence moving/stationary), `parked_off`, `parked_inferred`, `unknown`.
- **Inferred parked gaps.** When the head unit falls silent and the rows on both sides of
  the gap say power off, show the same SoC and the same plug codes, and the nearest full
  readings on both sides show the same odometer, the SoC, plug codes, power level, gear,
  lock and odometer are taken as unchanged through the gap, in the separate state
  `parked_inferred`. Every statistic is split by state, so inferred pairs stay visible.
- **Pairing.** Each cloud row of `mqtt:vehicleInfo`, `mqtt:smartCharge`,
  `mqtt:energyConsumption` and `poll:ok` is paired with the car's value at the row's `t`
  (speed is interpolated between moving rows at most 5 s apart). Poll odometer is
  reported separately as contaminated, poll SoC both in full and only where `asleep` is
  false or `charging.soc` is present, poll GPS by `gps_fresh`.
- **Lags.** Speed: the constant lag (0-300 s, step 5) with the smallest mean absolute
  error against the car's speed over the cloud samples taken while driving. SoC and
  odometer: per sample, the time since the car stopped showing the cloud's value (and the
  time since it last showed it), within 1 h either way; and the constant lag (0-1800 s,
  step 10) at which the most cloud values equal the car's. Plug and charge transitions:
  the delay to the first poll, the first smartCharge push and (charge only) the first
  vehicleInfo push whose `charge_power_kw` is > 0 (or 0) showing the new state.
- **Extra sections.** vehicle_on read as BYD's powerGear (awake rows with speed < 1 or
  null) against the car's power level and cadence; on/off callouts with car on = `on`
  alone, = acc/on/ok, and = cadence moving/stationary; SoC agreement under half-up
  rounding and floor; pushes after car-side changes and while parked; poll intervals and
  error/skip details by period (before the car app, car timeline fresh / inferred /
  unknown); online/asleep after car power off/on; poll SoC/odometer equal to the car while
  a vehicleInfo push of the last hour differs (a car-app contamination hint).

Unknown keys and new `detail` values are ignored or counted, never fatal. A section with
no paired samples says so.

```sh
python -m pytest python/tests/test_mapping_compare.py -p no:cacheprovider
```
