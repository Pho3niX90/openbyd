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

Full run of 2026-11-19 on one machine: AMD Ryzen 9 5900X (12 cores, 24 threads), 64 GiB, Windows 11 10.0.26200, Python 3.12.13, rustc 1.100.0, release builds. pyBYD at commit `2c3d80648e` (`parity/UPSTREAM_COMMIT`). openbyd: the load results were produced at commit `2327866020`, the micro-benchmark and MQTT results at `587100f40a` (the commits in between changed how `load.py` reports failures and made `mqtt_idle.py` wait for loopback sockets to clear, nothing that is measured). The machine was shared with other builds and tests (12 to 19% of all cores busy before the micro-benchmarks), so read every figure together with its spread. A full run takes about 65 minutes (54 for the load benchmark, almost all of it pyBYD at 200 and 1000 accounts).

### Where openbyd is not faster

* **Reply models.** `openbyd.models.parse` is not faster than pyBYD's pydantic validation plus `model_dump`; for the realtime and HVAC models it is slower. The headline row, the full realtime payload, was 210 us for openbyd against 154 us for pyBYD in the run tabled below, which is *within noise* only because pyBYD's repeats ranged widely; in the two earlier full runs it was 208 against 102 us and 221 against 101 us, which is 2.0 to 2.2 times slower, and openbyd's own figure stayed at 208 to 221 us. HVAC is about 2 times slower, energy consumption about 1.1 times; the rest of the models are within noise of pyBYD (a few rows come out faster or slower from run to run). These are the figures before the model-parsing optimisation that another branch is making; the commit above says which code they describe.
* **The cipher alone.** openbyd's own `encode_envelope`/`decode_envelope` and pyBYD with the Rust cipher installed are within noise of each other, as they run the same Rust code. The whole 190 to 460 times cipher speed-up is available to a pyBYD user with one line (`pybyd_accel.install()`). On one complete request on localhost the two are within a few percent (1.46 and 1.57 ms, which is mostly the fake server and the loopback).
* **Native openbyd with the default worker count** costs more CPU and memory per account than with one worker (4.4 to 5.8 against 2.1 to 2.8 ms per cycle, and 154 to 417 against 91 to 170 KiB per account), because 24 worker threads each have a stack and wake up to steal work. It is only faster in wall time where one worker is saturated (1000 accounts: 2.9 s against 14.2 s). For a service that has cores to spare and a poll interval of seconds, a small worker count is the cheaper setting.
* **Memory, pyBYD with the Rust cipher against native openbyd** is close at 1000 accounts (113 against 91 KiB per account) and at 50 accounts pyBYD with the Rust cipher used less than native openbyd with the default workers (146 against 417 KiB). CPU is where native openbyd is ahead of it (about 2 times).

### What an account costs

Figures at 1000 accounts, from the load benchmark below. "Accounts per core" is the number of accounts one core could poll once a minute, 60 000 ms divided by the CPU per cycle; it counts client CPU only, against a fake server over plain HTTP, so it is an upper bound.

| | CPU per poll cycle | Memory per account | Accounts per core at one cycle a minute |
|---|---|---|---|
| pyBYD | 350 ms (332 to 343 at 50 and 200 accounts) | 933 KiB | about 170 (and it did not keep up, see below) |
| pyBYD with openbyd's cipher | 5.4 ms | 113 KiB | about 11 000 |
| openbyd native, 1 worker | 2.8 ms | 91 KiB | about 21 000 |
| openbyd native, default workers | 4.8 ms | 154 KiB | about 12 000 |

