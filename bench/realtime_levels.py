"""Benchmark VehicleRealtimeData in Rust, through Python, and over localhost.

Build the release extension with parity/dev_install.py first. Set CARGO_TARGET_DIR
before running. The built-in corpus supplies the synthetic cases and the fake
server reply. Each --corpus adds a read-only input to the model benchmark.
The JSON contains only case shapes, timing samples and run metadata.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import datetime as dt
import json
import os
import re
import shutil
import statistics
import subprocess
import tempfile
import time
from collections import Counter
from collections.abc import Awaitable, Callable, Iterator
from dataclasses import dataclass
from importlib import metadata
from pathlib import Path
from typing import Any

import common
from micro import OPENBYD, PASSWORD, PYBYD, PYBYD_ACCEL, USER, VIN, accelerated

MODEL = "VehicleRealtimeData"
SYNTHETIC = common.CORPUS / f"{MODEL}.json"
VEHICLES = "/app/account/getAllListByUserId"
TRIGGER = "/vehicleInfo/vehicle/vehicleRealTimeRequest"
POLL = "/vehicleInfo/vehicle/vehicleRealTimeResult"
POLL_SETTINGS = {"poll_attempts": 1, "poll_interval": 0.0, "mqtt_timeout": 0.0}
ENERGY_NAMES = {0: "EV", 1: "ICE", 2: "HYBRID"}
ENERGY_WORDS = re.compile(r"\b(HYBRID|PHEV|DM-I|BEV|ICE)\b", re.IGNORECASE)
RUST_ROW = re.compile(
    r"^(?P<name>\S.*?)\s+(?P<median>[\d.]+ \S+s)"
    r"\s+\(min (?P<min>[\d.]+ \S+s), max (?P<max>[\d.]+ \S+s)\)$"
)
RUST_UNIT = {"ns": 1e-9, "µs": 1e-6, "ms": 1e-3}
RUST_BENCH = ["cargo", "bench", "-p", "openbyd-protocol", "--bench", "protocol"]
RUST_COMMAND = [*RUST_BENCH, "--", "realtime"]
RUST_PAYLOAD_MAIN = r"""
fn main() {
    let path = std::env::args_os().nth(1).expect("pass a corpus path");
    let corpus: Vec<Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let context = json!({"energy_type": {"EnergyType": 2}});
    let bench = Bench { filter: None };
    for (index, entry) in corpus.iter().enumerate() {
        let raw = &entry["payload"];
        let model = VehicleRealtimeData::parse(raw, &context).unwrap();
        let label = format!("realtime/corpus[{index}]");
        bench.run(&format!("{label} parse"), || {
            VehicleRealtimeData::parse(black_box(raw), &context).unwrap()
        });
        bench.run(&format!("{label} parse_owned+into_dump"), || {
            VehicleRealtimeData::parse_owned(black_box(raw.clone()), &context).unwrap().into_dump()
        });
        bench.run(&format!("{label} clone"), || black_box(raw).clone());
        bench.run(&format!("{label} dump"), || black_box(&model).dump());
    }
}
"""


@dataclass(frozen=True)
class Payload:
    index: int
    raw: dict[str, Any]
    energy_type: str
    context: Any
    headline: bool

    def shape(self) -> dict[str, Any]:
        """Describe top-level fields without exposing reply values."""
        return {
            "index": self.index,
            "keys": len(self.raw),
            "sentinel_or_empty": sum(
                value is None
                or value == -1
                or (isinstance(value, str) and value.strip() in {"", "--", "-1", "NaN", "nan"})
                or value == []
                or value == {}
                for value in self.raw.values()
            ),
            "energy_type": self.energy_type,
            "headline": self.headline,
        }


def read_corpus(path: Path) -> Iterator[Payload]:
    for index, entry in enumerate(json.loads(path.read_text(encoding="utf-8"))):
        raw, note = entry["payload"], str(entry.get("note", ""))
        context = entry.get("context")
        member = context.get("energy_type") if isinstance(context, dict) else None
        explicit_energy = member.get("EnergyType") if isinstance(member, dict) else raw.get("energyType")
        energy_type = {str(key): name for key, name in ENERGY_NAMES.items()}.get(str(explicit_energy))
        if energy_type is None:
            words = {word.upper() for word in ENERGY_WORDS.findall(note)}
            if words & {"HYBRID", "PHEV", "DM-I"}:
                energy_type = "HYBRID"
            elif "BEV" in words:
                energy_type = "EV"
            elif "ICE" in words:
                energy_type = "ICE"
            else:
                energy_type = "unspecified"
        if context is None and energy_type == "HYBRID":
            context = {"energy_type": {"EnergyType": 2}}
        yield Payload(index, raw, energy_type, context, note == "every field present")


def pre_run() -> dict[str, Any]:
    return {
        "started_at": dt.datetime.now().astimezone().isoformat(timespec="seconds"),
        "system_cpu_percent_before": common.background_cpu(),
    }


def rust_seconds(text: str) -> float:
    number, unit = text.split()
    return float(number) * RUST_UNIT[unit]


def run_command(command: list[str]) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        command,
        cwd=common.ROOT,
        env={**os.environ, "CARGO_TERM_COLOR": "never"},
        capture_output=True,
        encoding="utf-8",
        errors="replace",
    )
    if completed.returncode:
        raise RuntimeError(f"{command[0]} exited with code {completed.returncode}\n{completed.stderr}")
    return completed


def build_rust_payload_probe(directory: Path) -> Path:
    # The checked-in benchmark times parse+dump on every synthetic input. Use its
    # same timing loop and release profile for the remaining per-input rows.
    protocol = common.ROOT / "crates" / "openbyd-protocol" / "benches" / "protocol.rs"
    source = protocol.read_text(encoding="utf-8")
    budget = source[source.index("const BUDGET:"):source.index("fn corpus_dir()")]
    runner = source[source.index("struct Bench {"):source.index("fn parse_dump<")]
    program = directory / "realtime_payloads.rs"
    program.write_text(
        "use std::{hint::black_box, time::{Duration, Instant}};\n"
        "use openbyd_protocol::models::{base::BydModel, realtime::VehicleRealtimeData};\n"
        "use serde_json::{Value, json};\n"
        + budget + runner + RUST_PAYLOAD_MAIN,
        encoding="utf-8",
    )
    crate = json.dumps((common.ROOT / "crates" / "openbyd-protocol").as_posix())
    manifest = directory / "Cargo.toml"
    manifest.write_text(
        '[package]\nname = "realtime-payloads"\nversion = "0.0.0"\nedition = "2024"\n'
        '[[bin]]\nname = "realtime-payloads"\npath = "realtime_payloads.rs"\n'
        f'[dependencies]\nopenbyd-protocol = {{ path = {crate} }}\n'
        'serde_json = { version = "1", features = ["preserve_order", "float_roundtrip"] }\n'
        '[profile.release]\nlto = "thin"\ncodegen-units = 1\nstrip = "symbols"\n',
        encoding="utf-8",
    )
    # Keep the same dependency versions while adding the temporary binary.
    shutil.copyfile(common.ROOT / "Cargo.lock", directory / "Cargo.lock")
    artifacts = run_command([
        "cargo", "build", "--release", "--offline", "--manifest-path", str(manifest.resolve()),
        "--message-format=json",
    ]).stdout
    for line in artifacts.splitlines():
        artifact = json.loads(line)
        if artifact.get("reason") == "compiler-artifact" and artifact.get("executable"):
            return Path(artifact["executable"])
    raise RuntimeError("Cargo did not report the realtime payload probe executable")


def rust_rows(stdout: str) -> dict[str, dict[str, float]]:
    return {
        match["name"]: {key: rust_seconds(match[key]) for key in ("median", "min", "max")}
        for line in stdout.splitlines()
        if (match := RUST_ROW.fullmatch(line))
    }


def rust_runs(directory: Path, corpus_size: int) -> list[dict[str, Any]]:
    runs = []
    with tempfile.TemporaryDirectory(prefix="rust-realtime-", dir=directory) as scratch:
        probe = build_rust_payload_probe(Path(scratch))
        for run_index in range(3):
            print(f"Running Rust benchmark {run_index + 1}/3", flush=True)
            run = pre_run()
            official = run_command(RUST_COMMAND)
            probe_run = pre_run()
            extra = run_command([str(probe), str(SYNTHETIC)])
            operations = rust_rows(official.stdout)
            extra_operations = rust_rows(extra.stdout)
            if "realtime/full parse+dump (headline)" not in operations or len(extra_operations) != 4 * corpus_size:
                raise RuntimeError("could not read the Rust benchmark rows")
            runs.append({
                **run,
                "payload_probe": {**probe_run, "stdout": extra.stdout, "stderr": extra.stderr},
                "operations": {**operations, **extra_operations},
                "stdout": official.stdout,
                "stderr": official.stderr,
            })
    return runs


def model_functions(payload: Payload, context: Any) -> dict[str, Callable[[], dict]]:
    from openbyd import models
    from pybyd.models.realtime import VehicleRealtimeData
    from pybyd.models.vehicle import EnergyType

    raw = payload.raw
    if context is None:
        return {
            PYBYD: lambda: VehicleRealtimeData.model_validate(raw).model_dump(mode="json"),
            OPENBYD: lambda: models.parse(MODEL, raw),
        }
    python_context = context
    member = context.get("energy_type") if isinstance(context, dict) else None
    if isinstance(member, dict) and set(member) == {"EnergyType"}:
        python_context = {**context, "energy_type": EnergyType(member["EnergyType"])}
    return {
        PYBYD: lambda: VehicleRealtimeData.model_validate(raw, context=python_context).model_dump(mode="json"),
        OPENBYD: lambda: models.parse(MODEL, raw, context=context),
    }


def model_sample(call: Callable[[], dict], minimum_s: float) -> dict[str, float | int]:
    # Check the clock once per chunk to keep the timing overhead small.
    start = time.perf_counter()
    calls = 0
    while True:
        for _ in range(64):
            call()
        calls += 64
        elapsed = time.perf_counter() - start
        if elapsed >= minimum_s:
            return {"calls": calls, "elapsed_s": elapsed, "s_per_call": elapsed / calls}


def timings(samples: list[dict[str, Any]]) -> dict[str, Any]:
    return {"s_per_call": common.summarize([sample["s_per_call"] for sample in samples]), "samples": samples}


def implementation_order(keys: list[str], repeat: int) -> list[str]:
    # Alternate which model runs first, or rotate the three whole-call clients.
    start = repeat % len(keys)
    return keys[start:] + keys[:start]


def model_case(payload: Payload, context: Any, repeats: int, seconds: float) -> dict[str, Any]:
    member = context.get("energy_type") if isinstance(context, dict) else None
    context_label = "default" if context is None else "supplied"
    if isinstance(member, dict) and set(member) == {"EnergyType"} and member["EnergyType"] == 2:
        context_label = "HYBRID"
    case = {**payload.shape(), "context": context_label}
    try:
        implementations = model_functions(payload, context)
        expected, actual = (call() for call in implementations.values())
        if not isinstance(actual, dict) or actual != expected:
            return {**case, "error": "model dicts differ"}
        for call in implementations.values():
            for _ in range(64):
                call()
        keys = list(implementations)
        samples: dict[str, list[dict[str, Any]]] = {key: [] for key in keys}
        for repeat in range(repeats):
            for key in implementation_order(keys, repeat):
                samples[key].append(model_sample(implementations[key], seconds))
        return {**case, "implementations": {key: timings(values) for key, values in samples.items()}}
    except Exception as exc:
        # Pydantic errors may embed raw field values. Save only the exception's class.
        return {**case, "error": type(exc).__name__}


def model_group(payloads: list[Payload], repeats: int, seconds: float) -> dict[str, Any]:
    run = pre_run()
    rows = []
    for payload in payloads:
        rows.append(model_case(payload, None, repeats, seconds))
        if payload.context is not None:
            rows.append(model_case(payload, payload.context, repeats, seconds))
    return {**run, "rows": rows}


def check_requests(server: Any, count: int) -> None:
    received = Counter((request["endpoint"], request["code"]) for request in server.requests())
    server.clear_requests()
    expected = Counter({(TRIGGER, "0"): count, (POLL, "0"): count})
    if received != expected:
        raise RuntimeError(f"request counts or reply codes differ: {received!r} != {expected!r}")


async def client_batch(call: Callable[..., Awaitable[Any]], concurrency: int) -> list[Any]:
    if concurrency == 1:
        return [await call(VIN, **POLL_SETTINGS)]
    return await asyncio.gather(*(call(VIN, **POLL_SETTINGS) for _ in range(concurrency)))


async def client_sample(
    call: Callable[..., Awaitable[Any]], concurrency: int, minimum_s: float,
) -> dict[str, float | int]:
    start = time.perf_counter()
    calls = 0
    while True:
        await client_batch(call, concurrency)
        calls += concurrency
        elapsed = time.perf_counter() - start
        if elapsed >= minimum_s:
            return {"calls": calls, "elapsed_s": elapsed, "s_per_call": elapsed / calls}


async def whole_call(
    raw: dict[str, Any], concurrency: int, repeats: int, seconds: float,
) -> dict[str, Any]:
    import openbyd
    import pybyd
    from openbyd.testing import FakeServer, Reply
    from pybyd.models.realtime import VehicleRealtimeData
    from pybyd.models.vehicle import EnergyType

    ready = {**raw, "requestSerial": "S1", "time": raw.get("time", 1700000000)}
    expected = VehicleRealtimeData.model_validate(ready, context={"energy_type": EnergyType.EV}).model_dump(
        mode="json"
    )
    keys = [PYBYD, PYBYD_ACCEL, OPENBYD]
    with FakeServer([(USER, PASSWORD)]) as server:
        server.reply(VEHICLES, Reply.data([{"vin": VIN, "energyType": "0"}]))
        server.reply(TRIGGER, Reply.data({"requestSerial": "S1"}))
        server.reply(POLL, Reply.data(ready))
        config = {"username": USER, "password": PASSWORD, "base_url": server.url, "mqtt_enabled": False}

        async with contextlib.AsyncExitStack() as clients:
            by_key = {}
            for key in keys:
                with accelerated() if key == PYBYD_ACCEL else contextlib.nullcontext():
                    client = openbyd.Client(**config) if key == OPENBYD else pybyd.BydClient(pybyd.BydConfig(**config))
                    await clients.enter_async_context(client)
                    await client.login()
                    await client.get_vehicles()
                    server.clear_requests()
                    by_key[key] = client.get_vehicle_realtime
                    for count in (1, concurrency):
                        answers = await client_batch(by_key[key], count)
                        check_requests(server, count)
                        for result in answers:
                            if key != OPENBYD and isinstance(result, VehicleRealtimeData):
                                result = result.model_dump(mode="json")
                            if not isinstance(result, dict) or result != expected:
                                raise RuntimeError(f"{key} whole-call result differs from the expected model dump")

            rows = []
            for count in (1, concurrency):
                run = pre_run()
                samples: dict[str, list[dict[str, Any]]] = {key: [] for key in keys}
                for repeat in range(repeats):
                    for key in implementation_order(keys, repeat):
                        with accelerated() if key == PYBYD_ACCEL else contextlib.nullcontext():
                            sample = await client_sample(by_key[key], count, seconds)
                        check_requests(server, int(sample["calls"]))
                        samples[key].append(sample)
                rows.append({
                    **run,
                    "concurrency": count,
                    "implementations": {key: timings(values) for key, values in samples.items()},
                })
    return {"rows": rows, "poll_settings": POLL_SETTINGS, "requests_per_call": 2, "server_delay_s": 0}


def cell(summary: dict[str, float], scale: float) -> str:
    return f"{summary['median'] * scale:.2f} [{summary['min'] * scale:.2f}, {summary['max'] * scale:.2f}]"


def ratio(baseline: dict[str, float], ours: dict[str, float]) -> str:
    if common.overlap(baseline, ours):
        verdict = "overlap"
    else:
        verdict = "faster" if baseline["median"] > ours["median"] else "slower"
    return f"{baseline['median'] / ours['median']:.2f}x ({verdict})"


def report_rust(runs: list[dict[str, Any]]) -> None:
    print("\n## Level 1 · Rust (µs)\n")
    print("| Operation | Run 1 | Run 2 | Run 3 | Median of run medians [overall min, max] |")
    print("|---|---:|---:|---:|---:|")
    for name in runs[0]["operations"]:
        measurements = [run["operations"][name] for run in runs]
        overall = {
            "median": statistics.median(item["median"] for item in measurements),
            "min": min(item["min"] for item in measurements),
            "max": max(item["max"] for item in measurements),
        }
        print(f"| {name} | " + " | ".join(cell(item, 1e6) for item in [*measurements, overall]) + " |")


def report_models(title: str, group: dict[str, Any], headline_s: float | None) -> None:
    print(f"\n## {title} (µs)\n")
    print("| Index | Keys | Sentinel/empty | Energy type | Context | pyBYD | openbyd | pyBYD / openbyd |")
    print("|---:|---:|---:|---|---|---:|---:|---:|")
    for row in group["rows"]:
        index = f"{row['index']} (headline)" if row["headline"] else row["index"]
        shape = f"| {index} | {row['keys']} | {row['sentinel_or_empty']} | {row['energy_type']} | {row['context']} |"
        if "error" in row:
            print(f"{shape} — | — | {row['error']} |")
            continue
        impl = row["implementations"]
        theirs, ours = (impl[key]["s_per_call"] for key in (PYBYD, OPENBYD))
        flags = []
        if ours["median"] > theirs["median"]:
            flags.append("openbyd median slower")
        if headline_s is not None and ours["median"] >= 1.5 * headline_s:
            flags.append(f"{ours['median'] / headline_s:.2f}x headline")
        result = ", ".join([ratio(theirs, ours), *flags])
        print(f"{shape} {cell(theirs, 1e6)} | {cell(ours, 1e6)} | {result} |")


def report_clients(group: dict[str, Any]) -> None:
    print("\n## Level 3 · Whole call (ms)\n")
    print("| Call mode | pyBYD | pyBYD + Rust cipher | openbyd | pyBYD / openbyd | accelerated / openbyd |")
    print("|---|---:|---:|---:|---:|---:|")
    for row in group["rows"]:
        count = row["concurrency"]
        label = "Sequential latency" if count == 1 else f"{count} concurrent, wall time / calls"
        impl = row["implementations"]
        theirs, accelerated_py, ours = (impl[key]["s_per_call"] for key in (PYBYD, PYBYD_ACCEL, OPENBYD))
        print(
            f"| {label} | {cell(theirs, 1e3)} | {cell(accelerated_py, 1e3)} | {cell(ours, 1e3)} "
            f"| {ratio(theirs, ours)} | {ratio(accelerated_py, ours)} |"
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--corpus", action="append", type=Path, default=[], help="additional corpus (repeatable)")
    parser.add_argument("--json", type=Path, required=True, help="path for timing samples and run metadata")
    parser.add_argument("--concurrency", type=int, default=8, help="calls per concurrent batch (default: 8)")
    parser.add_argument("--repeats", type=int, default=7)
    parser.add_argument("--seconds", type=float, default=0.5, help="minimum measured time per repeat (default: 0.5)")
    parser.add_argument("--skip-rust", action="store_true", help="only run the Python and whole-call benchmarks")
    args = parser.parse_args()
    if args.concurrency < 2 or args.repeats < 1 or not 0 < args.seconds < float("inf"):
        parser.error("concurrency must be at least 2, repeats positive, and seconds finite and positive")
    if not args.skip_rust and not os.environ.get("CARGO_TARGET_DIR"):
        parser.error("set CARGO_TARGET_DIR to a build directory before running Rust benchmarks")
    if args.json.resolve() in {path.resolve() for path in [SYNTHETIC, *args.corpus]}:
        parser.error("--json must be separate from the input corpora")

    synthetic = list(read_corpus(SYNTHETIC))
    (headline,) = (payload for payload in synthetic if payload.headline)
    versions = common.versions_info()
    if versions["pybyd_commit_installed"] != versions["pybyd_commit_pinned"]:
        parser.error("install pyBYD from the commit in parity/UPSTREAM_COMMIT")
    args.json.parent.mkdir(parents=True, exist_ok=True)
    document = {
        "machine": common.machine_info(),
        "versions": {
            **versions,
            **{package: metadata.version(package) for package in ("aiohttp", "pydantic", "pydantic-core")},
        },
        "repeats": args.repeats,
        "minimum_s": args.seconds,
    }
    if not args.skip_rust:
        document["rust_command"] = RUST_COMMAND
        document["rust"] = rust_runs(args.json.parent, len(synthetic))
        report_rust(document["rust"])

    print("\nTiming the synthetic model corpus.", flush=True)
    document["synthetic"] = model_group(synthetic, args.repeats, args.seconds)
    report_models("Level 2 · Synthetic model corpus", document["synthetic"], None)
    headline_row = next(row for row in document["synthetic"]["rows"] if row["headline"] and row["context"] == "default")
    headline_s = headline_row.get("implementations", {}).get(OPENBYD, {}).get("s_per_call", {}).get("median")

    print("\nTiming the whole call on localhost.", flush=True)
    document["whole_call"] = asyncio.run(whole_call(headline.raw, args.concurrency, args.repeats, args.seconds))
    report_clients(document["whole_call"])

    document["additional"] = []
    failed = any("error" in row for row in document["synthetic"]["rows"])
    for index, path in enumerate(args.corpus, start=1):
        print(f"\nTiming additional corpus {index}.", flush=True)
        group = model_group(list(read_corpus(path)), args.repeats, args.seconds)
        document["additional"].append({"corpus": index, **group})
        report_models(f"Level 4 · Additional corpus {index}", group, headline_s)
        failed |= any("error" in row for row in group["rows"])

    args.json.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
