"""Replay benchmark: the backend's real mqtt-worker against openbyd's replay server.

The replay server (the ``replay_server`` example of openbyd-client) answers like BYD did for the
accounts and cars of a scenario (``bench/replay/SCENARIO.md``) and pushes the recorded MQTT
events. Each variant of the worker runs against the same scenario, from an empty database, and is
measured from outside like ``bench/load.py`` measures its clients:

* the operating system's view of the worker process (psutil): CPU seconds over the measured
  window, resident memory, threads;
* the worker's own Prometheus metrics, scraped from its health port while it runs: event-loop
  lag, hydrate times and gaps, slots;
* the replay server's ``stats``: BYD requests by endpoint and code, logins, MQTT connections,
  pushes sent and missed;
* the database afterwards: the samples stored, and the drives and charges the worker detected.

The correctness check: every variant must detect the same drives and charges from the same
scenario. The detections are compared run against run with the tolerances in
:data:`TOLERANCES`; every difference is listed. A second check compares a sample of the worker's
stored polls with the scenario's state at that time (the replay round trip).

One run of a variant: fresh Postgres and Redis containers on tmpfs (``--local-store``: a SQLite
file and the in-memory store), the replay server with ``--epoch now``, the backend's
``scripts/replay_seed.py``, then ``python -m app.mqtt_worker`` through ``bench/replay/launcher.py``
(the loopback-only network guard), from the backend checkout, with a fresh environment. Once the
worker answers on its health port the replay is started; after ``--minutes`` of wall time the
worker is stopped cleanly and everything is collected.

    python bench/replay_worker.py --backend E:/Projects/bydconnect-replay/backend \\
        --backend-python E:/tmp/openbyd-replay/venv-backend/Scripts/python.exe \\
        --scenario bench/replay/samples/demo.jsonl --minutes 20 --repeats 3

``--quick`` is a smoke run (2 variants, 3 minutes, results written under the scratch directory,
not ``bench/results/``). Run ``python bench/replay_worker.py --help`` for the rest.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import random
import re
import secrets
import shutil
import socket
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request
from bisect import bisect_right
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402

LAUNCHER = common.BENCH / "replay" / "launcher.py"
DEFAULT_SCENARIO = common.BENCH / "replay" / "samples" / "demo.jsonl"
HEALTH_PORT = 8081
PG_CONTAINER = "openbyd-replay-pg"
REDIS_CONTAINER = "openbyd-replay-redis"
PG_PORT = 55432
REDIS_PORT = 56379
PG_IMAGE = "postgres:16-alpine"
REDIS_IMAGE = "redis:7-alpine"

# ---------------------------------------------------------------------------
# Variants
# ---------------------------------------------------------------------------

#: The Python worker, started through the guard. ``{python}`` is ``--backend-python``.
PYTHON_WORKER = ("{python}", "{launcher}", "--guard-report", "{guard_report}", "--stdin-stop", "-m", "app.mqtt_worker")


@dataclass(frozen=True)
class Variant:
    """One way of running the worker: a command template and the settings that make it this variant.

    ``command`` is formatted with ``python`` (the backend's interpreter), ``launcher``
    (``bench/replay/launcher.py``), ``guard_report`` (a file the launcher writes) and ``backend``
    (the backend checkout, the working directory). ``env`` is added to the run's environment.
    ``expect`` maps a gauge of the worker's ``/metrics`` to the value it must have, so a variant
    whose setting did not take effect is reported instead of measured as something else.
    """

    key: str
    label: str
    command: tuple[str, ...]
    env: dict[str, str] = field(default_factory=dict)
    expect: dict[str, float] = field(default_factory=dict)


#: The table of variants; adding one is one more row. A future Rust worker binary is a row whose
#: ``command`` names the binary (for example ``("{backend}/target/release/byd-worker",)``) with
#: the same environment contract: it must read ``BYD_BASE_URL_OVERRIDE``, ``DATABASE_URL`` and
#: ``REDIS_URL``, serve ``/metrics`` on port 8081 and stop cleanly on standard-input ``stop`` (or
#: end of input). The launcher's network guard only covers Python processes; a native binary
#: must be contained another way (its only BYD URL is the override).
VARIANTS: dict[str, Variant] = {
    "python_cipher": Variant(
        "python_cipher",
        "pyBYD cipher (worker as on main)",
        PYTHON_WORKER,
        {"BYD_NATIVE_CIPHER": "false", "BYD_SHADOW_SAMPLE_RATE": "0"},
        {"bydconnect_byd_native_cipher": 0},
    ),
    "rust_cipher": Variant(
        "rust_cipher",
        "openbyd cipher",
        PYTHON_WORKER,
        {"BYD_NATIVE_CIPHER": "true", "BYD_SHADOW_SAMPLE_RATE": "0"},
        {"bydconnect_byd_native_cipher": 1},
    ),
    "rust_cipher_shadow": Variant(
        "rust_cipher_shadow",
        "openbyd cipher + shadow parsing (PR #271)",
        PYTHON_WORKER,
        {"BYD_NATIVE_CIPHER": "true", "BYD_SHADOW_SAMPLE_RATE": "1"},
        {"bydconnect_byd_native_cipher": 1},
    ),
}

QUICK_VARIANTS = ("python_cipher", "rust_cipher")


def render_command(variant: Variant, values: dict[str, str]) -> list[str]:
    """The variant's command with its placeholders filled in."""
    return [part.format(**values) for part in variant.command]


# ---------------------------------------------------------------------------
# Tolerances of the detection comparison
# ---------------------------------------------------------------------------

#: How far two runs' detections of the same drive or charge may differ. Times are replay seconds.
#: Two runs never poll at the same instant, so a boundary can move by the cadence that applies
#: when it is detected: a start is noticed while the car is parked (the worker's parked rule, after
#: MQTT_SILENCE_SECONDS = 300 s of silence), an end while it is active (MIN_SAMPLE_GAP_SECONDS =
#: 90 s, the shortest gap between two stationary samples). Distances, odometers and energy must
#: agree to their rounding (0.1), SoC to its (1 %).
TOLERANCES: dict[str, float] = {
    "start": 300.0,
    "end": 90.0,
    "km": 0.05,
    "start_odo": 0.05,
    "end_odo": 0.05,
    "start_soc": 0.5,
    "end_soc": 0.5,
    "energy_kwh": 0.05,
}

DRIVE_FIELDS = ("start", "end", "km", "start_odo", "end_odo")
CHARGE_FIELDS = ("start", "end", "start_soc", "end_soc", "energy_kwh")
DETECTION_FIELDS = {"drives": DRIVE_FIELDS, "charges": CHARGE_FIELDS}


# ---------------------------------------------------------------------------
# Pure helpers: Prometheus text, histograms
# ---------------------------------------------------------------------------

Samples = dict[str, list[tuple[dict[str, str], float]]]


def _parse_labels(text: str) -> dict[str, str]:
    labels: dict[str, str] = {}
    index = 0
    while index < len(text):
        eq = text.index("=", index)
        name = text[index:eq].strip().lstrip(",").strip()
        if text[eq + 1] != '"':
            raise ValueError(f"bad label value in {text!r}")
        value = []
        cursor = eq + 2
        while text[cursor] != '"':
            if text[cursor] == "\\":
                cursor += 1
                value.append({"n": "\n"}.get(text[cursor], text[cursor]))
            else:
                value.append(text[cursor])
            cursor += 1
        labels[name] = "".join(value)
        index = cursor + 1
        while index < len(text) and text[index] in ", ":
            index += 1
    return labels


def parse_prometheus(text: str) -> Samples:
    """``{metric name: [(labels, value), ...]}`` from the Prometheus text exposition format."""
    samples: Samples = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if "{" in line:
            name, rest = line.split("{", 1)
            labels_text, value_text = rest.rsplit("}", 1)
            labels = _parse_labels(labels_text)
        else:
            name, value_text = line.split(None, 1)
            labels = {}
        value_text = value_text.split()[0]
        samples.setdefault(name.strip(), []).append((labels, float(value_text)))
    return samples


def gauge(samples: Samples, name: str, **match: str) -> float | None:
    """The sum of a metric's values whose labels include ``match``; None when it is absent."""
    values = [value for labels, value in samples.get(name, []) if all(labels.get(k) == v for k, v in match.items())]
    return sum(values) if values else None


def histogram(samples: Samples, name: str, **match: str) -> dict[str, Any]:
    """A histogram's cumulative buckets, count and sum, summed over label sets that include ``match``."""
    buckets: dict[float, float] = {}
    for labels, value in samples.get(f"{name}_bucket", []):
        if all(labels.get(k) == v for k, v in match.items()):
            le = float("inf") if labels["le"] in ("+Inf", "Inf") else float(labels["le"])
            buckets[le] = buckets.get(le, 0.0) + value
    return {
        "buckets": dict(sorted(buckets.items())),
        "count": gauge(samples, f"{name}_count", **match) or 0.0,
        "sum": gauge(samples, f"{name}_sum", **match) or 0.0,
    }


def histogram_delta(before: dict[str, Any] | None, after: dict[str, Any]) -> dict[str, Any]:
    """What a histogram gained between two scrapes (counters only grow while the process lives)."""
    if before is None:
        return after
    return {
        "buckets": {le: count - before["buckets"].get(le, 0.0) for le, count in after["buckets"].items()},
        "count": after["count"] - before["count"],
        "sum": after["sum"] - before["sum"],
    }


def histogram_quantile(hist: dict[str, Any], q: float) -> float | None:
    """Prometheus' ``histogram_quantile``: linear interpolation inside the bucket holding rank q.

    In the +Inf bucket the answer is the largest finite bound (a lower bound of the truth).
    """
    total = hist["count"]
    if total <= 0:
        return None
    rank = q * total
    lower, below = 0.0, 0.0
    for le, cumulative in hist["buckets"].items():
        if cumulative >= rank:
            if le == float("inf"):
                return lower
            if cumulative == below:
                return le
            return lower + (le - lower) * (rank - below) / (cumulative - below)
        lower, below = (le if le != float("inf") else lower), cumulative
    return lower


def histogram_max_bound(hist: dict[str, Any]) -> tuple[str, float] | None:
    """The tightest statement about the largest observation: ``("<=", bound)`` or ``(">", bound)``."""
    total = hist["count"]
    if total <= 0:
        return None
    finite = 0.0
    for le, cumulative in hist["buckets"].items():
        if le == float("inf"):
            return (">", finite)
        if cumulative >= total:
            return ("<=", le)
        finite = le
    return (">", finite)


def summarize_histogram(hist: dict[str, Any]) -> dict[str, Any]:
    bound = histogram_max_bound(hist)
    return {
        "count": hist["count"],
        "mean": hist["sum"] / hist["count"] if hist["count"] else None,
        "p50": histogram_quantile(hist, 0.5),
        "p99": histogram_quantile(hist, 0.99),
        "max_bound": list(bound) if bound else None,
    }


def label_values(samples: Samples, name: str, label: str) -> list[str]:
    return sorted({labels[label] for labels, _ in samples.get(f"{name}_count", []) if label in labels})


LOOP_LAG = "bydconnect_worker_event_loop_lag_seconds"
HYDRATE_SECONDS = "bydconnect_worker_hydrate_seconds"
HYDRATE_GAP = "bydconnect_worker_hydrate_gap_seconds"
GAUGES = ("bydconnect_worker_slots", "bydconnect_worker_hydrates_in_flight", "bydconnect_worker_due_backlog")
SHADOW = "bydconnect_openbyd_shadow_total"
SHADOW_MISMATCH = "bydconnect_openbyd_shadow_field_mismatch_total"


def metrics_summary(first: Samples | None, last: Samples, gauge_peaks: dict[str, float]) -> dict[str, Any]:
    """The window's loop lag and hydrate figures (last scrape minus the one at replay start)."""

    def delta(name: str, **match: str) -> dict[str, Any]:
        return histogram_delta(histogram(first, name, **match) if first else None, histogram(last, name, **match))

    out: dict[str, Any] = {
        "loop_lag_s": summarize_histogram(delta(LOOP_LAG)),
        "hydrate_seconds": {"all": summarize_histogram(delta(HYDRATE_SECONDS))},
        "hydrate_gap_seconds": {"all": summarize_histogram(delta(HYDRATE_GAP))},
        "gauges_end": {name: gauge(last, name) for name in GAUGES},
        "gauges_peak": dict(gauge_peaks),
        "native_cipher": gauge(last, "bydconnect_byd_native_cipher"),
    }
    for reason in label_values(last, HYDRATE_SECONDS, "reason"):
        out["hydrate_seconds"][reason] = summarize_histogram(delta(HYDRATE_SECONDS, reason=reason))
    for reason in label_values(last, HYDRATE_GAP, "reason"):
        out["hydrate_gap_seconds"][reason] = summarize_histogram(delta(HYDRATE_GAP, reason=reason))
    shadow: dict[str, float] = {}
    for labels, value in last.get(SHADOW, []):
        shadow[labels.get("result", "?")] = shadow.get(labels.get("result", "?"), 0.0) + value
    out["shadow"] = shadow
    out["shadow_field_mismatches"] = sum(value for _labels, value in last.get(SHADOW_MISMATCH, []))
    return out


# ---------------------------------------------------------------------------
# Pure helpers: the scenario
# ---------------------------------------------------------------------------


@dataclass
class Scenario:
    """A scenario's header, counts, cars and per-car timeline of ``state`` and ``error`` lines in file order."""

    path: Path
    header: dict[str, Any]
    accounts: list[str]
    cars: dict[str, dict[str, Any]]
    counts: dict[str, int]
    timeline: dict[str, list[dict[str, Any]]]

    def summary(self) -> dict[str, Any]:
        """What the results may hold: the header, counts and the file's name, never its lines."""
        return {"file": self.path.name, "header": self.header, "counts": self.counts}


def read_scenario(path: Path) -> Scenario:
    timeline: dict[str, list[dict[str, Any]]] = {}
    accounts: list[str] = []
    cars: dict[str, dict[str, Any]] = {}
    counts: dict[str, int] = {}
    with path.open(encoding="utf-8") as handle:
        header = json.loads(handle.readline())
        if header.get("type") != "scenario" or header.get("version") != 1:
            raise SystemExit(f"{path.name}: not a version 1 replay scenario")
        for line in handle:
            if not line.strip():
                continue
            item = json.loads(line)
            kind = item.get("type", "?")
            counts[kind] = counts.get(kind, 0) + 1
            if kind == "account":
                accounts.append(item["account"])
            elif kind == "car":
                cars[item["vin"]] = item
            elif kind in ("state", "error"):
                timeline.setdefault(item["vin"], []).append(item)
    return Scenario(path, header, accounts, cars, counts, timeline)


# ---------------------------------------------------------------------------
# Pure helpers: detections and their comparison
# ---------------------------------------------------------------------------


def replay_seconds(unix_s: float | None, origin: dict[str, float]) -> float | None:
    """A worker timestamp as scenario seconds: ``start_at + (t - wall at start) * speed``."""
    if unix_s is None:
        return None
    return round(origin["start_at"] + (unix_s - origin["wall_unix_ms"] / 1000.0) * origin["speed"], 1)


def _num(value: Any) -> float | None:
    return None if value is None else round(float(value), 3)


def canonical_detections(raw: dict[str, Any], origin: dict[str, float]) -> dict[str, dict[str, list[dict[str, Any]]]]:
    """Per VIN, the closed drives and charges as sorted lists with times in replay seconds.

    ``raw`` is what :func:`extract` returns: rows with unix times.
    """
    out: dict[str, dict[str, list[dict[str, Any]]]] = {"drives": {}, "charges": {}}
    for row in raw.get("drives", []):
        out["drives"].setdefault(row["vin"], []).append(
            {
                "start": replay_seconds(row["start_at"], origin),
                "end": replay_seconds(row["end_at"], origin),
                "km": _num(row["km"]),
                "start_odo": _num(row["start_odo"]),
                "end_odo": _num(row["end_odo"]),
            }
        )
    for row in raw.get("charges", []):
        out["charges"].setdefault(row["vin"], []).append(
            {
                "start": replay_seconds(row["start_at"], origin),
                "end": replay_seconds(row["end_at"], origin),
                "start_soc": _num(row["start_soc"]),
                "end_soc": _num(row["end_soc"]),
                "energy_kwh": _num(row["energy_kwh"]),
            }
        )
    for kind in out.values():
        for items in kind.values():
            items.sort(key=lambda item: (item["start"] is None, item["start"] or 0.0, item["end"] or 0.0))
    return {kind: dict(sorted(per_vin.items())) for kind, per_vin in out.items()}


def _field_differs(field_name: str, a: Any, b: Any, tolerances: dict[str, float]) -> bool:
    if a is None and b is None:
        return False
    if a is None or b is None:
        return True
    return abs(float(a) - float(b)) > tolerances[field_name] + 1e-9


def _match(reference: list[dict[str, Any]], other: list[dict[str, Any]], tolerance: float) -> list[tuple[int | None, int | None]]:
    """Pair the items of two runs by start time, keeping their order.

    Both lists are sorted by start. Of all pairings that keep that order and only pair starts
    within ``tolerance``, this takes the one with the most pairs, and among those the one whose
    starts are closest in total, so a neighbouring item is never taken from the one it belongs
    to. Unpaired items appear with None on the other side; an item without a start is unpaired.
    """
    n, m = len(reference), len(other)

    def gap(i: int, j: int) -> float | None:
        a, b = reference[i]["start"], other[j]["start"]
        if a is None or b is None:
            return None
        distance = abs(a - b)
        return distance if distance <= tolerance + 1e-9 else None

    # best[i][j]: (pairs, -total gap) for reference[i:] against other[j:].
    best = [[(0, 0.0)] * (m + 1) for _ in range(n + 1)]
    for i in range(n - 1, -1, -1):
        for j in range(m - 1, -1, -1):
            options = [best[i + 1][j], best[i][j + 1]]
            distance = gap(i, j)
            if distance is not None:
                pairs_after, gap_after = best[i + 1][j + 1]
                options.append((pairs_after + 1, gap_after - distance))
            best[i][j] = max(options)
    pairs: list[tuple[int | None, int | None]] = []
    i = j = 0
    while i < n and j < m:
        distance = gap(i, j)
        if distance is not None and best[i][j] == (best[i + 1][j + 1][0] + 1, best[i + 1][j + 1][1] - distance):
            pairs.append((i, j))
            i, j = i + 1, j + 1
        elif best[i][j] == best[i + 1][j]:
            pairs.append((i, None))
            i += 1
        else:
            pairs.append((None, j))
            j += 1
    pairs.extend((k, None) for k in range(i, n))
    pairs.extend((None, k) for k in range(j, m))
    return pairs


def compare_detections(
    reference: dict[str, dict[str, list[dict[str, Any]]]],
    other: dict[str, dict[str, list[dict[str, Any]]]],
    tolerances: dict[str, float] = TOLERANCES,
) -> list[dict[str, Any]]:
    """Every difference between two runs' detections, one entry each.

    ``type`` is ``count`` (a VIN has a different number of drives or charges), ``missing`` (a
    reference item no item of the other run matches), ``extra`` (an item of the other run no
    reference item matches) or ``field`` (a matched pair differs in one field by more than its
    tolerance).
    """
    differences: list[dict[str, Any]] = []
    for kind, fields in DETECTION_FIELDS.items():
        ref_kind, other_kind = reference.get(kind, {}), other.get(kind, {})
        for vin in sorted(set(ref_kind) | set(other_kind)):
            ref_items, other_items = ref_kind.get(vin, []), other_kind.get(vin, [])
            if len(ref_items) != len(other_items):
                differences.append(
                    {"kind": kind, "vin": vin, "type": "count", "reference": len(ref_items), "other": len(other_items)}
                )
            for i, j in _match(ref_items, other_items, tolerances["start"]):
                if j is None:
                    differences.append({"kind": kind, "vin": vin, "type": "missing", "reference": ref_items[i], "other": None})
                elif i is None:
                    differences.append({"kind": kind, "vin": vin, "type": "extra", "reference": None, "other": other_items[j]})
                else:
                    for name in fields:
                        a, b = ref_items[i].get(name), other_items[j].get(name)
                        if _field_differs(name, a, b, tolerances):
                            differences.append(
                                {
                                    "kind": kind,
                                    "vin": vin,
                                    "type": "field",
                                    "field": name,
                                    "start": ref_items[i]["start"],
                                    "reference": a,
                                    "other": b,
                                    "tolerance": tolerances[name],
                                }
                            )
    return differences


def detection_counts(detections: dict[str, dict[str, list[dict[str, Any]]]]) -> dict[str, int]:
    return {kind: sum(len(items) for items in per_vin.values()) for kind, per_vin in detections.items()}


# ---------------------------------------------------------------------------
# Pure helpers: the replay round trip
# ---------------------------------------------------------------------------

#: Stored poll field -> (scenario state key, BYD endpoint family that carries it).
ROUNDTRIP_FIELDS: dict[str, tuple[str, str]] = {
    "online": ("online", "realtime"),
    "currentSoc": ("soc", "realtime"),
    "odometer": ("odometer_km", "realtime"),
    "speed": ("speed_kmh", "realtime"),
    "charging.isCharging": ("charging", "charging"),
}

#: A reply is made when the request arrives and stamped when the worker stores it, a little later;
#: a state that changed within this many replay seconds before the stamp may be the one answered.
ROUNDTRIP_WINDOW_S = 15.0


def _decimals(value: float) -> int:
    text = repr(float(value))
    if "e" in text or "." not in text:
        return 0
    return len(text.split(".", 1)[1].rstrip("0"))


def values_match(stored: Any, expected: Any) -> bool:
    """Whether a stored value is the scenario's, allowing for rounding to the stored precision."""
    if isinstance(expected, bool) or isinstance(stored, bool):
        return bool(stored) == bool(expected) if isinstance(stored, bool) and isinstance(expected, bool) else False
    if expected is None or stored is None:
        return stored is None and expected is None
    try:
        stored_f, expected_f = float(stored), float(expected)
    except (TypeError, ValueError):
        return stored == expected
    step = 10.0 ** -_decimals(stored_f)
    return abs(stored_f - expected_f) <= step / 2 + 1e-9


def _poll_value(payload: dict[str, Any], key: str) -> Any:
    if key == "charging.isCharging":
        charging = payload.get("charging")
        return charging.get("isCharging") if isinstance(charging, dict) else None
    return payload.get(key)


def states_in_window(scenario: Scenario, vin: str, t: float, window: float) -> tuple[list[dict[str, Any]], bool]:
    """The states of ``vin`` that applied at some point in ``[t - window, t]``, and whether an error
    line (other than the one-off session expiry ``1005``) applied in that span: then the poll is
    not checked. A ``1005`` leaves the previous state in force."""
    lines = [line for line in scenario.timeline.get(vin, []) if str(line.get("code")) != "1005"]
    times = [line["t"] for line in lines]
    first = max(0, bisect_right(times, t - window) - 1)
    last = bisect_right(times, t)
    applied = lines[first:last] if last else []
    states = [line for line in applied if line["type"] == "state"]
    erred = any(line["type"] == "error" for line in applied)
    return states, erred


def roundtrip_check(
    polls: list[dict[str, Any]], scenario: Scenario, origin: dict[str, float], window: float = ROUNDTRIP_WINDOW_S
) -> dict[str, Any]:
    """Compare stored successful polls with the scenario's state at their time.

    For a car that is offline (``online`` false) only ``online`` is compared: BYD answers a
    sleeping car with placeholder zeros, which the backend drops. Every mismatch names the field
    and the BYD endpoint family that carries it.
    """
    by_field = {name: {"checked": 0, "matched": 0, "endpoint": endpoint} for name, (_key, endpoint) in ROUNDTRIP_FIELDS.items()}
    mismatches: list[dict[str, Any]] = []
    checked_polls = skipped = 0
    for poll in polls:
        t = replay_seconds(poll["received_at"], origin)
        if t is None:
            continue
        states, erred = states_in_window(scenario, poll["vin"], t, window)
        if erred or not states:
            skipped += 1
            continue
        checked_polls += 1
        payload = poll.get("payload") or {}
        for name, (key, endpoint) in ROUNDTRIP_FIELDS.items():
            stored = _poll_value(payload, name)
            if stored is None:
                continue
            candidates = [state for state in states if state.get("online") or name == "online"]
            if not candidates:
                continue
            by_field[name]["checked"] += 1
            if any(values_match(stored, state.get(key)) for state in candidates):
                by_field[name]["matched"] += 1
            elif len(mismatches) < 50:
                mismatches.append(
                    {
                        "vin": poll["vin"],
                        "t": t,
                        "field": name,
                        "endpoint": endpoint,
                        "stored": stored,
                        "expected": [state.get(key) for state in candidates],
                    }
                )
    checked = sum(entry["checked"] for entry in by_field.values())
    matched = sum(entry["matched"] for entry in by_field.values())
    return {
        "polls": len(polls),
        "polls_checked": checked_polls,
        "polls_skipped": skipped,
        "values_checked": checked,
        "values_matched": matched,
        "match_rate": matched / checked if checked else None,
        "by_field": by_field,
        "mismatches": mismatches,
        "window_s": window,
    }


# ---------------------------------------------------------------------------
# The database (runs in the backend's environment: ``--extract``)
# ---------------------------------------------------------------------------


def _unix(value: Any) -> float | None:
    if value is None:
        return None
    import datetime as dt

    if isinstance(value, str):
        value = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    if value.tzinfo is None:
        value = value.replace(tzinfo=dt.timezone.utc)
    return value.timestamp()


async def extract(database_url: str, poll_sample: int, seed: int) -> dict[str, Any]:
    """Samples stored, detections and a sample of successful polls, read with the backend's models."""
    sys.path.insert(0, os.getcwd())
    from sqlalchemy import func, select
    from sqlalchemy.ext.asyncio import AsyncSession, create_async_engine

    from app.db_models import Car, CloudChargeRow, CloudDriveRow, CloudLog, IngestEvent

    engine = create_async_engine(database_url)
    try:
        async with AsyncSession(engine) as db:
            grouped = await db.execute(
                select(IngestEvent.source, IngestEvent.kind, IngestEvent.detail, func.count())
                .group_by(IngestEvent.source, IngestEvent.kind, IngestEvent.detail)
            )
            ingest = [
                {"source": source, "kind": kind, "detail": detail, "count": count}
                for source, kind, detail, count in sorted(grouped.all(), key=lambda r: (r[0], r[1], r[2] or ""))
            ]
            polls_rows = (
                await db.execute(
                    select(IngestEvent.vin, IngestEvent.received_at, IngestEvent.payload)
                    .where(IngestEvent.source == "poll", IngestEvent.kind == "ok")
                    .order_by(IngestEvent.received_at)
                )
            ).all()
            polls = [{"vin": vin, "received_at": _unix(at), "payload": payload} for vin, at, payload in polls_rows]
            if len(polls) > poll_sample:
                polls = sorted(random.Random(seed).sample(polls, poll_sample), key=lambda p: p["received_at"])
            drives = [
                {
                    "vin": vin,
                    "start_at": _unix(start),
                    "end_at": _unix(end),
                    "km": km,
                    "start_odo": start_odo,
                    "end_odo": end_odo,
                }
                for vin, start, end, km, start_odo, end_odo in (
                    await db.execute(
                        select(
                            Car.vin, CloudDriveRow.start_at, CloudDriveRow.end_at, CloudDriveRow.km,
                            CloudDriveRow.start_odo, CloudDriveRow.end_odo,
                        ).join(Car, Car.id == CloudDriveRow.car_id)
                    )
                ).all()
            ]  # fmt: skip
            charges = [
                {
                    "vin": vin,
                    "start_at": _unix(start),
                    "end_at": _unix(end),
                    "start_soc": start_soc,
                    "end_soc": end_soc,
                    "energy_kwh": energy,
                }
                for vin, start, end, start_soc, end_soc, energy in (
                    await db.execute(
                        select(
                            Car.vin, CloudChargeRow.start_at, CloudChargeRow.end_at, CloudChargeRow.start_soc,
                            CloudChargeRow.end_soc, CloudChargeRow.energy_kwh,
                        ).join(Car, Car.id == CloudChargeRow.car_id)
                    )
                ).all()
            ]  # fmt: skip
            open_sessions = {"drives": 0, "charges": 0}
            for drive_log, charge_log in (await db.execute(select(CloudLog.drive_log, CloudLog.charge_log))).all():
                if isinstance(drive_log, dict) and drive_log.get("openDrive"):
                    open_sessions["drives"] += 1
                if isinstance(charge_log, dict) and charge_log.get("openCharge"):
                    open_sessions["charges"] += 1
    finally:
        await engine.dispose()
    return {
        "ingest": ingest,
        "polls_ok_total": len(polls_rows),
        "polls": polls,
        "drives": drives,
        "charges": charges,
        "open_at_end": open_sessions,
    }


# ---------------------------------------------------------------------------
# Processes and stores
# ---------------------------------------------------------------------------


def _docker(*args: str, check: bool = True, timeout: float = 120) -> subprocess.CompletedProcess[str]:
    done = subprocess.run(["docker", *args], capture_output=True, text=True, timeout=timeout, check=False)
    if check and done.returncode != 0:
        raise RuntimeError(f"docker {' '.join(args[:2])} failed: {done.stderr.strip()[:300]}")
    return done


def port_free(port: int) -> bool:
    with socket.socket() as sock:
        return sock.connect_ex(("127.0.0.1", port)) != 0


class DockerStore:
    """Postgres and Redis in fresh containers on tmpfs, on loopback ports. Their CPU is not measured."""

    def __init__(self) -> None:
        self.env = {
            "DATABASE_URL": f"postgresql+asyncpg://replay:replay@127.0.0.1:{PG_PORT}/replay",
            "DATABASE_SSL": "disable",
            "REDIS_URL": f"redis://127.0.0.1:{REDIS_PORT}/0",
        }

    def start(self) -> None:
        self.stop()
        for port in (PG_PORT, REDIS_PORT):
            if not port_free(port):
                raise SystemExit(f"port {port} is in use; the replay containers need it")
        _docker(
            "run", "-d", "--rm", "--name", PG_CONTAINER, "-p", f"127.0.0.1:{PG_PORT}:5432",
            "--tmpfs", "/var/lib/postgresql/data:rw", "-e", "POSTGRES_USER=replay", "-e", "POSTGRES_PASSWORD=replay",
            "-e", "POSTGRES_DB=replay", PG_IMAGE,
        )  # fmt: skip
        _docker(
            "run", "-d", "--rm", "--name", REDIS_CONTAINER, "-p", f"127.0.0.1:{REDIS_PORT}:6379", "--tmpfs", "/data:rw",
            REDIS_IMAGE, "redis-server", "--save", "", "--appendonly", "no",
        )  # fmt: skip
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            ready = _docker("exec", PG_CONTAINER, "pg_isready", "-h", "127.0.0.1", "-U", "replay", check=False)
            pong = _docker("exec", REDIS_CONTAINER, "redis-cli", "ping", check=False)
            if ready.returncode == 0 and pong.stdout.strip() == "PONG":
                return
            time.sleep(1)
        raise RuntimeError("Postgres or Redis did not become ready in 90 s")

    def stop(self) -> None:
        _docker("rm", "-f", PG_CONTAINER, REDIS_CONTAINER, check=False)


class LocalStore:
    """A SQLite file and the in-memory session store: a quick smoke test without Docker."""

    def __init__(self, directory: Path) -> None:
        self.path = directory / "replay.db"
        self.env = {"DATABASE_URL": f"sqlite+aiosqlite:///{self.path.as_posix()}", "REDIS_URL": "memory://"}

    def start(self) -> None:
        for suffix in ("", "-wal", "-shm", "-journal"):
            Path(f"{self.path}{suffix}").unlink(missing_ok=True)

    def stop(self) -> None:
        pass


def stats_delta(before: Any, after: Any) -> Any:
    """``after - before`` for the replay server's counters: numbers subtract, objects recurse, a
    key that only ``after`` has counts from zero. ``mqtt.open`` is a level, not a counter, and is kept."""
    if isinstance(after, dict):
        earlier = before if isinstance(before, dict) else {}
        return {key: value if key in ("open", "accounts", "cars") else stats_delta(earlier.get(key), value) for key, value in after.items()}
    if isinstance(after, bool) or not isinstance(after, (int, float)):
        return after
    return after - (before if isinstance(before, (int, float)) and not isinstance(before, bool) else 0)


def replay_server_command(args: argparse.Namespace) -> list[str | Path]:
    """The replay server to start: ``--replay-server`` (a ``.py`` file runs under this Python) or the built example."""
    path = args.replay_server or common.example("replay_server")
    return [sys.executable, path] if str(path).endswith(".py") else [path]


class ReplayServer:
    """The ``replay_server`` example: the fake BYD server and test broker playing a scenario."""

    def __init__(self, args: argparse.Namespace, ca_path: Path, label: str) -> None:
        cmd: list[str | Path] = [
            *replay_server_command(args), "--scenario", args.scenario.resolve(), "--epoch", "now",
            "--speed", str(args.speed), "--start-at", str(args.start_at), "--ca-out", ca_path,
        ]  # fmt: skip
        if args.delay_ms is not None:
            cmd += ["--delay-ms", str(args.delay_ms)]
        if args.corpus is not None:
            cmd += ["--corpus", args.corpus.resolve()]
        self.child = common.Child(cmd, label)
        self.url = self.child.expect("URL", 120)
        self.mqtt = self.child.expect("MQTT", 30)
        self.ca = self.child.expect("CA", 30)
        self.child.expect("READY", 60)

    def command(self, text: str, timeout: float = 60) -> str:
        return self.child.ask(text, timeout)

    def start(self) -> dict[str, Any]:
        line = self.command("start")
        if not line.startswith("STARTED"):
            raise RuntimeError(f"replay_server answered {line!r} to start")
        return json.loads(line[len("STARTED") :])

    def stats(self) -> dict[str, Any]:
        return json.loads(self.command("stats", 120))

    def clock(self) -> dict[str, Any]:
        return json.loads(self.command("clock"))

    def close(self) -> None:
        self.child.close()


#: Variables of the harness's own environment a child may inherit: what the OS and Python need to
#: start. Everything else (a developer's DATABASE_URL, SENTRY_DSN, API keys) is left behind.
_INHERITED = (
    "PATH", "PATHEXT", "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "COMSPEC", "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE", "USERPROFILE", "HOMEDRIVE", "HOMEPATH", "HOME", "LANG", "LC_ALL", "TZ",
)  # fmt: skip


def child_environment(scratch: Path, settings: dict[str, str]) -> dict[str, str]:
    """A fresh environment for the seeding script and the worker."""
    env = {name: os.environ[name] for name in _INHERITED if name in os.environ}
    tmp = scratch / "tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    env.update({"TEMP": str(tmp), "TMP": str(tmp), "TMPDIR": str(tmp), "PYTHONUNBUFFERED": "1", "PYTHONIOENCODING": "utf-8"})
    env.update(settings)
    return env


def http_get(url: str, timeout: float = 5) -> tuple[int, bytes]:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:  # noqa: S310 - loopback only
            return response.status, response.read()
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read()


def wait_healthy(worker: common.Child, timeout: float) -> float:
    started = time.monotonic()
    deadline = started + timeout
    while time.monotonic() < deadline:
        if worker.proc.poll() is not None:
            raise RuntimeError(f"the worker exited (code {worker.proc.returncode}); see {worker.log}")
        try:
            status, body = http_get(f"http://127.0.0.1:{HEALTH_PORT}/healthz", timeout=2)
            if status == 200 and b'"ok"' in body:
                return time.monotonic() - started
        except OSError:
            pass
        time.sleep(0.5)
    raise RuntimeError(f"the worker did not answer on port {HEALTH_PORT} within {timeout:.0f} s; see {worker.log}")


def scrape() -> Samples | None:
    try:
        status, body = http_get(f"http://127.0.0.1:{HEALTH_PORT}/metrics", timeout=5)
    except OSError:
        return None
    return parse_prometheus(body.decode("utf-8", "replace")) if status == 200 else None


def run_backend_python(args: argparse.Namespace, cmd: list[str], env: dict[str, str], timeout: float) -> str:
    done = subprocess.run(
        [str(args.backend_python), *cmd], cwd=args.backend, env=env, capture_output=True, text=True, timeout=timeout, check=False
    )
    if done.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd[:3])} failed ({done.returncode}): {done.stderr.strip()[-2000:]}")
    return done.stdout


