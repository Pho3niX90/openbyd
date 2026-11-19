"""Run every benchmark and the report.

    python bench/run_all.py            # the full run (about an hour and a half on the reference machine)
    python bench/run_all.py --quick    # a smoke test, under a minute or two

Exits non-zero when any benchmark fails to run, when openbyd gives a different answer from pyBYD in a
micro-benchmark row, when a load run's request count differs from the plan, or when MQTT connections do not
all come up. CI can call ``python bench/run_all.py --quick``.

It needs the extension built with the fake server (``python parity/dev_install.py``), pyBYD at the pinned
commit, psutil and a Rust toolchain on PATH. ``pip install ".[bench]"`` installs psutil; install pyBYD at the pinned commit separately.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402

SCRIPTS = ("micro", "load", "mqtt_idle")


def preflight() -> list[str]:
    problems = []
    for module in ("psutil", "pybyd", "openbyd"):
        try:
            __import__(module)
        except ImportError as exc:
            problems.append(f"cannot import {module}: {exc}")
    try:
        import openbyd.testing  # noqa: F401
    except ImportError as exc:
        problems.append(f"{exc} (run `python parity/dev_install.py`)")
    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--quick", action="store_true", help="small sizes, for a smoke test")
    parser.add_argument("--skip-build", action="store_true", help="do not run cargo build first")
    parser.add_argument("--only", nargs="+", choices=SCRIPTS, help="run only these benchmarks")
    args = parser.parse_args(argv)

    problems = preflight()
    if problems:
        print("\n".join(f"PROBLEM: {problem}" for problem in problems))
        return 2
    if not args.skip_build:
        common.build_examples()

    failed = []
    started = time.perf_counter()
    for name in args.only or SCRIPTS:
        cmd = [sys.executable, str(common.BENCH / f"{name}.py"), "--skip-build"]
        if args.quick:
            cmd.append("--quick")
        print(f"\n=== {name} ===", flush=True)
        done = subprocess.run(cmd, cwd=common.ROOT, check=False)
        if done.returncode != 0:
            failed.append(name)
    print(f"\nfinished in {time.perf_counter() - started:.0f} s", flush=True)

    report = subprocess.run([sys.executable, str(common.BENCH / "report.py"), *(["--quick"] if args.quick else [])], cwd=common.ROOT, check=False)
    if report.returncode != 0:
        failed.append("report")
    if failed:
        print(f"FAILED: {', '.join(failed)}")
        return 1
    print("all benchmarks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
