"""Load benchmark: what does one account cost in CPU and memory?

N accounts each log in, list their vehicles and run K poll cycles (realtime, GPS, charging status,
HVAC status, energy consumption) with a short think time between cycles, against the fake BYD
server running in a process of its own. Each implementation runs as its own process and is
measured from outside with psutil.

Run ``python bench/load.py --help``. ``--quick`` is a smoke test. Results go to ``bench/results/``.

Adding an implementation is one more entry in :data:`IMPLEMENTATIONS`; a Python one also gets a
branch in :func:`python_worker`, a native one is a binary that speaks the line protocol of
``common.measure_child``.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402

VIN = "TESTVIN0000000001"
PASSWORD = "secret"
#: HTTP requests one account makes: login, vehicle list, then per cycle realtime (trigger and poll),
#: GPS (trigger and poll), charging, HVAC and energy.
REQUESTS_PER_CYCLE = 7
REQUESTS_PER_ACCOUNT_FIXED = 2


def expected_requests(accounts: int, cycles: int) -> int:
    return accounts * (REQUESTS_PER_ACCOUNT_FIXED + REQUESTS_PER_CYCLE * cycles)


@dataclass(frozen=True)
class Implementation:
    key: str
    label: str
    kind: str  # "python" or "native"
    arg: str | int  # python: the --worker name; native: tokio worker threads (0 = default)
    #: Runs that take many minutes (pure-Python cipher at 200+ accounts) are not repeated.
    slow: bool = False


#: The table of implementations. A new one is one more row.
IMPLEMENTATIONS: dict[str, Implementation] = {
    "pybyd": Implementation("pybyd", "pyBYD", "python", "pybyd", slow=True),
    "pybyd_accel": Implementation("pybyd_accel", "pyBYD + openbyd cipher", "python", "pybyd_accel"),
    "openbyd_1": Implementation("openbyd_1", "openbyd native, 1 worker", "native", 1),
    "openbyd_default": Implementation("openbyd_default", "openbyd native, default workers", "native", 0),
}


# ---------------------------------------------------------------------------
# The Python worker (runs in a child process)
# ---------------------------------------------------------------------------


def python_worker(args: argparse.Namespace) -> None:
    """Child process: one asyncio loop, one pyBYD ``BydClient`` per account, MQTT off."""
    import statistics

    import pybyd

    if args.worker == "pybyd_accel":
        from openbyd import pybyd_accel

        if not pybyd_accel.install():
            raise SystemExit("openbyd's cipher could not be installed into pyBYD")
    elif args.worker != "pybyd":
        raise SystemExit(f"unknown worker {args.worker}")

    calls_ok = 0
    errors: dict[str, int] = {}
    failed = 0

    def record_error(exc: BaseException) -> None:
        nonlocal failed
        failed += 1
        text = f"{type(exc).__name__}: {exc}"[:160]
        if len(errors) < 5 or text in errors:
            errors[text] = errors.get(text, 0) + 1

    async def call(awaitable: Any) -> None:
        nonlocal calls_ok
        try:
            await awaitable
            calls_ok += 1
        except Exception as exc:  # noqa: BLE001 - counted and reported
            record_error(exc)

    async def account(index: int) -> None:
        if args.ramp_ms:
            await asyncio.sleep(index * args.ramp_ms / 1000 / args.accounts)
        config = pybyd.BydConfig(
            username=f"user{index}@example.com",
            password=PASSWORD,
            base_url=args.url,
            mqtt_enabled=False,
        )
        poll = {"poll_attempts": 10, "poll_interval": args.poll_interval, "mqtt_timeout": 0.0, "signal_retries": 2}
        try:
            client = pybyd.BydClient(config)
            await client.async_start()
        except Exception as exc:  # noqa: BLE001
            record_error(exc)
            return
        try:
            await call(client.login())
            await call(client.get_vehicles())
            for cycle in range(args.cycles):
                if cycle:
                    await asyncio.sleep(args.think_ms / 1000)
                await call(client.get_vehicle_realtime(VIN, **poll))
                await call(client.get_gps_info(VIN, **poll))
                await call(client.get_charging_status(VIN))
                await call(client.get_hvac_status(VIN))
                await call(client.get_energy_consumption(VIN))
        finally:
            await client.async_close()

    async def main() -> dict[str, Any]:
        lags: list[float] = []
        stop = asyncio.Event()

        async def monitor() -> None:
            # How late a 10 ms timer fires: the time the loop was blocked or busy.
            while not stop.is_set():
                started = time.perf_counter()
                await asyncio.sleep(0.01)
                # On Windows asyncio's clock ticks every 15.6 ms and a timer may fire up to a tick
                # early, so a lag below that resolution reads as 0 rather than negative.
                lags.append(max(0.0, (time.perf_counter() - started - 0.01) * 1000))

        watcher = asyncio.create_task(monitor())
        started = time.perf_counter()
        await asyncio.gather(*(account(i) for i in range(args.accounts)))
        elapsed = time.perf_counter() - started
        stop.set()
        await watcher
        return {
            "implementation": args.worker,
            "accounts": args.accounts,
            "cycles": args.cycles,
            "calls_ok": calls_ok,
            "calls_failed": failed,
            "errors": errors,
            "elapsed_s": elapsed,
            "loop_lag_ms": {
                "median": statistics.median(lags) if lags else 0.0,
                "p99": common.percentile(lags, 0.99),
                "max": max(lags) if lags else 0.0,
                "samples": len(lags),
            },
        }

    print("READY", flush=True)
    if sys.stdin.readline().strip() != "go":
        return
    result = asyncio.run(main())
    print("RESULT " + json.dumps(result), flush=True)
    sys.stdin.readline()


# ---------------------------------------------------------------------------
# The driver
# ---------------------------------------------------------------------------


def worker_command(impl: Implementation, url: str, accounts: int, args: argparse.Namespace) -> list[str | Path]:
    shared = [
        "--url", url,
        "--accounts", str(accounts),
        "--cycles", str(args.cycles),
        "--think-ms", str(args.think_ms),
        "--poll-interval", str(args.poll_interval),
        "--ramp-ms", str(args.ramp_ms),
    ]  # fmt: skip
    if impl.kind == "python":
        return [sys.executable, Path(__file__).resolve(), "--worker", str(impl.arg), *shared]
    return [common.example("load"), *shared, "--workers", str(impl.arg)]


def run_one(impl: Implementation, accounts: int, args: argparse.Namespace) -> dict[str, Any]:
    expected = expected_requests(accounts, args.cycles)
    with common.FakeServerProcess(accounts, args.delay_ms) as server:
        run = common.measure_child(
            worker_command(impl, server.url, accounts, args),
            f"{impl.key}-{accounts}",
            run_timeout=args.timeout,
        )
        stats = server.stats()
    reported, measured = run["reported"], run["measured"]
    cycles_total = accounts * args.cycles
    requests = stats["requests"]
    ok_code = stats["by_code"].get("0", 0)
    row: dict[str, Any] = {
        "implementation": impl.key,
        "label": impl.label,
        "accounts": accounts,
        "cycles": args.cycles,
        "server": {
            "requests": requests,
            "expected_requests": expected,
            "requests_ok": ok_code,
            "by_code": stats["by_code"],
            "by_endpoint": stats["by_endpoint"],
            "connections": stats["connections"],
        },
        "request_count_ok": requests == expected and ok_code == expected,
        "reported": reported,
        "measured": measured,
        "derived": {
            "cpu_ms_per_cycle": measured["cpu_s"] * 1000 / cycles_total,
            "cpu_ms_per_request": measured["cpu_s"] * 1000 / requests if requests else None,
            "memory_per_account_kib": (measured["rss_peak_mib"] - measured["rss_before_mib"]) * 1024 / accounts,
            "memory_end_per_account_kib": (measured["rss_after_mib"] - measured["rss_before_mib"]) * 1024 / accounts,
            "requests_per_s": requests / measured["wall_s"] if measured["wall_s"] else None,
            "cpu_utilisation": measured["cpu_s"] / measured["wall_s"] if measured["wall_s"] else None,
        },
    }
    return row


def repeats_for(impl: Implementation, accounts: int, args: argparse.Namespace) -> int:
    if args.repeats is not None:
        return args.repeats
    if args.quick or (impl.slow and accounts >= 200):
        return 1
    return 3


def run_repeated(impl: Implementation, accounts: int, args: argparse.Namespace) -> dict[str, Any]:
    """Run the same load several times; the row is the run with the median CPU, plus the spread of all."""
    runs = [run_one(impl, accounts, args) for _ in range(repeats_for(impl, accounts, args))]
    ranked = sorted(runs, key=lambda row: row["derived"]["cpu_ms_per_cycle"])
    row = ranked[len(ranked) // 2]
    row["repeats"] = {
        "runs": len(runs),
        **{
            name: common.summarize([run["derived"][name] for run in runs])
            for name in ("cpu_ms_per_cycle", "memory_per_account_kib", "requests_per_s")
        },
        "all_request_counts_ok": all(run["request_count_ok"] for run in runs),
        "all_calls_failed": [run["reported"]["calls_failed"] for run in runs],
    }
    row["request_count_ok"] = row["repeats"]["all_request_counts_ok"]
    return row


def run(args: argparse.Namespace) -> dict[str, Any]:
    keys = args.impls or list(IMPLEMENTATIONS)
    unknown = [key for key in keys if key not in IMPLEMENTATIONS]
    if unknown:
        raise SystemExit(f"unknown implementations {unknown}; choose from {list(IMPLEMENTATIONS)}")
    common.require_examples("fake_server", "load", build=not args.skip_build)
    rows: list[dict[str, Any]] = []
    for accounts in args.sizes:
        for key in keys:
            impl = IMPLEMENTATIONS[key]
            print(f"load: {impl.label}, {accounts} accounts x {args.cycles} cycles ...", flush=True)
            started = time.perf_counter()
            row = run_repeated(impl, accounts, args)
            took = time.perf_counter() - started
            derived, reported = row["derived"], row["reported"]
            print(
                f"  {took:.1f} s ({row['repeats']['runs']} run(s)): cpu {derived['cpu_ms_per_cycle']:.1f} ms/cycle, "
                f"{derived['memory_per_account_kib']:.0f} KiB/account, "
                f"{derived['requests_per_s']:.0f} req/s, failed {reported['calls_failed']}, "
                f"requests {row['server']['requests']}/{row['server']['expected_requests']}",
                flush=True,
            )
            rows.append(row)
    return {
        "settings": {
            "sizes": args.sizes,
            "cycles": args.cycles,
            "think_ms": args.think_ms,
            "poll_interval_s": args.poll_interval,
            "server_delay_ms": args.delay_ms,
            "ramp_ms": args.ramp_ms,
            "repeats": args.repeats if args.repeats is not None else "3 (1 for quick runs, and for pyBYD at 200+ accounts)",
            "requests_per_account": REQUESTS_PER_ACCOUNT_FIXED + REQUESTS_PER_CYCLE * args.cycles,
        },
        "rows": rows,
    }


def check(results: dict[str, Any]) -> list[str]:
    """Problems that make a result untrustworthy: a request count that differs from the plan, or failures."""
    problems = []
    for row in results["rows"]:
        where = f"{row['label']} at {row['accounts']} accounts"
        if not row["request_count_ok"]:
            server = row["server"]
            problems.append(f"{where}: server saw {server['requests']} requests ({server['by_code']}), expected {server['expected_requests']}")
        if any(row["repeats"]["all_calls_failed"]):
            problems.append(f"{where}: calls failed in some run ({row['repeats']['all_calls_failed']}): {row['reported']['errors']}")
    return problems


def add_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--quick", action="store_true", help="small sizes, for a smoke test")
    parser.add_argument("--sizes", type=int, nargs="+", help="accounts per run (default 50 200 1000)")
    parser.add_argument("--cycles", type=int, help="poll cycles per account (default 5)")
    parser.add_argument("--think-ms", type=int, default=100, help="think time between cycles")
    parser.add_argument("--poll-interval", type=float, default=0.05, help="seconds slept before a realtime/GPS poll")
    parser.add_argument("--delay-ms", type=int, help="artificial delay of each fake-server reply (default 20)")
    parser.add_argument("--ramp-ms", type=int, default=0, help="spread the accounts' start over this many ms")
    parser.add_argument("--impls", nargs="+", help=f"implementations to run (default all: {' '.join(IMPLEMENTATIONS)})")
    parser.add_argument("--repeats", type=int, help="runs per implementation and size (default 3; 1 for pyBYD at 200+ accounts)")
    parser.add_argument("--timeout", type=float, default=4 * 3600, help="seconds one run may take")
    parser.add_argument("--skip-build", action="store_true", help="do not run cargo build first")


def settle_defaults(args: argparse.Namespace) -> None:
    if args.quick:
        args.sizes = args.sizes or [5, 15]
        args.cycles = args.cycles or 2
        args.delay_ms = 5 if args.delay_ms is None else args.delay_ms
    else:
        args.sizes = args.sizes or [50, 200, 1000]
        args.cycles = args.cycles or 5
        args.delay_ms = 20 if args.delay_ms is None else args.delay_ms


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    add_arguments(parser)
    parser.add_argument("--worker", help=argparse.SUPPRESS)  # internal: run as a Python worker process
    parser.add_argument("--url", help=argparse.SUPPRESS)
    parser.add_argument("--accounts", type=int, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if args.worker:
        python_worker(args)
        return 0
    settle_defaults(args)
    results = run(args)
    path = common.write_results("load", args.quick, results)
    print(f"wrote {path}")
    problems = check(results)
    for problem in problems:
        print("PROBLEM:", problem)
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