def _line(output: str, prefix: str) -> str:
    for line in output.splitlines():
        if line.startswith(prefix):
            return line[len(prefix) :].strip()
    raise RuntimeError(f"no {prefix!r} line in output: {output[-500:]}")


# ---------------------------------------------------------------------------
# One run
# ---------------------------------------------------------------------------


def run_one(variant: Variant, repeat: int, scenario: Scenario, args: argparse.Namespace) -> dict[str, Any]:
    import psutil

    tag = f"{variant.key}-{repeat}"
    run_dir = args.scratch / "runs" / f"{time.strftime('%Y%m%d-%H%M%S')}-{tag}"
    run_dir.mkdir(parents=True, exist_ok=True)
    store: DockerStore | LocalStore = LocalStore(run_dir) if args.local_store else DockerStore()
    problems: list[str] = []
    if not port_free(HEALTH_PORT):
        raise SystemExit(f"port {HEALTH_PORT} is in use: another worker is running; the harness would scrape it")
    replay = None
    worker = None
    try:
        store.start()
        replay = ReplayServer(args, run_dir / "ca.pem", f"replay_server-{tag}")
        settings = {
            "BYD_SETTINGS_ENV_FILE": "",
            "BYD_BASE_URL_OVERRIDE": replay.url,
            "SSL_CERT_FILE": replay.ca,
            "CREDENTIAL_ENCRYPTION_KEY": secrets.token_urlsafe(32),
            "JWT_SECRET": secrets.token_urlsafe(48),
            "SENTRY_DSN": "",
            "MQTT_MAX_CLIENTS": str(max(150, len(scenario.accounts))),
            "POD_NAME": "replay-worker",
            **store.env,
        }
        env = child_environment(run_dir, settings)
        seeded = json.loads(
            _line(
                run_backend_python(args, [str(LAUNCHER), "-m", "scripts.replay_seed", "--scenario", str(args.scenario.resolve())], env, 600),
                "SEEDED",
            )
        )
        guard_file = run_dir / "guard.json"
        values = {"python": str(args.backend_python), "launcher": str(LAUNCHER), "guard_report": str(guard_file), "backend": str(args.backend)}
        launched_at = time.monotonic()
        worker = common.Child(render_command(variant, values), f"worker-{tag}", env={**env, **variant.env}, cwd=args.backend)
        health_s = wait_healthy(worker, 300)
        process = common.measured_process(worker.proc.pid)
        ready = common.process_snapshot(process)
        first_scrape = scrape()
        slots_full_s = None
        if args.start_when_claimed:
            deadline = time.monotonic() + args.claim_timeout
            while time.monotonic() < deadline:
                samples = scrape()
                if samples and (gauge(samples, "bydconnect_worker_slots") or 0) >= len(scenario.accounts):
                    slots_full_s = time.monotonic() - launched_at
                    break
                time.sleep(args.scrape_seconds)
            if slots_full_s is None:
                problems.append(f"the worker did not hold a slot for every account within {args.claim_timeout:.0f} s; the replay was started anyway")
        origin = replay.start()
        stats_at_start = replay.stats()
        before = common.process_snapshot(process)
        first_scrape = scrape() or first_scrape
        sampler = common.Sampler(process.pid, interval=0.25)
        sampler.start()
        psutil.cpu_percent(interval=None)
        started = time.perf_counter()
        peaks: dict[str, float] = {}
        last_scrape = first_scrape
        scrapes = 0
        next_progress = started + 30
        while (elapsed := time.perf_counter() - started) < args.minutes * 60:
            if worker.proc.poll() is not None:
                raise RuntimeError(f"the worker exited during the run (code {worker.proc.returncode}); see {worker.log}")
            samples = scrape()
            if samples:
                scrapes += 1
                last_scrape = samples
                for name in GAUGES:
                    value = gauge(samples, name)
                    if value is not None:
                        peaks[name] = max(peaks.get(name, value), value)
                slots = gauge(samples, "bydconnect_worker_slots") or 0
                if slots_full_s is None and slots >= len(scenario.accounts):
                    slots_full_s = time.monotonic() - launched_at
            if time.perf_counter() >= next_progress:
                next_progress += 30
                stats = replay.stats()
                print(
                    f"    {elapsed:5.0f} s: slots {gauge(last_scrape or {}, 'bydconnect_worker_slots')}, "
                    f"requests {stats.get('requests')}, logins {stats.get('logins')}, "
                    f"pushes {stats.get('mqtt', {}).get('pushes_sent')}",
                    flush=True,
                )
            time.sleep(args.scrape_seconds)
        wall = time.perf_counter() - started
        final = scrape() or last_scrape
        after = common.process_snapshot(process)
        system_cpu = psutil.cpu_percent(interval=None)
        peak_os = common.os_peak_rss(process)
        sampler.stop()
        total_cpu = after["cpu_s"]
        # Stop the worker cleanly: SIGINT inside the process (see launcher.py).
        stop_started = time.monotonic()
        worker.send("stop")
        clean = True
        try:
            guard = json.loads(worker.expect("GUARD_REPORT", 120))
            worker.proc.wait(timeout=30)
        except (TimeoutError, RuntimeError, subprocess.TimeoutExpired):
            clean = False
            worker.proc.kill()
            guard = json.loads(guard_file.read_text(encoding="utf-8")) if guard_file.exists() else {"blocked_total": None, "blocked": []}
        stop_s = time.monotonic() - stop_started
        stats = replay.stats()
        clock = replay.clock()
        replay.close()
        replay = None
        raw = json.loads(
            _line(
                run_backend_python(
                    args,
                    [str(Path(__file__).resolve()), "--extract", json.dumps({"database_url": store.env["DATABASE_URL"], "poll_sample": args.poll_sample, "seed": repeat})],
                    env,
                    600,
                ),
                "EXTRACT",
            )
        )
    finally:
        if worker is not None and worker.proc.poll() is None:
            worker.send("stop")
            try:
                worker.proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                worker.proc.kill()
        if worker is not None:
            worker.close()
        if replay is not None:
            replay.close()
        store.stop()

    accounts = len(scenario.accounts)
    peak_rss = max(sampler.peak_rss, peak_os or 0, after["rss"])
    measured = {
        "wall_s": wall,
        "cpu_s": after["cpu_s"] - before["cpu_s"],
        "cpu_user_s": after["cpu_user_s"] - before["cpu_user_s"],
        "cpu_system_s": after["cpu_system_s"] - before["cpu_system_s"],
        "cpu_total_s": total_cpu,
        "cpu_before_start_s": before["cpu_s"],
        "rss_ready_mib": common.mib(ready["rss"]),
        "rss_start_mib": common.mib(before["rss"]),
        "rss_end_mib": common.mib(after["rss"]),
        "rss_peak_mib": common.mib(peak_rss),
        "rss_peak_source": "os" if peak_os else "sampled",
        "threads_start": before["threads"],
        "threads_peak": max(sampler.peak_threads, after["threads"]),
        "samples": sampler.samples,
        "system_cpu_percent": system_cpu,
        "health_after_s": health_s,
        "slots_full_after_s": slots_full_s,
        "stop_s": stop_s,
        "stopped_cleanly": clean,
    }
    detections = canonical_detections(raw, origin)
    metrics = metrics_summary(first_scrape, final or {}, peaks)
    metrics["scrapes"] = scrapes
    row: dict[str, Any] = {
        "variant": variant.key,
        "label": variant.label,
        "repeat": repeat,
        "accounts": accounts,
        "cars": len(scenario.cars),
        "minutes": args.minutes,
        "origin": origin,
        "measured": measured,
        "derived": {
            "cpu_s_per_account": measured["cpu_s"] / accounts,
            "cpu_ms_per_account_minute": measured["cpu_s"] * 1000 / accounts / (wall / 60),
            "cpu_utilisation": measured["cpu_s"] / wall,
            "memory_per_account_kib": measured["rss_peak_mib"] * 1024 / accounts,
            "memory_growth_per_account_kib": (measured["rss_peak_mib"] - measured["rss_ready_mib"]) * 1024 / accounts,
        },
        "metrics": metrics,
        # ``stats`` counts from the server's start (the logins and first polls before the replay
        # began are in it); ``window`` is what the worker asked for during the measured window.
        "replay": {"stats": stats, "window": stats_delta(stats_at_start, stats), "clock": clock},
        "seed": seeded,
        "store": {
            "kind": "local" if args.local_store else "docker",
            "ingest": raw["ingest"],
            "polls_ok": raw["polls_ok_total"],
            "mqtt_rows": sum(item["count"] for item in raw["ingest"] if item["source"] == "mqtt"),
            "open_at_end": raw["open_at_end"],
        },
        "detections": detections,
        "detection_counts": detection_counts(detections),
        "roundtrip": roundtrip_check(raw["polls"], scenario, origin),
        "guard": guard,
        "log": worker.log.name if worker is not None else None,
    }
    for name, expected in variant.expect.items():
        seen = gauge(final or {}, name)
        if seen != expected:
            problems.append(f"{name} is {seen}, expected {expected}: the variant's setting did not take effect")
    if not clean:
        problems.append("the worker did not stop cleanly within 150 s and was killed")
    elif guard.get("exit_code") not in (0, None):
        problems.append(f"the worker exited with code {guard.get('exit_code')}")
    compared = sum((metrics.get("shadow") or {}).values()) if isinstance(metrics.get("shadow"), dict) else 0
    wants_shadow = float(variant.env.get("BYD_SHADOW_SAMPLE_RATE", "0") or 0) > 0
    if wants_shadow and not compared:
        problems.append("shadow parsing was asked for but no reply was compared: the variant ran without it")
    if not wants_shadow and compared:
        problems.append(f"shadow parsing was off but {compared:.0f} replies were compared")
    if stats.get("logins", 0) < accounts:
        problems.append(f"only {stats.get('logins')} logins for {accounts} accounts")
    if raw["polls_ok_total"] == 0:
        problems.append("no successful poll was stored")
    row["problems"] = problems
    return row


