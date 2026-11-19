"""Where the time of one VehicleRealtimeData reply goes, from the outside: a Markdown table.

    python bench/profile_realtime.py                 # the model through Python, and pyBYD's split
    python bench/profile_realtime.py --whole-call    # also one Client.get_vehicle_realtime, split up
    python bench/profile_realtime.py --corpus FILE   # the mean over every payload of another corpus file

Model rows (the full synthetic reply, ``parity/corpus/VehicleRealtimeData.json``, "every field
present"; minimum and median per call over ``--repeats`` runs of ``--number`` calls):

- ``openbyd.models.parse``: the whole call (input to Value, parse, dump, raw);
- ``_Value(raw)``: converting the input dict to a ``serde_json::Value`` and dropping it, the part
  that is left now (removing it needs models that parse from something other than a Value);
- ``_Value.to_python()``: building the Python dict of ``raw`` from a Value, what the dump's ``raw``
  cost before it was built from the caller's own objects (and still costs in the client, whose
  replies arrive as JSON text);
- pyBYD's ``model_validate``, ``model_dump(mode="json")`` and the same dump without ``raw``.

Whole-call rows (``--whole-call``; the fake BYD server in its own process, no delay, keep-alive,
MQTT off): wall and client CPU per ``get_vehicle_realtime`` (a trigger and one poll), the time
the client spends in each request (HTTP and decoding, from ``request_observer``), one signed
request through ``openbyd.lowlevel``, one hop through the asyncio bridge with no work
(``_native.sleep(0)``), and the envelope cipher on a reply- and a request-sized body.

The finer split (parse stages, allocations per stage) needs an instrumented build; see
``crates/openbyd-protocol/benches/README.md``. Payloads are synthetic; with ``--corpus`` only
timings are printed, never values.
"""

from __future__ import annotations

import argparse
import asyncio
import gc
import json
import statistics
import sys
import time
import timeit
from collections.abc import Callable
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402

import openbyd  # noqa: E402
from openbyd import _native  # noqa: E402

NAME = "VehicleRealtimeData"
VIN = "TESTVIN0000000001"
USER = "user0@example.com"
PASSWORD = "secret"
POLL = {"poll_attempts": 10, "poll_interval": 0.0, "mqtt_timeout": 0.0, "signal_retries": 2}


def corpus_payloads(path: Path) -> list[tuple[str, dict[str, Any]]]:
    entries = json.loads(path.read_text(encoding="utf-8"))
    return [(entry.get("note", ""), entry["payload"]) for entry in entries if isinstance(entry.get("payload"), dict)]


