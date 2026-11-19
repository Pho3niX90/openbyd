# Benchmarks: pyBYD against openbyd

This directory compares [pyBYD](https://github.com/jkaberg/pyBYD) with openbyd, its Rust port, for an application that holds one client per customer account and wants to know what each account costs in CPU and memory.

```
bench/
  micro.py       one operation, pyBYD against openbyd, in the same Python process
  load.py        N accounts doing poll cycles against a fake server; CPU and memory per account
  mqtt_idle.py   memory and threads of connected-but-idle MQTT accounts
  report.py      renders bench/results/*.json as Markdown tables
  run_all.py     runs all three and the report; exits non-zero on any failure or wrong answer
  common.py      shared helpers
  results/       one JSON file per run: date, machine, openbyd and pyBYD commits, every number
../crates/openbyd-client/examples/
  fake_server.rs the fake BYD server as a process of its own (load benchmark)
  load.rs        openbyd's side of the load benchmark
  mqtt_idle.rs   the TLS test broker, and openbyd's side of the MQTT benchmark
```

## Results

Full run of 2026-11-19 (evening) on one machine: AMD Ryzen 9 5900X (12 cores, 24 threads), 64 GiB, Windows 11 10.0.26200, Python 3.12.13, rustc 1.100.0, release builds. openbyd at commit `7104731ad4` (version 0.1.1); pyBYD at commit `2c3d80648e` (`parity/UPSTREAM_COMMIT`). Other work was running on the machine during parts of the run (about 10% of all cores busy before the micro-benchmarks, more during the load runs), so read every figure with its spread. A full run takes about 45 minutes, almost all of it pyBYD at 200 and 1000 accounts.

### Headlines

- **Cipher:** 229 to 256 times faster than pyBYD's Python cipher. pyBYD with `openbyd.pybyd_accel` installed is within noise of openbyd's own call.
- **One signed request on localhost:** 105.6 ms in pyBYD, 1.37 ms in pyBYD with the Rust cipher, 1.24 ms in openbyd.
- **Reply models:** openbyd is 2.3 to 5.3 times faster on every model. The full realtime reply takes 96 us in pyBYD and 36 us in openbyd. An earlier version of this page showed openbyd slower here; that was before two optimisation passes.
- **CPU per poll cycle:** about 250 ms in pyBYD (314 ms at 1000 accounts), 3.5 to 5.2 ms in pyBYD with the Rust cipher, 1.6 to 2.4 ms in openbyd on one worker thread.
- **Memory per account:** about 950 KiB in pyBYD, 113 to 148 KiB with the Rust cipher, 89 to 226 KiB in openbyd on one worker thread.
- **Idle MQTT connection:** 727 KiB and one thread in pyBYD; 54 to 80 KiB and no extra thread in openbyd.

### Where openbyd is not faster

- **Cipher alone:** openbyd's direct call and pyBYD with the Rust cipher are the same code, and measure the same.
- **Default tokio workers:** the native client on the default worker count uses more CPU per cycle (3.4 to 5.2 ms) and more memory per account than on one worker, in exchange for finishing sooner. One worker is the efficient setting for this workload.
- **Memory at 50 accounts:** openbyd on one worker used more memory per account (226 KiB) than pyBYD with the Rust cipher (148 KiB); from 200 accounts up it uses less.

### What could not be measured

- TLS on the HTTP side and real network latency: the fake server speaks plain HTTP on localhost.
- Linux and macOS: only this Windows machine was available. CI has measured reply parsing on Linux at about half pyBYD's time, before the last optimisation.
- pyBYD with 1000 idle MQTT connections, and the delivery of a push message.

### Micro-benchmarks

Run on 2026-11-19T20:49:48+02:00. AMD Ryzen 9 5900X 12-Core Processor, 24 logical cores, 63.9 GiB, Windows-11-10.0.26200-SP0; Python 3.12.13, rustc 1.100.0 (0ef1bc386 2026-11-09). openbyd commit `7104731ad4`; pyBYD commit `2c3d80648e`.

Other activity on the machine before the run: 10% of all cores.

Each cell is the median time per operation of 5 measurements of at least 0.5 s, with half the spread (max - min) / median as +-. "openbyd vs pyBYD" is a factor of the medians, or "within noise" when the measurements' ranges overlap.

#### Envelope cipher

| Row | pyBYD | pyBYD + openbyd cipher | openbyd | openbyd vs pyBYD |
|---|---|---|---|---|
| encode 512 B | 7.25 ms (+-1%) | 34.3 us (+-1%) | 31.6 us (+-1%) | 229.5x faster |
| decode 512 B | 7.41 ms (+-3%) | 32.0 us (+-1%) | 29.0 us (+-3%) | 255.9x faster |
| encode 2 KB | 28.47 ms (+-2%) | 126.2 us (+-3%) | 122.0 us (+-2%) | 233.4x faster |
| decode 2 KB | 28.31 ms (+-1%) | 112.2 us (+-1%) | 110.7 us (+-3%) | 255.7x faster |
| encode 8 KB | 112.78 ms (+-2%) | 499.7 us (+-3%) | 480.5 us (+-5%) | 234.7x faster |
| decode 8 KB | 110.90 ms (+-2%) | 425.4 us (+-3%) | 439.5 us (+-7%) | 252.4x faster |

#### Reply models

| Row | pyBYD | openbyd | openbyd vs pyBYD |
|---|---|---|---|
| VehicleRealtimeData, every field present (headline) | 96.1 us (+-2%) | 36.0 us (+-2%) | 2.7x faster |
| AcCurrentLimit (2 payloads) | 5.1 us (+-2%) | 1.6 us (+-2%) | 3.3x faster |
| BookingList (2 payloads) | 8.6 us (+-8%) | 3.1 us (+-1%) | 2.8x faster |
| ChargingStatus (1 payload) | 4.5 us (+-1%) | 1.7 us (+-2%) | 2.6x faster |
| EnergyConsumption (2 payloads) | 47.0 us (+-2%) | 15.3 us (+-2%) | 3.1x faster |
| GpsInfo (2 payloads) | 4.8 us (+-5%) | 1.4 us (+-3%) | 3.5x faster |
| HvacStatus (4 payloads) | 9.2 us (+-2%) | 3.7 us (+-2%) | 2.5x faster |
| LatestConfigFunction (1 payload) | 5.2 us (+-2%) | 1.8 us (+-5%) | 2.9x faster |
| PushNotificationState (2 payloads) | 12.6 us (+-22%) | 2.4 us (+-8%) | 5.2x faster |
| Vehicle (3 payloads) | 7.7 us (+-14%) | 3.2 us (+-1%) | 2.4x faster |
| VehicleLatestConfig (2 payloads) | 90.0 us (+-3%) | 22.4 us (+-1%) | 4.0x faster |
| VehicleMsgItemInfo (2 payloads) | 7.5 us (+-3%) | 2.4 us (+-2%) | 3.1x faster |
| VehicleMsgListResponse (1 payload) | 19.0 us (+-3%) | 8.1 us (+-4%) | 2.4x faster |
| VehicleRealtimeData (5 payloads) | 57.6 us (+-3%) | 20.0 us (+-1%) | 2.9x faster |

*per payload, averaged over the model's corpus payloads; pyBYD also validates and builds Python objects*
*the full realtime payload, 3.6 KB of JSON*

#### shadow.compare

| Row | openbyd |
|---|---|
| VehicleRealtimeData (every field present) | 70.1 us (+-6%) |
| GpsInfo (valid fix, speed in km/h (coordinates anonymised)) | 2.9 us (+-1%) |

*absolute cost of one comparison (parse in openbyd + compare with pyBYD's dump); no pyBYD equivalent. Answer must be [] (no differing field).*

#### One signed request, localhost

| Row | pyBYD | pyBYD + openbyd cipher | openbyd | openbyd vs pyBYD |
|---|---|---|---|---|
| sequential (latency) | 105.60 ms (+-0%) | 1.37 ms (+-2%) | 1.24 ms (+-2%) | 84.9x faster |
| 200 at a time (throughput) | 104.58 ms (+-1%) | 1.17 ms (+-3%) | 778.6 us (+-10%) | 134.3x faster |

*inner payload built once and shared; reply is the 3.6 KB realtime payload; the server runs in a thread of this process, so it shares the CPU but not the GIL*

#### Login

| Row | pyBYD | openbyd | openbyd vs pyBYD |
|---|---|---|---|
| one login (sequential) | 23.37 ms (+-1%) | 530.3 us (+-2%) | 44.1x faster |

*pyBYD's BydClient.login() against openbyd.lowlevel.login(); both hash the password, sign and open the reply; compared on the user id*

*Source: `bench/results/micro-20261119-204948.json`*

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

`run_all.py --quick` exits non-zero if a benchmark fails, if openbyd gives a different answer from pyBYD in any micro row, if a load run's request count differs from the plan or a call fails, or if MQTT connections do not all come up. **CI can call it** (`python bench/run_all.py --quick`) as a check that the suite and both libraries still work together; do not compare its numbers. Quick results are not kept (`bench/.gitignore`), the results of full runs are.

To add an implementation (for example the Python `openbyd.Client`): add an `Impl` to the rows it applies to in `micro.py`, a row to `IMPLEMENTATIONS` in `load.py` (a `--worker` branch for Python, or a binary speaking the same `READY` / `go` / `RESULT` / `quit` line protocol), and a row in `mqtt_idle.py`.

## How to read the numbers

* The spread is real information. This machine is a shared workstation, other builds and tests ran during these runs (the system-wide CPU use before the micro-benchmarks is in the JSON), and short measurements on Windows move by tens of percent. Where the ranges of two sides overlap the tables say **within noise** and no winner is named. The raw samples are in the JSON files.
* Micro-benchmarks are one thread in one process. A native Rust cipher call from Python also pays the call, the GIL release and the copy of its arguments, which is why it can come out equal to or even a little slower than pyBYD with the Rust cipher installed: both run the same Rust code.
* The load benchmark's CPU figures are comparable between rows; its wall time and requests per second mostly show the think time and the fake server's delay, except where an implementation is CPU-bound (pyBYD with its Python cipher is, and falls behind the planned schedule).
* CPU time on Windows is read from the operating system's per-process counters, which are accounted in scheduler ticks (about 15.6 ms). Over a run of seconds this averages out; for the shortest runs a few percent of noise remains. This is why the fast implementations are repeated.

## Known limits

* **Localhost and a fake server.** There is no network latency, loss or TLS on the HTTP side (the fake server speaks plain HTTP), and the server's replies are the corpus payloads with a fixed delay. Real BYD replies vary in size. A real deployment adds TLS to every request on both sides (openbyd uses rustls, pyBYD uses aiohttp with OpenSSL), which this suite does not measure.
* **One machine, Windows.** CPU, memory and thread counts depend on the operating system (thread stacks, the allocator, scheduler accounting) and on the CPU. Linux numbers will differ, probably in the same direction. The resident size on Windows is the working set.
* **Python on Windows runs asyncio's proactor loop**, whose timers tick every 15.6 ms: event-loop lags below that read as 0.
* **pyBYD pays for validation.** Its models validate and build objects; openbyd's `models.parse` returns a dict.
* **The load benchmark's accounts start together** (no ramp), a login storm; a service that spreads its polling would see lower lag. `--ramp-ms` spreads the start.
* **openbyd's model parsing is being optimised on another branch.** The model rows are the figures at the commit named above, not a verdict.
* **The idle MQTT benchmark** disables certificate verification on both sides (see above), uses one topic per account and delivers no messages; it measures the cost of holding a connection, not of handling a push.
* **Loopback sockets.** After a load run thousands of sockets linger in TIME_WAIT for a couple of minutes, and paho (which opens a socket pair per client on Windows) then fails with "address already in use". The first full run lost its pyBYD MQTT row to this; `mqtt_idle.py` now waits for the sockets to clear and the MQTT rows above are from a rerun.
* **Memory** is the process's resident size. For Python it includes interpreter and import growth that happens during the run (it is the baseline's imports that are excluded), for the Rust binaries the heap and the thread stacks.