# ---------------------------------------------------------------------------
# Repeats, comparison, driver
# ---------------------------------------------------------------------------

AGGREGATED = (
    ("derived", "cpu_s_per_account"),
    ("derived", "cpu_ms_per_account_minute"),
    ("derived", "memory_per_account_kib"),
    ("derived", "memory_growth_per_account_kib"),
    ("measured", "cpu_s"),
    ("measured", "rss_peak_mib"),
    ("measured", "threads_peak"),
)


def aggregate(rows: list[dict[str, Any]]) -> dict[str, Any]:
    """Per variant: the median and range of each measured figure over its repeats."""
    out: dict[str, Any] = {}
    for key in dict.fromkeys(row["variant"] for row in rows):
        runs = [row for row in rows if row["variant"] == key]
        figures = {f"{group}.{name}": common.summarize([float(run[group][name]) for run in runs]) for group, name in AGGREGATED}
        lag = [run["metrics"]["loop_lag_s"]["p99"] for run in runs if run["metrics"]["loop_lag_s"]["p99"] is not None]
        if lag:
            figures["metrics.loop_lag_p99_s"] = common.summarize(lag)
        out[key] = {"label": runs[0]["label"], "runs": len(runs), "figures": figures}
    return out


def compare_runs(rows: list[dict[str, Any]], tolerances: dict[str, float] = TOLERANCES) -> list[dict[str, Any]]:
    """Every run's detections against every other run's.

    ``same_variant`` marks two repeats of one variant: what they differ in is what two runs of
    the same code differ in (they never poll at the same instants), the yardstick for reading
    the differences between variants.
    """
    out = []
    for index, reference in enumerate(rows):
        for row in rows[index + 1 :]:
            differences = compare_detections(reference["detections"], row["detections"], tolerances)
            out.append(
                {
                    "reference": f"{reference['variant']}#{reference['repeat']}",
                    "other": f"{row['variant']}#{row['repeat']}",
                    "same_variant": reference["variant"] == row["variant"],
                    "equal": not differences,
                    "differences": differences,
                }
            )
    return out


