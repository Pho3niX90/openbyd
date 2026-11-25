"""Compare one car's own readings with what BYD's cloud said about the same car at the same moments.

Reads a pairs file written by ``export_pairs.py`` (a ``meta`` line, ``car`` lines, ``cloud``
lines, ``raw`` lines; see bench/mapping/README.md) and writes ``results.json`` (every number) and
``report.md`` (tables) to an output directory::

    python -m bench.mapping.compare <pairs.jsonl> --out-dir <dir>

The car is the ground truth. The tool builds a car-state timeline in which a reading
holds only while it is fresh, pairs every cloud value with the car's value at the cloud
row's ``t``, and reports aggregates only: counts, quantiles, means, confusion matrices,
code tables and fitted slopes. The decode section does the same for every raw BYD field
(contingency tables against the car's enum readings, fits against its numbers). It never
writes or prints an input row, the ``t`` of an individual event, or any per-event list
(not even derived lags). Standard library only (and export_pairs' line check for raw lines),
deterministic.
"""

from __future__ import annotations

import argparse
import bisect
import json
import math
import random
import re
import sys
import zlib
from collections import Counter
from collections.abc import Iterable, Iterator
from pathlib import Path
from typing import Any

from bench.mapping import export_pairs

VERSION = 1

# --- constants (every threshold the analysis uses) -----------------------------------------------

# How long a car row stays true after its own time, by the row's cadence. Rows come about
# every 2 s while moving, 60 s while on-and-stationary or charging, 300 s while parked
# (event). After this the timeline is "unknown", not "unchanged". Rows without a cadence
# come from the fallback path, about once a minute. Measured on the first real export:
# moving rows are up to ~6.5 s apart (p99; rows arrive in sends of 2-3), parked event rows
# often ~600 s apart (one beat missed, p90 588 s), hence 8 s and 660 s.
FRESH_S: dict[str | None, float] = {"moving": 8.0, "stationary": 90.0, "charging": 90.0, "event": 660.0, None: 90.0}
FULL_CADENCES = frozenset(("moving", "stationary", "charging"))
# Keys an event (or cadence-less) row never carries. At a moment whose newest row is such a
# row, these come from the newest full row instead, if that row is still fresh.
FULL_ONLY = frozenset((
    "speed_kph", "odometer_km", "average_speed_kph", "outside_temp_c", "remaining_kwh", "lifetime_kwh",
    "tyre_kpa", "tyre_temp_c", "charging_power_kw", "charge_remaining_min", "charged_kwh", "motor_power_kw",
))
# A car row identical to the previous one apart from ``t`` and at most this much later is a
# resend. Shorter than the ~2 s moving cadence, so a steady drive is not mistaken for resends.
DUP_WINDOW_S = 1.5
DRIVING_MIN_KPH = 1.0
# Speed: lag candidates for the best-fit lag; a candidate counts only with at least this
# share of the largest per-candidate sample count.
SPEED_LAGS_S = tuple(range(0, 301, 5))
# Car speed against GPS speed: only rows whose fix is at most this old and whose speeds are at least this.
FIX_MAX_AGE_S = 2.0
SPEED_CMP_MIN_KPH = 5.0
# Value lags (SoC, odometer): how far back or ahead a matching car value is searched, and the
# candidates of the best-fit constant lag (the lag at which the most cloud values equal the car's).
VALUE_LAG_WINDOW_S = 3600.0
VALUE_LAGS_S = tuple(range(0, 1801, 10))
# The best-fit lag is "determined" only when the best candidates form one block at most this wide.
VALUE_LAG_MAX_TIED_SPAN_S = 300
# Charge power agreement.
POWER_ABS_TOL_KW = 0.2
POWER_REL_TOL = 0.10
# Position: implied lag (distance / speed) only at or above this car speed.
GPS_LAG_MIN_KPH = 10.0
# Energy over the last 50 km: the car reading 50 km back must be within this many km of exactly 50 km back.
ENERGY_WINDOW_KM = 50.0
ENERGY_ODO_TOL_KM = 1.0
# energy_distribution shares "sum to 100" when within this many points.
ENERGY_SHARE_TOL = 2.0
# Samples at least this far apart are separate clusters (independent units for awake-poll statistics).
CLUSTER_GAP_S = 600.0
# Moving rows separated by more than this start a new drive.
DRIVE_GAP_S = 600.0
SECONDS_PER_DAY = 86400

STATES = ("driving", "on_stationary", "charging", "plugged_idle", "parked_off", "parked_inferred", "unknown")
# Inferred parked gaps: the plug codes that must match on both sides, and the keys taken as constant.
PLUG_KEYS = ("charging_gun_raw", "charger_state_raw", "charging_type_raw")
INFERRED_KEYS = ("soc_pct", "power_level", "gear", "lock") + PLUG_KEYS
POWER_LEVELS = ("off", "acc", "on", "ok", "unknown")
CHANNELS = ("mqtt:vehicleInfo", "mqtt:smartCharge", "mqtt:energyConsumption", "poll:ok")
CAR_KEYS = (
    "cadence", "speed_kph", "soc_pct", "twelve_volt", "odometer_km", "power_level", "gear", "doors", "lock",
    "alarm", "alarm_raw", "twelve_volt_level", "average_speed_kph", "outside_temp_c", "remaining_kwh",
    "lifetime_kwh", "tyre_kpa", "tyre_temp_c", "charging_state_raw", "charging_gun_raw", "charging_power_kw",
    "charging_type_raw", "charger_state_raw", "charge_remaining_min", "charged_kwh", "motor_power_kw",
    "has_fix", "fix_accuracy_m", "fix_age_s", "fix_speed_kph",
)
DETAIL = re.compile(r"[0-9]{1,5}|[a-z_]{1,24}")
ON_LEVELS = frozenset(("acc", "on", "ok"))
CAR_EVENTS = ("power_on", "power_off", "start_moving", "stop_moving", "door_change", "plug_change")
# Periods around the car-app device registration (meta.first_seen_t) and the first car row.
PERIODS = ("before_registration", "registered_no_rows", "car_rows")
HALF_DAY_S = 43200.0
TRANSITION_CATEGORIES = ("no_rows", "only_asleep", "any_awake", "no_awake_flag")
POWER_ON_LABELS = frozenset(("acc", "on", "ok"))
SPEED_BANDS = ((0.0, 20.0, "<20"), (20.0, 60.0, "20-60"), (60.0, 100.0, "60-100"), (100.0, math.inf, ">100"))
AWAKE_LABELS = ("awake", "asleep", "no_flag")
POWER_RESOLUTION_KW = 0.1
# How long after a car-side change a cloud row still counts as the response to it.
EVENT_RESPONSE_WINDOW_S = 3600.0
# A vehicleInfo push older than this is not compared with a poll for the contamination hint.
CONTAMINATION_PUSH_MAX_AGE_S = 3600.0
# Energy windows: a car endpoint at most this old counts as fresh for the sensitivity split.
ENDPOINT_FRESH_S = 600.0
# An odometer reading below the running maximum is out of order when one of this many next readings recovers.
ODO_RECOVERY_READINGS = 3
# What pyBYD's vehicle_on says about BYD's powerGear when the car is not moving and not asleep.
POWER_GEAR = {"true": "powerGear 3 (ON)", "false": "powerGear 1 (OFF) or unknown + vehicleState 0/2",
              "null": "neither known", "absent": "vehicle_on absent"}
# Decode (raw BYD fields against the car). An integer, enum or boolean field (by its table kind) with at most
# this many distinct values is enum-like; a float field, or an integer one with more values, is numeric.
ENUM_MAX_VALUES = 20
# Independent evidence. A raw value supports a mapping only when it was seen in at least this many time
# clusters (pairs CLUSTER_GAP_S or more apart) and runs of its own; each car value needs this many episodes
# (for a fit: car-state episodes); a finding needs this many test clusters.
MIN_EPISODES = 3
MIN_TEST_CLUSTERS = 3
# Train gates, applied to unrounded values on the train half: at least DECODE_MIN_N pairs; for an enum field
# the bias-corrected uncertainty coefficient U(car | field) and Goodman-Kruskal lambda (the share of the
# errors of always guessing the car's modal value that the field removes); for a numeric field |r| of the
# first differences within clusters (so two unrelated trends do not correlate).
DECODE_MIN_N = 10
DECODE_MIN_U = 0.3
DECODE_MIN_LAMBDA = 0.3
DECODE_MIN_R = 0.8
# Held-out gates on the test half: an enum mapping's balanced accuracy (mean recall over the car values)
# must beat the majority-class predictor's (1 / number of car values) by HELD_OUT_MIN_GAIN, covering at least
# HELD_OUT_MIN_COVERAGE of the test pairs; a fit, with the train slope and offset, must reach r >=
# HELD_OUT_MIN_R and a smaller mean absolute error than the train mean.
HELD_OUT_MIN_GAIN = 0.2
HELD_OUT_MIN_COVERAGE = 0.5
HELD_OUT_MIN_R = 0.8
# Significance on the test half: PERMUTATIONS shifts (seeded per field and reading); Benjamini-Hochberg at FDR_Q
# over every examined hypothesis.
PERMUTATIONS = 2000
FDR_Q = 0.05
# A reply carrying its own sample time is paired at that time, and not at all when the sample time is more
# than STALE_SAMPLE_S from the reply's receive time, or marked stale by the exporter (BYD's 0 or no time).
STALE_SAMPLE_S = 300.0
SAMPLE_TIME_PATHS = ("time", "timestamp", "updateTime", "data.gpsTimeStamp", "gpsTimeStamp")
# pyBYD's numeric "no reading" values, never fitted: -999 (rate idle), -129 (temperatures), and -1 on a path
# whose table range starts at -1.
FIT_SENTINELS = (-999.0, -129.0)
# Secondary: the strongest fields re-evaluated with the car reading taken this much earlier, on one cohort.
DECODE_LAGS_S = (0, 30, 60, 120)
LAG_FIELDS = 5
DOOR_KEYS = ("fl", "fr", "rl", "rr", "hood", "trunk")
ENUM_READINGS = ("doors_any_open", *(f"doors.{d}" for d in DOOR_KEYS), "lock", "gear", "power_level", "cadence",
                 "moving", "charging_gun_raw", "charger_state_raw", "charging_type_raw", "car_state")
PLAIN_NUMBERS = ("speed_kph", "motor_power_kw", "charging_power_kw", "soc_pct", "odometer_km", "outside_temp_c",
                 "remaining_kwh", "lifetime_kwh", "twelve_volt", "charge_remaining_min", "charged_kwh")
# Tyre corner labels are unconfirmed on the car, so each raw tyre field is fitted against every slot and the mean.
TYRE_SLOTS = ("fl", "fr", "rl", "rr", "mean")
NUMBER_READINGS = PLAIN_NUMBERS + tuple(f"{key}[{slot}]" for key in ("tyre_kpa", "tyre_temp_c") for slot in TYRE_SLOTS)


class _Mark:
    """A sentinel with a readable name."""

    def __init__(self, name: str) -> None:
        self.name = name

    def __repr__(self) -> str:
        return self.name


UNKNOWN = _Mark("UNKNOWN")  # no fresh car reading
ABSENT = _Mark("ABSENT")  # a fresh car row that does not carry the key (or a cloud row without it)


def isnum(value: Any) -> bool:
    return type(value) in (int, float) and math.isfinite(value)


def r3(value: float) -> float:
    out = round(float(value), 3)
    return 0.0 if out == 0 else out


