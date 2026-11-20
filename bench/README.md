# Benchmarks: pyBYD against openbyd

This directory compares [pyBYD](https://github.com/jkaberg/pyBYD) with openbyd, its Rust port, for an application that holds one client per customer account and wants to know what each account costs in CPU and memory.

```
bench/
  micro.py       one operation, pyBYD against openbyd, in the same Python process
  load.py        N accounts doing poll cycles against a fake server; CPU and memory per account
  mqtt_idle.py   memory and threads of connected-but-idle MQTT accounts
  replay_worker.py  a whole backend worker against replayed BYD behaviour; CPU, memory and what it detected
  replay_detect.py  step the backend's detection drivers through a scenario and compare detections exactly
  replay/        replay scenarios: the format (SCENARIO.md), generator, privacy check, exporter, and the worker's launcher
  realtime_levels.py   one reply model (VehicleRealtimeData) in Rust, through Python and as a whole call
  profile_realtime.py  where the time of one realtime reply goes, from outside (no instrumented build)
  linux_docker.ps1/.sh the Rust and Python levels on Linux, in a container, from a Windows machine
  report.py      renders bench/results/*.json as Markdown tables
  run_all.py     runs all three and the report; exits non-zero on any failure or wrong answer
  common.py      shared helpers
  results/       one JSON file per run: date, machine, openbyd and pyBYD commits, every number
../crates/openbyd-client/examples/
  fake_server.rs the fake BYD server as a process of its own (load benchmark)
  load.rs        openbyd's side of the load benchmark
  mqtt_idle.rs   the TLS test broker, and openbyd's side of the MQTT benchmark
  replay_server.rs  the fake server and broker answering a scenario on a replay clock (replay benchmark)
```

## Results

Full run of 2026-11-19 (evening) on one machine: AMD Ryzen 9 5900X (12 cores, 24 threads), 64 GiB, Windows 11 10.0.26200, Python 3.12.13, rustc 1.100.0, release builds. openbyd at commit `7104731ad4` (version 0.1.1); pyBYD at commit `2c3d80648e` (`parity/UPSTREAM_COMMIT`). Other work was running on the machine during parts of the run (about 10% of all cores busy before the micro-benchmarks, more during the load runs), so read every figure with its spread. A full run takes about 45 minutes, almost all of it pyBYD at 200 and 1000 accounts.

The micro-benchmark tables and "VehicleRealtimeData at every level" were measured again on 2026-11-20 at commit `f464ab6f90`, after the cipher's tables were folded and reply parsing was sped up (both described under that heading). The load and MQTT tables are still the 0.1.1 run; the three fast implementations of the load benchmark were run again and are quoted in the headlines.

### Headlines

- **Cipher:** about 1,600 to 2,000 times faster than pyBYD's Python cipher (it was 229 to 256 times in 0.1.1: one block went from about 670 ns to 69 ns). pyBYD with `openbyd.pybyd_accel` installed gets the same cipher and is 880 to 1,600 times faster than without.
- **One signed request on localhost:** 109.6 ms in pyBYD, 0.64 ms in pyBYD with the Rust cipher, 0.45 ms in openbyd (1.37 and 1.24 ms in 0.1.1).
- **One whole `get_vehicle_realtime` call** (two requests): 125 ms in pyBYD, 1.19 ms in pyBYD with the Rust cipher, 0.64 ms in openbyd (1.99 and 1.51 ms in 0.1.1). On Linux: 122 ms, 1.19 ms and 0.71 ms.
- **Reply models:** openbyd is 2.5 to 4.8 times faster on every model. The full realtime reply takes 100 us in pyBYD and 27 us in openbyd (36 us in 0.1.1), and 96 against 25 us on Linux. An earlier version of this page showed openbyd slower here; that was before three optimisation passes.
- **CPU per poll cycle:** about 250 ms in pyBYD (314 ms at 1000 accounts). The fast implementations, run again on 2026-11-20 (`results/partial-load-20261120-013842.json`, three runs each): 2.9 to 3.6 ms in pyBYD with the Rust cipher (3.5 to 5.2 in 0.1.1), 0.8 to 1.0 ms in openbyd on one worker thread (1.6 to 2.4), 1.3 to 3.6 ms on the default workers (3.4 to 5.2). Memory per account did not change.
- **Memory per account:** about 950 KiB in pyBYD, 113 to 148 KiB with the Rust cipher, 89 to 226 KiB in openbyd on one worker thread.
- **Idle MQTT connection:** 727 KiB and one thread in pyBYD; 54 to 80 KiB and no extra thread in openbyd 0.1.1, and 100 to 130 KiB since the switch to rumqttc-next (see "Idle MQTT connections").
- **The whole worker, replayed:** BYD Connect's real worker, serving 100 accounts for 30 minutes of recorded car behaviour, used 177 ms of CPU per account per minute with pyBYD's Python cipher and 40 ms with openbyd's: about 4.5 times less for the same polls, with peak memory down from 350 to 220 MiB. What remains is the worker's own work, which the cipher does not touch.

### Where openbyd is not faster

- **Cipher alone:** openbyd's direct call and pyBYD with the Rust cipher run the same cipher. Now that it takes a few microseconds, what pyBYD's codec does around it shows: 6 to 8 us against 4 to 5 us at 512 B, and within a quarter of each other at 8 KB.
- **Default tokio workers:** the native client on the default worker count uses more CPU per cycle (3.4 to 5.2 ms in the tables below, 1.3 to 3.6 ms when run again with the faster cipher) and more memory per account than on one worker, in exchange for finishing sooner. One worker is the efficient setting for this workload.
- **Memory at 50 accounts:** openbyd on one worker used more memory per account (226 KiB) than pyBYD with the Rust cipher (148 KiB); from 200 accounts up it uses less.

### What could not be measured

- TLS on the HTTP side and real network latency: the fake server speaks plain HTTP on localhost.
- A Rust worker in the replay: it does not exist yet. It will be one more row of the replay benchmark's table of variants.
- More than 30 minutes of the scenario per run, and so whole charges: the replay runs at the speed of the wall clock (see the limits).
- The load, MQTT and replay benchmarks on Linux, and anything on macOS. Reply parsing and the whole call were measured on Linux in a container on this machine (below).
- pyBYD with 1000 idle MQTT connections, and the delivery of a push message.

### VehicleRealtimeData at every level

One reply model, measured wherever "is openbyd slower?" can be asked, by `realtime_levels.py` (Windows) and `linux_docker.ps1` (the same code in a `python:3.12-slim` container on the same machine, glibc 2.41). Cells are the median of 7 measurements of at least 0.5 s, with the fastest and slowest in brackets; the Rust rows are the median of three runs of the Rust benchmark. "0.1.1" is commit `7104731ad4`, "now" is `f464ab6f90`. Windows files: `results/realtime-levels-20261120-0.1.1.json` and `results/realtime-levels-20261120-branch.json`.

#### Windows

| Level | pyBYD | openbyd 0.1.1 | openbyd now | now against pyBYD |
|---|---|---|---|---|
| Rust only: `parse` (system allocator) | | 17.6 us [17.4, 19.4] | 14.8 us [14.1, 15.7] | |
| Rust only: `parse_owned` + `into_dump`, with a 6.5 us clone of the input | | 28.9 us [28.4, 33.2] | 27.7 us [27.3, 29.3] | |
| Python API, the full synthetic reply (141 keys) | 98.1 us [96.2, 101.8] | 36.7 us [36.2, 37.9] | 26.7 us [26.3, 27.6] | 3.7x faster |
| Python API, the other synthetic payloads (3 to 9 keys) | 25 to 47 us | 12.6 to 16.7 us | 8.1 to 11.4 us | 3.1 to 4.2x faster |
| Python API, 22 real replies from more cars (3 to 141 keys), mean | 76.6 us | 28.6 us | 20.8 us | 3.1 to 4.1x faster |
| Whole call, one after another | 125 ms [123, 136] | 1.51 ms [1.46, 1.53] | 0.64 ms [0.64, 0.65] | 195x faster |
| Whole call, 8 at a time, per call | 122 ms [121, 129] | 0.95 ms [0.88, 1.01] | 0.32 ms [0.31, 0.33] | 380x faster |
| The same with pyBYD on the Rust cipher, one after another | 1.19 ms [1.17, 1.21] (1.99 in 0.1.1) | | 0.64 ms | 1.9x faster |
| The same with pyBYD on the Rust cipher, 8 at a time | 0.99 ms [0.96, 1.00] (1.64 in 0.1.1) | | 0.32 ms | 3.1x faster |

#### Linux (container)

| Level | pyBYD | openbyd 0.1.1 | openbyd now | now against pyBYD |
|---|---|---|---|---|
| Rust only: `parse` (system allocator) | | 15.0 us | 12.9 us [12.6, 13.4] | |
| Rust only: `parse_owned` + `into_dump`, with the clone | | 24.4 us | 22.4 us [21.9, 23.8] | |
| Python API, the full synthetic reply | 96.2 us [95.1, 96.8] | 36.4 us [36.1, 36.9] | 25.1 us [25.0, 25.6] | 3.8x faster |
| Python API, 22 real replies, mean | 76.5 us | 28.0 us | 20.0 us | 2.9 to 4.1x faster |
| Whole call, one after another | 122 ms [120, 126] | 1.56 ms | 0.71 ms [0.69, 0.75] | 172x faster |
| Whole call, 8 at a time, per call | 121 ms [120, 121] | 0.72 ms | 0.25 ms [0.25, 0.26] | 480x faster |
| The same with pyBYD on the Rust cipher, one after another | 1.19 ms [1.17, 1.23] (2.04 in 0.1.1) | | 0.71 ms | 1.7x faster |
| The same with pyBYD on the Rust cipher, 8 at a time | 0.89 ms [0.89, 0.91] (1.29 in 0.1.1) | | 0.25 ms | 3.5x faster |
| Python API, full reply, wheel built without mimalloc (glibc malloc) | 96.5 us | 40.5 us | 29.7 us [29.4, 30.0] | 3.2x faster |

A whole call is `Client.get_vehicle_realtime` against the fake server in the same process: a trigger request and one poll, with the poll interval at zero and MQTT off on both sides, after login and one warm-up call. openbyd returns the dump as a dict; pyBYD returns its model object.

What changed between the two columns:

* **The cipher.** The white-box cipher made every output byte of a round through a chain of three nibble lookup tables; those chains are now folded into the round tables when the crate is built (`crates/openbyd-crypto/build/compile.rs`), which is checked entry by entry and leaves the output unchanged. One block takes 69 ns instead of 670, and the 9.7 KB envelope of a realtime reply decodes in about 66 us instead of about 550. The cipher was about 55% of the client's CPU in a call; pyBYD with `pybyd_accel` gets the same gain. Base64 is decoded a group at a time (34 us to 6 us for that envelope).
* **`raw` from the caller's objects.** `openbyd.models.parse` used to convert the reply to Rust values, parse, and convert the whole reply back to Python for the dump's `raw`. It now copies the caller's own objects for `raw`.
* **Key plans.** A model's first parse records which keys its fields read; later parses fill those from one pass over the reply instead of building a hash table of it.

Where the time of one reply goes, before and after, is in [`crates/openbyd-protocol/benches/README.md`](../crates/openbyd-protocol/benches/README.md).

### Micro-benchmarks

Run on 2026-11-20T02:58:49+02:00. AMD Ryzen 9 5900X 12-Core Processor, 24 logical cores, 63.9 GiB, Windows-11-10.0.26200-SP0; Python 3.12.13, rustc 1.100.0 (0ef1bc386 2026-11-09). openbyd commit `f464ab6f90`; pyBYD commit `2c3d80648e`.

Other activity on the machine before the run: 4% of all cores.

Each cell is the median time per operation of 5 measurements of at least 0.5 s, with half the spread (max - min) / median as +-. "openbyd vs pyBYD" is a factor of the medians, or "within noise" when the measurements' ranges overlap.

#### Envelope cipher

| Row | pyBYD | pyBYD + openbyd cipher | openbyd | openbyd vs pyBYD |
|---|---|---|---|---|
| encode 512 B | 7.38 ms (+-2%) | 6.4 us (+-1%) | 4.7 us (+-2%) | 1578.5x faster |
| decode 512 B | 7.36 ms (+-1%) | 8.4 us (+-4%) | 4.0 us (+-3%) | 1827.7x faster |
| encode 2 KB | 29.01 ms (+-5%) | 19.8 us (+-3%) | 17.2 us (+-3%) | 1683.4x faster |
| decode 2 KB | 29.38 ms (+-4%) | 23.6 us (+-4%) | 14.8 us (+-2%) | 1989.2x faster |
| encode 8 KB | 118.46 ms (+-2%) | 74.5 us (+-1%) | 67.1 us (+-1%) | 1764.5x faster |
| decode 8 KB | 115.18 ms (+-3%) | 82.2 us (+-6%) | 57.3 us (+-1%) | 2008.4x faster |

#### Reply models

| Row | pyBYD | openbyd | openbyd vs pyBYD |
|---|---|---|---|
| VehicleRealtimeData, every field present (headline) | 100.4 us (+-3%) | 27.5 us (+-4%) | 3.7x faster |
| AcCurrentLimit (2 payloads) | 5.7 us (+-15%) | 1.5 us (+-5%) | 3.7x faster |
| BookingList (2 payloads) | 8.6 us (+-3%) | 2.8 us (+-2%) | 3.1x faster |
| ChargingStatus (1 payload) | 4.7 us (+-2%) | 1.5 us (+-4%) | 3.2x faster |
| EnergyConsumption (2 payloads) | 49.0 us (+-3%) | 14.4 us (+-4%) | 3.4x faster |
| GpsInfo (2 payloads) | 5.5 us (+-17%) | 1.3 us (+-4%) | 4.4x faster |
| HvacStatus (4 payloads) | 9.8 us (+-2%) | 2.8 us (+-3%) | 3.5x faster |
| LatestConfigFunction (1 payload) | 5.4 us (+-3%) | 1.6 us (+-3%) | 3.3x faster |
| PushNotificationState (2 payloads) | 10.2 us (+-5%) | 2.2 us (+-3%) | 4.5x faster |
| Vehicle (3 payloads) | 7.8 us (+-3%) | 2.7 us (+-2%) | 2.9x faster |
| VehicleLatestConfig (2 payloads) | 93.7 us (+-3%) | 19.6 us (+-2%) | 4.8x faster |
| VehicleMsgItemInfo (2 payloads) | 7.7 us (+-3%) | 2.1 us (+-5%) | 3.8x faster |
| VehicleMsgListResponse (1 payload) | 19.9 us (+-8%) | 7.9 us (+-5%) | 2.5x faster |
| VehicleRealtimeData (5 payloads) | 60.7 us (+-3%) | 14.3 us (+-3%) | 4.2x faster |

*per payload, averaged over the model's corpus payloads; pyBYD also validates and builds Python objects*
*the full realtime payload, 3.6 KB of JSON*

#### shadow.compare

| Row | openbyd |
|---|---|
| VehicleRealtimeData (every field present) | 67.3 us (+-5%) |
| GpsInfo (valid fix, speed in km/h (coordinates anonymised)) | 2.9 us (+-6%) |

*absolute cost of one comparison (parse in openbyd + compare with pyBYD's dump); no pyBYD equivalent. Answer must be [] (no differing field).*

#### One signed request, localhost

| Row | pyBYD | pyBYD + openbyd cipher | openbyd | openbyd vs pyBYD |
|---|---|---|---|---|
| sequential (latency) | 109.62 ms (+-1%) | 642.4 us (+-2%) | 451.7 us (+-0%) | 242.7x faster |
| 200 at a time (throughput) | 109.16 ms (+-1%) | 491.4 us (+-2%) | 197.9 us (+-1%) | 551.5x faster |

*inner payload built once and shared; reply is the 3.6 KB realtime payload; the server runs in a thread of this process, so it shares the CPU but not the GIL*

#### Login

| Row | pyBYD | openbyd | openbyd vs pyBYD |
|---|---|---|---|
| one login (sequential) | 23.47 ms (+-1%) | 340.6 us (+-2%) | 68.9x faster |

*pyBYD's BydClient.login() against openbyd.lowlevel.login(); both hash the password, sign and open the reply; compared on the user id*

*Source: `bench/results/micro-20261120-025849.json`*

### Load: what an account costs

Run on 2026-11-19T21:26:50+02:00. AMD Ryzen 9 5900X 12-Core Processor, 24 logical cores, 63.9 GiB, Windows-11-10.0.26200-SP0; Python 3.12.13, rustc 1.100.0 (0ef1bc386 2026-11-09). openbyd commit `7104731ad4`; pyBYD commit `2c3d80648e`.

Each account logs in, lists its vehicles and runs 5 poll cycles (realtime, GPS, charging status, HVAC status, energy consumption; 37 requests in all) with 100 ms think time between cycles. The fake server runs in its own process and delays every reply by 20 ms; pollable reads sleep 0.05 s before their poll. CPU is user + system seconds of the client process during the run, divided by accounts x cycles (so it includes the one-off login and vehicle list); with several runs the table shows the median run and the range. Memory is the peak resident size minus the size before the run, per account.

#### 50 accounts

| Implementation | CPU ms / cycle | Memory / account | Peak RSS | Requests / s | Wall | Threads (peak) | Loop lag median / p99 / max | Failed calls | Requests seen / planned |
|---|---|---|---|---|---|---|---|---|---|
| pyBYD | 246.5 (range 246.1-247.4, 3 runs) | 981 KiB | 102 MiB | 30 | 61.9 s | 37 | 783.3 / 2873 / 3607 ms | 0 | 1850 / 1850 |
| pyBYD + openbyd cipher | 3.5 (range 3.4-4.1, 3 runs) | 148 KiB | 65 MiB | 853 | 2.2 s | 23 | 0.0 / 26 / 30 ms | 0 | 1850 / 1850 |
| openbyd native, 1 worker | 1.9 (range 1.2-1.9, 3 runs) | 226 KiB | 17 MiB | 792 | 2.3 s | 6 | n/a (no event loop) | 0 | 1850 / 1850 |
| openbyd native, default workers | 3.4 (range 1.8-4.0, 3 runs) | 438 KiB | 27 MiB | 815 | 2.3 s | 29 | n/a (no event loop) | 0 | 1850 / 1850 |

#### 200 accounts

| Implementation | CPU ms / cycle | Memory / account | Peak RSS | Requests / s | Wall | Threads (peak) | Loop lag median / p99 / max | Failed calls | Requests seen / planned |
|---|---|---|---|---|---|---|---|---|---|
| pyBYD | 251.9 | 949 KiB | 239 MiB | 29 | 252.5 s | 37 | 2857.3 / 21616 / 22218 ms | 0 | 7400 / 7400 |
| pyBYD + openbyd cipher | 4.0 (range 3.9-4.5, 3 runs) | 123 KiB | 82 MiB | 1832 | 4.0 s | 37 | 9.4 / 79 / 92 ms | 0 | 7400 / 7400 |
| openbyd native, 1 worker | 1.6 (range 1.6-1.8, 3 runs) | 115 KiB | 28 MiB | 2787 | 2.7 s | 6 | n/a (no event loop) | 0 | 7400 / 7400 |
| openbyd native, default workers | 3.8 (range 2.0-4.0, 3 runs) | 211 KiB | 47 MiB | 3221 | 2.3 s | 29 | n/a (no event loop) | 0 | 7400 / 7400 |

#### 1000 accounts

| Implementation | CPU ms / cycle | Memory / account | Peak RSS | Requests / s | Wall | Threads (peak) | Loop lag median / p99 / max | Failed calls | Requests seen / planned |
|---|---|---|---|---|---|---|---|---|---|
| pyBYD | 314.2 | 935 KiB | 967 MiB | 23 | 1590.9 s | 37 | 15630.9 / 93535 / 109089 ms | 0 | 37000 / 37000 |
| pyBYD + openbyd cipher | 5.2 (range 4.9-5.3, 3 runs) | 113 KiB | 168 MiB | 1405 | 26.3 s | 37 | 416.5 / 828 / 1113 ms | 0 | 37000 / 37000 |
| openbyd native, 1 worker | 2.4 (range 2.2-2.5, 3 runs) | 89 KiB | 92 MiB | 3029 | 12.2 s | 6 | n/a (no event loop) | 0 | 37000 / 37000 |
| openbyd native, default workers | 5.2 (range 4.0-7.0, 3 runs) | 154 KiB | 156 MiB | 11468 | 3.2 s | 29 | n/a (no event loop) | 0 | 37000 / 37000 |

*Source: `bench/results/load-20261119-212650.json`*

### Idle MQTT connections

Run on 2026-11-19T21:29:28+02:00. AMD Ryzen 9 5900X 12-Core Processor, 24 logical cores, 63.9 GiB, Windows-11-10.0.26200-SP0; Python 3.12.13, rustc 1.100.0 (0ef1bc386 2026-11-09). openbyd commit `7104731ad4`; pyBYD commit `2c3d80648e`.

N connections to the TLS test broker (a separate process), each subscribed to its own topic, then idle (5.0 s window after 3.0 s to settle, keepalive 120 s). **Certificate verification is off on both sides** (pyBYD's paho `tls_set` is patched in the benchmark's worker process only). Memory per connection is the idle resident size minus the size before connecting, over N; threads are those added by the connections.

| Implementation | Connections | Memory / connection | Idle RSS | Peak RSS | Threads (idle) | Threads added | Idle CPU | Connected |
|---|---|---|---|---|---|---|---|---|
| pyBYD (paho, one thread per client) | 200 | 727 KiB | 195 MiB | 195 MiB | 209 | 201 | 0.62% of a core | 200 / 200 |
| openbyd MqttRuntime (tokio) | 200 | 80 KiB | 22 MiB | 22 MiB | 29 | 1 | 0.00% of a core | 200 / 200 |
| openbyd MqttRuntime (tokio) | 1000 | 54 KiB | 59 MiB | 59 MiB | 29 | 1 | 0.00% of a core | 1000 / 1000 |

*Source: `bench/results/mqtt_idle-20261119-212928.json`*

**Since the switch to rumqttc-next** (rumqttc-v5-next 0.34.0, replacing rumqttc 0.25.1), an idle openbyd connection costs about 45 KiB more. In three runs of each version, alternating, on 2026-11-20: 130 KiB per connection at 200 (78 KiB before) and 100 KiB at 1000 (55 KiB before), about 104 MiB resident for 1000 connections. Threads, idle CPU, connect time and connect success did not change. Most of the extra is allocated inside the library for every connection: four 65,536-bit packet-identifier tables where rumqttc 0.25.1 had one (+24 KiB), and an event queue reserved for 128 events of 400 bytes instead of 100 of 392 (+13 KiB). Message delivery (3 KB pushes; round trip, a 50,000-message burst, 500 connections at once) was level within the run-to-run spread.

### Replay: the whole worker

This result shows the timing noise in detections when the worker schedules its own polls. Use [deterministic detection replay](#deterministic-detection-replay-replay_detectpy) to check whether variants detect the same drives and charges.

Runs of 2026-11-20, 00:00 to 04:00, on the same machine. The backend's mqtt-worker (backend commit `a5ae63a1e5`, Python 3.12.13, pyBYD 0.0.85, openbyd 0.1.1) against the replay server playing 30 minutes of a scenario exported from production: 100 accounts with one car each (75 electric, 25 hybrid), from a 13-day export of 231,832 car states, 55,217 errors and 577,289 pushes. The stretch replayed was chosen for being busier than most: in production 15 cars moved in it, about 30 drives started and ended, and three charges started. Every run starts from an empty database and the measured window begins once the worker holds a session for all 100 accounts. Postgres and Redis ran in containers; the reply templates were those of the corpus with the real realtime replies of several car models (the `corpus/realtime-samples` branch). The Python-cipher and Rust-cipher variants were run three times each, interleaved; the variant with shadow parsing twice, afterwards.

| Variant | CPU ms / account / minute | CPU s in the window | Peak RSS | Memory growth / account | Loop lag, mean / p99 | Time per poll | BYD requests | Polls stored |
|---|---|---|---|---|---|---|---|---|
| pyBYD cipher (the worker as it is today) | 176.6 (range 159.1-197.3, 3 runs) | 531 (478-592) | 350 MiB (347-352) | 2,229 KiB | 14-24 ms / 196-271 ms | 4.1-4.5 s | 9,838-10,213 | 1,072-1,113 |
| openbyd cipher | 39.6 (range 35.9-40.0, 3 runs) | 119 (108-120) | 220 MiB (219-220) | 877 KiB | 7 ms / 49 ms | 3.6-3.7 s | 10,202-10,410 | 1,103-1,124 |
| openbyd cipher + shadow parsing | 37.1 (36.2-38.0, 2 runs) | 112 (109-114) | 220 MiB | 883 KiB | 7 ms / 49 ms | 3.7-3.8 s | 10,315-10,368 | 1,118-1,122 |

*CPU is user + system time of the worker process over the 30 minutes, divided by 100 accounts. Memory growth is the peak resident size minus the size when the worker was ready, before it had claimed an account. Loop lag is the worker's own histogram (how late a one-second timer fires); 49 ms is what an idle worker reads on Windows. In every run the replay sent the same 996 pushes over MQTT (0 to 2 of them while the account was reconnecting), expired 4 sessions and saw 103 logins: the 100 accounts and 3 logins after an expiry.*

What the table says:

- **The cipher was three quarters of the worker's CPU.** With the same scenario and nearly the same requests (the Python-cipher worker made 2 to 5% fewer, because it falls behind), the worker used 4.5 times less CPU with openbyd's cipher: 48.6 to 58.0 ms of CPU per BYD request against 10.4 to 11.7 ms. One core would carry about 340 accounts at this activity with the Python cipher and about 1,500 with openbyd's.
- **The rest is the worker.** The 40 ms per account per minute that remain are everything else the worker does: pyBYD's request building and reply models, the HTTP client, the database and Redis writes, the drive and charge logic, and one MQTT thread per account (110 to 125 threads). A Rust client addresses the first two and the threads; the micro and load benchmarks above measure only those.
- **Memory:** 130 MiB less at the peak, 877 KiB per account instead of 2,229 KiB.
- **Responsiveness:** with the Python cipher the worker's event loop ran a mean of 14 to 24 ms late with a 99th percentile of 196 to 271 ms, a poll took 4.1 to 4.5 s, and 10 to 13 cars at a time were due for a poll and waiting at the worst moment. With openbyd's cipher lag stayed at the floor this machine can measure, a poll took 3.6 to 3.8 s, and 7 cars waited at the worst moment (10 in one run).
- **Shadow parsing costs nothing measurable:** its two runs sit inside the range of the runs without it.
- **Spread:** the Python-cipher runs differ by 20% and the Rust-cipher runs by 10%, in step with how busy the rest of the machine was (5 to 12% of all cores during the runs; another benchmark ran on it for part of the night). The ranges do not overlap.

**Drives and charges.** Every run, of every variant, ended with 17 drives and 8 charges, counting the ones still open when the window closed (10 or 11 drives and 3 charges closed, 6 or 7 drives and 5 charges open). They were not the same within the comparison's tolerances, and that is a result, not noise to be averaged:

| Pairs of runs | Pairs | Same detections | Differences per pair |
|---|---|---|---|
| Two repeats of one variant | 7 | 0 | 1 to 4 (mean 2.4) |
| openbyd cipher against openbyd cipher + shadow parsing | 6 | 0 | 2 to 6 (mean 3.7) |
| pyBYD cipher against either openbyd-cipher variant | 15 | 0 | 4 to 7 (mean 5.1) |

No two runs agreed, not even two runs of the same code: where a drive or a charge ends depends on when the worker happens to poll, and between two repeats of one variant a drive's end moved by 109 to 306 s and a charge's by 130 to 588 s, against a tolerance of 90 s. One drive had or had not been closed by the end of the window. Between the Python-cipher worker and the Rust-cipher worker there were about twice as many differences, all of the same kinds (a drive's end 127 to 303 s apart, a charge's end 98 to 611 s apart, the drive at the window's edge, and one drive whose start moved by just over 300 s and so counted as two different drives). Five of the 100 cars account for every difference. Two of them differ in 14 and in all 15 of the pairings of a Python-cipher run with a Rust-cipher run, but in only 3 and 1 of the 7 pairings of two repeats: for those two cars the workers place a drive's or a charge's end minutes apart consistently, because the Python-cipher worker polls later and less often and so sees the car stop at a different moment. The other three differ about as often between repeats as between variants. No drive or charge was found by one variant and not at all by the other. Distances, odometers and energy agreed wherever a drive was matched; a charge's final SoC differed by one point where its end moved.

**The replay itself.** 99.7 to 99.8% of the values in the worker's stored polls (SoC, odometer, speed, online, charging: about 5,000 values per run) equal the scenario's state at that moment, and `online` and `charging` always did; the rest are values that changed in the seconds between the reply and the moment it was stored. The network guard refused 66 to 81 lookups of the geocoder per run and nothing else, under one second of CPU.

**Two things the replay found that are not about cost.** With shadow parsing on, openbyd's comparison called 1,241 of about 9,000 replies different from pyBYD's result: every one of the capabilities objects the worker builds from a car's configuration, on all 44 fields, and one realtime reply on `rate`. The capabilities case reproduces outside the replay (`openbyd.shadow.compare` on pyBYD's `VehicleCapabilities` built from the corpus's own payload returns every field), so it is the comparison, not the replay, and it would count a mismatch on every such read in production. And in a synthetic scenario with two cars on one account, the worker stored the second car's polls with the first car's higher odometer; the export has one car per account, so the figures above are not affected.

*Source: `bench/results/replay_worker-20261120-035344.json`. It holds the measurements and the counts; the drives and charges themselves stay with the scenario.*

### Replay: detections, deterministic

**The three variants detect exactly the same drives and charges.** With the poll schedule taken from the recording and the clock virtual (`replay_detect.py`), every variant gave identical results in both of its runs, and the variants agree on every stored value. No tolerance was applied. The comparison covered every column of every closed drive and charge, and the drives and charges still open at the end.

Run of 2026-11-20 on the same machine and the same export as above: the first 8 accounts (one car each), the first day of the 13 (86,400 s, 4,945 steps per run), all three variants twice. Backend commit `0cbe35e3` (the replay driver on top of the worker of `bench/replay-worker`), Python 3.12.13, pyBYD 0.0.85, openbyd 0.1.1, Postgres on tmpfs.

| Variant | Drives | Charges | Open at the end | Cars with detections | Identical across 2 runs | Against the pyBYD-cipher worker |
|---|---|---|---|---|---|---|
| pyBYD cipher | 11 | 3 | 0 | 4 of 8 | Yes | (reference) |
| openbyd cipher | 11 | 3 | 0 | 4 of 8 | Yes | Exactly equal |
| openbyd cipher + shadow parsing | 11 | 3 | 0 | 4 of 8 | Yes | Exactly equal |

*In every run 13 of the 4,945 steps ended with the account's MQTT subscription down, as many in every run. The launcher retried no connections and refused 2,686 geocoder lookups per run.*

**A wider run, all 13 days.** Before the 8-account run, 25 accounts over the whole 13 days were replayed with all three variants twice. That run found 448 drives and 190 charges, plus 4 drives and 3 charges still open. Five of the six runs agreed exactly with one another, the pyBYD-cipher run against both openbyd variants included. The sixth was the second pyBYD-cipher run, and it split one charge in two. In that run Windows had refused one loopback connection (`WinError 52`, out of ephemeral ports), the worker took it for a BYD outage, and its re-login failed at exactly the moment the charge was cut. Nine such connection failures happened across the six runs. The other eight changed no detection, and the repeat check caught the one that did. The launcher now retries a loopback connection the machine itself failed (see the deterministic-replay section below). The 13-day run is therefore no proof of determinism, and its result file is not published. Its comparisons, made afterwards on the private file, found no difference between the variants beyond that one charge.

**Against the real-time replay above.** In the real-time runs, 17 drives and 8 charges in 30 minutes ended minutes apart between any two runs, of one variant or of two. That spread comes from when the worker happens to poll, not from the cipher or the parsing. With the poll times fixed, no variant moves a drive or a charge by a second.

**What this replay does not test:** the worker's own scheduling (the moments it polls are taken from the recording), concurrency between accounts in one worker, BYD's latency, real Redis, and the geocoder. Two-car accounts are covered only by the synthetic sample. The run covers 8 of the 100 accounts and 1 of the 13 days, to save machine time. The 13-day run above shows the same agreement over 25 accounts. The remaining accounts and days run with the same command (`--accounts`, `--until`, `--merge` for chunks).

*Source: `bench/results/replay_detect-20261120-153535-769165.json` (aggregate figures only; the drives and charges stay with the scenario).*

## What each benchmark measures, and why

### Micro-benchmarks (`micro.py`)

One operation, run in one Python process, on the same input for every implementation. Before anything is timed each row is run once on every side and the answers are compared (JSON text for models, the exact string or bytes for the cipher, the decrypted reply for requests); a difference fails the run. Then each side is warmed up, a loop count is chosen so one measurement lasts at least 0.5 s, and the measurement is repeated 5 times. The tables show the median and half the spread, `(max - min) / median / 2`, as `+-`. A comparison is called **within noise** when the min..max ranges of the two sides overlap.

| Row | Compares | Note |
|---|---|---|
| Envelope cipher: encode and decode at 512 B, 2 KB, 8 KB | pyBYD's `BangcleCodec` with its Python cipher; the same codec after `openbyd.pybyd_accel.install()`; `openbyd.encode_envelope` / `decode_envelope` | Pure CPU, one thread. |
| Reply models, one row per model in `parity/corpus/` | `Model.model_validate(raw).model_dump(mode="json")` against `openbyd.models.parse(name, raw)` | Per payload, averaged over the model's corpus payloads. No `context` is passed on either side. The headline row is the corpus payload noted "every field present" (the full realtime payload). Not like for like: pyBYD builds a validated pydantic object and then dumps it; openbyd parses and converts straight to a dict. Both return the same dict. |
| `openbyd.shadow.compare` | openbyd only | Its absolute cost: one parse in Rust plus the comparison with pyBYD's own dump. There is no pyBYD equivalent. |
| One complete signed request | pyBYD's `post_token_json` through its `SecureTransport` (plain and with the Rust cipher), against `openbyd.lowlevel.post_token_json` | Against the fake server on localhost, which runs in a thread of the benchmark process. Sequential gives the latency of one request; 200 at a time gives the throughput. The reply is the 3.6 KB realtime payload. The same session, the same inner payload and the same server on both sides. |
| One login | pyBYD's `BydClient.login()` against `openbyd.lowlevel.login` | Password hashing, signing and opening the reply. |

Other operations both libraries expose publicly in Python: none that is not covered above. In openbyd, `models.properties` (the values of a model's `@property` attributes), `runtime.sleep` and `runtime.stats` have no pyBYD counterpart in one call and were left out. Operations that exist in openbyd only through internal test hooks (the MQTT payload decoder, the per-endpoint API flows, the car layer) were left out too; the in-Python `openbyd.Client` is being added by another branch and will be one more `Impl` per row.

### Load (`load.py`)

*What does one account cost?* N accounts each log in, list their vehicles, and run K = 5 poll cycles. A cycle reads realtime, GPS, charging status, HVAC status and energy consumption for one car, with 100 ms think time between cycles. That is 7 HTTP requests per cycle (realtime and GPS are a trigger followed by one poll, after a 50 ms wait) and 37 per account. The fake server runs in a process of its own (`fake_server` example) with the corpus payloads and delays every reply by 20 ms, so its CPU is not counted and the clients see something like a network.

Each implementation is a process of its own, measured from outside with psutil:

| Implementation | What runs |
|---|---|
| pyBYD | One `BydClient` per account, all on one asyncio loop, MQTT off. |
| pyBYD + openbyd cipher | The same after `openbyd.pybyd_accel.install()`. |
| openbyd native, 1 worker | `examples/load.rs`: one `BydClient::connect` per account on one shared `HttpPool`, a multi-threaded tokio runtime with 1 worker thread. |
| openbyd native, default workers | The same with tokio's default (one worker per core, 24 here). |

The driver starts the process, waits until it has imported everything and says `READY`, takes a baseline, says `go`, and reads the process's CPU time, resident memory and thread count when the work is done and before it exits. CPU is user plus system seconds *during the run only* (start-up and imports are excluded). Memory per account is the peak resident size minus the resident size at `READY`, over N. The Python variants also report **event-loop lag**: a task sleeps 10 ms in a loop, and the lag is how late it wakes up (median, 99th percentile and maximum). It is the time the loop was blocked or busy, and what a pyBYD service pays in delayed everything else.

All implementations must make the same number of requests. The fake server counts what it received; a run whose count differs from `N x 37`, or in which any call fails, fails the benchmark. The native runs are repeated 3 times and pyBYD with the Rust cipher too; pyBYD's own runs at 200 and 1000 accounts take many minutes and are run once. The table shows the run with the median CPU and the range of CPU.

Derived figures: **CPU ms per poll cycle** is the run's CPU over `N x K` (it includes the login and the vehicle list, 2 of 37 requests); **memory per account**; **requests per second** is the requests the server counted over the wall time (it is bounded by the think time and the server's delay, so it only says something when it is far below the others); **loop lag** for Python.

### Idle MQTT (`mqtt_idle.py`)

*What does a connected-but-idle account cost?* N connections to the TLS test broker (the `mqtt_idle broker` example, a process of its own), each subscribed to its own topic and then idle for a settling time, then a 5 s window. Reported: resident memory per connection (idle resident size minus the size before connecting, over N), the threads added, the CPU used in the window, and whether the broker saw N open connections and N subscriptions (otherwise the run fails). openbyd: N `MqttRuntime`s on one tokio runtime (`mqtt_idle client`). pyBYD: N `BydMqttRuntime`s, each with its own paho client and network thread, N = 200 only.

**Certificate verification is off on both sides.** pyBYD always uses TLS with the system's certificates and the test broker's certificate is made for the run, so the pyBYD worker patches paho's `Client.tls_set` (in that process only) to use `cert_reqs=CERT_NONE` and `tls_insecure_set(True)`. The openbyd example installs a certificate verifier that accepts anything. Both sides still do the full TLS handshake and encrypt the connection; neither checks a chain.

### Replay (`replay_worker.py`)

*What does the whole worker cost?* The other benchmarks measure the client library. This one measures the application that uses it: the mqtt-worker of BYD Connect's backend, which holds one BYD session and one MQTT connection per account, decides when to poll each car, stores what it reads and works out drives and charges. The worker is the real one, unmodified. What is replaced is BYD.

**The scenario.** A scenario (`bench/replay/SCENARIO.md`) says what a set of cars did over time: each car's state, the moments BYD answered an error, and the moments a push arrived. `replay/generate.py` makes synthetic ones. `replay/export_prod.py` builds one from a production table of recorded polls and pushes, anonymised inside the database pod before anything leaves it: accounts and VINs are renumbered with a salt that exists only in the pod's memory, coordinates are removed in SQL, every time is shifted by one random offset, only an allow-list of typed and bounded keys is written, and every line passes a leak check (anything shaped like a VIN, a database id, an e-mail address or a coordinate pair stops the export). Exported scenarios are never committed.

**The replay server.** `replay_server` (the fake BYD server and the TLS test broker in one process of its own) answers every account's requests from its cars' state at the current replay time. It fills the reply templates of `parity/corpus/` with the scenario's values, so the worker receives complete raw replies in BYD's wire format, encrypted and signed as BYD's are. It answers the recorded error codes while they last, expires sessions at a recorded 1005, and publishes the recorded pushes over MQTT, sealed with the account's session key. A car has no position in a scenario: the server gives each one a made-up anchor and moves it around that anchor as its odometer advances.

**A run.** For each variant, from nothing: fresh Postgres and Redis containers (data in memory), the replay server, the backend's seeding script (one entitled user per scenario account, with that account's cars), then `python -m app.mqtt_worker` from the backend checkout. With `--start-when-claimed` the replay clock starts once the worker holds a session for every account, so logins are not in the measured window. After `--minutes` the worker is stopped cleanly and everything is collected. Runs are sequential, variants interleaved (`A B A B`), and each starts from an empty database.

| Variant | What runs |
|---|---|
| pyBYD cipher | The worker as it is in production today: pyBYD with its Python envelope cipher (`BYD_NATIVE_CIPHER=false`). |
| openbyd cipher | The same worker with pyBYD's cipher replaced by openbyd's (`BYD_NATIVE_CIPHER=true`). |
| openbyd cipher + shadow parsing | The same, and every reply is also parsed by openbyd and compared with pyBYD's result (`BYD_SHADOW_SAMPLE_RATE=1`). |

A variant is one row of `VARIANTS` in `replay_worker.py`: a command and its environment. A worker written in Rust will be one more row.

**What is measured**, all from outside the worker: CPU seconds (user + system) and resident memory of the worker process over the window, and its threads (psutil); the worker's own Prometheus metrics, scraped every 5 s (event-loop lag, time per poll); the replay server's counters (requests by endpoint and by code, pushes sent); and the database afterwards (polls and pushes stored, drives and charges detected). The containers' and the replay server's CPU are not counted.

**Detection timing noise.** After each run the closed drives and charges are read from the database as lists per car, with times in replay seconds, and every run is compared with every other run. Two runs never poll at the same instants, so a boundary may move even between repeats of one variant. The comparison allows a start to differ by 300 s and an end by 90 s, distances by 0.05 km and SoC by 0.5, and lists differences beyond those thresholds. A check of sampled stored polls against the scenario shows whether the replay's replies carry the scenario's values through pyBYD and the backend's mapping. The deterministic replay below checks the detections themselves.

**Nothing leaves the machine.** The worker is started through `replay/launcher.py`, which before importing anything else makes every lookup of, and every connection to, an address that is not loopback fail, and reports what it refused. The backend also has a setting that sends every BYD request to one URL whatever the account's country. Either would be enough; both are on.

**Results that come from real cars.** For a scenario that is not synthetic, `bench/results/` gets aggregate figures only: no list of drives or charges, no car, no scenario name or start. The complete results stay next to the scenario, outside the repository.

### Deterministic detection replay (`replay_detect.py`)

The harness gives each account an in-process replay server and a separate guarded driver process. It advances the server's manual clock once per timeline second that has a state, error or push. The driver runs due timers, processes every push delivered by the broker, then polls the account's cars once if the schedule calls for it. A poll includes session recovery and MQTT resubscription before the next step can begin. A 1005 expiry schedules a poll. The [driver protocol](replay/DRIVER.md) describes the clock and MQTT barrier.

The three cipher variants use the backend's detection code against a fresh Postgres database on tmpfs and session state in memory. Up to eight account lanes run side by side, with the longest timelines queued first. Each variant runs twice by default. The harness compares every stored detection value and the sessions still open at the end. It reports a failed repeat check before comparing variants. The start-time window only pairs detections for listing differences. A paired value must still agree exactly. Exit status is 0 for agreement, 1 for a failed run or repeat check, and 2 for confirmed variant differences.

A run makes hundreds of thousands of short connections to the replay servers, and Windows can run out of ephemeral ports: a connect to `127.0.0.1` then fails (`WinError 52`), the worker takes it for a BYD outage, and its detections change. The first run on the export showed exactly that. Nine connects failed across six runs, and in one of them a re-login that failed split one charge in two, so the repeat check failed. The launcher therefore retries a loopback connect that the machine itself failed (`--retry-local-connects`). Nothing was sent, so the replay server never sees the attempt. The results record each retry as `connect_retries`, and a connect that fails every try fails the run.

Run from the openbyd checkout with the extension built with the testing feature. Use a separate backend checkout containing `scripts/replay_driver.py` and its installed Python environment:

```powershell
python bench\replay_detect.py --backend <backend> --backend-python <backend-venv>\Scripts\python.exe `
    --scenario <scenario.jsonl> --private-dir <private-directory>
```

`OPENBYD_REPLAY_BACKEND` and `OPENBYD_REPLAY_BACKEND_PYTHON` can supply the backend paths. Without `--scenario`, the harness uses the synthetic demo. Use `--quick --local-store` for a SQLite smoke run of the two cipher variants, or `--reference --quick` to exercise the protocol with the synthetic test driver. Quick aggregate results go under `--scratch`. Complete synthetic results default to `<scratch>/complete`.

For a long run, split accounts into disjoint chunks, for example `--accounts 1-20` and `--accounts acct-0021..acct-0040`. A single bare number selects the first N accounts, and lists accept names, numbers and comma-separated ranges. Merge the complete files with `python bench\replay_detect.py --merge <chunk1.json> <chunk2.json>`. The merge checks the scenario, virtual window, variants, repeats, store, corpus and backend versions before recomputing the comparisons. It reports coverage and records any omitted accounts in the complete file. Use `--until SECONDS` for a shorter window or `--no-repeat` for one sizing run with determinism unchecked.

For an export, the complete file and working logs go outside the repository. With no `--private-dir` they go to `results/` and `work/` next to the scenario. With an explicit private directory the logs go in its `work/` subdirectory. Split scenarios are removed at the end of each account lane. The results in `bench/results/` contain counts and field summaries. Use `python bench\report.py <aggregate-results.json>` to render them.

The schedule comes from the recording. This replay does not exercise the worker's own scheduling, production concurrency, network latency or real Redis. The launcher blocks outbound geocoder lookups and the comparison omits address text. The export has one car per account and the demo also covers a two-car account. Wall times are progress and sizing notes.

## How to run

You need a Rust toolchain, Python 3.11 or newer, and a checkout of this repository. From the repository root (PowerShell shown; use `.venv/bin/python` elsewhere):

```powershell
uv venv --python 3.12 .venv
uv pip install --python .venv\Scripts\python.exe maturin psutil "pybyd @ git+https://github.com/jkaberg/pyBYD@$(Get-Content parity\UPSTREAM_COMMIT)"
.venv\Scripts\python.exe parity\dev_install.py          # release build of the extension, with the fake server
.venv\Scripts\python.exe bench\run_all.py --quick       # smoke test, about a minute
.venv\Scripts\python.exe bench\run_all.py               # the full run
```

`pip install ".[bench]"` installs psutil; pyBYD at the pinned commit is always installed separately, as on the second line. The scripts build the example binaries themselves (`cargo build --release -p openbyd-client --features testing --examples`); pass `--skip-build` once they are built. Scratch logs go to `target/bench-scratch` (set `OPENBYD_BENCH_SCRATCH` to move them). Each script can also be run alone (`bench\micro.py`, `bench\load.py`, `bench\mqtt_idle.py`) and takes `--help`; `bench\report.py` prints the newest full results as Markdown (`--quick` for the newest quick ones).

`realtime_levels.py --json OUT [--corpus FILE]` runs the levels above (set `CARGO_TARGET_DIR`; `--corpus` adds another file of replies to the model rows, reported by index and shape only). `pwsh bench/linux_docker.ps1 [-ScratchDir DIR] [-CorpusRt FILE]` builds and measures in a Linux container; it needs Docker and about 1.5 GB of scratch space. `profile_realtime.py [--whole-call]` splits one reply, and one call, into parts.

`run_all.py --quick` exits non-zero if a benchmark fails, if openbyd gives a different answer from pyBYD in any micro row, if a load run's request count differs from the plan or a call fails, or if MQTT connections do not all come up. **CI can call it** (`python bench/run_all.py --quick`) as a check that the suite and both libraries still work together; do not compare its numbers. Quick results are not kept (`bench/.gitignore`), the results of full runs are.

The replay benchmark needs more than the others: Docker (for Postgres and Redis), a checkout of the backend with its own Python environment, and for the measured runs a scenario. With a synthetic scenario:

```powershell
.venv\Scripts\python.exe -m bench.replay.generate --accounts 50 --hours 6 --seed demo --profile busy --out E:\tmp\replay\busy.jsonl
.venv\Scripts\python.exe bench\replay_worker.py --backend <backend>\backend --backend-python <backend venv>\Scripts\python.exe `
    --scenario E:\tmp\replay\busy.jsonl --start-at 900 --minutes 30 --repeats 3 --start-when-claimed
.venv\Scripts\python.exe bench\report.py           # the newest results of every benchmark, the replay among them
```

Without `--scenario` it replays `bench/replay/samples/demo.jsonl` (3 accounts, 4 cars); `--quick --local-store` is a three-minute smoke run on SQLite and an in-memory store that needs no Docker. The replay runs at the speed of the wall clock, so pick `--start-at` and `--minutes` so that drives and charges both start and finish inside the window (`python -m bench.replay.scenario summary <file>` counts what a scenario holds). A run takes `--minutes` plus about a minute of set-up, plus the time the worker needs to claim every account (it claims 10 every 15 s). `--corpus` points the replay server at another directory of reply templates. Results files of separate invocations with the same scenario and settings can be combined as repeats with `--merge`.

To add an implementation (for example the Python `openbyd.Client`): add an `Impl` to the rows it applies to in `micro.py`, a row to `IMPLEMENTATIONS` in `load.py` (a `--worker` branch for Python, or a binary speaking the same `READY` / `go` / `RESULT` / `quit` line protocol), and a row in `mqtt_idle.py`.

## How to read the numbers

* The spread is real information. This machine is a shared workstation, other builds and tests ran during these runs (the system-wide CPU use before the micro-benchmarks is in the JSON), and short measurements on Windows move by tens of percent. Where the ranges of two sides overlap the tables say **within noise** and no winner is named. The raw samples are in the JSON files.
* Micro-benchmarks are one thread in one process. A native Rust cipher call from Python also pays the call, the GIL release and the copy of its arguments, which is why it can come out equal to or even a little slower than pyBYD with the Rust cipher installed: both run the same Rust code.
* The load benchmark's CPU figures are comparable between rows; its wall time and requests per second mostly show the think time and the fake server's delay, except where an implementation is CPU-bound (pyBYD with its Python cipher is, and falls behind the planned schedule).
* CPU time on Windows is read from the operating system's per-process counters, which are accounted in scheduler ticks (about 15.6 ms). Over a run of seconds this averages out; for the shortest runs a few percent of noise remains. This is why the fast implementations are repeated.

## Known limits

* **Localhost and a fake server.** There is no network latency, loss or TLS on the HTTP side (the fake server speaks plain HTTP), and the server's replies are the corpus payloads with a fixed delay. Real BYD replies vary in size. A real deployment adds TLS to every request on both sides (openbyd uses rustls, pyBYD uses aiohttp with OpenSSL), which this suite does not measure.
* **One machine, Windows.** CPU, memory and thread counts depend on the operating system (thread stacks, the allocator, scheduler accounting) and on the CPU. Reply parsing and the whole call were also measured on Linux, in a container on the same machine, and came out the same way; the load and MQTT benchmarks were not. The resident size on Windows is the working set.
* **Python on Windows runs asyncio's proactor loop**, whose timers tick every 15.6 ms: event-loop lags below that read as 0.
* **pyBYD pays for validation.** Its models validate and build objects; openbyd's `models.parse` returns a dict.
* **The load benchmark's accounts start together** (no ramp), a login storm; a service that spreads its polling would see lower lag. `--ramp-ms` spreads the start.
* **The idle MQTT benchmark** disables certificate verification on both sides (see above), uses one topic per account and delivers no messages; it measures the cost of holding a connection, not of handling a push.
* **Loopback sockets.** After a load run thousands of sockets linger in TIME_WAIT for a couple of minutes, and paho (which opens a socket pair per client on Windows) then fails with "address already in use". The first full run lost its pyBYD MQTT row to this; `mqtt_idle.py` now waits for the sockets to clear and the MQTT rows above are from a rerun. The load benchmark can run out of them too: with tens of thousands of sockets in TIME_WAIT on the machine, one of three 1000-account runs on the default workers failed 6,050 calls on 2026-11-20 with connect errors (`os error 10048`, "lacked sufficient buffer space"), and 13 runs with the ports clear failed none. `load.py` does not wait between runs; leave a couple of minutes after other loopback-heavy work.
* **The replay is not BYD.** It answers in a few milliseconds from the same machine, without TLS on the HTTP side; real BYD takes hundreds of milliseconds to seconds, which does not change the worker's CPU per request but does change how many polls fit in a minute when the worker is busy. Production stored 19 derived fields per poll, not BYD's replies: the replay fills every other field of a reply (tyre pressures, doors, windows, range, energy history, capabilities, the charging schedule) from a template, so those fields are realistic but constant. Recorded pushes include the ones BYD sent in answer to production's own polls, and they arrive at production's times, not in answer to the replayed worker's polls. An error is replayed for as long as it lasted in production, whenever the worker asks. Cars have made-up positions.
* **The replay runs at wall-clock speed.** The worker stamps what it stores with its own clock and its cadences are in real seconds, so a faster replay would need the worker's clocks scaled as well, and its database's and Redis's. A run therefore covers `--minutes` of the scenario, not days: a long charge that starts in the window is still open when it ends, and is counted as open, not compared.
* **The worker's lookups of street addresses are refused** by the network guard (it asks a public geocoder for the address of each drive's start and end). A refused lookup is retried at the car's later polls, at about 11 ms of CPU each; the results count the refusals. Nothing else the worker does reaches outside.
* **Loop lag in the replay results** is the worker's own histogram of how late a one-second timer fires. On Windows a timer cannot be more precise than 15.6 ms and the histogram's buckets are coarse, so an idle worker already reads a 99th percentile near 49 ms; only differences well above that mean something. Memory per account divides the whole process's peak by the accounts, the interpreter and the application included.
* **Memory** is the process's resident size. For Python it includes interpreter and import growth that happens during the run (it is the baseline's imports that are excluded), for the Rust binaries the heap and the thread stacks.