def interleaved(keys: list[str], repeats: int) -> list[tuple[str, int]]:
    """``A B C A B C``: each round runs every variant once, so drift over time hits all alike."""
    return [(key, repeat) for repeat in range(1, repeats + 1) for key in keys]


def build_replay_server(args: argparse.Namespace) -> None:
    cargo = shutil.which("cargo")
    if cargo is None:
        raise SystemExit("cargo is not on PATH (see AGENTS.md for the Rust environment)")
    cmd = [cargo, "build", "-j", str(args.jobs), "--release", "-p", "openbyd-client", "--features", "testing", "--example", "replay_server"]
    print("+", " ".join(cmd), f"(target {common.target_dir()})", flush=True)
    if subprocess.run(cmd, cwd=common.ROOT, check=False).returncode != 0:
        raise SystemExit("building replay_server failed")


def backend_versions(args: argparse.Namespace) -> dict[str, Any]:
    probe = (
        "import json, importlib.metadata as m\n"
        "def v(n):\n"
        "    try: return m.version(n)\n"
        "    except m.PackageNotFoundError: return None\n"
        "print(json.dumps({n: v(n) for n in ('pybyd', 'openbyd', 'paho-mqtt', 'aiohttp', 'sqlalchemy')}))\n"
    )
    done = subprocess.run([str(args.backend_python), "-c", probe], capture_output=True, text=True, timeout=60, check=False)
    packages = json.loads(done.stdout) if done.returncode == 0 else {}

    def git(*cmd: str) -> str:
        try:
            out = subprocess.run(["git", *cmd], cwd=args.backend, capture_output=True, text=True, timeout=30, check=False)
        except OSError:
            return ""
        return out.stdout.strip()

    return {
        "backend_commit": git("rev-parse", "HEAD") or "unknown",
        "backend_branch": git("rev-parse", "--abbrev-ref", "HEAD") or "unknown",
        "backend_dirty": bool(git("status", "--porcelain", "--", ".")),
        "packages": packages,
        "backend_python": platform_python(args),
    }