def day(t: float) -> int:
    return int(t // SECONDS_PER_DAY)


def label(value: Any) -> str:
    """The table label of a cloud or car value: true/false/null/absent, a code, or a string."""
    if value is ABSENT:
        return "absent"
    if value is UNKNOWN:
        return "unknown"
    if value is None:
        return "null"
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isnum(value):
        return str(int(value)) if float(value).is_integer() else str(r3(value))
    return str(value)


# --- aggregates ------------------------------------------------------------------------------------


def quantile(ordered: list[float], p: float) -> float:
    """Linear interpolation between closest ranks (numpy's default)."""
    pos = p * (len(ordered) - 1)
    lo = math.floor(pos)
    hi = min(lo + 1, len(ordered) - 1)
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (pos - lo)


QUANTILES = (("min", 0.0), ("p5", 0.05), ("p25", 0.25), ("median", 0.5), ("p75", 0.75), ("p95", 0.95), ("max", 1.0))


def _states(counter: Counter[str]) -> dict[str, int]:
    return {state: counter[state] for state in STATES if counter[state]}


# Set by Analysis: maps a time to the index of the drive it belongs to (driving samples only).
DRIVE_OF: list[Any] = [None]


def _units(samples: list[tuple[float, str]]) -> dict[str, Any]:
    """Independent units behind a set of (t, car state) samples: distinct days, clusters (samples
    separated by >= CLUSTER_GAP_S start a new cluster) and, for driving samples, distinct drives."""
    times = sorted(t for t, _ in samples)
    clusters = sum(1 for i, t in enumerate(times) if i == 0 or t - times[i - 1] >= CLUSTER_GAP_S)
    out: dict[str, Any] = {"days": len({day(t) for t in times}), "clusters": clusters}
    driving = [t for t, st in samples if st == "driving"]
    if driving and DRIVE_OF[0] is not None:
        out["drives"] = len({DRIVE_OF[0](t) for t in driving})
    return out


class Dist:
    """A distribution of paired values, each tagged with its time and the car state."""

    def __init__(self) -> None:
        self.items: list[tuple[float, float, str]] = []

    def add(self, value: float, t: float, state: str) -> None:
        self.items.append((float(value), float(t), state))

    def __len__(self) -> int:
        return len(self.items)

    def values(self) -> list[float]:
        return [v for v, _, _ in self.items]

    def summary(self, by_state: bool = True) -> dict[str, Any]:
        out: dict[str, Any] = {"n": len(self.items), **_units([(t, s) for _, t, s in self.items]),
                               "states": _states(Counter(s for _, _, s in self.items))}
        if not self.items:
            out["note"] = "no samples"
            return out
        values = sorted(v for v, _, _ in self.items)
        for name, p in QUANTILES:
            out[name] = r3(quantile(values, p))
        out["mean"] = r3(sum(values) / len(values))
        if by_state and len({s for _, _, s in self.items}) > 1:
            groups: dict[str, list[float]] = {}
            for v, _, s in self.items:
                groups.setdefault(s, []).append(v)
            out["by_state"] = {}
            for state in STATES:
                if state in groups:
                    ordered = sorted(groups[state])
                    out["by_state"][state] = {"n": len(ordered), "median": r3(quantile(ordered, 0.5)),
                                              "p5": r3(quantile(ordered, 0.05)), "p95": r3(quantile(ordered, 0.95)),
                                              "mean": r3(sum(ordered) / len(ordered))}
        return out


class Tally:
    """A confusion matrix or code table: counts per (row label, column label)."""

    def __init__(self) -> None:
        self.cells: Counter[tuple[str, str]] = Counter()
        self.samples: list[tuple[float, str]] = []

    def add(self, row: str, col: str, t: float, state: str) -> None:
        self.cells[(row, col)] += 1
        self.samples.append((float(t), state))

    def count(self, rows: Iterable[str] | None = None, cols: Iterable[str] | None = None) -> int:
        rs = None if rows is None else set(rows)
        cs = None if cols is None else set(cols)
        return sum(n for (r, c), n in self.cells.items() if (rs is None or r in rs) and (cs is None or c in cs))

    def summary(self) -> dict[str, Any]:
        rows: dict[str, dict[str, int]] = {}
        for (r, c), n in sorted(self.cells.items()):
            rows.setdefault(r, {})[c] = n
        out: dict[str, Any] = {"n": sum(self.cells.values()), **_units(self.samples),
                               "states": _states(Counter(st for _, st in self.samples)), "rows": rows}
        if not self.cells:
            out["note"] = "no samples"
        return out


class Fit:
    """Least squares y = slope * x + offset."""

    def __init__(self) -> None:
        self.points: list[tuple[float, float]] = []
        self.samples: list[tuple[float, str]] = []

    def add(self, x: float, y: float, t: float, state: str) -> None:
        self.points.append((float(x), float(y)))
        self.samples.append((float(t), state))

    def summary(self) -> dict[str, Any]:
        n = len(self.points)
        out: dict[str, Any] = {"n": n, **_units(self.samples),
                               "states": _states(Counter(st for _, st in self.samples))}
        if n < 2:
            out["note"] = "fewer than 2 samples"
            return out
        mx = sum(x for x, _ in self.points) / n
        my = sum(y for _, y in self.points) / n
        sxx = sum((x - mx) ** 2 for x, _ in self.points)
        syy = sum((y - my) ** 2 for _, y in self.points)
        sxy = sum((x - mx) * (y - my) for x, y in self.points)
        if sxx == 0:
            out["note"] = "x does not vary"
            return out
        slope = sxy / sxx
        out["slope"] = r3(slope)
        out["offset"] = r3(my - slope * mx)
        out["r"] = r3(sxy / math.sqrt(sxx * syy)) if syy > 0 else None
        return out


def share(part: int, whole: int) -> float | None:
    return r3(part / whole) if whole else None


# --- reading the file ------------------------------------------------------------------------------


def read_pairs(path: Path) -> tuple[dict[str, Any], list[dict[str, Any]], list[dict[str, Any]], Counter[str],
                                    list[dict[str, Any]]]:
    """Stream the file: the meta line, car rows, cloud rows, counts of skipped lines and raw rows."""
    meta: dict[str, Any] = {}
    car: list[dict[str, Any]] = []
    cloud: list[dict[str, Any]] = []
    raw: list[dict[str, Any]] = []
    skipped: Counter[str] = Counter()
    with path.open("r", encoding="utf-8") as handle:
        for line in handle:
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except ValueError:
                skipped["invalid_json"] += 1
                continue
            if type(row) is not dict:
                skipped["not_an_object"] += 1
                continue
            kind = row.get("type")
            if kind == "meta":
                meta = row
            elif kind in ("car", "cloud"):
                if not isnum(row.get("t")):
                    skipped[f"{kind}_without_t"] += 1
                    continue
                if kind == "car":
                    car.append({k: v for k, v in row.items() if k in CAR_KEYS or k == "t"})
                else:
                    if type(row.get("source")) is not str or type(row.get("kind")) is not str:
                        skipped["cloud_without_source"] += 1
                        continue
                    cloud.append(row)
            elif kind == "raw":
                # The exporter's strict check again: a raw line it refuses is counted and skipped, never used.
                try:
                    export_pairs.check_line(line if line.endswith("\n") else line + "\n")
                except export_pairs.ep._ExportError:
                    skipped["raw_invalid"] += 1
                    continue
                raw.append(row)
            else:
                skipped["unknown_type"] += 1
    return meta, car, cloud, skipped, raw


def dedupe(rows: list[dict[str, Any]]) -> tuple[list[dict[str, Any]], int, int]:
    """Sort car rows by t; drop exact duplicates and resends (same content within DUP_WINDOW_S)."""
    rows = sorted(rows, key=lambda r: r["t"])
    kept: list[dict[str, Any]] = []
    exact = near = 0
    previous_key = None
    previous_t = -math.inf
    for row in rows:
        content = json.dumps({k: v for k, v in row.items() if k != "t"}, sort_keys=True)
        if content == previous_key and row["t"] == previous_t:
            exact += 1
            continue
        if content == previous_key and row["t"] - previous_t <= DUP_WINDOW_S:
            near += 1
            continue
        kept.append(row)
        previous_key, previous_t = content, row["t"]
    return kept, exact, near


def cloud_get(row: dict[str, Any], path: str) -> Any:
    """A cloud value by dotted path: ABSENT when the key is missing, None when it (or its parent) is null."""
    value: Any = row
    for part in path.split("."):
        if value is None:
            return None
        if type(value) is not dict or part not in value:
            return ABSENT
        value = value[part]
    return value


def channel(row: dict[str, Any]) -> str:
    return f"{row['source']}:{row['kind']}"


def awake(row: dict[str, Any]) -> str:
    """awake when the row says asleep false (BYD realtime online), asleep when true, else no_flag."""
    a = row.get("asleep")
    return "awake" if a is False else "asleep" if a is True else "no_flag"


# --- the car timeline ------------------------------------------------------------------------------


class Snapshot:
    __slots__ = ("state", "power", "moving", "plugged", "charging", "cadence")

    def __init__(self, state: str, power: str, moving: bool, plugged: bool | None, charging: bool | None,
                 cadence: str | None = None) -> None:
        self.state, self.power, self.moving, self.plugged, self.charging = state, power, moving, plugged, charging
        self.cadence = cadence


class Car:
    """The car-state timeline: values hold while fresh, then become UNKNOWN."""

    def __init__(self, rows: list[dict[str, Any]]) -> None:
        self.rows = rows
        self.times = [r["t"] for r in rows]
        self.full_idx = [i for i, r in enumerate(rows) if r.get("cadence") in FULL_CADENCES]
        self.full_times = [rows[i]["t"] for i in self.full_idx]
        self.moving_times = [r["t"] for r in rows if r.get("cadence") == "moving"]
        self.odo_idx = [i for i, r in enumerate(rows) if isnum(r.get("odometer_km"))]
        self.odo_times = [rows[i]["t"] for i in self.odo_idx]
        self._snap: dict[float, Snapshot] = {}
        self._runs: dict[str, list[tuple[float, float, Any]]] = {}
        self.inferred = self._infer_gaps()
        self.drive_starts = [t for i, t in enumerate(self.moving_times)
                             if i == 0 or t - self.moving_times[i - 1] > DRIVE_GAP_S]
        self.segment_starts = [t for i, t in enumerate(self.moving_times)
                               if i == 0 or t - self.moving_times[i - 1] > FRESH_S["moving"]]

    def drive_of(self, t: float) -> int:
        """Index of the drive (moving rows less than DRIVE_GAP_S apart) at or before t."""
        return bisect.bisect_right(self.drive_starts, t) - 1

    def segment_of(self, t: float) -> int:
        """Index of the continuous moving segment (rows within the moving freshness) at or before t."""
        return bisect.bisect_right(self.segment_starts, t) - 1

    @staticmethod
    def fresh(row: dict[str, Any]) -> float:
        return FRESH_S.get(row.get("cadence"), FRESH_S[None])

    def _infer_gaps(self) -> dict[int, dict[str, Any]]:
        """Head-unit-asleep gaps across which the parked car provably did not change.

        A gap follows row i when the next row comes after row i's freshness ran out. It is
        inferred "parked and unchanged" when rows i and i+1 both say power off, show the same
        SoC and the same plug codes, and the nearest full readings on both sides show the
        same odometer (consecutive rows, so no moving row lies between them). The value of
        each INFERRED_KEYS key is then taken as constant through the gap.
        """
        out: dict[int, dict[str, Any]] = {}
        rows = self.rows
        for i in range(len(rows) - 1):
            a, b = rows[i], rows[i + 1]
            if b["t"] - a["t"] <= self.fresh(a):
                continue
            if a.get("power_level") != "off" or b.get("power_level") != "off":
                continue
            if not isnum(a.get("soc_pct")) or a.get("soc_pct") != b.get("soc_pct"):
                continue
            if any(a.get(k) != b.get(k) for k in PLUG_KEYS):
                continue
            before = bisect.bisect_right(self.odo_times, a["t"]) - 1
            after = bisect.bisect_left(self.odo_times, b["t"])
            if before < 0 or after >= len(self.odo_idx):
                continue
            odo = rows[self.odo_idx[before]]["odometer_km"]
            if rows[self.odo_idx[after]]["odometer_km"] != odo:
                continue
            values = {k: a[k] for k in INFERRED_KEYS if k in a}
            values["odometer_km"] = odo
            out[i] = values
        return out

    def locate(self, t: float) -> tuple[int, bool] | None:
        """The row whose reading holds at ``t`` and whether it holds only by inference."""
        i = bisect.bisect_right(self.times, t) - 1
        if i < 0:
            return None
        if t - self.rows[i]["t"] <= self.fresh(self.rows[i]):
            return i, False
        if i in self.inferred:
            return i, True
        return None

    def latest(self, t: float) -> int | None:
        loc = self.locate(t)
        return None if loc is None or loc[1] else loc[0]

    def value(self, key: str, t: float, interpolate: bool = False) -> Any:
        loc = self.locate(t)
        if loc is None:
            return UNKNOWN
        i, inferred = loc
        if inferred:
            return self.inferred[i].get(key, UNKNOWN)
        row = self.rows[i]
        if key in row:
            v = row[key]
            if interpolate and isnum(v) and row.get("cadence") == "moving" and t > row["t"] and i + 1 < len(self.rows):
                nxt = self.rows[i + 1]
                gap = nxt["t"] - row["t"]
                if nxt.get("cadence") == "moving" and isnum(nxt.get(key)) and 0 < gap <= FRESH_S["moving"]:
                    return v + (nxt[key] - v) * (t - row["t"]) / gap
            return v
        if key in FULL_ONLY and row.get("cadence") not in FULL_CADENCES:
            j = bisect.bisect_right(self.full_times, t) - 1
            if j < 0:
                return UNKNOWN
            full = self.rows[self.full_idx[j]]
            if t - full["t"] > self.fresh(full):
                return UNKNOWN
            return full.get(key, ABSENT)
        return ABSENT

    def snapshot(self, t: float) -> Snapshot:
        cached = self._snap.get(t)
        if cached is not None:
            return cached
        loc = self.locate(t)
        if loc is None:
            snap = Snapshot("unknown", "unknown", False, None, None)
        elif loc[1]:
            values = self.inferred[loc[0]]
            gun, charger = values.get("charging_gun_raw"), values.get("charger_state_raw")
            plugged_i = True if gun == 2 or charger == 1 else False if gun == 1 or charger == 0 else None
            ctype_i = values.get("charging_type_raw")
            if ctype_i == 2:
                charging_i: bool | None = True
            elif plugged_i is False or ctype_i == 1:
                charging_i = False
            else:
                charging_i = None
            snap = Snapshot("parked_inferred", "off", False, plugged_i, charging_i, "inferred")
        else:
            i = loc[0]
            row = self.rows[i]
            cadence = row.get("cadence")
            speed = self.value("speed_kph", t)
            power = self.value("power_level", t)
            power_label = power if power in ("off", "acc", "on", "ok") else "unknown"
            gun = self.value("charging_gun_raw", t)
            charger = self.value("charger_state_raw", t)
            ctype = self.value("charging_type_raw", t)
            cpower = self.value("charging_power_kw", t)
            if gun == 2 or charger == 1:
                plugged: bool | None = True
            elif gun == 1 or charger == 0:
                plugged = False
            else:
                plugged = None
            # The raw codes come with every row; charging_power_kw may come from an older full
            # row (FULL_ONLY), so the codes decide first.
            if ctype == 2:
                charging: bool | None = True
            elif plugged is False or ctype == 1:
                charging = False
            elif isnum(cpower):
                charging = cpower > 0
            else:
                charging = None
            moving = cadence == "moving" and isnum(speed) and speed >= DRIVING_MIN_KPH
            if moving:
                state = "driving"
            elif charging:
                state = "charging"
            elif plugged:
                state = "plugged_idle"
            elif power_label in ON_LEVELS or cadence in ("moving", "stationary"):
                state = "on_stationary"
            else:
                state = "parked_off"
            snap = Snapshot(state, power_label, moving, plugged, charging, cadence)
        self._snap[t] = snap
        return snap

    def state(self, t: float) -> str:
        return self.snapshot(t).state

    def last_known(self, key: str, t: float) -> tuple[float, Any] | None:
        """The newest reading of ``key`` at or before ``t``, however old (only for odometer-like keys)."""
        if key == "odometer_km":
            j = bisect.bisect_right(self.odo_times, t) - 1
            return None if j < 0 else (self.odo_times[j], self.rows[self.odo_idx[j]]["odometer_km"])
        for i in range(bisect.bisect_right(self.times, t) - 1, -1, -1):
            if isnum(self.rows[i].get(key)):
                return self.rows[i]["t"], self.rows[i][key]
        return None

    def moved_between(self, start: float, end: float) -> bool:
        lo = bisect.bisect_right(self.moving_times, start)
        hi = bisect.bisect_right(self.moving_times, end)
        return hi > lo

    def runs(self, key: str) -> list[tuple[float, float, Any]]:
        """Runs of equal values of ``key``: (first row t, last row t, value)."""
        if key not in self._runs:
            runs: list[tuple[float, float, Any]] = []
            for row in self.rows:
                v = row.get(key)
                if not isnum(v):
                    continue
                if runs and runs[-1][2] == v:
                    runs[-1] = (runs[-1][0], row["t"], v)
                else:
                    runs.append((row["t"], row["t"], v))
            self._runs[key] = runs
        return self._runs[key]

    def value_lag(self, key: str, value: float, t: float) -> tuple[float, float] | None:
        """How stale a cloud value is against the car's own series of ``key``.

        Returns (lag, upper) in seconds: 0, 0 when the car currently shows the value; else
        ``t`` minus the moment the car stopped showing it (the first row of the next run:
        the least the cloud can be behind) and ``t`` minus the last row that showed it (the
        most); negative (both equal) when the car shows the value only later. The nearer
        match wins, within VALUE_LAG_WINDOW_S. None when the car never showed it.
        """
        runs = self.runs(key)
        starts = [r[0] for r in runs]
        k = bisect.bisect_right(starts, t) - 1
        if k >= 0 and runs[k][2] == value:
            return 0.0, 0.0
        back = ahead = None
        j = k - 1
        while j >= 0 and t - runs[j][1] <= VALUE_LAG_WINDOW_S:
            if runs[j][2] == value:
                back = (t - runs[j + 1][0], t - runs[j][1])
                break
            j -= 1
        j = k + 1
        while j < len(runs) and runs[j][0] - t <= VALUE_LAG_WINDOW_S:
            if runs[j][2] == value:
                ahead = (t - runs[j][0], t - runs[j][0])
                break
            j += 1
        if back is None:
            return ahead
        if ahead is None or abs(back[0]) <= abs(ahead[0]):
            return back
        return ahead

    def agreement_lag(self, key: str, samples: list[tuple[float, float]]) -> dict[str, Any]:
        """The constant lag at which the most cloud values equal the car's fresh value ``lag`` seconds earlier.

        Every candidate is scored on one common cohort: the samples with a fresh car value at
        every candidate lag (samples missing a value at any candidate are excluded and counted),
        so a candidate cannot look better by dropping disagreeing samples. When lag 0 agrees as
        well as the best candidate the best lag is 0 (no evidence of lag); otherwise it is the
        middle of the first contiguous block of tied candidates. ``determined`` needs one block
        no wider than VALUE_LAG_MAX_TIED_SPAN_S that does not start at 0 or is that narrow.
        """
        cohort: list[tuple[float, float, list[Any]]] = []
        for t, v in samples:
            values = [self.value(key, t - lag) for lag in VALUE_LAGS_S]
            if all(isnum(c) for c in values):
                cohort.append((t, v, values))
        n = len(cohort)
        curve = [{"lag_s": lag, "matched": sum(1 for _, v, vals in cohort if vals[k] == v)}
                 for k, lag in enumerate(VALUE_LAGS_S)]
        out: dict[str, Any] = {"n_samples": len(samples), "cohort_n": n, "excluded_missing_car_value": len(samples) - n,
                               "best_lag_s": None, "agreement_at_best": None, "tied_lags_s": None, "tied_blocks": 0,
                               "determined": False, "agreement_at_lag0": share(curve[0]["matched"], n),
                               "matched_by_lag": curve if n else []}
        if n:
            top = max(c["matched"] for c in curve)
            tied = [c["lag_s"] for c in curve if c["matched"] == top]
            step = VALUE_LAGS_S[1] - VALUE_LAGS_S[0]
            blocks = [[tied[0]]]
            for lag in tied[1:]:
                if lag - blocks[-1][-1] == step:
                    blocks[-1].append(lag)
                else:
                    blocks.append([lag])
            first = blocks[0]
            best = 0 if first[0] == 0 else first[len(first) // 2]
            out.update(best_lag_s=best, agreement_at_best=share(top, n), tied_lags_s=[first[0], first[-1]],
                       tied_blocks=len(blocks),
                       determined=len(blocks) == 1 and first[-1] - first[0] <= VALUE_LAG_MAX_TIED_SPAN_S)
        return out

    def segments(self) -> Iterator[tuple[float, float, str]]:
        """Fresh and inferred time segments (start, end, state); the rest of the window is unknown."""
        for i, row in enumerate(self.rows):
            end = row["t"] + self.fresh(row)
            if i + 1 < len(self.rows):
                end = min(end, self.rows[i + 1]["t"])
            if end > row["t"]:
                yield row["t"], end, self.state(row["t"])
            if i in self.inferred:
                yield end, self.rows[i + 1]["t"], "parked_inferred"


def state_hours(car: Car, lo: float, hi: float) -> dict[str, float]:
    """Hours per car state within [lo, hi]; uncovered time is unknown."""
    hours: Counter[str] = Counter()
    covered = 0.0
    for start, end, state in car.segments():
        a, b = max(start, lo), min(end, hi)
        if b > a:
            hours[state] += b - a
            covered += b - a
    hours["unknown"] += max(0.0, (hi - lo) - covered)
    return {state: r3(hours[state] / 3600) for state in STATES}


# --- the analysis ----------------------------------------------------------------------------------


class Analysis:
    def __init__(self, meta: dict[str, Any], car_rows: list[dict[str, Any]], cloud: list[dict[str, Any]],
                 skipped: Counter[str], raw: list[dict[str, Any]] | None = None) -> None:
        self.meta = meta
        self.raw = sorted(raw or [], key=lambda r: r["t"])
        self.raw_car_rows = len(car_rows)
        rows, self.exact_dups, self.near_dups = dedupe(car_rows)
        self.car = Car(rows)
        self.cloud = sorted(cloud, key=lambda r: r["t"])
        self.skipped = skipped
        self._events: dict[str, list[float]] | None = None
        DRIVE_OF[0] = self.car.drive_of
        first_seen = meta.get("first_seen_t")
        self.first_car = self.car.times[0] if self.car.times else math.inf
        # Registration of the car-app device; without meta.first_seen_t the first car row stands in.
        self.registration = float(first_seen) if isnum(first_seen) else self.first_car
        self.registration_source = "meta.first_seen_t" if isnum(first_seen) else "first car row"
        self.by_channel: dict[str, list[dict[str, Any]]] = {}
        for row in self.cloud:
            self.by_channel.setdefault(channel(row), []).append(row)

    def rows_of(self, name: str) -> list[dict[str, Any]]:
        return self.by_channel.get(name, [])

    # 1 -------------------------------------------------------------------------------------------
    def inventory(self) -> dict[str, Any]:
        car, rows = self.car, self.car.rows
        out: dict[str, Any] = {}
        car_days = {day(r["t"]) for r in rows}
        cloud_days = {day(r["t"]) for r in self.cloud}
        out["days"] = {"car": len(car_days), "cloud": len(cloud_days), "both": len(car_days & cloud_days)}
        out["car_rows"] = {"read": self.raw_car_rows, "kept": len(rows), "exact_duplicates_dropped": self.exact_dups,
                           "resends_dropped": self.near_dups,
                           "by_cadence": {label(k): n for k, n in sorted(Counter(
                               r.get("cadence", "none") for r in rows).items())}}
        out["skipped_lines"] = dict(sorted(self.skipped.items()))
        details: Counter[str] = Counter()
        for row in self.cloud:
            d = row.get("detail", ABSENT)
            if d is ABSENT:
                d_label = "-"
            elif type(d) is str and DETAIL.fullmatch(d):
                d_label = d
            elif isnum(d) and float(d).is_integer() and 0 <= d < 100000:
                d_label = str(int(d))
            else:
                d_label = "other"
            details[f"{channel(row)}:{d_label}"] += 1
        out["cloud_rows"] = {"total": len(self.cloud), "by_source_kind_detail": dict(sorted(details.items()))}
        car_keys: Counter[str] = Counter()
        for r in rows:
            for k, v in r.items():
                if k == "t":
                    continue
                if type(v) is dict:
                    for sub in v:
                        car_keys[f"{k}.{sub}"] += 1
                car_keys[k] += 1
        out["car_keys"] = dict(sorted(car_keys.items()))
        cloud_keys: dict[str, dict[str, dict[str, int]]] = {}
        for row in self.cloud:
            per = cloud_keys.setdefault(channel(row), {})
            for k, v in row.items():
                if k in ("type", "t", "source", "kind"):
                    continue
                items = [(k, v)]
                if type(v) is dict:
                    items += [(f"{k}.{sub}", sv) for sub, sv in v.items()]
                for name, val in items:
                    cell = per.setdefault(name, {"value": 0, "null": 0})
                    cell["null" if val is None else "value"] += 1
        out["cloud_keys"] = {ch: dict(sorted(v.items())) for ch, v in sorted(cloud_keys.items())}
        # Freshness of the car timeline.
        if rows:
            lo, hi = rows[0]["t"], rows[-1]["t"]
            covered = sum(b - a for a, b, _ in car.segments())
            out["car_fresh_share"] = share_f(covered, hi - lo + car.fresh(rows[-1]))
            out["car_hours_by_state"] = state_hours(car, lo, hi + car.fresh(rows[-1]))
        else:
            out["car_fresh_share"] = None
            out["car_hours_by_state"] = {}
        if self.cloud:
            lo, hi = self.cloud[0]["t"], self.cloud[-1]["t"]
            hours = state_hours(car, lo, hi)
            out["cloud_window_hours_by_car_state"] = hours
            total = sum(hours.values())
            out["car_fresh_share_in_cloud_window"] = share_f(total - hours.get("unknown", 0.0), total)
        else:
            hours = {}
            out["cloud_window_hours_by_car_state"] = {}
            out["car_fresh_share_in_cloud_window"] = None
        groups = {
            "poll:ok": lambda r: channel(r) == "poll:ok",
            "poll:attempt (ok+error)": lambda r: r["source"] == "poll" and r["kind"] in ("ok", "error"),
            "mqtt:any": lambda r: r["source"] == "mqtt",
            "mqtt:vehicleInfo": lambda r: channel(r) == "mqtt:vehicleInfo",
            "mqtt:smartCharge": lambda r: channel(r) == "mqtt:smartCharge",
        }
        intervals: dict[str, Any] = {}
        rates: dict[str, Any] = {}
        for name, test in groups.items():
            times = [r["t"] for r in self.cloud if test(r)]
            dist = Dist()
            for a, b in zip(times, times[1:]):
                dist.add(b - a, b, car.state(b))
            intervals[name] = dist.summary()
            counts = Counter(car.state(t) for t in times)
            rates[name] = {state: {"rows": counts[state], "hours": hours.get(state, 0.0),
                                   "per_hour": r3(counts[state] / hours[state]) if hours.get(state) else None}
                           for state in STATES}
        out["cloud_interval_s_by_car_state"] = intervals
        out["cloud_rows_per_car_state_hour"] = rates
        return out

    # 2 -------------------------------------------------------------------------------------------
    def soc(self) -> dict[str, Any]:
        out: dict[str, Any] = {}
        def every(row: dict[str, Any]) -> bool:
            return True

        def byd_only(row: dict[str, Any]) -> bool:
            # Poll soc is restored from the previous snapshot (possibly car-app fed) when BYD gave none.
            return cloud_get(row, "asleep") is False or isnum(cloud_get(row, "charging.soc"))

        variants = [
            ("mqtt:vehicleInfo soc", "mqtt:vehicleInfo", "soc", every),
            ("mqtt:smartCharge soc", "mqtt:smartCharge", "soc", every),
            ("mqtt:smartCharge charging.soc", "mqtt:smartCharge", "charging.soc", every),
            ("poll:ok soc (all, may be restored)", "poll:ok", "soc", every),
            ("poll:ok soc (BYD's: asleep false or charging.soc present)", "poll:ok", "soc", byd_only),
            ("poll:ok soc (asleep false: realtime awake)", "poll:ok", "soc",
             lambda row: cloud_get(row, "asleep") is False),
            ("poll:ok soc (asleep true, no charging.soc: restored)", "poll:ok", "soc",
             lambda row: cloud_get(row, "asleep") is True and not isnum(cloud_get(row, "charging.soc"))),
            ("poll:ok charging.soc", "poll:ok", "charging.soc", every),
        ]
        for name, ch, key, keep in variants:
            out[name] = self._soc_one(ch, key, keep)
        return out

    def _soc_one(self, ch: str, key: str, keep: Any) -> dict[str, Any]:
        car = self.car
        diff, lag, upper = Dist(), Dist(), Dist()
        rule_hits: Counter[str] = Counter()
        pair_units: list[tuple[float, str]] = []
        changing: list[tuple[float, float]] = []
        decimals = present = no_car = unmatched = 0
        for row in self.rows_of(ch):
            v = cloud_get(row, key)
            if not isnum(v) or not keep(row):
                continue
            present += 1
            if not float(v).is_integer():
                decimals += 1
            t = row["t"]
            snap = car.snapshot(t)
            c = car.value("soc_pct", t)
            if not isnum(c):
                no_car += 1
                continue
            diff.add(v - c, t, snap.state)
            pair_units.append((t, snap.state))
            rule_hits["round_half_up"] += math.floor(v + 0.5) == c
            rule_hits["floor"] += math.floor(v) == c
            rule_hits["exact"] += v == c
            if snap.state in ("driving", "charging"):
                whole = math.floor(v + 0.5)
                changing.append((t, whole))
                found = car.value_lag("soc_pct", whole, t)
                if found is None:
                    unmatched += 1
                else:
                    lag.add(found[0], t, snap.state)
                    upper.add(found[1], t, snap.state)
        negative = sum(1 for v, _, _ in lag.items if v < 0)
        best = car.agreement_lag("soc_pct", changing)
        best.update(_units([(t, car.state(t)) for t, _ in changing]))
        return {"cloud_values": present, "with_decimals": decimals, "no_fresh_car_value": no_car,
                "diff_cloud_minus_car": diff.summary(),
                "rounding_rules": {"pairs": len(pair_units), **_units(pair_units),
                                   "rule_evaluations_agreeing": {k: rule_hits[k] for k in
                                                                 ("round_half_up", "floor", "exact")}},
                "better_rounding_rule": _better_rule(rule_hits, len(pair_units)),
                "lag_s_while_changing": lag.summary(), "lag_upper_s_while_changing": upper.summary(),
                "lag_negative_share": share(negative, len(lag)), "lag_unmatched": unmatched,
                "best_fit_lag_while_changing": best}

    # 3 -------------------------------------------------------------------------------------------
    def odometer(self) -> dict[str, Any]:
        out: dict[str, Any] = {}
        for ch in ("mqtt:vehicleInfo", "mqtt:energyConsumption", "poll:ok"):
            car = self.car
            diff, lag = Dist(), Dist()
            agree = Tally()
            driving: list[tuple[float, float]] = []
            decimals = present = no_car = unmatched = 0
            for row in self.rows_of(ch):
                v = cloud_get(row, "odometer_km")
                if not isnum(v):
                    continue
                present += 1
                if not float(v).is_integer():
                    decimals += 1
                t = row["t"]
                snap = car.snapshot(t)
                c = car.value("odometer_km", t)
                if not isnum(c):
                    no_car += 1
                    continue
                diff.add(v - c, t, snap.state)
                agree.add("equal" if math.floor(v) == c else "differ", awake(row), t, snap.state)
                if snap.state == "driving":
                    driving.append((t, math.floor(v)))
                    found = car.value_lag("odometer_km", math.floor(v), t)
                    if found is None:
                        unmatched += 1
                    else:
                        lag.add(found[0], t, snap.state)
            name = ch + (" (contaminated: max of BYD, previous snapshot and drive-log odometers)"
                         if ch == "poll:ok" else "")
            best = car.agreement_lag("odometer_km", driving)
            best.update(_units([(t, "driving") for t, _ in driving]))
            out[name] = {"cloud_values": present, "with_decimals": decimals, "no_fresh_car_value": no_car,
                         "floor_equals_car_x_asleep_flag": agree.summary(),
                         "diff_cloud_minus_car_km": diff.summary(), "lag_s_while_driving": lag.summary(),
                         "lag_negative_share": share(sum(1 for v, _, _ in lag.items if v < 0), len(lag)),
                         "lag_unmatched": unmatched, "best_fit_lag_while_driving": best}
        out["note"] = ("car odometer is whole km (truncated at source); an unbiased cloud value with decimals "
                       "reads 0 to 1 km above it; lag matches floor(cloud) against the car value. The poll odometer "
                       "is a maximum over BYD's value, the previous (car-app-fed) snapshot and drive-log odometers, "
                       "so poll agreement with the car while asleep carries no BYD information; only awake rows "
                       "say anything about BYD.")
        return out

    # 4 -------------------------------------------------------------------------------------------
    def speed(self) -> dict[str, Any]:
        car = self.car
        out: dict[str, Any] = {}
        for ch in ("mqtt:vehicleInfo", "poll:ok"):
            vs_car, vs_fix = Dist(), Dist()
            driving: list[tuple[float, float]] = []
            for row in self.rows_of(ch):
                v = cloud_get(row, "speed_kph")
                if not isnum(v):
                    continue
                t = row["t"]
                snap = car.snapshot(t)
                c = car.value("speed_kph", t, interpolate=True)
                if isnum(c):
                    vs_car.add(v - c, t, snap.state)
                f = car.value("fix_speed_kph", t, interpolate=True)
                if isnum(f):
                    vs_fix.add(v - f, t, snap.state)
                if snap.state == "driving":
                    driving.append((t, v))
            # One common cohort: driving samples with a car speed at every candidate lag.
            cohort = []
            for t, v in driving:
                speeds = [car.value("speed_kph", t - lag, interpolate=True) for lag in SPEED_LAGS_S]
                if all(isnum(c) for c in speeds):
                    cohort.append((v, speeds))
            raw = [(lag, sum(abs(v - sp[k]) for v, sp in cohort) / len(cohort)) if cohort else (lag, None)
                   for k, lag in enumerate(SPEED_LAGS_S)]
            curve = [{"lag_s": lag, "n": len(cohort), "mae_kph": None if mae is None else r3(mae)} for lag, mae in raw]
            best_raw = min((x for x in raw if x[1] is not None), key=lambda x: (x[1], x[0]), default=None)
            best = None if best_raw is None else {"lag_s": best_raw[0], "mae_kph": r3(best_raw[1]), "n": len(cohort)}
            units = _units([(t, "driving") for t, _ in driving])
            out[ch] = {"diff_cloud_minus_car_speed_kph": vs_car.summary(),
                       "diff_cloud_minus_car_gps_speed_kph": vs_fix.summary(),
                       "best_lag": {"n_driving_samples": len(driving), **units, "cohort_n": len(cohort),
                                    "excluded_missing_car_value": len(driving) - len(cohort),
                                    "states": {"driving": len(driving)} if driving else {},
                                    "best_lag_s": best["lag_s"] if best else None,
                                    "mae_at_best_kph": best["mae_kph"] if best else None,
                                    "n_at_best": best["n"] if best else 0,
                                    "mae_at_lag0_kph": curve[0]["mae_kph"], "n_at_lag0": curve[0]["n"],
                                    "curve": curve if driving else []}}
        ratio, offset, fit = Dist(), Dist(), Fit()
        bands = {name: Dist() for _, _, name in SPEED_BANDS}
        segments: set[int] = set()
        for row in car.rows:
            s, f, age = row.get("speed_kph"), row.get("fix_speed_kph"), row.get("fix_age_s")
            if isnum(s) and isnum(f) and isnum(age) and age <= FIX_MAX_AGE_S and s >= SPEED_CMP_MIN_KPH \
                    and f >= SPEED_CMP_MIN_KPH:
                st = car.state(row["t"])
                ratio.add(s / f, row["t"], st)
                offset.add(s - f, row["t"], st)
                fit.add(f, s, row["t"], st)
                segments.add(car.segment_of(row["t"]))
                for lo, hi, name in SPEED_BANDS:
                    if lo <= f < hi:
                        bands[name].add(s / f, row["t"], st)
        out["car_speed_vs_gps_speed"] = {"fit_car_on_gps": fit.summary(),
                                         "driving_segments": len(segments),
                                         "ratio_car_over_gps_by_gps_speed_band": {k: d.summary(by_state=False)
                                                                                 for k, d in bands.items()},
                                         "ratio_car_over_gps": ratio.summary(),
                                         "diff_car_minus_gps_kph": offset.summary()}
        return out

    # 5 -------------------------------------------------------------------------------------------
    def power_state(self) -> dict[str, Any]:
        """vehicle_on, online and asleep against the car, with the on/off callouts under three
        definitions of "car on": power_level ``on`` alone (the only level verified on hardware),
        acc/on/ok, and the cadence of the newest car row (moving or stationary rows mean the car
        is powered). Driving always counts as on."""
        car = self.car
        out: dict[str, Any] = {}
        definitions = {
            "power_on_only": lambda snap: snap.power == "on" or snap.moving,
            "power_acc_on_ok": lambda snap: snap.power in ON_LEVELS or snap.moving,
            "cadence_powered": lambda snap: snap.cadence in ("moving", "stationary"),
        }
        offs = {
            "power_on_only": lambda snap: snap.power == "off" and not snap.moving,
            "power_acc_on_ok": lambda snap: snap.power == "off" and not snap.moving,
            "cadence_powered": lambda snap: snap.cadence in ("charging", "event", "inferred"),
        }
        for ch in ("mqtt:vehicleInfo", "poll:ok"):
            on_moving, on_still = Tally(), Tally()
            online_power, online_state, asleep_power, asleep_state = Tally(), Tally(), Tally(), Tally()
            gear_power, gear_state, gear_cadence = Tally(), Tally(), Tally()
            calls: dict[str, Counter[str]] = {name: Counter() for name in definitions}
            for row in self.rows_of(ch):
                t = row["t"]
                snap = car.snapshot(t)
                von = label(cloud_get(row, "vehicle_on"))
                (on_moving if snap.moving else on_still).add(von, snap.power, t, snap.state)
                online = label(cloud_get(row, "online"))
                asleep = label(cloud_get(row, "asleep"))
                online_power.add(online, snap.power, t, snap.state)
                online_state.add(online, snap.state, t, snap.state)
                asleep_power.add(asleep, snap.power, t, snap.state)
                asleep_state.add(asleep, snap.state, t, snap.state)
                speed = cloud_get(row, "speed_kph")
                if cloud_get(row, "asleep") is False and (not isnum(speed) or speed < 1):
                    gear = POWER_GEAR.get(von, "vehicle_on " + von)
                    gear_power.add(gear, snap.power, t, snap.state)
                    gear_state.add(gear, snap.state, t, snap.state)
                    gear_cadence.add(gear, label(snap.cadence) if snap.cadence else "unknown", t, snap.state)
                for name, is_on in definitions.items():
                    c = calls[name]
                    on, off = is_on(snap), offs[name](snap)
                    c["car_on"] += on
                    c["car_off"] += off
                    c["car_driving"] += snap.moving
                    c["vehicle_on_false_while_car_on"] += von == "false" and on
                    c["vehicle_on_false_while_driving"] += von == "false" and snap.moving
                    c["vehicle_on_null_while_car_on"] += von == "null" and on
                    c["vehicle_on_true_while_car_off"] += von == "true" and off
                    c["asleep_true_while_car_on"] += asleep == "true" and on
                    c["asleep_true_while_driving"] += asleep == "true" and snap.moving
                    c["online_false_while_car_on"] += online == "false" and on
                    c["online_true_while_car_off"] += online == "true" and off
                    c["asleep_false_while_car_off"] += asleep == "false" and off
            out[ch] = {"vehicle_on_x_power_level_moving": on_moving.summary(),
                       "vehicle_on_x_power_level_not_moving": on_still.summary(),
                       "online_x_power_level": online_power.summary(), "online_x_car_state": online_state.summary(),
                       "asleep_x_power_level": asleep_power.summary(), "asleep_x_car_state": asleep_state.summary(),
                       "power_gear_x_power_level": gear_power.summary(),
                       "power_gear_x_car_state": gear_state.summary(),
                       "power_gear_x_car_cadence": gear_cadence.summary(),
                       "callouts": {name: {k: c[k] for k in sorted(c)} for name, c in calls.items()}}
        out["note"] = ("power_gear tables: cloud rows with asleep false and speed < 1 or null, where pyBYD's "
                       "vehicle_on can only come from powerGear: true = powerGear 3 (ON), false = powerGear 1 "
                       "(OFF) or unknown gear with vehicleState 0/2, null = neither known. Car power_level 'on' is "
                       "the only level verified on hardware; cadence (moving/stationary rows mean powered) is the "
                       "cross-check.")
        return out

    # 6 -------------------------------------------------------------------------------------------
    def plug_charging(self) -> dict[str, Any]:
        car = self.car
        out: dict[str, Any] = {}
        cloud_fields = ("charging.is_connected", "charging.is_charging", "charging.connect_state")
        car_fields = ("charging_gun_raw", "charger_state_raw", "charging_type_raw")
        pairs = [("charging.is_connected", "charging_gun_raw"), ("charging.is_connected", "charger_state_raw"),
                 ("charging.is_charging", "charging_type_raw"), ("charging.connect_state", "charging_gun_raw"),
                 ("charging.connect_state", "charger_state_raw"), ("charging.connect_state", "charging_type_raw")]
        for ch in ("poll:ok", "mqtt:smartCharge"):
            by_label: dict[str, Any] = {}
            for wake in AWAKE_LABELS:
                rows = [r for r in self.rows_of(ch) if awake(r) == wake]
                if not rows:
                    continue
                tables: dict[str, Any] = {}
                derived = {"charging.is_connected x car_plugged": Tally(),
                           "charging.is_charging x car_charging": Tally()}
                cells = {pair: Tally() for pair in pairs}
                for row in rows:
                    t = row["t"]
                    snap = car.snapshot(t)
                    values = {cf: label(cloud_get(row, cf)) for cf in cloud_fields}
                    car_values = {kf: label(car.value(kf, t)) for kf in car_fields}
                    for (cf, kf), tally in cells.items():
                        tally.add(values[cf], car_values[kf], t, snap.state)
                    derived["charging.is_connected x car_plugged"].add(
                        values["charging.is_connected"], "unknown" if snap.plugged is None else label(snap.plugged),
                        t, snap.state)
                    derived["charging.is_charging x car_charging"].add(
                        values["charging.is_charging"], "unknown" if snap.charging is None else label(snap.charging),
                        t, snap.state)
                for name, tally in derived.items():
                    tables[name] = tally.summary()
                for (cf, kf), tally in cells.items():
                    tables[f"{cf} x {kf}"] = tally.summary()
                by_label[wake] = tables
            by_label["asleep_runs"] = self._constant_runs(ch)
            out[ch] = by_label
        out["transitions"] = self._transitions()
        out["note"] = ("tables are split by the row's asleep flag (awake = BYD realtime online). car plugged: "
                       "charging_gun_raw 2 or charger_state_raw 1 (unplugged: gun 1 or charger 0); car charging: "
                       "charging_type_raw 2; not charging when unplugged or type 1; otherwise charging_power_kw > 0; "
                       "car labels unknown/absent mean no fresh reading / a fresh row without the key; poll "
                       "connect_state is always null (dropped by a serializer). asleep_runs: a run is a maximal "
                       "stretch of consecutive asleep rows; a constant run shows one (is_charging, is_connected) "
                       "pair throughout, so its 'right' rows are right only when the car did not change.")
        return out

    def _constant_runs(self, ch: str) -> dict[str, Any]:
        """Runs of consecutive asleep rows and whether their (is_charging, is_connected) pair ever changes."""
        car = self.car
        runs: list[list[dict[str, Any]]] = []
        current: list[dict[str, Any]] = []
        for row in self.rows_of(ch):
            if awake(row) == "asleep":
                current.append(row)
            elif current:
                runs.append(current)
                current = []
        if current:
            runs.append(current)
        constant_rows = constant_runs = 0
        right = Tally()
        for run in runs:
            pairs = {(label(cloud_get(r, "charging.is_charging")), label(cloud_get(r, "charging.is_connected")))
                     for r in run}
            constant = len(pairs) == 1
            constant_runs += constant
            constant_rows += len(run) * constant
            for r in run:
                snap = car.snapshot(r["t"])
                if snap.state == "unknown":
                    continue
                ok = cloud_get(r, "charging.is_charging") == snap.charging and \
                    cloud_get(r, "charging.is_connected") == snap.plugged
                right.add("both_right" if ok else "not_both_right", "constant_run" if constant else "varying_run",
                          r["t"], snap.state)
        return {"asleep_rows": sum(len(r) for r in runs), "runs": len(runs), "constant_runs": constant_runs,
                "rows_in_constant_runs": constant_rows, "both_flags_x_run_kind": right.summary()}

    def _transitions(self) -> dict[str, Any]:
        """Car-side plug and charge transitions against each cloud signal. Each event is put in one
        category by the cloud rows between it and the next transition of the same kind: no_rows,
        only_asleep, any_awake, or no_awake_flag (rows without an asleep flag, e.g. smartCharge);
        shown / not shown is counted within each."""
        car, rows = self.car, self.car.rows
        kinds = {"plug": ("plugged", "charging.is_connected", "plug_in", "unplug"),
                 "charge": ("charging", "charging.is_charging", "charge_start", "charge_stop")}

        def flag(field: str) -> Any:
            def get(row: dict[str, Any]) -> bool | None:
                v = cloud_get(row, field)
                return v if type(v) is bool else None
            return get

        def power_shown(row: dict[str, Any]) -> bool | None:
            v = cloud_get(row, "charge_power_kw")
            return None if not isnum(v) else v > 0

        out: dict[str, Any] = {"cloud_rows": {"poll": len(self.rows_of("poll:ok")),
                                              "push": len(self.rows_of("mqtt:smartCharge")),
                                              "push_vehicleInfo_power": len(self.rows_of("mqtt:vehicleInfo"))}}
        for kind, (attr, field, up, down) in kinds.items():
            groups = {"poll": (self.rows_of("poll:ok"), flag(field)),
                      "push": (self.rows_of("mqtt:smartCharge"), flag(field))}
            if kind == "charge":
                # vehicleInfo charge_power_kw > 0 shown as charging, 0 as not; null says nothing.
                groups["push_vehicleInfo_power"] = (self.rows_of("mqtt:vehicleInfo"), power_shown)
            reversals = self._reversals(lambda t, a=attr: getattr(car.snapshot(t), a))
            for name, new in ((up, True), (down, False)):
                times = [(r[0], i) for i, r in enumerate(reversals) if r[1] is new and r[2]]
                uncertain = sum(1 for r in reversals if r[1] is new and not r[2])
                result: dict[str, Any] = {"car_transitions": len(times),
                                          "time_uncertain_across_a_gap_not_measured": uncertain}
                for gname, (crow, shows) in groups.items():
                    delay = Dist()
                    categories: dict[str, Counter[str]] = {c: Counter() for c in TRANSITION_CATEGORIES}
                    censored: Counter[str] = Counter()
                    stale_seen = 0
                    ctimes = [r["t"] for r in crow]
                    for tc, i in times:
                        tn, why = self._window_end(reversals, i)
                        window = crow[bisect.bisect_left(ctimes, tc):bisect.bisect_left(ctimes, tn)]
                        if not window:
                            censored[why] += 1
                        labels = {awake(r) for r in window}
                        category = ("no_rows" if not window else "any_awake" if "awake" in labels
                                    else "only_asleep" if labels == {"asleep"} else "no_awake_flag")
                        shown = None
                        saw_old = False
                        for r in window:
                            v = shows(r)
                            if v is new:
                                shown = r["t"] - tc
                                break
                            if v is (not new):
                                saw_old = True
                        categories[category]["shown" if shown is not None else "not_shown"] += 1
                        if shown is not None:
                            delay.add(shown, tc, car.state(tc))
                        stale_seen += saw_old
                    result[gname] = {"categories": {c: {"events": sum(v.values()), "shown": v["shown"],
                                                        "not_shown": v["not_shown"]}
                                                    for c, v in categories.items()},
                                     "right_censored_no_row_before_window_closed": dict(sorted(censored.items())),
                                     "shown": len(delay), "old_state_shown_after_transition": stale_seen,
                                     "delay_s": delay.summary(by_state=False)}
                out[name] = result
        return out

    def _reversals(self, value_at: Any) -> list[tuple[float, bool, bool]]:
        """Every observed change of a derived car flag, in time order: (time, new value, timed).

        A change between consecutive fresh rows is timed at the first row showing the new value.
        A change across a stale gap is time-uncertain: it is placed at the last row still showing
        the old value (the earliest moment it can have happened), so it closes the window of the
        transition before it, and it is never measured itself."""
        car = self.car
        out: list[tuple[float, bool, bool]] = []
        prev_value = None
        prev_row = None
        for row in car.rows:
            value = value_at(row["t"])
            if value is None:
                continue
            if prev_value is not None and value != prev_value:
                timed = row["t"] - prev_row["t"] <= car.fresh(prev_row)
                out.append((row["t"] if timed else prev_row["t"], value, timed))
            prev_value, prev_row = value, row
        return out

    def _window_end(self, reversals: list[tuple[float, Any, bool]], i: int) -> tuple[float, str]:
        """When the response window of reversal i closes, and why."""
        if i + 1 < len(reversals):
            return reversals[i + 1][0], "next_reversal" if reversals[i + 1][2] else "next_reversal_time_uncertain"
        rows = self.car.rows
        return rows[-1]["t"] + self.car.fresh(rows[-1]), "end_of_observation"

    def freeze_and_stale(self) -> dict[str, Any]:
        """SoC freeze across adjacent asleep polls, and the connected flag after an unplug."""
        car = self.car
        plug_rev = self._reversals(lambda t: car.snapshot(t).plugged)
        rev_times = [r[0] for r in plug_rev]

        def episode(t: float) -> int:
            return bisect.bisect_right(rev_times, t)

        polls = self.rows_of("poll:ok")
        same = Tally()
        for a, b in zip(polls, polls[1:]):
            if awake(a) != "asleep" or awake(b) != "asleep":
                continue
            if car.snapshot(a["t"]).plugged is not True or car.snapshot(b["t"]).plugged is not True:
                continue
            if episode(a["t"]) != episode(b["t"]):
                continue
            va, vb = cloud_get(a, "soc"), cloud_get(b, "soc")
            if isnum(va) and isnum(vb):
                same.add("changed" if va != vb else "unchanged", "pairs", b["t"], car.state(b["t"]))
        filtered = [r for r in polls if awake(r) == "asleep" and car.snapshot(r["t"]).plugged is True]
        first = Tally()
        for a, b in zip(filtered, filtered[1:]):
            va, vb = cloud_get(a, "soc"), cloud_get(b, "soc")
            if isnum(va) and isnum(vb):
                first.add("changed" if va != vb else "unchanged", "pairs", b["t"], car.state(b["t"]))
        unplugs = [r[0] for r in plug_rev if r[1] is False]
        stale = {w: Dist() for w in AWAKE_LABELS}
        no_unplug_seen = Counter()
        for r in polls:
            t = r["t"]
            if cloud_get(r, "charging.is_connected") is not True or car.snapshot(t).plugged is not False:
                continue
            k = bisect.bisect_right(unplugs, t) - 1
            if k < 0:
                no_unplug_seen[awake(r)] += 1
            else:
                stale[awake(r)].add((t - unplugs[k]) / 3600, t, car.state(t))
        return {"soc_freeze_adjacent_asleep_polls_same_observed_plug_episode": same.summary(),
                "soc_freeze_filter_first_asleep_plugged_polls": first.summary(),
                "connected_true_while_car_unplugged_hours_since_last_observed_unplug": {
                    w: d.summary() for w, d in stale.items() if len(d)},
                "connected_true_while_car_unplugged_no_unplug_observed": dict(sorted(no_unplug_seen.items())),
                "note": ("adjacent = consecutive poll:ok rows, both asleep, both with the car plugged in and no "
                         "observed plug change between them; filter-first = asleep polls with the car plugged in, "
                         "paired with the next such poll. Hours are since the last observed unplug; continuity "
                         "(that the car stayed unplugged throughout) is not established.")}

    # b-e -----------------------------------------------------------------------------------------
    def car_events(self) -> dict[str, list[float]]:
        """Car-side changes between consecutive fresh rows: power on/off, start/stop moving,
        door change, plug change. A change across a stale gap is not an event (its time is unknown)."""
        if self._events is not None:
            return self._events
        car, rows = self.car, self.car.rows
        events: dict[str, list[float]] = {k: [] for k in CAR_EVENTS}
        for a, b in zip(rows, rows[1:]):
            if b["t"] - a["t"] > car.fresh(a):
                continue
            pa, pb = a.get("power_level"), b.get("power_level")
            if pa == "off" and pb in ON_LEVELS:
                events["power_on"].append(b["t"])
            if pa in ON_LEVELS and pb == "off":
                events["power_off"].append(b["t"])
            sa, sb = car.snapshot(a["t"]), car.snapshot(b["t"])
            if sb.moving and not sa.moving:
                events["start_moving"].append(b["t"])
            if sa.moving and not sb.moving:
                events["stop_moving"].append(b["t"])
            if type(a.get("doors")) is dict and type(b.get("doors")) is dict and a["doors"] != b["doors"]:
                events["door_change"].append(b["t"])
            if sa.plugged is not None and sb.plugged is not None and sa.plugged != sb.plugged:
                events["plug_change"].append(b["t"])
        self._events = events
        return events

    @staticmethod
    def _first_after(times: list[float], t: float, limit: float) -> float | None:
        k = bisect.bisect_left(times, t)
        if k < len(times) and times[k] - t <= limit:
            return times[k] - t
        return None

    def period(self, t: float) -> str:
        """before_registration (before meta.first_seen_t), registered_no_rows (registered, no car
        row yet) or car_rows (from the first car row on)."""
        if t >= self.first_car:
            return "car_rows"
        return "before_registration" if t < self.registration else "registered_no_rows"

    def pushes(self) -> dict[str, Any]:
        car = self.car
        events = self.car_events()
        targets = {"mqtt:vehicleInfo": [r["t"] for r in self.rows_of("mqtt:vehicleInfo")],
                   "mqtt:remoteControl": [r["t"] for r in self.rows_of("mqtt:remoteControl")],
                   "poll:ok": [r["t"] for r in self.rows_of("poll:ok")]}
        after: dict[str, Any] = {}
        for name, times in events.items():
            per: dict[str, Any] = {"car_events": len(times)}
            for target, ttimes in targets.items():
                delay = Dist()
                none = 0
                for t in times:
                    d = self._first_after(ttimes, t, EVENT_RESPONSE_WINDOW_S)
                    if d is None:
                        none += 1
                    else:
                        delay.add(d, t, car.state(t))
                per[target] = {"none_within_window": none,
                               "delay_s": delay.summary(by_state=False)}
            after[name] = per
        parked_off: Counter[str] = Counter()
        carried: Counter[str] = Counter()
        by_state = Tally()
        for row in self.rows_of("mqtt:vehicleInfo"):
            snap = car.snapshot(row["t"])
            by_state.add("vehicleInfo", snap.state, row["t"], snap.state)
            if snap.power == "off" and not snap.moving:
                parked_off[snap.state] += 1
                for k, v in row.items():
                    if k not in ("type", "t", "source", "kind", "detail") and v is not None:
                        carried[k] += 1
        for row in self.rows_of("mqtt:remoteControl"):
            snap = car.snapshot(row["t"])
            by_state.add("remoteControl", snap.state, row["t"], snap.state)
        # vehicleInfo against the nearest remoteControl push (needs no car data).
        rc = targets["mqtt:remoteControl"]
        gap = Dist()
        rc_first = 0
        for row in self.rows_of("mqtt:vehicleInfo"):
            t = row["t"]
            k = bisect.bisect_left(rc, t)
            near = [rc[j] for j in (k - 1, k) if 0 <= j < len(rc)]
            if near:
                other = min(near, key=lambda x: (abs(x - t), x))
                gap.add(abs(other - t), t, car.state(t))
                rc_first += other <= t
        periods = Tally()
        for ch in ("mqtt:vehicleInfo", "mqtt:remoteControl"):
            for row in self.rows_of(ch):
                periods.add(ch, self.period(row["t"]), row["t"], car.state(row["t"]))
        lo = car.times[0] if car.times else math.inf
        hi = car.times[-1] if car.times else -math.inf
        overlap = {ch: sum(1 for r in self.rows_of(ch) if lo <= r["t"] <= hi)
                   for ch in ("mqtt:vehicleInfo", "mqtt:remoteControl", "mqtt:smartCharge", "mqtt:energyConsumption")}
        return {"pushes_in_car_observation_window": overlap,
                "first_cloud_row_after_car_event": after,
                "pushes_x_car_state": by_state.summary(),
                "vehicleInfo_while_car_off_not_moving_by_state": dict(sorted(parked_off.items())),
                "vehicleInfo_while_car_off_keys_with_values": dict(sorted(carried.items())),
                "vehicleInfo_to_nearest_remoteControl_s": gap.summary(by_state=False),
                "vehicleInfo_with_remoteControl_at_or_before_share": share(rc_first, len(gap)),
                "pushes_x_period": periods.summary(),
                "note": (f"delays are to the first cloud row of each kind within {EVENT_RESPONSE_WINDOW_S:g} s "
                         f"after the car-side change")}

    def polls(self) -> dict[str, Any]:
        car = self.car
        intervals = {p: Dist() for p in PERIODS}
        oks = self.rows_of("poll:ok")
        times = [r["t"] for r in oks]
        for a, b in zip(times, times[1:]):
            intervals[self.period(b)].add(b - a, b, car.state(b))
        awake_by_period = Tally()
        for r in oks:
            awake_by_period.add(awake(r), self.period(r["t"]), r["t"], car.state(r["t"]))
        detail_state, detail_period = Tally(), Tally()
        for row in self.cloud:
            if row["source"] != "poll":
                continue
            d = row.get("detail")
            text = d if type(d) is str and DETAIL.fullmatch(d) else "-" if d is None else "other"
            name = f"{row['kind']}:{text}"
            t = row["t"]
            st = car.state(t)
            detail_state.add(name, st, t, st)
            detail_period.add(name, self.period(t), t, st)
        per_period: dict[str, Any] = {}
        for period in PERIODS:
            n = awake_by_period.count(cols=[period])
            per_period[period] = {"poll_ok": n, "awake": awake_by_period.count(["awake"], [period]),
                                  "awake_share": share(awake_by_period.count(["awake"], [period]), n),
                                  "interval_s": intervals[period].summary()}
        # The unreachable check: asleep polls with vehicle_on null and vehicle_unreachable true.
        unreachable: dict[str, Any] = {}
        for period in PERIODS:
            asleep_rows = [r for r in oks if awake(r) == "asleep" and self.period(r["t"]) == period]
            unreachable[period] = {
                "asleep_polls": len(asleep_rows),
                "vehicle_on_null": sum(1 for r in asleep_rows if cloud_get(r, "vehicle_on") is None),
                "vehicle_unreachable_true": sum(1 for r in asleep_rows if cloud_get(r, "vehicle_unreachable") is True)}
        bins: dict[int, Counter[str]] = {}
        if math.isfinite(self.registration):
            for r in oks:
                k = math.floor((r["t"] - self.registration) / HALF_DAY_S)
                c = bins.setdefault(k, Counter())
                c["poll_ok"] += 1
                c["asleep"] += awake(r) == "asleep"
                c["unreachable"] += cloud_get(r, "vehicle_unreachable") is True
        half_days = [{"bin": k, "poll_ok": c["poll_ok"], "asleep_share": share(c["asleep"], c["poll_ok"]),
                      "vehicle_unreachable_share": share(c["unreachable"], c["poll_ok"])}
                     for k, c in sorted(bins.items())]
        return {"registration_source": self.registration_source,
                "per_period": per_period,
                "awake_x_period": awake_by_period.summary(),
                "asleep_unreachable_check": unreachable,
                "half_day_bins_from_registration": half_days,
                "poll_rows_kind_detail_x_car_state": detail_state.summary(),
                "poll_rows_kind_detail_x_period": detail_period.summary(),
                "note": ("bin k covers [k, k+1) half-days after the car-app registration (negative: before). "
                         "vehicle_unreachable true means an optional BYD call returned 6002 (vehicle "
                         "unreachable); asleep with vehicle_on null means the realtime reply was not online.")}

    def online_latency(self) -> dict[str, Any]:
        """How soon after car power off / on the cloud says asleep / online, and for how long
        after power off the cloud still says online. Each response window closes at the next
        observed power change, including one seen across a gap (time-uncertain, not measured)."""
        car = self.car

        def powered(t: float) -> bool | None:
            loc = car.locate(t)
            if loc is None or loc[1]:
                return None
            level = car.rows[loc[0]].get("power_level")
            return True if level in POWER_ON_LABELS else False if level == "off" else None

        reversals = self._reversals(powered)
        timed = [(r[0], "on" if r[1] else "off", i) for i, r in enumerate(reversals) if r[2]]
        offs = [t for t, kind, _ in timed if kind == "off"]
        checks = {"asleep_true_after_power_off": ("off", "asleep", True),
                  "online_false_after_power_off": ("off", "online", False),
                  "online_true_after_power_on": ("on", "online", True),
                  "asleep_false_after_power_on": ("on", "asleep", False)}
        out: dict[str, Any] = {"time_uncertain_power_changes_across_a_gap": sum(1 for r in reversals if not r[2])}
        for ch in ("poll:ok", "mqtt:vehicleInfo"):
            rows = self.rows_of(ch)
            rtimes = [r["t"] for r in rows]
            res: dict[str, Any] = {}
            for name, (kind, field, want) in checks.items():
                delay = Dist()
                not_shown = 0
                censored: Counter[str] = Counter()
                for t0, ev, i in timed:
                    if ev != kind:
                        continue
                    t_end, why = self._window_end(reversals, i)
                    window = rows[bisect.bisect_left(rtimes, t0):bisect.bisect_left(rtimes, t_end)]
                    if not window:
                        censored[why] += 1
                        continue
                    hit = next((r["t"] - t0 for r in window if cloud_get(r, field) is want), None)
                    if hit is None:
                        not_shown += 1
                    else:
                        delay.add(hit, t0, car.state(t0))
                res[name] = {"car_events": sum(1 for _, e, _ in timed if e == kind),
                             "right_censored_no_row_before_window_closed": dict(sorted(censored.items())),
                             "rows_in_window_but_not_shown": not_shown,
                             "delay_s": delay.summary(by_state=False)}
            since = {"online_true": Dist(), "online_false": Dist()}
            for r in rows:
                t = r["t"]
                snap = car.snapshot(t)
                k = bisect.bisect_right(offs, t) - 1
                if snap.power != "off" or snap.moving or k < 0:
                    continue
                # No observed power change (timed or not) between that power-off and t.
                later = [x for x in reversals if offs[k] < x[0] <= t]
                if later:
                    continue
                online = cloud_get(r, "online")
                if type(online) is bool:
                    since["online_true" if online else "online_false"].add((t - offs[k]) / 3600, t, snap.state)
            res["hours_since_car_power_off_when_cloud_says"] = {k: d.summary() for k, d in since.items()}
            out[ch] = res
        return out

    def contamination(self) -> dict[str, Any]:
        """Poll SoC and odometer equal to the car's current value while the newest earlier
        vehicleInfo push differs: a hint the poll value came from car-app-fed state."""
        car = self.car
        pushes = self.rows_of("mqtt:vehicleInfo")
        ptimes = [r["t"] for r in pushes]
        out: dict[str, Any] = {}
        for key, car_key in (("soc", "soc_pct"), ("odometer_km", "odometer_km")):
            table, by_asleep = Tally(), Tally()
            age = Dist()
            for row in self.rows_of("poll:ok"):
                v = cloud_get(row, key)
                t = row["t"]
                c = car.value(car_key, t)
                if not isnum(v) or not isnum(c):
                    continue
                st = car.state(t)
                rnd = math.floor(v) if key == "odometer_km" else math.floor(v + 0.5)
                equal = "poll_equals_car" if rnd == c else "poll_differs_from_car"
                k = bisect.bisect_left(ptimes, t) - 1
                while k >= 0 and not isnum(cloud_get(pushes[k], key)):
                    k -= 1
                if k < 0:
                    push = "no_earlier_push"
                else:
                    age.add((t - ptimes[k]) / 3600, t, st)
                    if t - ptimes[k] > CONTAMINATION_PUSH_MAX_AGE_S:
                        push = "no_push_within_window"
                    else:
                        push = "push_equals_poll" if cloud_get(pushes[k], key) == v else "push_differs"
                table.add(equal, push, t, st)
                by_asleep.add(equal, f"asleep {label(cloud_get(row, 'asleep'))}", t, st)
            out[key] = {"poll_vs_car_x_latest_push": table.summary(), "poll_vs_car_x_asleep": by_asleep.summary(),
                        "latest_push_age_h": age.summary(by_state=False),
                        "hint_count": table.count(["poll_equals_car"], ["push_differs"])}
        return out

    # 7 -------------------------------------------------------------------------------------------
    def charge_power(self) -> dict[str, Any]:
        car = self.car
        out: dict[str, Any] = {}
        for ch in ("mqtt:vehicleInfo", "poll:ok"):
            diff, ratio, fit, absdiff = Dist(), Dist(), Fit(), Dist()
            within = cloud_ge = 0
            kinds_by_wake: Counter[str] = Counter()
            positive_not_charging = Dist()
            positive_unplugged = Dist()
            denominators: Counter[str] = Counter()
            while_charging: Counter[str] = Counter()
            for row in self.rows_of(ch):
                t = row["t"]
                snap = car.snapshot(t)
                v = cloud_get(row, "charge_power_kw")
                if snap.state == "charging":
                    kind = ("null" if v is None else "absent" if v is ABSENT else "zero" if isnum(v) and v == 0
                            else "positive" if isnum(v) and v > 0 else "negative")
                    while_charging[kind] += 1
                    kinds_by_wake[f"{awake(row)}:{kind}"] += 1
                    c = car.value("charging_power_kw", t)
                    if isnum(v) and isnum(c) and c > 0:
                        diff.add(v - c, t, snap.state)
                        absdiff.add(abs(v - c), t, snap.state)
                        cloud_ge += v >= c
                        ratio.add(v / c, t, snap.state)
                        fit.add(c, v, t, snap.state)
                        within += abs(v - c) <= POWER_ABS_TOL_KW or abs(v - c) <= POWER_REL_TOL * c
                elif isnum(v):
                    denominators[snap.state] += 1
                    if v > 0:
                        positive_not_charging.add(v, t, snap.state)
                        if snap.plugged is False:
                            positive_unplugged.add(v, t, snap.state)
            out[ch] = {"while_car_charging": {"diff_cloud_minus_car_kw": diff.summary(),
                                              "abs_diff_kw": absdiff.summary(),
                                              "cloud_ge_car": {"count": cloud_ge, "n": len(diff)},
                                              "resolution_kw": POWER_RESOLUTION_KW,
                                              "cloud_value_kinds_by_asleep_flag": dict(sorted(kinds_by_wake.items())),
                                              "ratio_cloud_over_car": ratio.summary(),
                                              "fit_cloud_on_car": fit.summary(),
                                              "within_tolerance_share": share(within, len(diff)),
                                              "cloud_value_kinds": dict(sorted(while_charging.items()))},
                       "cloud_positive_while_car_not_charging": positive_not_charging.summary(),
                       "cloud_positive_while_car_unplugged": positive_unplugged.summary(),
                       "cloud_numbers_while_car_not_charging_by_state": {s: denominators[s] for s in STATES
                                                                          if denominators[s]}}
        return out

    # 8 -------------------------------------------------------------------------------------------
    def time_to_full(self) -> dict[str, Any]:
        car = self.car
        out: dict[str, Any] = {}
        for ch in ("poll:ok", "mqtt:smartCharge", "mqtt:vehicleInfo"):
            for key in ("charging.minutes_to_full", "minutes_to_full"):
                diff = Dist()
                by_wake = {w: Dist() for w in AWAKE_LABELS}
                present = no_car = 0
                for row in self.rows_of(ch):
                    v = cloud_get(row, key)
                    if not isnum(v):
                        continue
                    present += 1
                    t = row["t"]
                    c = car.value("charge_remaining_min", t)
                    # The car value can come from an older full row; not once the codes say not charging.
                    if not isnum(c) or car.snapshot(t).charging is False:
                        no_car += 1
                        continue
                    diff.add(v - c, t, car.state(t))
                    by_wake[awake(row)].add(v - c, t, car.state(t))
                out[f"{ch} {key}"] = {"cloud_values": present, "no_car_value": no_car,
                                      "diff_cloud_minus_car_min": diff.summary(),
                                      "by_asleep_flag": {w: d.summary() for w, d in by_wake.items() if len(d)}}
        return out

    # 9 -------------------------------------------------------------------------------------------
    def position(self) -> dict[str, Any]:
        car = self.car
        out: dict[str, Any] = {}
        variants = [("poll:ok gps_fresh true", "poll:ok", lambda r: cloud_get(r, "gps_fresh") is True),
                    ("poll:ok gps_fresh false", "poll:ok", lambda r: cloud_get(r, "gps_fresh") is False),
                    ("poll:ok gps_fresh null/absent", "poll:ok", lambda r: cloud_get(r, "gps_fresh") in (None, ABSENT)),
                    ("mqtt:vehicleInfo", "mqtt:vehicleInfo", lambda r: True)]
        for name, ch, keep in variants:
            dist = {"stationary": Dist(), "moving": Dist(), "speed_unknown": Dist()}
            implied, parked, parked_age = Dist(), Dist(), Dist()
            counts: Counter[str] = Counter()
            step_zero = Tally()
            for row in self.rows_of(ch):
                if not keep(row):
                    continue
                t = row["t"]
                snap = car.snapshot(t)
                counts["rows"] += 1
                counts[f"gps_present_{label(cloud_get(row, 'gps_present'))}"] += 1
                counts["gps_zero"] += cloud_get(row, "gps_zero") is True
                step = cloud_get(row, "gps_step_m")
                if isnum(step):
                    step_zero.add("step_0" if step == 0 else "step_positive", snap.state, t, snap.state)
                d = cloud_get(row, "gps_dist_m")
                if isnum(d):
                    speed = cloud_get(row, "gps_car_speed_kph")
                    if not isnum(speed):
                        speed = car.value("speed_kph", t)
                    if not isnum(speed):
                        dist["speed_unknown"].add(d, t, snap.state)
                    elif speed < DRIVING_MIN_KPH:
                        dist["stationary"].add(d, t, snap.state)
                    else:
                        dist["moving"].add(d, t, snap.state)
                        if speed >= GPS_LAG_MIN_KPH:
                            implied.add(d / (speed / 3.6), t, snap.state)
                last_d, last_age = cloud_get(row, "gps_last_dist_m"), cloud_get(row, "gps_last_age_s")
                if isnum(last_d) and isnum(last_age) and snap.state != "driving":
                    fix_t = t - last_age
                    odo_fix, odo_now = car.last_known("odometer_km", fix_t), car.last_known("odometer_km", t)
                    if odo_fix and odo_now and odo_fix[1] == odo_now[1] and not car.moved_between(fix_t, t):
                        parked.add(last_d, t, snap.state)
                        parked_age.add(last_age / 3600, t, snap.state)
                    else:
                        counts["gps_last_not_parked_or_unprovable"] += 1
            out[name] = {"counts": dict(sorted(counts.items())),
                         "gps_dist_m_car_stationary": dist["stationary"].summary(),
                         "gps_dist_m_car_moving": dist["moving"].summary(),
                         "gps_dist_m_car_speed_unknown": dist["speed_unknown"].summary(),
                         "implied_lag_s_moving": implied.summary(),
                         "parked_gps_last_dist_m": parked.summary(),
                         "parked_gps_last_age_h": parked_age.summary(),
                         "gps_step_x_car_state": step_zero.summary(),
                         "gps_step_zero_share": share(step_zero.count(rows=["step_0"]), step_zero.count()),
                         "parked_zero_m": {"count": sum(1 for v in parked.values() if v == 0), "n": len(parked)}}
        out["parked_zero_m_share_side_by_side"] = {
            k: share(out[k]["parked_zero_m"]["count"], out[k]["parked_zero_m"]["n"])
            for k in ("poll:ok gps_fresh true", "poll:ok gps_fresh false")}
        out["note"] = ("gps_last_dist_m is agreement with an earlier car fix, not position accuracy. gps_fresh "
                       "true only says this fetch mapped a GPS object; the data cannot tell BYD's own fix from a "
                       "restored or car-app-fed position, so a 0 m parked distance is not evidence that BYD's "
                       "position is right.")
        return out

    # 10 ------------------------------------------------------------------------------------------
    def energy(self) -> dict[str, Any]:
        car, rows = self.car, self.car.rows
        candidates = [i for i in car.odo_idx if isnum(rows[i].get("lifetime_kwh"))]
        # A reading below the running maximum whose next readings recover is out of order (a resend
        # or late row): it is dropped and counted. A backward step that persists starts a new
        # monotonic segment; a 50 km window that would cross it is skipped and counted.
        both: list[int] = []
        out_of_order = backward_steps = 0
        running = -math.inf
        for k, i in enumerate(candidates):
            odo = rows[i]["odometer_km"]
            if odo < running:
                ahead = [rows[x]["odometer_km"] for x in candidates[k + 1:k + 1 + ODO_RECOVERY_READINGS]]
                if any(v >= running for v in ahead):
                    out_of_order += 1
                    continue
                backward_steps += 1
                running = odo
            both.append(i)
            running = max(running, odo)
        both_t = [rows[i]["t"] for i in both]
        odos = [rows[i]["odometer_km"] for i in both]
        seg_start = [0] * len(odos)
        for k in range(1, len(odos)):
            seg_start[k] = k if odos[k] < odos[k - 1] else seg_start[k - 1]
        # Cumulative remaining_kwh drop over consecutive moving rows (signed, so regen counts).
        drop = [0.0] * len(rows)
        for i in range(1, len(rows)):
            a, b = rows[i - 1], rows[i]
            step = 0.0
            if a.get("cadence") == "moving" and b.get("cadence") == "moving" and isnum(a.get("remaining_kwh")) \
                    and isnum(b.get("remaining_kwh")) and b["t"] - a["t"] <= FRESH_S["moving"]:
                step = a["remaining_kwh"] - b["remaining_kwh"]
            drop[i] = drop[i - 1] + step
        out: dict[str, Any] = {}
        for ch in CHANNELS:
            ratio, diff, ratio_rem, diff_rem, car_age = Dist(), Dist(), Dist(), Dist(), Dist()
            windows: dict[float, list[tuple[float, float, float | None]]] = {}
            present = no_window = crosses_backward = 0
            for row in self.rows_of(ch):
                v = cloud_get(row, "last50km_energy_kwh")
                if not isnum(v):
                    continue
                present += 1
                t = row["t"]
                j = bisect.bisect_right(both_t, t) - 1
                if j < 0:
                    no_window += 1
                    continue
                now = rows[both[j]]
                target = now["odometer_km"] - ENERGY_WINDOW_KM
                lo = seg_start[j]
                k = bisect.bisect_left(odos, target, lo, j + 1)
                best = min((c for c in (k - 1, k) if lo <= c <= j), key=lambda c: (abs(odos[c] - target), -c),
                           default=None)
                if best is None or abs(odos[best] - target) > ENERGY_ODO_TOL_KM:
                    if lo > 0 and odos[lo] > target + ENERGY_ODO_TOL_KM:
                        crosses_backward += 1
                    else:
                        no_window += 1
                    continue
                then = rows[both[best]]
                state = car.state(t)
                car_age.add(t - now["t"], t, state)
                used = now["lifetime_kwh"] - then["lifetime_kwh"]
                diff.add(v - used, t, state)
                if used > 0:
                    ratio.add(v / used, t, state)
                rem = drop[both[j]] - drop[both[best]]
                diff_rem.add(v - rem, t, state)
                windows.setdefault(now["odometer_km"], []).append(
                    (t - now["t"], v / used if used > 0 else None, v / rem if rem > 0 else None))
            out[ch] = {"cloud_values": present, "no_car_50km_window": no_window,
                       "windows_crossing_a_backward_odometer_step": crosses_backward,
                       "per_50km_window": {"all_endpoints": _per_window(windows, math.inf),
                                           "endpoint_age_le_10_min": _per_window(windows, ENDPOINT_FRESH_S)},
                       "vs_lifetime_kwh": {"ratio_cloud_over_car": ratio.summary(),
                                           "diff_cloud_minus_car_kwh": diff.summary()},
                       "vs_remaining_kwh_drop_while_driving": {"ratio_cloud_over_car": ratio_rem.summary(),
                                                               "diff_cloud_minus_car_kwh": diff_rem.summary()},
                       "car_reading_age_s": car_age.summary(by_state=False)}
        shares: dict[str, Any] = {}
        for ch in CHANNELS:
            sums, per = Dist(), {k: Dist() for k in ("drive", "electric", "hvac", "other")}
            nulls = objects = 0
            for row in self.rows_of(ch):
                obj = cloud_get(row, "energy_distribution")
                if obj is None:
                    nulls += 1
                if type(obj) is not dict:
                    continue
                objects += 1
                t = row["t"]
                state = car.state(t)
                numbers = {k: obj[k] for k in per if isnum(obj.get(k))}
                for k, v in numbers.items():
                    per[k].add(v, t, state)
                if numbers:
                    sums.add(sum(numbers.values()), t, state)
            near = sum(1 for v, _, _ in sums.items if abs(v - 100) <= ENERGY_SHARE_TOL)
            shares[ch] = {"objects": objects, "null": nulls, "sum": sums.summary(by_state=False),
                          "sum_within_tolerance_of_100_share": share(near, len(sums)),
                          "ranges": {k: d.summary(by_state=False) for k, d in per.items()}}
        out["energy_distribution"] = shares
        out["odometer_backward_steps_persistent"] = backward_steps
        out["odometer_readings_out_of_order_dropped"] = out_of_order
        out["note"] = (f"car energy over the last {ENERGY_WINDOW_KM:g} km = lifetime_kwh at the newest car reading "
                       f"at or before the cloud row minus lifetime_kwh at the reading whose odometer is nearest "
                       f"{ENERGY_WINDOW_KM:g} km lower (tolerance {ENERGY_ODO_TOL_KM:g} km); remaining_kwh drop sums "
                       f"consecutive moving rows only")
        return out

    # 11 ------------------------------------------------------------------------------------------
    def hvac(self) -> dict[str, Any]:
        out: dict[str, Any] = {}
        for ch in CHANNELS:
            tally = Tally()
            for row in self.rows_of(ch):
                t = row["t"]
                state = self.car.state(t)
                tally.add(label(cloud_get(row, "hvac_on")), state, t, state)
            out[ch] = tally.summary()
        return out

    # 12 ------------------------------------------------------------------------------------------
    def car_reference(self) -> dict[str, Any]:
        car = self.car
        fit = Fit()
        volts: dict[str, Dist] = {p: Dist() for p in POWER_LEVELS}
        tyres = {w: Dist() for w in ("fl", "fr", "rl", "rr")}
        spread, temp = Dist(), Dist()
        doors: dict[str, Counter[str]] = {}
        gears: Counter[str] = Counter()
        powers: Counter[str] = Counter()
        locks: Counter[str] = Counter()
        seen: Counter[str] = Counter()
        for row in car.rows:
            t = row["t"]
            state = car.state(t)
            for k in row:
                seen[k] += 1
            if isnum(row.get("soc_pct")) and isnum(row.get("remaining_kwh")):
                fit.add(row["soc_pct"], row["remaining_kwh"], t, state)
            if isnum(row.get("twelve_volt")):
                volts[row.get("power_level") if row.get("power_level") in POWER_LEVELS else "unknown"].add(
                    row["twelve_volt"], t, state)
            tk = row.get("tyre_kpa")
            if type(tk) is dict:
                values = [v for w, v in tk.items() if w in tyres and isnum(v)]
                for w, v in tk.items():
                    if w in tyres and isnum(v):
                        tyres[w].add(v, t, state)
                if len(values) == 4:
                    spread.add(max(values) - min(values), t, state)
            if isnum(row.get("outside_temp_c")):
                temp.add(row["outside_temp_c"], t, state)
            if type(row.get("doors")) is dict:
                for door, value in row["doors"].items():
                    doors.setdefault(str(door), Counter())[label(value)] += 1
            if "gear" in row:
                gears[label(row["gear"])] += 1
            if "power_level" in row:
                powers[label(row["power_level"])] += 1
            if "lock" in row:
                locks[label(row["lock"])] += 1
        f = fit.summary()
        if "slope" in f:
            f["implied_kwh_at_100_pct"] = r3(f["slope"] * 100)
        return {"kwh_per_soc_pct_fit_remaining_on_soc": f,
                "twelve_volt_by_power_level": {p: d.summary() for p, d in volts.items()},
                "tyre_kpa_by_slot_labels_unconfirmed": {w: d.summary(by_state=False) for w, d in tyres.items()},
                "tyre_kpa_spread_of_four": spread.summary(by_state=False),
                "outside_temp_c": temp.summary(),
                "car_keys_never_seen": [k for k in CAR_KEYS if not seen[k]],
                "door_values": {d: dict(sorted(c.items())) for d, c in sorted(doors.items())},
                "gear_values": dict(sorted(gears.items())), "power_level_values": dict(sorted(powers.items())),
                "lock_values": dict(sorted(locks.items()))}

    # 13 ------------------------------------------------------------------------------------------
    def limits(self, car_seen: dict[str, int],
               cloud_keys: dict[str, dict[str, dict[str, int]]]) -> list[dict[str, str]]:
        cloud_with_value: Counter[str] = Counter()
        for per in cloud_keys.values():
            for key, cell in per.items():
                cloud_with_value[key] += cell["value"]
        counterpart = {
            "soc_pct": ("soc", "charging.soc"), "odometer_km": ("odometer_km",), "speed_kph": ("speed_kph",),
            "power_level": ("vehicle_on",), "charging_gun_raw": ("charging.is_connected", "charging.connect_state"),
            "charger_state_raw": ("charging.is_connected", "charging.connect_state"),
            "charging_type_raw": ("charging.is_charging",), "charging_power_kw": ("charge_power_kw",),
            "charge_remaining_min": ("charging.minutes_to_full", "minutes_to_full"),
            "has_fix": ("gps_present", "gps_dist_m"), "lifetime_kwh": ("last50km_energy_kwh",),
        }
        out: list[dict[str, str]] = []
        for key in CAR_KEYS:
            if key in ("cadence",) or not car_seen.get(key):
                continue
            if key not in counterpart:
                out.append({"quantity": key, "side": "car_only", "reason": "no cloud field in this export"})
            elif not any(cloud_with_value[c] for c in counterpart[key]):
                out.append({"quantity": key, "side": "car_only",
                            "reason": f"cloud counterpart {'/'.join(counterpart[key])} never had a value"})
        no_car = {"hvac_on": "no car reading of climate state", "energy_distribution": "car has no energy split",
                  "online": "car has no connectivity reading", "asleep": "car has no connectivity reading",
                  "vehicle_unreachable": "BYD error flag, no car counterpart",
                  "last_updated_t": "server clock, not a sample time",
                  "charging.connect_state": "raw BYD code; compared only as a code table"}
        for key, reason in no_car.items():
            if cloud_with_value[key]:
                out.append({"quantity": key, "side": "cloud_only", "reason": reason})
        return out

    # 18 ------------------------------------------------------------------------------------------
    def _enum_readings(self, t: float, snap: Snapshot) -> dict[str, str]:
        """The car's enum readings at ``t`` (fresh or inferred), as table labels; unknown ones left out.

        ``doors_any_open`` is open when any door reads open, and closed only when all six (four
        doors, hood, trunk) are read and closed.
        """
        car = self.car
        out: dict[str, str] = {}
        doors = car.value("doors", t)
        if type(doors) is dict:
            known = {str(d): v for d, v in doors.items() if v in ("open", "closed")}
            if "open" in known.values():
                out["doors_any_open"] = "open"
            elif all(known.get(d) == "closed" for d in DOOR_KEYS):
                out["doors_any_open"] = "closed"
            for d in DOOR_KEYS:
                if d in known:
                    out[f"doors.{d}"] = known[d]
        for key in ("lock", "gear", "power_level", "cadence", *PLUG_KEYS):
            value = car.value(key, t)
            if value is not UNKNOWN and value is not ABSENT and value is not None:
                out[key] = label(value)
        out["moving"] = label(snap.moving)
        out["car_state"] = snap.state
        return out

    def _number_readings(self, t: float) -> dict[str, float]:
        """The car's numbers at ``t`` (speed interpolated while moving); tyres per slot and their mean."""
        out: dict[str, float] = {}
        for key in PLAIN_NUMBERS:
            value = self.car.value(key, t, interpolate=key == "speed_kph")
            if isnum(value):
                out[key] = float(value)
        for key in ("tyre_kpa", "tyre_temp_c"):
            value = self.car.value(key, t)
            if type(value) is dict:
                slots = {w: float(value[w]) for w in TYRE_SLOTS[:4] if isnum(value.get(w))}
                out.update({f"{key}[{w}]": x for w, x in slots.items()})
                if len(slots) == 4:
                    out[f"{key}[mean]"] = sum(slots.values()) / 4
        return out

    def _reading_at(self, t: float, reading: str, numeric: bool) -> Any:
        """One car reading at ``t`` from fresh rows only (not inferred), for the lag evaluation."""
        snap = self.car.snapshot(t)
        if snap.state in ("unknown", "parked_inferred"):
            return None
        return self._number_readings(t).get(reading) if numeric else self._enum_readings(t, snap).get(reading)

    def decode(self) -> dict[str, Any]:
        """Each raw BYD field against the car's own readings (aggregates only; see DECODE_NOTE)."""
        car = self.car
        by_endpoint: Counter[str] = Counter()
        lines = with_fields = 0
        status_counts: Counter[str] = Counter()
        codes = Tally()
        counts: dict[str, Counter[str]] = {}
        specs: dict[str, Any] = {}
        samples: dict[str, list[tuple[float, str, Any, int]]] = {}
        inferred: Counter[str] = Counter()
        readings: list[tuple[dict[str, str], dict[str, float]]] = []
        pair_times: list[float] = []
        ages: dict[str, Dist] = {}
        for row in self.raw:
            t = row["t"]
            source, endpoint = row.get("source"), row["endpoint"]
            lines += 1
            by_endpoint[f"{source}:{endpoint}"] += 1
            fields = row.get("fields") if type(row.get("fields")) is dict else None
            table = export_pairs.raw_table(source, endpoint)
            sample = None
            invalid_time = False
            if fields:
                invalid_time = any(f"{path}_stale" in fields for path in SAMPLE_TIME_PATHS)
                sample = next((fields[f"{path}_t"] for path in SAMPLE_TIME_PATHS
                               if isnum(fields.get(f"{path}_t"))), None)
            when = t if sample is None else float(sample)
            snap = car.snapshot(when)
            if "code" in row:
                codes.add(f"{endpoint}:{label(row['code'])}", snap.state, t, snap.state)
            if fields is None:
                continue
            with_fields += 1
            if invalid_time:
                status = "sample_time_invalid"
            elif sample is not None and abs(t - sample) > STALE_SAMPLE_S:
                status = "stale_sample"
            elif snap.state == "unknown":
                status = "car_unknown"
            elif snap.state == "parked_inferred":
                status = "inferred_parked"
            else:
                status = "paired_with_car"
            status_counts[status] += 1
            index = -1
            if status == "paired_with_car":
                readings.append((self._enum_readings(when, snap), self._number_readings(when)))
                pair_times.append(when)
                index = len(readings) - 1
            for key, value in fields.items():
                if key.endswith("_stale"):
                    continue
                name = f"{endpoint}:{key}"
                if key.endswith("_t"):
                    if isnum(value):
                        ages.setdefault(name[:-2], Dist()).add(t - value, t, snap.state)
                    continue
                spec = table.get(key)
                if spec is None:
                    continue
                specs[name] = spec
                counts.setdefault(name, Counter())[label(value)] += 1
                if status == "paired_with_car":
                    samples.setdefault(name, []).append((when, snap.state, value, index))
                elif status == "inferred_parked":
                    inferred[name] += 1
        clusters = _global_clusters(pair_times)
        constant: dict[str, dict[str, Any]] = {}
        enums: dict[str, dict[str, Any]] = {}
        numbers: dict[str, dict[str, Any]] = {}
        unranked: dict[str, int] = {}
        family: list[dict[str, Any]] = []  # every hypothesis: {"kind", "field", "reading", "out", "work"}
        best_of: list[dict[str, Any]] = []
        for name in sorted(counts):
            seen = counts[name]
            spec = specs[name]
            if len(seen) <= 1:
                constant[name] = {"value": next(iter(seen)), "lines": sum(seen.values())}
                continue
            rows = sorted(samples.get(name, []), key=lambda r: (r[0], r[3]))
            if spec.kind in ("int", "enum", "bool") and len(seen) <= ENUM_MAX_VALUES:
                kind, readings_of, build = "enum", ENUM_READINGS, _enum_hypothesis
            elif spec.kind in ("int", "float"):
                kind, readings_of, build = "numeric", NUMBER_READINGS, _numeric_hypothesis
            else:
                unranked[name] = len(seen)  # many distinct strings: neither a table nor a fit
                continue
            found = {}
            for order, reading in enumerate(readings_of):
                result = build(rows, readings, clusters, reading, spec)
                if result is not None:
                    entry = {"kind": kind, "field": name, "reading": reading, "order": order, **result}
                    found[reading] = entry
                    family.append(entry)
            best = max(found.values(), key=lambda e: (e["work"]["gate"], e["work"]["score"], -e["order"]),
                       default=None)
            record = {"table_kind": spec.kind, "distinct": len(seen), "inferred_parked_pairs": inferred[name],
                      "best_reading": None if best is None else best["reading"],
                      "hypotheses": {reading: entry["out"] for reading, entry in found.items()}}
            if kind == "enum":
                record["value_counts"] = dict(sorted(seen.items()))
                enums[name] = record
            else:
                numbers[name] = record
            if best is not None:
                best_of.append(best)
        # Each field's best reading (chosen on the train half) is tested on the test half when it passed the
        # train gates; every other hypothesis keeps p = 1 and stays in the family.
        for entry in best_of:
            work, out = entry["work"], entry["out"]
            if not work["gate"]:
                continue
            key = f"{entry['kind']}|{entry['field']}|{entry['reading']}"
            if entry["kind"] == "enum":
                work["p"] = _enum_shift_p(work["test_x"], work["test_y"], work["test_clusters"], key)
                out["held_out"] = _enum_held_out(work)
            else:
                work["p"] = _numeric_shift_p(work["test_clusters_xy"], key)
                out["held_out"] = _numeric_held_out(work)
            out["p_value"] = r3(work["p"])
            out["tested"] = True
        passed = _benjamini_hochberg([entry["work"].get("p", 1.0) for entry in family], FDR_Q)
        for entry, ok in zip(family, passed, strict=True):
            out, work = entry["out"], entry["work"]
            out["bh_pass"] = ok
            held = out.get("held_out") or {}
            if (ok and out.get("tested") and held.get("pass") and work["gate"]
                    and out["test_clusters"] >= MIN_TEST_CLUSTERS):
                out["verdict"] = "predicts"
        rank_enum: list[dict[str, Any]] = []
        rank_numeric: list[dict[str, Any]] = []
        for entry in best_of:
            out = entry["out"]
            item = {"field": entry["field"], "reading": entry["reading"], "verdict": out["verdict"], "n": out["n"],
                    "n_train": out["n_train"], "n_test": out["n_test"], "test_clusters": out["test_clusters"],
                    "days": out.get("days", 0), "p_value": out.get("p_value"), "bh_pass": out["bh_pass"],
                    "held_out": out.get("held_out"), "_score": entry["work"]["score"]}
            if entry["kind"] == "enum":
                item.update(u_train=out["train"]["u"], lambda_train=out["train"]["lambda"],
                            supported_raw_values=len(out["raw_values_supported"]),
                            episodes_min=min(out["episodes_by_car_value"].values(), default=0))
                rank_enum.append(item)
            else:
                item.update(r_diff_train=out["r_diff_train"], episodes=out["episodes"], clusters=out["clusters"],
                            slope_train=out["slope_train"], offset_train=out["offset_train"])
                rank_numeric.append(item)
        for ranking in (rank_enum, rank_numeric):
            ranking.sort(key=lambda x: (-_RANK[x["verdict"]], -x["_score"], x["field"]))
        lags = self._decode_lags(rank_enum, rank_numeric, samples, specs, clusters)
        for ranking in (rank_enum, rank_numeric):
            for item in ranking:
                del item["_score"]
        tested = [entry for entry in family if entry["out"].get("tested")]
        return {
            "note": DECODE_NOTE,
            "lines": {"raw_lines": lines, "with_fields": with_fields,
                      **{key: status_counts[key] for key in ("paired_with_car", "car_unknown", "inferred_parked",
                                                             "stale_sample", "sample_time_invalid")},
                      "by_source_endpoint": dict(sorted(by_endpoint.items()))},
            "clusters": {"total": len(set(clusters)), "train": len({c for c in clusters if c % 2 == 0}),
                         "test": len({c for c in clusters if c % 2 == 1})},
            "tests": {"hypotheses": len(family), "tested_on_test_half": len(tested), "bh_pass": sum(passed),
                      "predicts": sum(entry["out"]["verdict"] == "predicts" for entry in family), "q": FDR_Q,
                      "permutations": PERMUTATIONS},
            "codes_x_car_state": codes.summary(),
            "constant_fields": constant,
            "unranked_fields_distinct": unranked,
            "ranking_enum": rank_enum,
            "ranking_numeric": rank_numeric,
            "lags_secondary": lags,
            "time_fields_age_s": {name: dist.summary() for name, dist in sorted(ages.items())},
            "enum_fields": enums,
            "numeric_fields": numbers,
        }

    def _decode_lags(self, rank_enum: list[dict[str, Any]], rank_numeric: list[dict[str, Any]],
                     samples: dict[str, list[tuple[float, str, Any, int]]], specs: dict[str, Any],
                     clusters: list[int]) -> dict[str, Any]:
        """The strongest fields with the car read DECODE_LAGS_S earlier, on the cohort present at every lag."""
        out: dict[str, Any] = {}
        for ranking, is_numeric in ((rank_enum, False), (rank_numeric, True)):
            for item in ranking[:LAG_FIELDS]:
                name, reading = item["field"], item["reading"]
                cohort = []
                for when, _, value, index in sorted(samples.get(name, []), key=lambda r: (r[0], r[3])):
                    if is_numeric and not _fit_value(value, specs[name]):
                        continue
                    car_values = [self._reading_at(when - lag, reading, is_numeric) for lag in DECODE_LAGS_S]
                    if all(v is not None for v in car_values):
                        cohort.append((clusters[index], value, car_values))
                result: dict[str, Any] = {"cohort_n": len(cohort)}
                scores: dict[str, float | None] = {}
                for k, lag in enumerate(DECODE_LAGS_S):
                    if is_numeric:
                        dx, dy = _cluster_diffs([(c, cv[k], float(v)) for c, v, cv in cohort])
                        r = _pearson(dx, dy) if len(dx) >= 2 else None
                        scores[str(lag)] = None if r is None else abs(r)
                    else:
                        car_labels = [cv[k] for _, _, cv in cohort]
                        scores[str(lag)] = (_association([label(v) for _, v, _ in cohort], car_labels)["u"]
                                            if len(set(car_labels)) >= 2 else None)
                if len(cohort) < DECODE_MIN_N or any(score is None for score in scores.values()):
                    result["note"] = "lag not comparable"
                else:
                    result["by_lag_s"] = {lag: r3(score) for lag, score in scores.items()}
                    result["best_lag_s"] = max(DECODE_LAGS_S, key=lambda lag: (scores[str(lag)], -lag))
                out[f"{name} x {reading}"] = result
        return out

    def run(self) -> dict[str, Any]:
        inventory = self.inventory()
        results = {
            "version": VERSION,
            "input": {k: self.meta.get(k) for k in ("version", "car_days", "cloud_days", "model", "energy",
                                                    "device_source", "app_version") if k in self.meta},
            "constants": {"fresh_s": {label(k) if k else "none": v for k, v in FRESH_S.items()},
                          "dup_window_s": DUP_WINDOW_S, "driving_min_kph": DRIVING_MIN_KPH,
                          "speed_lags_s": [SPEED_LAGS_S[0], SPEED_LAGS_S[-1], SPEED_LAGS_S[1] - SPEED_LAGS_S[0]],
"fix_max_age_s": FIX_MAX_AGE_S,
                          "speed_cmp_min_kph": SPEED_CMP_MIN_KPH, "value_lag_window_s": VALUE_LAG_WINDOW_S,
                          "value_lags_s": [VALUE_LAGS_S[0], VALUE_LAGS_S[-1], VALUE_LAGS_S[1] - VALUE_LAGS_S[0]],
                          "value_lag_max_tied_span_s": VALUE_LAG_MAX_TIED_SPAN_S,
                          "power_abs_tol_kw": POWER_ABS_TOL_KW, "power_rel_tol": POWER_REL_TOL,
                          "gps_lag_min_kph": GPS_LAG_MIN_KPH, "energy_window_km": ENERGY_WINDOW_KM,
                          "energy_odo_tol_km": ENERGY_ODO_TOL_KM, "energy_share_tol": ENERGY_SHARE_TOL,

                          "event_response_window_s": EVENT_RESPONSE_WINDOW_S,
                          "contamination_push_max_age_s": CONTAMINATION_PUSH_MAX_AGE_S,
                          "inferred_gap_rule": "power off on both sides, same SoC, same plug codes, same odometer"},
            "inventory": inventory,
            "soc": self.soc(),
            "odometer": self.odometer(),
            "speed": self.speed(),
            "vehicle_on": self.power_state(),
            "plug_charging": self.plug_charging(),
            "charge_power": self.charge_power(),
            "time_to_full": self.time_to_full(),
            "position": self.position(),
            "energy": self.energy(),
            "hvac_on": self.hvac(),
            "car_reference": self.car_reference(),
            "pushes": self.pushes(),
            "polls": self.polls(),
            "online_latency": self.online_latency(),
            "poll_contamination": self.contamination(),
            "freeze_and_stale": self.freeze_and_stale(),
            "decode": self.decode(),
        }
        results["limits"] = self.limits(inventory["car_keys"], inventory["cloud_keys"])
        return results


_RANK = {"predicts": 2, "observation": 1, "no association": 0}
DECODE_NOTE = (
    "Pairing: a raw line is paired with the car at its own sample time when it carries one (time, timestamp, "
    "updateTime, gpsTimeStamp) and at its receive time only when it has no sample-time path at all; a reply "
    "whose sample time the exporter marked stale (BYD's 0, null) or that lies more than 5 min from its receive "
    "time is not paired. Pairs in inferred parked gaps are counted per field and kept out. The car's freshness "
    "rules are the rest of the report's. "
    "Split: the paired lines' time clusters (10 min apart) alternate between a train half (even) and a test "
    "half (odd). Fields: integer, enum and boolean fields (by their table kind) with at most 20 values get a "
    "contingency table against every car reading; float fields, and integer ones with more values, a fit "
    "raw = slope * car + offset (pyBYD's sentinels -999, -129, and -1 where the range starts there, are not "
    "fitted, row by row). "
    "Family: every examined (field, reading) pair with n >= 10 pairs and at least two car values is one "
    "hypothesis. Each field's best reading is chosen on the train half only (the train gates first, then U or "
    "|r|), and only it is tested, on the test half only, if it passed the train gates: for an enum field U >= "
    "0.3 and lambda >= 0.3 on supported train pairs (a raw value is supported when seen in 3 clusters and 3 "
    "runs) with 3 episodes of each car value; for a fit |r| >= 0.8 of first differences within clusters, with 3 "
    "car-state episodes and 3 clusters. The test is a permutation (2000 draws, seeded): enum fields rotate the "
    "raw series inside each test cluster and then as a whole against the car's, fits shift the raw series in "
    "time within each test cluster and correlate first differences. Every other hypothesis, "
    "including one that fails a gate, has p = 1 and stays in the family. Benjamini-Hochberg runs over the "
    "whole family at q = 0.05. "
    "Verdict: 'predicts' needs the BH pass, the train gates (with repeat support), at least 3 test clusters, "
    "and the held-out check on the test half: an enum mapping learned on the train half reaching a balanced "
    "accuracy at least 0.2 above the majority-class predictor's (1 / car values) on at least half of the test "
    "pairs, or a fit with the train slope reaching r >= 0.8 and a smaller mean absolute error than the train "
    "mean. A hypothesis that meets the gates or the effect thresholds otherwise is an 'observation', the rest "
    "'no association'. All thresholds apply to unrounded values. Lags (the car read 30, 60, 120 s earlier) are "
    "secondary and compared on the pairs present at every lag."
)


def _episodes(samples: Iterable[tuple[float, str]]) -> Counter[str]:
    """Runs of one value, per value: a new run at every change and after a gap of CLUSTER_GAP_S or more."""
    runs: Counter[str] = Counter()
    previous: tuple[float, str] | None = None
    for t, value in sorted(samples, key=lambda s: (s[0], str(s[1]))):
        if previous is None or value != previous[1] or t - previous[0] >= CLUSTER_GAP_S:
            runs[value] += 1
        previous = (t, value)
    return runs


def _global_clusters(times: list[float]) -> list[int]:
    """The time cluster of each paired line (by index): a new cluster after a gap of CLUSTER_GAP_S or more."""
    order = sorted(range(len(times)), key=lambda i: (times[i], i))
    out = [0] * len(times)
    cluster = 0
    for k, i in enumerate(order):
        if k and times[i] - times[order[k - 1]] >= CLUSTER_GAP_S:
            cluster += 1
        out[i] = cluster
    return out


def _association(raw: list[str], car: list[str]) -> dict[str, Any]:
    """Unrounded association of raw labels with car labels: U, lambda, purity, baseline, the table and modes."""
    n = len(raw)
    cells: Counter[tuple[str, str]] = Counter(zip(raw, car))
    by_raw: Counter[str] = Counter(raw)
    by_car: Counter[str] = Counter(car)
    out: dict[str, Any] = {"n": n, "u": None, "lambda": None, "purity": None, "baseline": None, "rows": {},
                           "modal": {}}
    if not n:
        return out
    entropy = -sum(k / n * math.log(k / n) for k in by_car.values())
    information = sum(k / n * math.log(k * n / (by_raw[r] * by_car[c])) for (r, c), k in cells.items())
    corrected = information - (len(by_raw) - 1) * (len(by_car) - 1) / (2 * n)
    table: dict[str, dict[str, int]] = {}
    for (r, c), k in sorted(cells.items()):
        table.setdefault(r, {})[c] = k
    modal = {r: max(sorted(cols.items()), key=lambda item: item[1]) for r, cols in table.items()}
    purity = sum(k for _, k in modal.values()) / n
    baseline = max(by_car.values()) / n
    out.update(u=max(0.0, corrected) / entropy if entropy > 0 else None,
               **{"lambda": (purity - baseline) / (1 - baseline) if baseline < 1 else None},
               purity=purity, baseline=baseline, rows=table,
               modal={r: {"car": c, "share": r3(k / by_raw[r])} for r, (c, k) in modal.items()})
    return out


def _effect(a: dict[str, Any]) -> bool:
    return (a["n"] >= DECODE_MIN_N and a["u"] is not None and a["u"] >= DECODE_MIN_U
            and a["lambda"] is not None and a["lambda"] >= DECODE_MIN_LAMBDA)


def _rounded(a: dict[str, Any]) -> dict[str, Any]:
    return {"n": a["n"], **{k: None if a[k] is None else r3(a[k]) for k in ("u", "lambda", "purity", "baseline")}}


def _enum_hypothesis(rows: list[tuple[float, str, Any, int]],
                     readings: list[tuple[dict[str, str], dict[str, float]]], clusters: list[int], reading: str,
                     spec: Any) -> dict[str, Any] | None:
    """A raw enum field against one car reading: support, train gates and the halves; None if not a hypothesis."""
    pairs = [(t, state, label(value), readings[i][0][reading], clusters[i]) for t, state, value, i in rows
             if reading in readings[i][0]]
    if len(pairs) < DECODE_MIN_N or len({p[3] for p in pairs}) < 2:
        return None
    clusters_by_raw: dict[str, set[int]] = {}
    for _, _, raw, _, cluster in pairs:
        clusters_by_raw.setdefault(raw, set()).add(cluster)
    raw_runs = _episodes((t, raw) for t, _, raw, _, _ in pairs)
    supported = sorted(raw for raw, seen in clusters_by_raw.items()
                       if len(seen) >= MIN_EPISODES and raw_runs[raw] >= MIN_EPISODES)
    kept = [p for p in pairs if p[2] in supported]
    car_runs = _episodes((t, car) for t, _, _, car, _ in kept)
    repeated = len(car_runs) >= 2 and min(car_runs.values()) >= MIN_EPISODES
    train = [p for p in kept if p[4] % 2 == 0]
    test = [p for p in pairs if p[4] % 2 == 1]
    on_train = _association([p[2] for p in train], [p[3] for p in train])
    every = _association([p[2] for p in pairs], [p[3] for p in pairs])
    train_ok = len({p[2] for p in train}) >= 2 and _effect(on_train)
    gate = train_ok and repeated
    out: dict[str, Any] = {
        "n": len(pairs), "n_train": len(train), "n_test": len(test), "test_clusters": len({p[4] for p in test}),
        **_units([(t, state) for t, state, _, _, _ in pairs]),
        "states": _states(Counter(state for _, state, _, _, _ in pairs)),
        "raw_values_supported": supported,
        "clusters_by_raw_value": {raw: len(seen) for raw, seen in sorted(clusters_by_raw.items())},
        "raw_runs_by_value": dict(sorted(raw_runs.items())), "episodes_by_car_value": dict(sorted(car_runs.items())),
        "all_pairs": _rounded(every), "train": _rounded(on_train), "train_gate": train_ok, "repeated": repeated,
        "rows": every["rows"], "modal": every["modal"], "bh_pass": False,
        "verdict": "observation" if gate or _effect(every) else "no association",
    }
    work = {"gate": gate, "score": on_train["u"] or 0.0, "train": [(p[2], p[3]) for p in train],
            "test_x": [p[3] for p in test], "test_y": [p[2] for p in test], "test_clusters": [p[4] for p in test]}
    return {"out": out, "work": work}


def _fit_value(value: Any, spec: Any) -> bool:
    """Whether a raw value is fitted: a number that is not one of pyBYD's "no reading" sentinels."""
    return isnum(value) and value not in FIT_SENTINELS and not (value == -1 and spec.low == -1)


def _cluster_diffs(points: list[tuple[int, float, float]]) -> tuple[list[float], list[float]]:
    """First differences (car, raw) of consecutive points within the same cluster."""
    dx: list[float] = []
    dy: list[float] = []
    for (c1, x1, y1), (c2, x2, y2) in zip(points, points[1:]):
        if c1 == c2:
            dx.append(x2 - x1)
            dy.append(y2 - y1)
    return dx, dy


def _least_squares(xs: list[float], ys: list[float]) -> tuple[float, float] | None:
    n = len(xs)
    if n < 2:
        return None
    mx, my = sum(xs) / n, sum(ys) / n
    sxx = sum((x - mx) ** 2 for x in xs)
    if sxx <= 0:
        return None
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sxx
    return slope, my - slope * mx


def _numeric_hypothesis(rows: list[tuple[float, str, Any, int]],
                        readings: list[tuple[dict[str, str], dict[str, float]]], clusters: list[int], reading: str,
                        spec: Any) -> dict[str, Any] | None:
    """A numeric field against one car number: differences within clusters, train gates, the halves."""
    points = [(t, state, clusters[i], readings[i][1][reading], float(value)) for t, state, value, i in rows
              if reading in readings[i][1] and _fit_value(value, spec)]
    if (len(points) < DECODE_MIN_N or len({p[3] for p in points}) < 2 or len({p[4] for p in points}) < 2):
        return None
    fit = Fit()
    for t, state, _, x, y in points:
        fit.add(x, y, t, state)
    out = fit.summary()
    train = [p for p in points if p[2] % 2 == 0]
    test = [p for p in points if p[2] % 2 == 1]
    dx_train, dy_train = _cluster_diffs([(c, x, y) for _, _, c, x, y in train])
    dx_all, dy_all = _cluster_diffs([(c, x, y) for _, _, c, x, y in points])
    r_train = _pearson(dx_train, dy_train) if len(dx_train) >= 2 else None
    r_all = _pearson(dx_all, dy_all) if len(dx_all) >= 2 else None
    episodes = sum(_episodes((t, state) for t, state, _, _, _ in points).values())
    n_clusters = len({p[2] for p in points})
    repeated = episodes >= MIN_EPISODES and n_clusters >= MIN_EPISODES
    train_ok = len(train) >= DECODE_MIN_N and r_train is not None and abs(r_train) >= DECODE_MIN_R
    line = _least_squares([p[3] for p in train], [p[4] for p in train])
    gate = train_ok and repeated and line is not None
    effect = r_all is not None and len(points) >= DECODE_MIN_N and abs(r_all) >= DECODE_MIN_R
    by_cluster: dict[int, list[tuple[float, float]]] = {}
    for _, _, c, x, y in test:
        by_cluster.setdefault(c, []).append((x, y))
    out.update(n_train=len(train), n_test=len(test), test_clusters=len(by_cluster), episodes=episodes,
               clusters=n_clusters, r_diff_train=None if r_train is None else r3(r_train),
               r_diff_all=None if r_all is None else r3(r_all), train_gate=train_ok, repeated=repeated,
               slope_train=None if line is None else r3(line[0]), offset_train=None if line is None else r3(line[1]),
               bh_pass=False, verdict="observation" if gate or effect else "no association")
    work = {"gate": gate, "score": abs(r_train) if r_train is not None else 0.0, "line": line,
            "train_mean": sum(p[4] for p in train) / len(train) if train else None,
            "test_points": [(x, y) for _, _, _, x, y in test], "test_clusters_xy": list(by_cluster.values())}
    return {"out": out, "work": work}


def _pearson(xs: list[float], ys: list[float]) -> float | None:
    n = len(xs)
    if n < 2:
        return None
    mx, my = sum(xs) / n, sum(ys) / n
    sxx = sum((x - mx) ** 2 for x in xs)
    syy = sum((y - my) ** 2 for y in ys)
    if sxx <= 0 or syy <= 0:
        return None
    return sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / math.sqrt(sxx * syy)


def _information(x: list[int], y: list[int]) -> float:
    """Mutual information of two coded label series (n and both margins are the same under a rotation)."""
    n = len(x)
    cells = Counter(zip(x, y))
    bx, by = Counter(x), Counter(y)
    return sum(k / n * math.log(k * n / (bx[a] * by[b])) for (a, b), k in cells.items())


def _codes(values: list[Any]) -> list[int]:
    index: dict[Any, int] = {}
    return [index.setdefault(v, len(index)) for v in values]


def _enum_shift_p(car: list[str], raw: list[str], clusters: list[int], key: str) -> float:
    """Permutation p-value of the mutual information of the raw labels against the car's over the test pairs.

    Each of PERMUTATIONS draws (seeded by ``key``) rotates the raw series inside every test cluster by its
    own offset and then rotates the whole series by a tenth to nine tenths of its length. Both keep the
    raw series' own runs; the whole-series rotation also moves values between clusters, so a mapping that
    shows only between clusters is tested too, and the draws are not limited to the few hundred distinct
    rotations of one series.
    """
    n = len(raw)
    low = max(1, n // 10)
    if n - low < low:
        return 1.0
    segments: list[tuple[int, int]] = []
    for i, cluster in enumerate(clusters):
        if not segments or cluster != clusters[segments[-1][0]]:
            segments.append((i, i + 1))
        else:
            segments[-1] = (segments[-1][0], i + 1)
    x, y = _codes(car), _codes(raw)
    observed = _information(x, y)
    rng = random.Random(zlib.crc32(key.encode("utf-8")))
    hits = 0
    for _ in range(PERMUTATIONS):
        rotated: list[int] = []
        for start, stop in segments:
            part = y[start:stop]
            k = rng.randrange(len(part))
            rotated += part[k:] + part[:k]
        k = rng.randint(low, n - low)
        hits += _information(x, rotated[k:] + rotated[:k]) >= observed - 1e-12
    return (1 + hits) / (1 + PERMUTATIONS)


def _numeric_shift_p(clusters_xy: list[list[tuple[float, float]]], key: str) -> float:
    """Permutation p-value of |r| of first differences, the raw series shifted in time within each test cluster.

    Each draw shifts every cluster's raw series by its own offset (at least a tenth of the cluster,
    either way) and pairs only the overlap, so no gap is crossed and no level jump is created.
    """
    def statistic(shifts: list[int]) -> float:
        dx: list[float] = []
        dy: list[float] = []
        for points, k in zip(clusters_xy, shifts):
            xs = [p[0] for p in points]
            ys = [p[1] for p in points]
            if k > 0:
                xs, ys = xs[:-k], ys[k:]
            elif k < 0:
                xs, ys = xs[-k:], ys[:k]
            dx += [b - a for a, b in zip(xs, xs[1:])]
            dy += [b - a for a, b in zip(ys, ys[1:])]
        r = _pearson(dx, dy)
        return 0.0 if r is None else abs(r)

    observed = statistic([0] * len(clusters_xy))
    options = []
    for points in clusters_xy:
        size = len(points)
        low = max(1, size // 10)
        options.append([k for k in range(-(size - 3), size - 2) if abs(k) >= low] or [0])
    if all(o == [0] for o in options):
        return 1.0
    rng = random.Random(zlib.crc32(key.encode("utf-8")))
    hits = sum(statistic([rng.choice(o) for o in options]) >= observed - 1e-12 for _ in range(PERMUTATIONS))
    return (1 + hits) / (1 + PERMUTATIONS)


def _benjamini_hochberg(pvalues: list[float], q: float) -> list[bool]:
    """Which p-values pass Benjamini-Hochberg at false discovery rate ``q``."""
    m = len(pvalues)
    order = sorted(range(m), key=lambda i: (pvalues[i], i))
    cut = 0
    for rank, i in enumerate(order, 1):
        if pvalues[i] <= rank / m * q:
            cut = rank
    passed = set(order[:cut])
    return [i in passed for i in range(m)]


def _enum_held_out(work: dict[str, Any]) -> dict[str, Any]:
    """The train half's value-to-reading mapping on the test half: coverage and balanced accuracy."""
    votes: dict[str, Counter[str]] = {}
    for raw, car in work["train"]:
        votes.setdefault(raw, Counter())[car] += 1
    mapping = {raw: max(sorted(c.items()), key=lambda item: item[1])[0] for raw, c in votes.items()}
    test = list(zip(work["test_y"], work["test_x"]))
    covered = [(raw, car) for raw, car in test if raw in mapping]
    out: dict[str, Any] = {"test_n": len(test), "covered": len(covered), "coverage": share(len(covered), len(test))}
    if not covered:
        out.update(balanced_accuracy=None, majority_baseline=None, accuracy=None, majority_share=None, **{
            "pass": False})
        return out
    by_car: Counter[str] = Counter(car for _, car in covered)
    hits: Counter[str] = Counter(car for raw, car in covered if mapping[raw] == car)
    balanced = sum(hits[c] / by_car[c] for c in by_car) / len(by_car)
    baseline = 1 / len(by_car)
    coverage = len(covered) / len(test)
    out.update(balanced_accuracy=r3(balanced), majority_baseline=r3(baseline),
               accuracy=r3(sum(hits.values()) / len(covered)), majority_share=r3(max(by_car.values()) / len(covered)),
               **{"pass": len(by_car) >= 2 and balanced - baseline >= HELD_OUT_MIN_GAIN
                  and coverage >= HELD_OUT_MIN_COVERAGE})
    return out


def _numeric_held_out(work: dict[str, Any]) -> dict[str, Any]:
    """The train half's line on the test half: r and mean absolute error against the train mean's."""
    slope, offset = work["line"]
    points = work["test_points"]
    out: dict[str, Any] = {"test_n": len(points)}
    if len(points) < 2:
        out.update(r=None, mae=None, mae_train_mean=None, **{"pass": False})
        return out
    predicted = [slope * x + offset for x, _ in points]
    actual = [y for _, y in points]
    r = _pearson(predicted, actual)
    mae = sum(abs(p - a) for p, a in zip(predicted, actual)) / len(points)
    mae_mean = sum(abs(work["train_mean"] - a) for a in actual) / len(points)
    out.update(r=None if r is None else r3(r), mae=r3(mae), mae_train_mean=r3(mae_mean),
               **{"pass": r is not None and r >= HELD_OUT_MIN_R and mae < mae_mean})
    return out


def _per_window(windows: dict[float, list[tuple[float, float | None, float | None]]],
                max_age: float) -> dict[str, Any]:
    """Ratios per distinct 50 km window (keyed by its ending odometer, not shown): the median ratio
    of the polls in each window whose car endpoint is at most max_age old, and the median of those."""
    rows = []
    for polls in windows.values():
        keep = [x for x in polls if x[0] <= max_age]
        life = sorted(x[1] for x in keep if x[1] is not None)
        rem = sorted(x[2] for x in keep if x[2] is not None)
        if keep:
            rows.append({"polls": len(keep), "ratio_lifetime": r3(quantile(life, 0.5)) if life else None,
                         "ratio_remaining_drop": r3(quantile(rem, 0.5)) if rem else None})
    rows.sort(key=lambda x: (x["ratio_lifetime"] is None, x["ratio_lifetime"] or 0, x["polls"]))
    life = sorted(x["ratio_lifetime"] for x in rows if x["ratio_lifetime"] is not None)
    rem = sorted(x["ratio_remaining_drop"] for x in rows if x["ratio_remaining_drop"] is not None)
    return {"windows": len(rows), "per_window": rows,
            "median_of_window_ratios_lifetime": r3(quantile(life, 0.5)) if life else None,
            "median_of_window_ratios_remaining_drop": r3(quantile(rem, 0.5)) if rem else None}


def _better_rule(hits: Counter[str], pairs: int) -> str | None:
    """Which of round-half-up and floor matches the car more often (None without pairs)."""
    if not pairs:
        return None
    up, down = hits["round_half_up"], hits["floor"]
    if up == down:
        return "tie (cloud values are whole numbers)" if hits["exact"] == up else "tie"
    return "round_half_up" if up > down else "floor"


def share_f(part: float, whole: float) -> float | None:
    return r3(part / whole) if whole > 0 else None


# --- the report ------------------------------------------------------------------------------------


def _states_text(states: dict[str, int]) -> str:
    return ", ".join(f"{k} {v}" for k, v in states.items()) or "-"


def _units_text(s: dict[str, Any]) -> str:
    text = f"{s.get('days', 0)} / {s.get('clusters', 0)}"
    return text + (f" / {s['drives']}" if "drives" in s else "")


def _cell(value: Any) -> str:
    return "-" if value is None else str(value)


def md_dists(title: str, items: dict[str, dict[str, Any]]) -> list[str]:
    lines = [f"**{title}**", "",
             "| quantity | n | days / clusters / drives | car states | min | p5 | p25 | median | p75 | p95 | max "
             "| mean |",
             "|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for name, s in items.items():
        if not s.get("n"):
            lines.append(f"| {name} | 0 | 0 | - | no samples | | | | | | | |")
            continue
        q = " | ".join(_cell(s.get(k)) for k in ("min", "p5", "p25", "median", "p75", "p95", "max", "mean"))
        lines.append(f"| {name} | {s['n']} | {_units_text(s)} | {_states_text(s['states'])} | {q} |")
        for state, b in s.get("by_state", {}).items():
            lines.append(f"| - {state} | {b['n']} | | | | {b['p5']} | | {b['median']} | | {b['p95']} | | "
                         f"{b['mean']} |")
    lines.append("")
    return lines


def md_tally(title: str, s: dict[str, Any]) -> list[str]:
    lines = [f"**{title}** (n = {s['n']}, days / clusters / drives = {_units_text(s)}, "
             f"car states: {_states_text(s['states'])})", ""]
    if not s["n"]:
        return lines + ["No paired samples.", ""]
    cols = sorted({c for row in s["rows"].values() for c in row})
    lines.append("| cloud \\ car | " + " | ".join(cols) + " |")
    lines.append("|---" * (len(cols) + 1) + "|")
    for r, row in s["rows"].items():
        lines.append(f"| {r} | " + " | ".join(str(row.get(c, 0)) for c in cols) + " |")
    lines.append("")
    return lines


def md_fit(title: str, s: dict[str, Any]) -> list[str]:
    body = ", ".join(f"{k} {s[k]}" for k in ("slope", "offset", "r", "implied_kwh_at_100_pct") if k in s)
    return [f"**{title}**: n = {s['n']}, days / clusters / drives = {_units_text(s)}, "
            f"car states: {_states_text(s['states'])}; "
            f"{body or s.get('note', '')}", ""]


def md_kv(title: str, values: dict[str, Any]) -> list[str]:
    return [f"**{title}**: " + (", ".join(f"{k} {v}" for k, v in values.items()) or "none"), ""]


def render(results: dict[str, Any]) -> str:
    lines: list[str] = ["# Car against cloud", "",
                        "Aggregates only. Every table gives n (paired samples), the distinct days they span, "
                        "the clusters (samples >= 10 min apart are separate clusters), the drives behind driving "
                        "samples, and the car-state split. Differences are cloud minus car.", ""]
    inv = results["inventory"]
    lines += render_regime(results)
    lines += ["## Constants", "", "```json", json.dumps(results["constants"], sort_keys=True), "```", ""]
    lines += ["## 1. Inventory", ""]
    lines += md_kv("Days covered", inv["days"])
    lines += md_kv("Car rows", {k: v for k, v in inv["car_rows"].items() if k != "by_cadence"})
    lines += md_kv("Car rows by cadence", inv["car_rows"]["by_cadence"])
    lines += md_kv("Skipped lines", inv["skipped_lines"])
    lines += md_kv("Car timeline fresh share", {"whole car window": inv["car_fresh_share"],
                                                "cloud window": inv["car_fresh_share_in_cloud_window"]})
    lines += md_kv("Car hours by state (car window)", inv["car_hours_by_state"])
    lines += md_kv("Car hours by state (cloud window)", inv["cloud_window_hours_by_car_state"])
    lines += ["**Cloud rows by source:kind:detail**", "", "| bucket | rows |", "|---|---|"]
    lines += [f"| {k} | {v} |" for k, v in inv["cloud_rows"]["by_source_kind_detail"].items()] + [""]
    lines += md_kv("Car keys (rows carrying each)", inv["car_keys"])
    for ch, keys in inv["cloud_keys"].items():
        lines += md_kv(f"Cloud keys on {ch} (value/null)", {k: f"{v['value']}/{v['null']}" for k, v in keys.items()})
    lines += md_dists("Cloud row interval (s), by car state at the later row", inv["cloud_interval_s_by_car_state"])
    lines += ["**Cloud rows per hour of car state (cloud window)**", "",
              "| group | " + " | ".join(STATES) + " |", "|---" * (len(STATES) + 1) + "|"]
    for g, per in inv["cloud_rows_per_car_state_hour"].items():
        lines.append(f"| {g} | " + " | ".join(
            f"{per[s]['rows']} rows / {per[s]['hours']} h = {_cell(per[s]['per_hour'])}"
                                              for s in STATES) + " |")
    lines.append("")

    lines += ["## 2. SoC", ""]
    for name, s in results["soc"].items():
        lines += [f"### {name}", "", f"cloud values {s['cloud_values']}, with decimals {s['with_decimals']}, "
                  f"no fresh car value {s['no_fresh_car_value']}, "
                  f"lag unmatched {s['lag_unmatched']}, negative-lag share {_cell(s['lag_negative_share'])}", ""]
        lines += md_dists("SoC", {"cloud - car (pct)": s["diff_cloud_minus_car"],
                                  "lag (s), car driving or charging": s["lag_s_while_changing"],
                                  "lag upper bound (s)": s["lag_upper_s_while_changing"]})
        lines += md_kv("Best-fit constant lag while driving or charging", s["best_fit_lag_while_changing"])
        rr = s["rounding_rules"]
        lines += [f"**Rounding rules**: {rr['pairs']} pairs (days / clusters / drives = {_units_text(rr)}); "
                  "rule evaluations agreeing with the car: "
                  + ", ".join(f"{k} {v} of {rr['pairs']}" for k, v in rr["rule_evaluations_agreeing"].items()), ""]
        lines += md_kv("Better rounding rule", {"rule": s["better_rounding_rule"]})
    lines += ["## 3. Odometer", "", results["odometer"]["note"], ""]
    for name, s in results["odometer"].items():
        if name == "note":
            continue
        lines += [f"### {name}", "", f"cloud values {s['cloud_values']}, with decimals {s['with_decimals']}, "
                  f"no fresh car value {s['no_fresh_car_value']}, lag unmatched {s['lag_unmatched']}, "
                  f"negative-lag share {_cell(s['lag_negative_share'])}", ""]
        lines += md_dists("Odometer", {"cloud - car (km)": s["diff_cloud_minus_car_km"],
                                       "lag (s) while driving": s["lag_s_while_driving"]})
        lines += md_kv("Best-fit constant lag while driving", s["best_fit_lag_while_driving"])
        lines += md_tally("floor(cloud) equals car x asleep flag", s["floor_equals_car_x_asleep_flag"])
    lines += ["## 4. Speed", ""]
    for ch, s in results["speed"].items():
        if ch == "car_speed_vs_gps_speed":
            continue
        b = s["best_lag"]
        lines += [f"### {ch}", ""]
        lines += md_dists("Speed", {"cloud - car speed_kph": s["diff_cloud_minus_car_speed_kph"],
                                    "cloud - car GPS speed": s["diff_cloud_minus_car_gps_speed_kph"]})
        lines += [f"**Best-fit lag** (cloud samples while driving: n = {b['n_driving_samples']}, "
                  f"days / clusters / drives = {_units_text(b)}): "
                  f"best lag {_cell(b['best_lag_s'])} s with MAE {_cell(b['mae_at_best_kph'])} km/h "
                  f"(n {b['n_at_best']}); MAE at lag 0 {_cell(b['mae_at_lag0_kph'])} km/h (n {b['n_at_lag0']}).", ""]
    g = results["speed"]["car_speed_vs_gps_speed"]
    lines += md_fit(f"Fit car speed on GPS speed (driving segments {g['driving_segments']})", g["fit_car_on_gps"])
    lines += md_dists("Car speed / GPS speed by GPS speed band (km/h)", g["ratio_car_over_gps_by_gps_speed_band"])
    lines += md_dists("Car speed_kph against car GPS speed", {"car - GPS (km/h)": g["diff_car_minus_gps_kph"]})
    lines += ["## 5. vehicle_on, online, asleep", "", results["vehicle_on"]["note"], ""]
    for ch, s in results["vehicle_on"].items():
        if ch == "note":
            continue
        lines += [f"### {ch}", ""]
        for name, t in s.items():
            if name != "callouts":
                lines += md_tally(name, t)
        for definition, calls in s["callouts"].items():
            lines += md_kv(f"Callouts with car on = {definition} (counts)", calls)
    lines += ["## 6. Plug and charging state", "", results["plug_charging"]["note"], ""]
    for ch, by_label in results["plug_charging"].items():
        if ch in ("note", "transitions"):
            continue
        for wake, tables in by_label.items():
            if wake == "asleep_runs":
                continue
            lines += [f"### {ch} [{wake}]", ""]
            for name, t in tables.items():
                lines += md_tally(f"{name} [{wake}]", t)
        runs = by_label["asleep_runs"]
        lines += md_kv(f"{ch} asleep runs", {k: v for k, v in runs.items() if k != "both_flags_x_run_kind"})
        lines += md_tally(f"{ch} asleep rows: both flags right x run kind", runs["both_flags_x_run_kind"])
    lines += ["### Transition lags (first cloud row showing the new state)", ""]
    lines += md_kv("Cloud rows per signal (push = smartCharge; push_vehicleInfo_power = vehicleInfo "
                   "charge_power_kw > 0, charge transitions only)",
                   results["plug_charging"]["transitions"]["cloud_rows"])
    for name, s in results["plug_charging"]["transitions"].items():
        if name == "cloud_rows":
            continue
        lines += [f"**{name}**: car transitions {s['car_transitions']}, time-uncertain across a gap (not "
                  f"measured, but they close windows) {s['time_uncertain_across_a_gap_not_measured']}", ""]
        for g_name in ("poll", "push", "push_vehicleInfo_power"):
            if g_name not in s:
                continue
            gs = s[g_name]
            cats = "; ".join(f"{c}: {v['events']} events ({v['shown']} shown, {v['not_shown']} not)"
                             for c, v in gs["categories"].items())
            lines += [f"- {g_name}: {cats}; right-censored (no row before the window closed), by what closed "
                      f"it: {gs['right_censored_no_row_before_window_closed'] or 'none'}; old state shown after "
                      f"the transition {gs['old_state_shown_after_transition']}", ""]
            lines += md_dists(f"{name} {g_name} delay (s)", {"delay": gs["delay_s"]})
    lines += ["## 7. Charge power", ""]
    for ch, s in results["charge_power"].items():
        w = s["while_car_charging"]
        lines += [f"### {ch}", ""]
        ge = w["cloud_ge_car"]
        lines += [f"Both sides have {w['resolution_kw']} kW resolution. Cloud >= car in {ge['count']} of "
                  f"{ge['n']} pairs.", ""]
        lines += md_dists("While the car is charging", {"|cloud - car| (kW)": w["abs_diff_kw"],
                                                        "cloud - car (kW)": w["diff_cloud_minus_car_kw"]})
        lines += md_fit("Fit cloud on car", w["fit_cloud_on_car"])
        lines += md_kv("Within tolerance share", {"share": w["within_tolerance_share"]})
        lines += md_kv("Cloud value while the car is charging, by asleep flag", w["cloud_value_kinds_by_asleep_flag"])
        lines += md_dists("Cloud charge_power_kw > 0 while the car is not charging",
                          {"not charging": s["cloud_positive_while_car_not_charging"],
                           "car unplugged": s["cloud_positive_while_car_unplugged"]})
        lines += md_kv("Cloud numbers while the car is not charging, by state",
                       s["cloud_numbers_while_car_not_charging_by_state"])
    lines += ["## 8. Time to full", ""]
    items: dict[str, Any] = {}
    for k, v in results["time_to_full"].items():
        items[k] = v["diff_cloud_minus_car_min"]
        for wake, d in v["by_asleep_flag"].items():
            items[f"{k} [{wake}]"] = d
    lines += md_dists("cloud - car charge_remaining_min (min)", items)
    lines += ["## 9. Position", "", results["position"]["note"], ""]
    lines += md_kv("Agreement with an earlier car fix: share at 0 m while parked, side by side",
                   results["position"]["parked_zero_m_share_side_by_side"])
    for name, s in results["position"].items():
        if name in ("note", "parked_zero_m_share_side_by_side"):
            continue
        lines += [f"### {name}", ""]
        lines += md_kv("Counts", s["counts"])
        lines += md_dists("Distances", {"gps_dist_m, car stationary": s["gps_dist_m_car_stationary"],
                                        "gps_dist_m, car moving": s["gps_dist_m_car_moving"],
                                        "gps_dist_m, car speed unknown": s["gps_dist_m_car_speed_unknown"],
                                        "implied lag (s), moving": s["implied_lag_s_moving"],
                                        "agreement with an earlier car fix (gps_last_dist_m, m), parked":
                                            s["parked_gps_last_dist_m"],
                                        "age of that earlier car fix (h)": s["parked_gps_last_age_h"]})
        lines += md_tally("gps_step_m zero or positive x car state", s["gps_step_x_car_state"])
        lines += md_kv("gps_step_m == 0 share", {"share": s["gps_step_zero_share"]})
    lines += ["## 10. Energy", "", results["energy"]["note"], ""]
    for ch in CHANNELS:
        s = results["energy"][ch]
        lines += [f"### {ch}", "",
                  f"cloud values {s['cloud_values']}, no car 50 km window {s['no_car_50km_window']}", ""]
        lines += md_dists("last50km_energy_kwh", {
            "cloud / car lifetime_kwh used": s["vs_lifetime_kwh"]["ratio_cloud_over_car"],
            "cloud - car lifetime_kwh used": s["vs_lifetime_kwh"]["diff_cloud_minus_car_kwh"],
            "cloud / car remaining_kwh drop": s["vs_remaining_kwh_drop_while_driving"]["ratio_cloud_over_car"],
            "cloud - car remaining_kwh drop": s["vs_remaining_kwh_drop_while_driving"]["diff_cloud_minus_car_kwh"],
            "age of the car reading (s)": s["car_reading_age_s"]})
        en = results["energy"]
        lines += [f"Odometer readings out of order (dropped): {en['odometer_readings_out_of_order_dropped']}; "
                  f"persistent backward steps: {en['odometer_backward_steps_persistent']}", ""]
        lines += [f"Windows crossing a backward odometer step (skipped): "
                  f"{s['windows_crossing_a_backward_odometer_step']}", ""]
        for basis, pw in s["per_50km_window"].items():
            lines += [f"Per 50 km window ({basis}): {pw['windows']} windows; median of window ratios: lifetime "
                      f"{_cell(pw['median_of_window_ratios_lifetime'])}, remaining drop "
                      f"{_cell(pw['median_of_window_ratios_remaining_drop'])}; (polls, lifetime ratio, remaining "
                      "ratio): " + (", ".join(f"({x['polls']}, {_cell(x['ratio_lifetime'])}, "
                                              f"{_cell(x['ratio_remaining_drop'])})" for x in pw["per_window"])
                                    or "none"), ""]
        e = results["energy"]["energy_distribution"][ch]
        lines += [f"energy_distribution: objects {e['objects']}, null {e['null']}, "
                  f"sum within tolerance of 100: {_cell(e['sum_within_tolerance_of_100_share'])}", ""]
        lines += md_dists("energy_distribution", {"sum": e["sum"], **e["ranges"]})
    lines += ["## 11. hvac_on", ""]
    for ch, t in results["hvac_on"].items():
        lines += md_tally(f"{ch} hvac_on x car state", t)
    ref = results["car_reference"]
    lines += ["## 12. Car reference facts", ""]
    lines += md_fit("remaining_kwh on soc_pct (kWh per SoC percent)", ref["kwh_per_soc_pct_fit_remaining_on_soc"])
    lines += md_dists("12 V by power level", ref["twelve_volt_by_power_level"])
    lines += md_dists("Tyre pressure (kPa) by slot (corner labels unconfirmed)",
                      {**ref["tyre_kpa_by_slot_labels_unconfirmed"], "spread of four": ref["tyre_kpa_spread_of_four"]})
    lines += md_dists("Outside temperature (C)", {"outside_temp_c": ref["outside_temp_c"]})
    lines += md_kv("Car keys never seen", {"keys": ", ".join(ref["car_keys_never_seen"]) or "none"})
    lines += md_kv("Door values", {d: json.dumps(c, sort_keys=True) for d, c in ref["door_values"].items()})
    lines += md_kv("Gear values", ref["gear_values"])
    lines += md_kv("Power level values", ref["power_level_values"])
    lines += md_kv("Lock values", ref["lock_values"])
    lines += render_extra(results)
    lines += render_decode(results["decode"])
    lines += ["## 13. Limits", "", "| quantity | side | reason |", "|---|---|---|"]
    lines += [f"| {x['quantity']} | {x['side']} | {x['reason']} |" for x in results["limits"]] + [""]
    return "\n".join(lines)


def render_regime(results: dict[str, Any]) -> list[str]:
    """The regime summary at the top: awake share and BYD 'unreachable' per period and half-day bin."""
    po = results["polls"]
    lines = ["## 0. Regime around the car-app registration", "", po["note"], ""]
    lines += md_kv("poll:ok awake share by period",
                   {k: f"{v['awake']}/{v['poll_ok']} = {_cell(v['awake_share'])}" for k, v in po["per_period"].items()})
    lines += ["| period | asleep polls | of which vehicle_on null | of which vehicle_unreachable true |",
              "|---|---|---|---|"]
    lines += [f"| {k} | {v['asleep_polls']} | {v['vehicle_on_null']} | {v['vehicle_unreachable_true']} |"
              for k, v in po["asleep_unreachable_check"].items()] + [""]
    lines += ["| half-day bin from registration | poll:ok | asleep share | vehicle_unreachable share |",
              "|---|---|---|---|"]
    lines += [f"| {b['bin']} | {b['poll_ok']} | {_cell(b['asleep_share'])} | {_cell(b['vehicle_unreachable_share'])} |"
              for b in po["half_day_bins_from_registration"]] + [""]
    lines += md_tally("Pushes x period", results["pushes"]["pushes_x_period"])
    lines += md_kv("Pushes inside the car observation window (first to last car row)",
                   results["pushes"]["pushes_in_car_observation_window"])
    lines += md_tally("Poll rows kind:detail x period", po["poll_rows_kind_detail_x_period"])
    fs = results["freeze_and_stale"]
    lines += ["## 0b. Frozen charging values while asleep", "", fs["note"], ""]
    lines += md_tally("Cloud SoC across adjacent asleep polls in the same observed plug episode",
                      fs["soc_freeze_adjacent_asleep_polls_same_observed_plug_episode"])
    lines += md_tally("Cloud SoC across filter-first pairs of asleep plugged-in polls",
                      fs["soc_freeze_filter_first_asleep_plugged_polls"])
    lines += md_dists("is_connected true while the car is unplugged: hours since the last observed unplug "
                      "(continuity not established)",
                      fs["connected_true_while_car_unplugged_hours_since_last_observed_unplug"])
    lines += md_kv("is_connected true while unplugged, no unplug observed before",
                   fs["connected_true_while_car_unplugged_no_unplug_observed"])
    return lines


def render_extra(results: dict[str, Any]) -> list[str]:
    """Sections 14-17: pushes, polls, online/asleep latency, poll contamination."""
    lines: list[str] = []
    pu = results["pushes"]
    lines += ["## 14. Pushes", "", pu["note"], ""]
    lines += md_tally("Pushes x period", pu["pushes_x_period"])
    lines += md_tally("Pushes x car state", pu["pushes_x_car_state"])
    lines += md_kv("vehicleInfo while the car is off and not moving, by state",
                   pu["vehicleInfo_while_car_off_not_moving_by_state"])
    lines += md_kv("Keys with values on those pushes", pu["vehicleInfo_while_car_off_keys_with_values"])
    lines += md_dists("vehicleInfo to the nearest remoteControl push (s)",
                      {"gap": pu["vehicleInfo_to_nearest_remoteControl_s"]})
    lines += md_kv("Share of vehicleInfo with that remoteControl at or before it",
                   {"share": pu["vehicleInfo_with_remoteControl_at_or_before_share"]})
    for event, per in pu["first_cloud_row_after_car_event"].items():
        lines += [f"**{event}**: car events {per['car_events']}", ""]
        items = {}
        for target, res in per.items():
            if target == "car_events":
                continue
            items[f"{target} (none within window: {res['none_within_window']})"] = res["delay_s"]
        lines += md_dists(f"First cloud row after {event} (s)", items)
    po = results["polls"]
    lines += ["## 15. Polls", ""]
    lines += [f"Periods split at the car-app registration ({po['registration_source']}) and the first car row.", ""]
    lines += md_dists("poll:ok interval (s) by period", {k: v["interval_s"] for k, v in po["per_period"].items()})
    lines += md_kv("poll:ok awake share by period",
                   {k: f"{v['awake']}/{v['poll_ok']} = {_cell(v['awake_share'])}" for k, v in po["per_period"].items()})
    lines += md_tally("Poll rows kind:detail x car state", po["poll_rows_kind_detail_x_car_state"])
    lines += md_tally("Poll rows kind:detail x period", po["poll_rows_kind_detail_x_period"])
    lines += ["## 16. online / asleep after car power changes", ""]
    lines += [f"Time-uncertain power changes across a gap (close windows, not measured): "
              f"{results['online_latency']['time_uncertain_power_changes_across_a_gap']}", ""]
    for ch, res in results["online_latency"].items():
        if ch == "time_uncertain_power_changes_across_a_gap":
            continue
        lines += [f"### {ch}", ""]
        for name, r in res.items():
            if name == "hours_since_car_power_off_when_cloud_says":
                lines += md_dists("Hours since the car powered off, car still off", r)
            else:
                lines += [f"**{name}**: car events {r['car_events']}; right-censored (no row before the window "
                          f"closed): {r['right_censored_no_row_before_window_closed'] or 'none'}; rows in the "
                          f"window but not shown {r['rows_in_window_but_not_shown']}", ""]
                lines += md_dists(f"{name} (s)", {"delay": r["delay_s"]})
    lines += ["## 17. Poll SoC and odometer: car-app contamination hint", ""]
    for key, res in results["poll_contamination"].items():
        lines += [f"### {key}", "", f"poll equals car while the newest earlier push differs: {res['hint_count']}", ""]
        lines += md_tally("poll vs car x newest earlier push", res["poll_vs_car_x_latest_push"])
        lines += md_tally("poll vs car x asleep", res["poll_vs_car_x_asleep"])
        lines += md_dists("Age of that push (h)", {"age": res["latest_push_age_h"]})
    return lines


def render_decode(de: dict[str, Any]) -> list[str]:
    """Section 18: raw BYD fields against the car."""
    lines = ["## 18. Decode: raw BYD fields against the car", "", de["note"], ""]
    lines += md_kv("Raw lines", {k: v for k, v in de["lines"].items() if k != "by_source_endpoint"})
    lines += md_kv("Raw lines by source:endpoint", de["lines"]["by_source_endpoint"])
    lines += md_kv("Clusters (train = even, test = odd)", de["clusters"])
    lines += md_kv("Hypotheses and tests", de["tests"])
    lines += md_tally("Failure codes (endpoint:code) x car state", de["codes_x_car_state"])
    lines += ["**Constant fields** (one value over the whole window)", "", "| field | value | lines |", "|---|---|---|"]
    lines += [f"| {name} | {c['value']} | {c['lines']} |" for name, c in de["constant_fields"].items()] + [""]
    lines += md_kv("Fields with more than 20 distinct non-numeric values (distinct count)",
                   de["unranked_fields_distinct"])
    lines += ["**Enum fields: the car reading each predicts best** (chosen on the train half)", "",
              "| field | car reading | verdict | p (test) | BH | n | train / test | test clusters | U train "
              "| lambda train | held-out balanced accuracy / baseline / coverage |",
              "|---|---|---|---|---|---|---|---|---|---|---|"]
    for x in de["ranking_enum"]:
        held = x["held_out"] or {}
        lines.append(f"| {x['field']} | {x['reading']} | {x['verdict']} | {_cell(x['p_value'])} | "
                     f"{'pass' if x['bh_pass'] else '-'} | {x['n']} | {x['n_train']} / {x['n_test']} | "
                     f"{x['test_clusters']} | {_cell(x['u_train'])} | {_cell(x['lambda_train'])} | "
                     f"{_cell(held.get('balanced_accuracy'))} / {_cell(held.get('majority_baseline'))} / "
                     f"{_cell(held.get('coverage'))} |")
    lines.append("")
    for x in de["ranking_enum"]:
        if x["verdict"] == "no association":
            continue
        table = de["enum_fields"][x["field"]]["hypotheses"][x["reading"]]
        lines += md_tally(f"{x['field']} x {x['reading']} ({x['verdict']}; supported raw values "
                          f"{json.dumps(table['raw_values_supported'])}; episodes per car value "
                          f"{json.dumps(table['episodes_by_car_value'], sort_keys=True)})", table)
    lines += ["**Numeric fields: the car number each fits best** (chosen on the train half; r of first "
              "differences within clusters)", "",
              "| field | car number | verdict | p (test) | BH | n | train / test | test clusters | r diff train "
              "| slope / offset train | held-out r / MAE / MAE of train mean |",
              "|---|---|---|---|---|---|---|---|---|---|---|"]
    for x in de["ranking_numeric"]:
        held = x["held_out"] or {}
        lines.append(f"| {x['field']} | {x['reading']} | {x['verdict']} | {_cell(x['p_value'])} | "
                     f"{'pass' if x['bh_pass'] else '-'} | {x['n']} | {x['n_train']} / {x['n_test']} | "
                     f"{x['test_clusters']} | {_cell(x['r_diff_train'])} | {_cell(x['slope_train'])} / "
                     f"{_cell(x['offset_train'])} | {_cell(held.get('r'))} / {_cell(held.get('mae'))} / "
                     f"{_cell(held.get('mae_train_mean'))} |")
    lines.append("")
    lines += ["**Lags (secondary)**: the strongest fields with the car read earlier, on the pairs present at "
              "every lag", "",
              "| field x reading | cohort n | " + " | ".join(f"{lag} s" for lag in DECODE_LAGS_S) + " | best lag |",
              "|---" * (len(DECODE_LAGS_S) + 3) + "|"]
    for name, lag in de["lags_secondary"].items():
        if "note" in lag:
            lines.append(f"| {name} | {lag['cohort_n']} | " + " | ".join("-" for _ in DECODE_LAGS_S)
                         + f" | {lag['note']} |")
        else:
            lines.append(f"| {name} | {lag['cohort_n']} | "
                         + " | ".join(_cell(lag["by_lag_s"][str(s)]) for s in DECODE_LAGS_S)
                         + f" | {lag['best_lag_s']} |")
    lines.append("")
    lines += md_dists("Times (_t fields): age at the raw line's t (s)", de["time_fields_age_s"])
    return lines


# --- entry point -----------------------------------------------------------------------------------


def compare(path: Path, out_dir: Path) -> dict[str, Any]:
    meta, car, cloud, skipped, raw = read_pairs(path)
    results = Analysis(meta, car, cloud, skipped, raw).run()
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "results.json").write_text(json.dumps(results, indent=1) + "\n", encoding="utf-8")
    (out_dir / "report.md").write_text(render(results), encoding="utf-8")
    return results


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("pairs", type=Path, help="the pairs-*.jsonl file")
    parser.add_argument("--out-dir", type=Path, required=True, help="where results.json and report.md go")
    args = parser.parse_args(argv)
    results = compare(args.pairs, args.out_dir)
    inv = results["inventory"]
    print(f"wrote results.json and report.md to {args.out_dir}: {inv['car_rows']['kept']} car rows, "
          f"{inv['cloud_rows']['total']} cloud rows, {inv['days']['both']} days with both")
    return 0


if __name__ == "__main__":
    sys.exit(main())