def per_call(fn: Callable[[], Any], number: int, repeats: int) -> tuple[float, float]:
    """Minimum and median microseconds per call."""
    times = sorted(sample / number * 1e6 for sample in timeit.Timer(fn).repeat(repeats, number))
    return times[0], times[len(times) // 2]


def table(title: str, rows: list[tuple[str, float, float, str]]) -> str:
    lines = [f"### {title}", "", "| Row | min µs | median µs | Note |", "|---|---:|---:|---|"]
    lines += [f"| {name} | {low:.2f} | {mid:.2f} | {note} |" for name, low, mid, note in rows]
    return "\n".join(lines) + "\n"


def model_rows(raw: dict[str, Any], number: int, repeats: int) -> list[tuple[str, float, float, str]]:
    held = _native._Value
    value = held(raw)
    rows: list[tuple[str, float, float, str]] = []
    total = per_call(lambda: openbyd.models.parse(NAME, raw), number, repeats)
    rows.append(("openbyd.models.parse", *total, "the whole call"))
    rows.append(("_Value(raw)", *per_call(lambda: held(raw), number, repeats), "input dict to Value, and drop"))
    rows.append(("_Value.to_python()", *per_call(value.to_python, number, repeats), "raw from a Value (the client's path)"))
    try:
        from pybyd.models.realtime import VehicleRealtimeData
    except ImportError:
        rows.append(("pyBYD", float("nan"), float("nan"), "not installed"))
        return rows
    validate = VehicleRealtimeData.model_validate
    model = validate(raw)
    rows.append(("pyBYD model_validate", *per_call(lambda: validate(raw), number, repeats), ""))
    rows.append(("pyBYD model_dump(mode='json')", *per_call(lambda: model.model_dump(mode="json"), number, repeats), ""))
    rows.append((
        "pyBYD model_dump without raw",
        *per_call(lambda: model.model_dump(mode="json", exclude={"raw"}), number, repeats),
        "the difference is raw",
    ))
    return rows


async def whole_call_rows(url: str, calls: int, repeats: int) -> list[tuple[str, float, float, str]]:
    import psutil

    me = psutil.Process()
    rows: list[tuple[str, float, float, str]] = []

    async def measure(name: str, fn: Callable[[], Any], note: str) -> None:
        for _ in range(min(100, calls)):
            await fn()
        walls, cpus = [], []
        for _ in range(repeats):
            before_cpu, before = me.cpu_times(), time.perf_counter()
            for _ in range(calls):
                await fn()
            after, after_cpu = time.perf_counter(), me.cpu_times()
            walls.append((after - before) / calls * 1e6)
            cpus.append(((after_cpu.user - before_cpu.user) + (after_cpu.system - before_cpu.system)) / calls * 1e6)
        rows.append((f"{name} (wall)", min(walls), statistics.median(walls), note))
        rows.append((f"{name} (client CPU, all threads)", min(cpus), statistics.median(cpus), "psutil; coarse on Windows"))

    durations: list[float] = []
    client = openbyd.Client(username=USER, password=PASSWORD, base_url=url, mqtt_enabled=False)
    observed = openbyd.Client(
        username=USER,
        password=PASSWORD,
        base_url=url,
        mqtt_enabled=False,
        request_observer=lambda record: durations.append(record["duration"]),
    )
    for each in (client, observed):
        await each.start()
        await each.login()
        await each.get_vehicles()
    await measure("get_vehicle_realtime", lambda: client.get_vehicle_realtime(VIN, **POLL), "a trigger and one poll")
    durations.clear()
    await measure("get_vehicle_realtime, observed", lambda: observed.get_vehicle_realtime(VIN, **POLL), "the observer costs hops")
    spent = sorted(duration * 1e6 for duration in durations)
    rows.append(("one request, inside the client", spent[0], spent[len(spent) // 2], "request_observer: HTTP and decoding"))
    config = {"username": USER, "password": PASSWORD, "base_url": url}
    session = await openbyd.lowlevel.login(config)
    inner = {"vin": VIN, "deviceType": "0", "imeiMD5": "0" * 32, "networkType": "wifi", "random": "0" * 32,
             "timeStamp": str(int(time.time() * 1000)), "version": "1"}
    await measure(
        "lowlevel.post_token_json",
        lambda: openbyd.lowlevel.post_token_json(config, session, "/vehicleInfo/vehicle/vehicleRealTimeResult", inner),
        "one signed request, the 3.6 KB reply",
    )
    await measure("_native.sleep(0)", lambda: _native.sleep(0.0), "one hop through the asyncio bridge, no work")
    for each in (client, observed):
        await each.close()
    return rows


def cipher_rows(number: int, repeats: int) -> list[tuple[str, float, float, str]]:
    reply = json.dumps({"code": "0", "message": "SUCCESS", "identifier": "x", "respondData": "A" * 7200}, separators=(",", ":"))
    request = json.dumps({"request": "B" * 900})
    sealed_reply, sealed_request = _native.encode_envelope(reply), _native.encode_envelope(request)
    return [
        ("decode_envelope, realtime reply size", *per_call(lambda: _native.decode_envelope(sealed_reply), number, repeats),
         f"{len(sealed_reply)} characters"),
        ("encode_envelope, request size", *per_call(lambda: _native.encode_envelope(request), number, repeats),
         f"{len(sealed_request)} characters"),
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--number", type=int, default=2000, help="calls per timing run (default 2000)")
    parser.add_argument("--repeats", type=int, default=15, help="timing runs per row (default 15)")
    parser.add_argument("--note", default="every field present", help="the corpus payload to time")
    parser.add_argument("--corpus", type=Path, help="time every payload of this corpus file (timings only)")
    parser.add_argument("--whole-call", action="store_true", help="also split one Client.get_vehicle_realtime")
    parser.add_argument("--calls", type=int, default=500, help="calls per whole-call run (default 500)")
    parser.add_argument("--skip-build", action="store_true", help="do not build the fake_server example first")
    args = parser.parse_args()
    gc.disable()
    print(f"openbyd {openbyd.__version__ if hasattr(openbyd, '__version__') else ''}, Python {sys.version.split()[0]}\n")

    if args.corpus:
        payloads = corpus_payloads(args.corpus)
        totals = [per_call(lambda raw=raw: openbyd.models.parse(NAME, raw), args.number // 4, args.repeats)[0]
                  for _, raw in payloads]
        print(f"{len(payloads)} payloads from {args.corpus.name}: mean of the per-payload minimums "
              f"{statistics.mean(totals):.2f} µs, largest {max(totals):.2f} µs\n")
        return 0

    raw = dict(corpus_payloads(common.CORPUS / f"{NAME}.json"))[args.note]
    print(table(f"{NAME}, '{args.note}' ({len(raw)} keys), through Python", model_rows(raw, args.number, args.repeats)))
    if args.whole_call:
        common.require_examples("fake_server", build=not args.skip_build)
        with common.FakeServerProcess(2, 0) as server:
            rows = asyncio.run(whole_call_rows(server.url, args.calls, max(3, args.repeats // 3)))
        rows += cipher_rows(max(50, args.number // 20), args.repeats)
        print(table("One Client.get_vehicle_realtime against the fake server", rows))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