def platform_python(args: argparse.Namespace) -> str:
    done = subprocess.run([str(args.backend_python), "-c", "import platform; print(platform.python_version())"], capture_output=True, text=True, timeout=60, check=False)
    return done.stdout.strip()


def run(args: argparse.Namespace) -> dict[str, Any]:
    keys = args.variants or (list(QUICK_VARIANTS) if args.quick else list(VARIANTS))
    unknown = [key for key in keys if key not in VARIANTS]
    if unknown:
        raise SystemExit(f"unknown variants {unknown}; choose from {list(VARIANTS)}")
    if args.replay_server is None:
        if not args.skip_build:
            build_replay_server(args)
        if not common.example("replay_server").exists():
            raise SystemExit(f"{common.example('replay_server')} is missing: build it or drop --skip-build")
    scenario = read_scenario(args.scenario)
    name = args.scenario.name if scenario.header.get("source") == "synthetic" else "a scenario that is not synthetic"
    print(
        f"replay_worker: {name}: {len(scenario.accounts)} accounts, {len(scenario.cars)} cars, "
        f"{len(keys)} variant(s) x {args.repeats}, {args.minutes} min each, store {'local' if args.local_store else 'docker'}",
        flush=True,
    )
    rows: list[dict[str, Any]] = []
    for key, repeat in interleaved(keys, args.repeats):
        variant = VARIANTS[key]
        print(f"replay_worker: {variant.label} (run {repeat}) ...", flush=True)
        row = run_one(variant, repeat, scenario, args)
        m, d = row["measured"], row["derived"]
        print(
            f"  cpu {m['cpu_s']:.1f} s ({d['cpu_ms_per_account_minute']:.1f} ms/account/min), peak RSS {m['rss_peak_mib']:.0f} MiB, "
            f"threads {m['threads_peak']}, loop lag p99 {row['metrics']['loop_lag_s']['p99']}, "
            f"requests in window {row['replay']['window'].get('requests')}, "
            f"polls ok {row['store']['polls_ok']}, mqtt rows {row['store']['mqtt_rows']}, "
            f"drives {row['detection_counts']['drives']}, charges {row['detection_counts']['charges']}, "
            f"round trip {row['roundtrip']['match_rate']}, guard blocked {row['guard'].get('blocked_total')}",
            flush=True,
        )
        for problem in row["problems"]:
            print(f"  PROBLEM: {problem}", flush=True)
        rows.append(row)
    return {
        "settings": {
            "variants": keys,
            "variant_table": {key: {"label": VARIANTS[key].label, "command": list(VARIANTS[key].command), "env": VARIANTS[key].env} for key in keys},
            "repeats": args.repeats,
            "minutes": args.minutes,
            "speed": args.speed,
            "start_at": args.start_at,
            "server_delay_ms": args.delay_ms,
            "corpus": "parity/corpus" if args.corpus is None else f"{args.corpus.resolve().parent.parent.name}/parity/corpus",
            "store": "local (SQLite + memory://)" if args.local_store else f"docker ({PG_IMAGE}, {REDIS_IMAGE}, tmpfs)",
            "start_when_claimed": args.start_when_claimed,
            "scrape_seconds": args.scrape_seconds,
            "tolerances": TOLERANCES,
            "roundtrip_window_s": ROUNDTRIP_WINDOW_S,
            "poll_sample": args.poll_sample,
        },
        "scenario": scenario.summary(),
        "backend": backend_versions(args),
        "rows": rows,
        "variants": aggregate(rows),
        "comparison": compare_runs(rows),
    }


