"""Times openbyd's model parsing against pyBYD's, through Python, on the corpus payloads.

    python crates/openbyd-protocol/benches/compare_pybyd.py [--iterations N] [--runs R] [MODEL ...]

For every model with payloads in ``parity/corpus`` (the largest payload of
each, plus every realtime payload), one call is
``openbyd.models.parse(name, raw)`` against pyBYD's
``Model.model_validate(raw).model_dump(mode="json")``. Each side runs
``--iterations`` calls per run, ``--runs`` times, interleaved; the table shows
the median time per call with the spread of the runs ((max - min) / median),
and pyBYD's median divided by openbyd's.

Needs the extension built into the environment (``python parity/dev_install.py``)
and pyBYD at ``parity/UPSTREAM_COMMIT`` installed.
"""

from __future__ import annotations

import argparse
import importlib
import json
import statistics
import time
from pathlib import Path

from openbyd import models as openbyd_models

ROOT = Path(__file__).resolve().parents[3]
CORPUS = ROOT / "parity" / "corpus"


def pybyd_class(name: str):
    """pyBYD's model class of this name, from whichever ``pybyd.models`` module defines it."""
    package = importlib.import_module("pybyd.models")
    if hasattr(package, name):
        return getattr(package, name)
    for module in sorted(Path(package.__file__).parent.glob("*.py")):
        candidate = importlib.import_module(f"pybyd.models.{module.stem}")
        if hasattr(candidate, name):
            return getattr(candidate, name)
    raise LookupError(name)


def cases(selected: list[str]) -> list[tuple[str, str, dict]]:
    """``(label, model, payload)``: each model's largest payload, and every realtime one."""
    found = []
    for path in sorted(CORPUS.glob("*.json")):
        name = path.stem
        if selected and name not in selected:
            continue
        entries = json.loads(path.read_text(encoding="utf-8"))
        if name == "VehicleRealtimeData":
            for entry in entries:
                found.append((f"{name}: {entry['note'][:38]}", name, entry["payload"]))
        else:
            largest = max(entries, key=lambda entry: len(json.dumps(entry["payload"])))
            found.append((name, name, largest["payload"]))
    return found


def per_call(function, iterations: int) -> float:
    start = time.perf_counter()
    for _ in range(iterations):
        function()
    return (time.perf_counter() - start) / iterations


def spread(times: list[float]) -> float:
    """(max - min) / median of the runs."""
    return (max(times) - min(times)) / statistics.median(times)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--iterations", type=int, default=20_000)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("models", nargs="*", help="limit to these model names")
    args = parser.parse_args()

    print(f"{'payload':<62} {'openbyd':>10} {'+-':>5} {'pyBYD':>10} {'+-':>5} {'ratio':>7}")
    for label, name, payload in cases(args.models):
        cls = pybyd_class(name)
        # Same answer first: the timing is only meaningful if both agree.
        expected = cls.model_validate(payload).model_dump(mode="json")
        assert openbyd_models.parse(name, payload) == expected, f"{label}: dumps differ"

        def ours(name=name, payload=payload):
            return openbyd_models.parse(name, payload)

        def theirs(cls=cls, payload=payload):
            return cls.model_validate(payload).model_dump(mode="json")

        per_call(ours, args.iterations // 10)
        per_call(theirs, args.iterations // 10)
        ours_times, theirs_times = [], []
        for _ in range(args.runs):
            ours_times.append(per_call(ours, args.iterations))
            theirs_times.append(per_call(theirs, args.iterations))
        ours_median = statistics.median(ours_times)
        theirs_median = statistics.median(theirs_times)
        print(
            f"{label:<62} {ours_median * 1e6:>8.2f}us {spread(ours_times):>4.0%}"
            f" {theirs_median * 1e6:>8.2f}us {spread(theirs_times):>4.0%}"
            f" {theirs_median / ours_median:>6.2f}x"
        )


if __name__ == "__main__":
    main()