* **pyBYD at 1000 accounts overloaded its event loop.** One asyncio loop needs about 350 ms of CPU per cycle, so 1000 accounts that all start together need about 29 minutes of loop time for five cycles (the run took 1758 s). The event loop was late by a median of 17.5 s and up to 110 s, and 314 calls failed with `BydTransportError: Connection timeout to host` (most likely aiohttp's default 30 s connect timeout expiring while the loop was too busy to finish the connection). The server therefore saw 36 372 of the planned 37 000 requests, the shortfall (628) being exactly the failed triggers and the polls that they skipped, two per failed call. Every other implementation made all 37 000 requests with no failure. At 200 accounts pyBYD still completed, with the loop late by a median of 4 s.
* **pyBYD with openbyd's cipher** keeps the event loop responsive (median lag 0 at 50 accounts, 23 ms at 200, 440 ms with a maximum of 964 ms at 1000 accounts starting together) and completes the run 65 times faster than pyBYD at 1000 accounts.
* **Idle MQTT.** 200 idle pyBYD connections held 195 MiB (727 KiB each, 201 threads added, the second run measured the same 727 KiB). openbyd held 200 for 21 MiB (79 KiB each) and 1000 for 59 MiB (54 KiB each) on a fixed pool of worker threads (29 threads in all, none added per connection). Idle CPU was nil for both (pyBYD showed 0.62% of a core in the first full run and 0.00% in the second: paho's keepalive and wake-ups).

### Reproducibility

* Load CPU per cycle: the three runs of each fast implementation fell within about 12% of their median (the default-worker runs are wider: 2.9 to 7.4 ms at 50 accounts, 4.7 to 10.6 at 1000, one run in three was an outlier); pyBYD's three runs at 50 accounts gave 320 to 357 ms, and its figure at 200 and 1000 accounts (one run each) is within 6% of that.
* Memory per account and MQTT memory repeated to within 2% between runs. MQTT: pyBYD 727 and 727 KiB, openbyd 80 and 79 KiB at 200, 55 and 54 at 1000.
* Micro-benchmarks are the least reproducible: single operations of 5 to 700 us moved by 10 to 30% within a run and by up to 50% between runs (pyBYD's realtime parse: 102, 101 and 154 us). Rows whose ranges overlap are labelled *within noise*. Ratios of 50 times and more (the cipher, login, a full request through pyBYD's own cipher) are far outside any of this.
* Three full micro runs were made and the tables show the last. The first was made while 27% of the machine was busy with other work and the second from an uncommitted tree; their figures are quoted above where they differ, and their files were not kept.

### What could not be measured

* TLS on the HTTP side and real network latency: the fake server speaks plain HTTP on localhost.
* Linux and macOS: only this Windows machine was available.
* pyBYD with 1000 idle MQTT connections (one thread each; the request was 200) and a push being delivered (decrypt, parse, event hand-over).
* The Python `openbyd.Client` class (another branch is adding it) and any operation that openbyd has only through an internal test hook (see below).
* CPU below the scheduler tick (15.6 ms) for a single short run: the native runs last 2 to 14 s, so it is averaged over many ticks, and repeated three times.


### Tables

These are the tables `bench/report.py` renders from `bench/results/`.

### Micro-benchmarks

Run on 2026-11-19T16:41:14+02:00. AMD Ryzen 9 5900X 12-Core Processor, 24 logical cores, 63.9 GiB, Windows-11-10.0.26200-SP0; Python 3.12.13, rustc 1.100.0 (0ef1bc386 2026-11-09). openbyd commit `587100f40a`; pyBYD commit `2c3d80648e`.

Other activity on the machine before the run: 12% of all cores.

Each cell is the median time per operation of 5 measurements of at least 0.5 s, with half the spread (max - min) / median as +-. "openbyd vs pyBYD" is a factor of the medians, or "within noise" when the measurements' ranges overlap.

#### Envelope cipher

| Row | pyBYD | pyBYD + openbyd cipher | openbyd | openbyd vs pyBYD |
|---|---|---|---|---|
| encode 512 B | 10.83 ms (+-29%) | 49.5 us (+-8%) | 45.6 us (+-10%) | 237.6x faster |
| decode 512 B | 12.99 ms (+-22%) | 31.3 us (+-25%) | 28.3 us (+-2%) | 458.9x faster |
| encode 2 KB | 28.19 ms (+-24%) | 183.3 us (+-2%) | 150.9 us (+-16%) | 186.8x faster |
| decode 2 KB | 52.09 ms (+-2%) | 174.6 us (+-2%) | 169.5 us (+-12%) | 307.4x faster |
| encode 8 KB | 205.54 ms (+-25%) | 654.7 us (+-17%) | 708.1 us (+-6%) | 290.3x faster |
| decode 8 KB | 205.88 ms (+-6%) | 653.5 us (+-1%) | 676.2 us (+-15%) | 304.4x faster |

#### Reply models

| Row | pyBYD | openbyd | openbyd vs pyBYD |
|---|---|---|---|
| VehicleRealtimeData, every field present (headline) | 153.7 us (+-6%) | 210.2 us (+-23%) | within noise |
| AcCurrentLimit (2 payloads) | 9.4 us (+-4%) | 8.1 us (+-3%) | 1.2x faster |
| BookingList (2 payloads) | 14.7 us (+-4%) | 14.5 us (+-19%) | within noise |
| ChargingStatus (1 payload) | 7.0 us (+-18%) | 8.5 us (+-12%) | within noise |
| EnergyConsumption (2 payloads) | 78.8 us (+-5%) | 86.8 us (+-7%) | 1.1x slower |
| GpsInfo (2 payloads) | 5.3 us (+-35%) | 6.3 us (+-8%) | within noise |
| HvacStatus (4 payloads) | 10.1 us (+-30%) | 20.9 us (+-10%) | 2.1x slower |
| LatestConfigFunction (1 payload) | 9.2 us (+-11%) | 6.8 us (+-20%) | within noise |
| PushNotificationState (2 payloads) | 16.9 us (+-3%) | 7.4 us (+-18%) | 2.3x faster |
| Vehicle (3 payloads) | 13.5 us (+-3%) | 11.4 us (+-17%) | within noise |
| VehicleLatestConfig (2 payloads) | 160.7 us (+-8%) | 146.7 us (+-20%) | within noise |
| VehicleMsgItemInfo (2 payloads) | 13.3 us (+-7%) | 10.7 us (+-5%) | 1.2x faster |
| VehicleMsgListResponse (1 payload) | 17.8 us (+-2%) | 19.6 us (+-0%) | 1.1x slower |
| VehicleRealtimeData (5 payloads) | 55.3 us (+-2%) | 120.2 us (+-5%) | 2.2x slower |

*per payload, averaged over the model's corpus payloads; pyBYD also validates and builds Python objects*
*the full realtime payload, 3.6 KB of JSON*

#### shadow.compare

| Row | openbyd |
|---|---|
| VehicleRealtimeData (every field present) | 177.6 us (+-2%) |
| GpsInfo (valid fix, speed in km/h (coordinates anonymised)) | 8.8 us (+-23%) |

*absolute cost of one comparison (parse in openbyd + compare with pyBYD's dump); no pyBYD equivalent. Answer must be [] (no differing field).*

#### One signed request, localhost

| Row | pyBYD | pyBYD + openbyd cipher | openbyd | openbyd vs pyBYD |
|---|---|---|---|---|
| sequential (latency) | 147.95 ms (+-24%) | 1.57 ms (+-1%) | 1.46 ms (+-2%) | 101.3x faster |
| 200 at a time (throughput) | 149.62 ms (+-1%) | 1.23 ms (+-7%) | 1.12 ms (+-3%) | 133.3x faster |

*inner payload built once and shared; reply is the 3.6 KB realtime payload; the server runs in a thread of this process, so it shares the CPU but not the GIL*

#### Login

| Row | pyBYD | openbyd | openbyd vs pyBYD |
|---|---|---|---|
| one login (sequential) | 40.98 ms (+-21%) | 637.6 us (+-3%) | 64.3x faster |

*pyBYD's BydClient.login() against openbyd.lowlevel.login(); both hash the password, sign and open the reply; compared on the user id*

*Source: `bench/results/micro-20261119-164114.json`*

### Load: what an account costs

Run on 2026-11-19T16:19:21+02:00. AMD Ryzen 9 5900X 12-Core Processor, 24 logical cores, 63.9 GiB, Windows-11-10.0.26200-SP0; Python 3.12.13, rustc 1.100.0 (0ef1bc386 2026-11-09). openbyd commit `2327866020`; pyBYD commit `2c3d80648e`.

Each account logs in, lists its vehicles and runs 5 poll cycles (realtime, GPS, charging status, HVAC status, energy consumption; 37 requests in all) with 100 ms think time between cycles. The fake server runs in its own process and delays every reply by 20 ms; pollable reads sleep 0.05 s before their poll. CPU is user + system seconds of the client process during the run, divided by accounts x cycles (so it includes the one-off login and vehicle list); with several runs the table shows the median run and the range. Memory is the peak resident size minus the size before the run, per account.

#### 50 accounts

| Implementation | CPU ms / cycle | Memory / account | Peak RSS | Requests / s | Wall | Threads (peak) | Loop lag median / p99 / max | Failed calls | Requests seen / planned |
|---|---|---|---|---|---|---|---|---|---|
| pyBYD | 332.0 (range 320.5-357.1, 3 runs) | 979 KiB | 101 MiB | 22 | 83.4 s | 37 | 897.4 / 4328 / 4689 ms | 0 | 1850 / 1850 |
| pyBYD + openbyd cipher | 5.4 (range 5.1-5.7, 3 runs) | 146 KiB | 64 MiB | 817 | 2.3 s | 21 | 0.0 / 26 / 54 ms | 0 | 1850 / 1850 |
| openbyd native, 1 worker | 2.6 (range 2.3-2.9, 3 runs) | 170 KiB | 14 MiB | 811 | 2.3 s | 6 | n/a (no event loop) | 0 | 1850 / 1850 |
| openbyd native, default workers | 5.8 (range 2.9-7.4, 3 runs) | 417 KiB | 26 MiB | 827 | 2.2 s | 29 | n/a (no event loop) | 0 | 1850 / 1850 |

#### 200 accounts

| Implementation | CPU ms / cycle | Memory / account | Peak RSS | Requests / s | Wall | Threads (peak) | Loop lag median / p99 / max | Failed calls | Requests seen / planned |
|---|---|---|---|---|---|---|---|---|---|
| pyBYD | 342.9 | 947 KiB | 238 MiB | 22 | 344.0 s | 37 | 4073.0 / 27589 / 30294 ms | 0 | 7400 / 7400 |
| pyBYD + openbyd cipher | 4.2 (range 4.1-4.3, 3 runs) | 124 KiB | 81 MiB | 1716 | 4.3 s | 37 | 23.3 / 85 / 129 ms | 0 | 7400 / 7400 |
| openbyd native, 1 worker | 2.1 (range 2.0-2.2, 3 runs) | 112 KiB | 27 MiB | 2519 | 2.9 s | 6 | n/a (no event loop) | 0 | 7400 / 7400 |
| openbyd native, default workers | 4.4 (range 4.2-4.9, 3 runs) | 197 KiB | 45 MiB | 3269 | 2.3 s | 29 | n/a (no event loop) | 0 | 7400 / 7400 |

#### 1000 accounts

| Implementation | CPU ms / cycle | Memory / account | Peak RSS | Requests / s | Wall | Threads (peak) | Loop lag median / p99 / max | Failed calls | Requests seen / planned |
|---|---|---|---|---|---|---|---|---|---|
| pyBYD | 350.1 | 933 KiB | 965 MiB | 21 | 1757.6 s | 37 | 17542.5 / 94976 / 110585 ms | 314 | 36372 / 37000 (calls failed) |
| pyBYD + openbyd cipher | 5.4 (range 5.3-5.5, 3 runs) | 113 KiB | 167 MiB | 1359 | 27.2 s | 37 | 440.4 / 924 / 964 ms | 0 | 37000 / 37000 |
| openbyd native, 1 worker | 2.8 (range 2.7-2.8, 3 runs) | 91 KiB | 94 MiB | 2612 | 14.2 s | 6 | n/a (no event loop) | 0 | 37000 / 37000 |
| openbyd native, default workers | 4.8 (range 4.7-10.6, 3 runs) | 154 KiB | 156 MiB | 12923 | 2.9 s | 29 | n/a (no event loop) | 0 | 37000 / 37000 |

*Source: `bench/results/load-20261119-161921.json`*

### Idle MQTT connections

Run on 2026-11-19T16:43:28+02:00. AMD Ryzen 9 5900X 12-Core Processor, 24 logical cores, 63.9 GiB, Windows-11-10.0.26200-SP0; Python 3.12.13, rustc 1.100.0 (0ef1bc386 2026-11-09). openbyd commit `587100f40a`; pyBYD commit `2c3d80648e`.

N connections to the TLS test broker (a separate process), each subscribed to its own topic, then idle (5.0 s window after 3.0 s to settle, keepalive 120 s). **Certificate verification is off on both sides** (pyBYD's paho `tls_set` is patched in the benchmark's worker process only). Memory per connection is the idle resident size minus the size before connecting, over N; threads are those added by the connections.

| Implementation | Connections | Memory / connection | Idle RSS | Peak RSS | Threads (idle) | Threads added | Idle CPU | Connected |
|---|---|---|---|---|---|---|---|---|
| pyBYD (paho, one thread per client) | 200 | 727 KiB | 195 MiB | 195 MiB | 209 | 201 | 0.00% of a core | 200 / 200 |
| openbyd MqttRuntime (tokio) | 200 | 79 KiB | 21 MiB | 21 MiB | 29 | 1 | 0.00% of a core | 200 / 200 |
| openbyd MqttRuntime (tokio) | 1000 | 54 KiB | 59 MiB | 59 MiB | 29 | 1 | 0.00% of a core | 1000 / 1000 |

*Source: `bench/results/mqtt_idle-20261119-164328.json`*


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

`pip install ".[bench]"` installs the Python dependencies (psutil and pyBYD at the pinned commit) instead of the second line. The scripts build the example binaries themselves (`cargo build --release -p openbyd-client --features testing --examples`); pass `--skip-build` once they are built. Scratch logs go to `target/bench-scratch` (set `OPENBYD_BENCH_SCRATCH` to move them). Each script can also be run alone (`bench\micro.py`, `bench\load.py`, `bench\mqtt_idle.py`) and takes `--help`; `bench\report.py` prints the newest full results as Markdown (`--quick` for the newest quick ones).

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