# ---------------------------------------------------------------------------
# Scenarios that are not synthetic: what may be published
# ---------------------------------------------------------------------------


def is_private(results: dict[str, Any]) -> bool:
    """Whether the scenario came from real cars: anything but a synthetic one."""
    return results["scenario"]["header"].get("source") != "synthetic"


def difference_summary(differences: list[dict[str, Any]]) -> dict[str, int]:
    """How many differences of each sort: ``drives missing``, ``charges field end``, ..."""
    out: dict[str, int] = {}
    for diff in differences:
        name = " ".join(str(part) for part in (diff["kind"], diff["type"], diff.get("field")) if part)
        out[name] = out.get(name, 0) + 1
    return dict(sorted(out.items()))


_PUBLIC_DETAIL = re.compile(r"[0-9]{1,5}|[a-z_]{1,24}")


def publishable(results: dict[str, Any]) -> dict[str, Any]:
    """The results with everything about a single car taken out, for a scenario exported from production.

    What a real car did (when it drove, how far, its odometer, its state of charge) stays in the
    complete results under ``--private-dir``. Published are the measurements, the counts, and for
    each comparison how many differences of each sort there were.
    """
    header = results["scenario"]["header"]
    out = dict(results)
    out["redacted"] = True
    out["scenario"] = {
        "file": "production export (not published)",
        "header": {key: header.get(key) for key in ("version", "source", "duration_s", "accounts", "cars")},
        "counts": results["scenario"]["counts"],
    }
    rows = []
    for row in results["rows"]:
        cars_with = {kind: sum(1 for items in per_vin.values() if items) for kind, per_vin in row["detections"].items()}
        public = {key: value for key, value in row.items() if key != "log"}
        store = dict(row["store"])
        # The worker's own words for why a poll failed or was skipped: a code or a fixed word is kept.
        store["ingest"] = [
            {**item, "detail": item["detail"] if item["detail"] is None or _PUBLIC_DETAIL.fullmatch(str(item["detail"])) else "other"}
            for item in store["ingest"]
        ]
        rows.append(
            {
                **public,
                "store": store,
                "detections": {kind: {} for kind in row["detections"]},
                "cars_with_detections": cars_with,
                "roundtrip": {**row["roundtrip"], "mismatches": [], "mismatches_listed": len(row["roundtrip"]["mismatches"])},
            }
        )
    out["rows"] = rows
    out["settings"] = {key: value for key, value in results["settings"].items() if key != "merged_from"}
    out["comparison"] = [
        {
            "reference": entry["reference"],
            "other": entry["other"],
            "same_variant": entry.get("same_variant"),
            "equal": entry["equal"],
            "differences": [],
            "difference_count": len(entry["differences"]),
            "difference_summary": difference_summary(entry["differences"]),
            "cars_differing": len({diff["vin"] for diff in entry["differences"]}),
        }
        for entry in results["comparison"]
    ]
    return out


