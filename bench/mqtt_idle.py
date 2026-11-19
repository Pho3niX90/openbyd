"""Idle MQTT connections: memory and threads per connected-but-idle account.

A client process holds N connections to the TLS test broker (a process of its own, the
``mqtt_idle`` example binary), each subscribed to its own topic, and then idles. The driver waits
until the broker sees N open connections and N subscriptions, lets things settle, and reads the
client process's resident memory, thread count and CPU from outside.

* openbyd: the ``mqtt_idle client`` example: N ``MqttRuntime``s on one tokio runtime.
* pyBYD: ``pybyd._mqtt.BydMqttRuntime``, N of them: paho starts one thread per client.

pyBYD always verifies the broker's certificate against the system's certificate store, and the
test broker's certificate is made for the run. So that it can connect, this benchmark patches
paho's ``tls_set`` **inside the pyBYD worker process only** to turn certificate verification off
(``cert_reqs=CERT_NONE`` and ``tls_insecure_set(True)``). The openbyd example does the same with a
verifier that accepts any certificate. Nothing else is changed, and nothing outside this benchmark
is affected.

Run ``python bench/mqtt_idle.py --help``. ``--quick`` is a smoke test.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import statistics
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402


@dataclass(frozen=True)
class Implementation:
    key: str
    label: str
    kind: str  # "python" or "native"
    arg: str | int  # python: the --worker name; native: tokio worker threads (0 = default)
    sizes: tuple[int, ...]
    quick_sizes: tuple[int, ...]


#: The table of implementations. A new one is one more row.
IMPLEMENTATIONS: dict[str, Implementation] = {
    "pybyd": Implementation("pybyd", "pyBYD (paho, one thread per client)", "python", "pybyd", (200,), (20,)),
    "openbyd": Implementation("openbyd", "openbyd MqttRuntime (tokio)", "native", 0, (200, 1000), (20, 50)),
}


# ---------------------------------------------------------------------------
# The pyBYD worker (runs in a child process)
# ---------------------------------------------------------------------------


def python_worker(args: argparse.Namespace) -> None:
    import ssl

    import paho.mqtt.client as mqtt
    from pybyd._mqtt import BydMqttRuntime, MqttBootstrap

    original = mqtt.Client.tls_set

    def tls_set_without_verification(self: Any, *_args: Any, **_kwargs: Any) -> None:
        original(self, cert_reqs=ssl.CERT_NONE)
        self.tls_insecure_set(True)

    # Benchmark only: the test broker's certificate is not in the system's store.
    mqtt.Client.tls_set = tls_set_without_verification  # type: ignore[method-assign]

    print("READY", flush=True)
    if sys.stdin.readline().strip() != "go":
        return
    loop = asyncio.new_event_loop()
    started = time.perf_counter()
    runtimes = []
    failed = 0
    for index in range(args.count):
        runtime = BydMqttRuntime(
            loop=loop,
            decrypt_key_hex="00112233445566778899aabbccddeeff",
            on_event=lambda _event: None,
        )
        try:
            runtime.start(
                MqttBootstrap(
                    user_id=str(10_001 + index),
                    broker_host="127.0.0.1",
                    broker_port=args.port,
                    topic=f"oversea/res/{10_001 + index}",
                    client_id=f"bench-client-{index}",
                    username=str(10_001 + index),
                    password="bench",
                )
            )
            runtimes.append(runtime)
        except Exception as exc:  # noqa: BLE001
            failed += 1
            print(f"start failed: {exc!r}", file=sys.stderr, flush=True)
    result = {
        "implementation": "pybyd",
        "count": args.count,
        "started": len(runtimes),
        "failed": failed,
        "connect_s": time.perf_counter() - started,
    }
    print("RESULT " + json.dumps(result), flush=True)
    while True:
        command = sys.stdin.readline().strip()
        if command in {"quit", ""}:
            break
        print(json.dumps({"running": sum(1 for r in runtimes if r.is_running)}), flush=True)
    for runtime in runtimes:
        runtime.stop()


# ---------------------------------------------------------------------------
# The driver
# ---------------------------------------------------------------------------


def worker_command(impl: Implementation, port: int, count: int) -> list[str | Path]:
    if impl.kind == "python":
        return [sys.executable, Path(__file__).resolve(), "--worker", str(impl.arg), "--port", str(port), "--count", str(count)]
    return [common.example("mqtt_idle"), "client", "--port", str(port), "--count", str(count), "--workers", str(impl.arg)]


def run_one(impl: Implementation, count: int, settle: float, idle_window: float) -> dict[str, Any]:
    broker = common.Child([common.example("mqtt_idle"), "broker"], "mqtt_broker")
    child = None
    try:
        port = int(broker.expect("PORT", 60))
        broker.expect("READY", 60)
        child = common.Child(worker_command(impl, port, count), f"mqtt-{impl.key}-{count}")
        child.expect("READY", 120)
        process = common.measured_process(child.proc.pid)
        before = common.process_snapshot(process)
        sampler = common.Sampler(process.pid)
        sampler.start()
        started = time.perf_counter()
        child.send("go")
        reported = json.loads(child.expect("RESULT", 600))
        # Wait until the broker sees every connection open and subscribed.
        deadline = time.monotonic() + 120
        stats: dict[str, Any] = {}
        while time.monotonic() < deadline:
            stats = json.loads(broker.ask("stats"))
            if stats["open"] >= count and stats["subscribed"] >= count:
                break
            time.sleep(0.2)
        all_up_s = time.perf_counter() - started
        time.sleep(settle)
        # Idle window: memory and threads are read repeatedly, CPU over the whole window.
        cpu_start = process.cpu_times()
        rss_samples, thread_samples = [], []
        window_end = time.monotonic() + idle_window
        while time.monotonic() < window_end:
            rss_samples.append(process.memory_info().rss)
            thread_samples.append(process.num_threads())
            time.sleep(0.25)
        cpu_end = process.cpu_times()
        stats_after = json.loads(broker.ask("stats"))
        peak = common.os_peak_rss(process)
        sampler.stop()
        rss_idle = statistics.median(rss_samples)
        idle_cpu = (cpu_end.user + cpu_end.system) - (cpu_start.user + cpu_start.system)
        connected = count if stats_after["open"] >= count and stats_after["subscribed"] >= count else min(stats_after["open"], stats_after["subscribed"])
        return {
            "implementation": impl.key,
            "label": impl.label,
            "count": count,
            "reported": reported,
            "broker": {"open": stats_after["open"], "subscribed": stats_after["subscribed"]},
            "all_connected": stats_after["open"] >= count and stats_after["subscribed"] >= count,
            "connected": connected,
            "measured": {
                "all_up_s": all_up_s,
                "rss_before_mib": common.mib(before["rss"]),
                "rss_idle_mib": common.mib(rss_idle),
                "rss_peak_mib": common.mib(max(sampler.peak_rss, peak or 0)),
                "threads_before": before["threads"],
                "threads_idle": statistics.median(thread_samples),
                "threads_peak": max(sampler.peak_threads, max(thread_samples)),
                "idle_cpu_s": idle_cpu,
                "idle_window_s": idle_window,
                "idle_cpu_percent_of_one_core": 100 * idle_cpu / idle_window,
                "total_cpu_s_since_ready": (cpu_end.user + cpu_end.system) - (before["cpu_s"]),
            },
            "derived": {
                "memory_per_connection_kib": (rss_idle - before["rss"]) / 1024 / count,
                "peak_memory_per_connection_kib": (max(sampler.peak_rss, peak or 0) - before["rss"]) / 1024 / count,
                "threads_per_connection": (statistics.median(thread_samples) - before["threads"]) / count,
            },
        }
    finally:
        if child is not None:
            child.close()
        broker.close()


def run(args: argparse.Namespace) -> dict[str, Any]:
    keys = args.impls or list(IMPLEMENTATIONS)
    common.require_examples("mqtt_idle", build=not args.skip_build)
    rows = []
    for key in keys:
        impl = IMPLEMENTATIONS[key]
        sizes = args.sizes or (impl.quick_sizes if args.quick else impl.sizes)
        for count in sizes:
            print(f"mqtt_idle: {impl.label}, {count} connections ...", flush=True)
            row = run_one(impl, count, args.settle, args.idle_window)
            derived, measured = row["derived"], row["measured"]
            print(
                f"  {derived['memory_per_connection_kib']:.0f} KiB/connection, "
                f"{measured['threads_idle']:.0f} threads, connected {row['connected']}/{count}, "
                f"idle cpu {measured['idle_cpu_percent_of_one_core']:.2f}% of a core",
                flush=True,
            )
            rows.append(row)
    return {
        "settings": {
            "settle_s": args.settle,
            "idle_window_s": args.idle_window,
            "keepalive_s": 120,
            "tls_verification": "off on both sides (see the module documentation)",
        },
        "rows": rows,
    }


def check(results: dict[str, Any]) -> list[str]:
    return [
        f"{row['label']} at {row['count']}: only {row['connected']} connections were open and subscribed"
        for row in results["rows"]
        if not row["all_connected"]
    ]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--quick", action="store_true", help="small sizes, for a smoke test")
    parser.add_argument("--sizes", type=int, nargs="+", help="connections per run (default: per implementation)")
    parser.add_argument("--impls", nargs="+", choices=list(IMPLEMENTATIONS))
    parser.add_argument("--settle", type=float, help="seconds to wait after all are connected (default 3)")
    parser.add_argument("--idle-window", type=float, help="seconds of idle measurement (default 5)")
    parser.add_argument("--skip-build", action="store_true", help="do not run cargo build first")
    parser.add_argument("--worker", help=argparse.SUPPRESS)
    parser.add_argument("--port", type=int, help=argparse.SUPPRESS)
    parser.add_argument("--count", type=int, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if args.worker:
        python_worker(args)
        return 0
    args.settle = args.settle if args.settle is not None else (0.5 if args.quick else 3.0)
    args.idle_window = args.idle_window if args.idle_window is not None else (1.0 if args.quick else 5.0)
    results = run(args)
    path = common.write_results("mqtt_idle", args.quick, results)
    print(f"wrote {path}")
    problems = check(results)
    for problem in problems:
        print("PROBLEM:", problem)
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