#: Settings that must be the same in results files that are merged as repeats of one another.
MERGE_SAME = ("minutes", "speed", "start_at", "server_delay_ms", "corpus", "store", "start_when_claimed", "tolerances")


def merge_results(documents: list[dict[str, Any]], names: list[str]) -> dict[str, Any]:
    """Several results files of the same scenario and settings as one: each file's runs become further repeats."""
    base = documents[0]
    for name, document in zip(names, documents):
        if document.get("redacted"):
            raise SystemExit(f"{name}: a published (redacted) file cannot be merged; merge the complete ones")
        if document["scenario"] != base["scenario"]:
            raise SystemExit(f"{name}: not the same scenario as {names[0]}")
        differing = [key for key in MERGE_SAME if document["settings"].get(key) != base["settings"].get(key)]
        if differing:
            raise SystemExit(f"{name}: settings differ from {names[0]}: {differing}")
        if document["backend"].get("backend_commit") != base["backend"].get("backend_commit"):
            raise SystemExit(f"{name}: not the same backend commit as {names[0]}")
        if document["backend"].get("packages") != base["backend"].get("packages"):
            raise SystemExit(f"{name}: the backend's packages (pyBYD, openbyd, ...) are not the versions of {names[0]}")
    starts = [row["origin"]["wall_unix_ms"] for document in documents for row in document["rows"]]
    if len(set(starts)) != len(starts):
        raise SystemExit("the same run appears twice in the files to merge")
    seen: dict[str, int] = {}
    rows = []
    variant_table: dict[str, Any] = {}
    for document in documents:
        variant_table.update(document["settings"]["variant_table"])
        for row in document["rows"]:
            seen[row["variant"]] = seen.get(row["variant"], 0) + 1
            rows.append({**row, "repeat": seen[row["variant"]]})
    settings = {
        **base["settings"],
        "variants": list(seen),
        "variant_table": variant_table,
        "repeats": max(seen.values()),
        "repeats_by_variant": seen,
        "merged_from": names,
        "run_dates": [document.get("date") for document in documents],
    }
    return {
        "settings": settings,
        "scenario": base["scenario"],
        "backend": base["backend"],
        "rows": rows,
        "variants": aggregate(rows),
        "comparison": compare_runs(rows, base["settings"]["tolerances"]),
    }


def write(results: dict[str, Any], args: argparse.Namespace, private_dir: Path) -> Path:
    """Write the results: complete for a synthetic scenario; otherwise the complete file goes to
    ``private_dir`` and ``bench/results`` gets :func:`publishable`."""
    if is_private(results):
        # The complete document is never written inside the repository, not even for a moment.
        private_dir = private_dir.resolve()
        if common.ROOT in (private_dir, *private_dir.parents):
            raise SystemExit(f"{private_dir} is inside the repository: give --private-dir a directory outside it")
        private_dir.mkdir(parents=True, exist_ok=True)
        complete = common.write_results("replay_worker", args.quick, results, directory=private_dir)
        print(f"wrote {complete} (complete, private)")
        results = publishable(results)
    path = common.write_results("replay_worker", args.quick, results)
    if args.quick:
        kept = args.scratch / "results" / path.name
        kept.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(path), kept)
        path = kept
    return path


def add_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--backend", type=Path, default=os.environ.get("OPENBYD_REPLAY_BACKEND"), help="the backend directory (backend/ of the BYD Connect checkout); env OPENBYD_REPLAY_BACKEND")
    parser.add_argument("--backend-python", type=Path, default=os.environ.get("OPENBYD_REPLAY_BACKEND_PYTHON") or sys.executable, help="the Python with the backend installed (default: this one); env OPENBYD_REPLAY_BACKEND_PYTHON")
    parser.add_argument("--scenario", type=Path, default=DEFAULT_SCENARIO, help="scenario file (default bench/replay/samples/demo.jsonl)")
    parser.add_argument("--variants", nargs="+", help=f"variants to run (default all: {' '.join(VARIANTS)}; quick: {' '.join(QUICK_VARIANTS)})")
    parser.add_argument("--repeats", type=int, default=1, help="runs per variant, interleaved (default 1)")
    parser.add_argument("--minutes", type=float, help="measured wall time per run (default 15; quick 3)")
    parser.add_argument("--start-at", type=int, default=0, help="scenario second the replay starts at")
    parser.add_argument("--speed", type=float, default=1.0, help="replay speed; only 1 is supported with the real worker")
    parser.add_argument("--delay-ms", type=int, help="artificial delay of each replay-server reply")
    parser.add_argument("--corpus", type=Path, help="reply templates for the replay server (default: parity/corpus)")
    parser.add_argument("--private-dir", type=Path, help="where the complete results of a scenario that is not synthetic are kept (default: results/ next to the scenario); bench/results gets the aggregate figures only")
    parser.add_argument("--merge", type=Path, nargs="+", metavar="RESULTS", help="combine results files of separate runs of the same scenario and settings into one, as repeats")
    parser.add_argument("--local-store", action="store_true", help="SQLite and the in-memory store instead of Docker Postgres and Redis")
    parser.add_argument("--start-when-claimed", action="store_true", help="start the replay only once the worker holds a slot for every account")
    parser.add_argument("--claim-timeout", type=float, default=3600, help="seconds to wait for --start-when-claimed")
    parser.add_argument("--scrape-seconds", type=float, default=5.0, help="seconds between /metrics scrapes")
    parser.add_argument("--poll-sample", type=int, default=2000, help="stored polls checked against the scenario")
    parser.add_argument("--scratch", type=Path, default=Path(os.environ.get("OPENBYD_REPLAY_SCRATCH", common.SCRATCH / "replay")), help="run directories, logs and quick results; env OPENBYD_REPLAY_SCRATCH")
    parser.add_argument("--jobs", type=int, default=8, help="cargo build jobs")
    parser.add_argument("--replay-server", type=Path, help="a replay server other than the built example (same command line); a .py file runs under this Python")
    parser.add_argument("--skip-build", action="store_true", help="do not build replay_server first")
    parser.add_argument("--quick", action="store_true", help="smoke run: 2 variants, 3 minutes, results not kept in bench/results")


def check(results: dict[str, Any]) -> tuple[list[str], list[str]]:
    """``(problems, warnings)``: run problems, and detections that differ between runs."""
    problems, warnings = [], []
    for row in results["rows"]:
        problems += [f"{row['variant']}#{row['repeat']}: {p}" for p in row["problems"]]
        if row["guard"].get("blocked_total"):
            warnings.append(f"{row['variant']}#{row['repeat']}: the guard blocked {row['guard']['blocked']}")
        rate = row["roundtrip"]["match_rate"]
        if rate is not None and rate < 1:
            fields = sorted({m["field"] for m in row["roundtrip"]["mismatches"]})
            detail = f"fields {fields}" if "scenario" in results and is_private(results) else row["roundtrip"]["mismatches"][:3]
            warnings.append(f"{row['variant']}#{row['repeat']}: replay round trip matched {rate:.1%}: {detail}")
    for entry in results["comparison"]:
        if not entry["equal"]:
            warnings.append(f"detections of {entry['other']} differ from {entry['reference']}: {len(entry['differences'])} difference(s)")
    return problems, warnings


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    add_arguments(parser)
    parser.add_argument("--extract", help=argparse.SUPPRESS)  # internal: runs in the backend's Python
    args = parser.parse_args(argv)
    if args.extract:
        options = json.loads(args.extract)
        print("EXTRACT " + json.dumps(asyncio.run(extract(options["database_url"], options["poll_sample"], options["seed"]))), flush=True)
        return 0
    if args.merge:
        documents = [json.loads(path.read_text(encoding="utf-8")) for path in args.merge]
        results = merge_results(documents, [path.name for path in args.merge])
        path = write(results, args, args.private_dir or args.merge[0].resolve().parent)
        print(f"wrote {path}")
        problems, warnings = check(results)
        for warning in warnings:
            print("WARNING:", warning)
        for problem in problems:
            print("PROBLEM:", problem)
        return 1 if problems else 0
    if args.backend is None:
        raise SystemExit("--backend (or OPENBYD_REPLAY_BACKEND) is required: the backend directory of the BYD Connect checkout")
    args.backend = args.backend.resolve()
    if not (args.backend / "app" / "mqtt_worker.py").exists():
        raise SystemExit(f"{args.backend} is not the backend directory (no app/mqtt_worker.py)")
    if args.speed != 1:
        raise SystemExit("--speed other than 1 is not supported: the worker's clocks run in wall time (see bench/replay/launcher.py)")
    args.minutes = args.minutes if args.minutes is not None else (3.0 if args.quick else 15.0)
    args.scratch.mkdir(parents=True, exist_ok=True)
    results = run(args)
    path = write(results, args, args.private_dir or args.scenario.resolve().parent / "results")
    print(f"wrote {path}")
    problems, warnings = check(results)
    for warning in warnings:
        print("WARNING:", warning)
    for problem in problems:
        print("PROBLEM:", problem)
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
