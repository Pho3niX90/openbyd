"""The pure parts of the replay benchmark (bench/replay_worker.py) and its launcher's network guard
(bench/replay/launcher.py). Needs neither the backend nor Docker nor the replay server."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import textwrap
from pathlib import Path
from typing import Any

import pytest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "bench"))

import replay_worker as rw  # noqa: E402
import report  # noqa: E402

LAUNCHER = ROOT / "bench" / "replay" / "launcher.py"
VIN1, VIN2 = "TESTVIN0000000001", "TESTVIN0000000002"
ORIGIN = {"wall_unix_ms": 1_800_000_000_000, "start_at": 600, "speed": 1.0, "epoch_unix_ms": 1_799_999_400_000}
WALL0 = ORIGIN["wall_unix_ms"] / 1000


def at(t: float) -> float:
    """The unix time of replay second ``t`` in :data:`ORIGIN`."""
    return WALL0 + (t - ORIGIN["start_at"])


# ---------------------------------------------------------------------------
# Variants
# ---------------------------------------------------------------------------


def test_variant_table():
    assert list(rw.VARIANTS) == ["python_cipher", "rust_cipher", "rust_cipher_shadow"]
    python, rust, shadow = (rw.VARIANTS[k] for k in rw.VARIANTS)
    assert python.env == {"BYD_NATIVE_CIPHER": "false", "BYD_SHADOW_SAMPLE_RATE": "0"}
    assert rust.env == {"BYD_NATIVE_CIPHER": "true", "BYD_SHADOW_SAMPLE_RATE": "0"}
    assert shadow.env == {"BYD_NATIVE_CIPHER": "true", "BYD_SHADOW_SAMPLE_RATE": "1"}
    assert python.expect == {"bydconnect_byd_native_cipher": 0}
    assert rust.expect == shadow.expect == {"bydconnect_byd_native_cipher": 1}
    assert set(rw.QUICK_VARIANTS) <= set(rw.VARIANTS)


def test_python_variants_run_the_real_worker_through_the_guard():
    values = {"python": "py.exe", "launcher": "L.py", "guard_report": "g.json", "backend": "B"}
    for variant in rw.VARIANTS.values():
        assert rw.render_command(variant, values) == ["py.exe", "L.py", "--guard-report", "g.json", "--stdin-stop", "-m", "app.mqtt_worker"]


def test_a_native_worker_is_one_more_row():
    native = rw.Variant("rust_worker", "Rust worker", ("{backend}/bin/worker", "--metrics-port", "8081"), {"X": "1"})
    assert rw.render_command(native, {"python": "p", "launcher": "l", "guard_report": "g", "backend": "B"}) == [
        "B/bin/worker",
        "--metrics-port",
        "8081",
    ]


def test_interleaved_order():
    assert rw.interleaved(["a", "b", "c"], 2) == [("a", 1), ("b", 1), ("c", 1), ("a", 2), ("b", 2), ("c", 2)]


# ---------------------------------------------------------------------------
# Metrics
# ---------------------------------------------------------------------------

METRICS = textwrap.dedent(
    """\
    # HELP bydconnect_worker_event_loop_lag_seconds How late a one-second timer fired
    # TYPE bydconnect_worker_event_loop_lag_seconds histogram
    bydconnect_worker_event_loop_lag_seconds_bucket{le="0.005"} 80.0
    bydconnect_worker_event_loop_lag_seconds_bucket{le="0.01"} 90.0
    bydconnect_worker_event_loop_lag_seconds_bucket{le="0.05"} 99.0
    bydconnect_worker_event_loop_lag_seconds_bucket{le="0.1"} 100.0
    bydconnect_worker_event_loop_lag_seconds_bucket{le="+Inf"} 100.0
    bydconnect_worker_event_loop_lag_seconds_count 100.0
    bydconnect_worker_event_loop_lag_seconds_sum 0.9
    bydconnect_worker_hydrate_seconds_bucket{le="1.0",reason="forced"} 2.0
    bydconnect_worker_hydrate_seconds_bucket{le="+Inf",reason="forced"} 3.0
    bydconnect_worker_hydrate_seconds_count{reason="forced"} 3.0
    bydconnect_worker_hydrate_seconds_sum{reason="forced"} 4.5
    bydconnect_worker_hydrate_seconds_bucket{le="1.0",reason="moving"} 1.0
    bydconnect_worker_hydrate_seconds_bucket{le="+Inf",reason="moving"} 1.0
    bydconnect_worker_hydrate_seconds_count{reason="moving"} 1.0
    bydconnect_worker_hydrate_seconds_sum{reason="moving"} 0.5
    bydconnect_worker_slots 3.0
    bydconnect_byd_native_cipher 1.0
    bydconnect_openbyd_shadow_total{model="VehicleRealtimeData",result="match"} 7.0
    bydconnect_openbyd_shadow_total{model="ChargingStatus",result="mismatch"} 1.0
    weird_metric{a="x\\"y",b="c,d"} 2.5e+00
    """
)


def test_parse_prometheus():
    samples = rw.parse_prometheus(METRICS)
    assert rw.gauge(samples, "bydconnect_worker_slots") == 3.0
    assert rw.gauge(samples, "missing") is None
    assert samples["weird_metric"] == [({"a": 'x"y', "b": "c,d"}, 2.5)]
    assert rw.gauge(samples, "bydconnect_worker_hydrate_seconds_count", reason="moving") == 1.0
    assert rw.label_values(samples, "bydconnect_worker_hydrate_seconds", "reason") == ["forced", "moving"]


def test_histogram_quantiles_and_bounds():
    samples = rw.parse_prometheus(METRICS)
    lag = rw.histogram(samples, rw.LOOP_LAG)
    assert lag["count"] == 100 and lag["sum"] == pytest.approx(0.9)
    assert rw.histogram_quantile(lag, 0.5) == pytest.approx(0.005 * 50 / 80)
    assert rw.histogram_quantile(lag, 0.99) == pytest.approx(0.05)
    assert rw.histogram_max_bound(lag) == ("<=", 0.1)
    hydrate = rw.histogram(samples, rw.HYDRATE_SECONDS)
    assert hydrate["count"] == 4 and hydrate["buckets"] == {1.0: 3.0, float("inf"): 4.0}
    # The top observation is in +Inf: only a lower bound is known.
    assert rw.histogram_max_bound(hydrate) == (">", 1.0)
    assert rw.histogram_quantile(hydrate, 0.99) == 1.0
    empty = {"buckets": {}, "count": 0.0, "sum": 0.0}
    assert rw.histogram_quantile(empty, 0.5) is None and rw.histogram_max_bound(empty) is None


def test_histogram_delta_covers_only_the_window():
    before = rw.parse_prometheus(METRICS)
    after = rw.parse_prometheus(
        METRICS.replace('le="0.1"} 100.0', 'le="0.1"} 110.0')
        .replace('le="+Inf"} 100.0', 'le="+Inf"} 110.0')
        .replace("seconds_count 100.0", "seconds_count 110.0")
        .replace("seconds_sum 0.9", "seconds_sum 1.7")
    )
    window = rw.histogram_delta(rw.histogram(before, rw.LOOP_LAG), rw.histogram(after, rw.LOOP_LAG))
    assert window["count"] == 10 and window["sum"] == pytest.approx(0.8)
    assert rw.histogram_quantile(window, 0.5) == pytest.approx(0.05 + 0.05 * 5 / 10)
    summary = rw.metrics_summary(before, after, {"bydconnect_worker_slots": 4.0})
    assert summary["loop_lag_s"]["count"] == 10
    assert summary["native_cipher"] == 1.0
    assert summary["shadow"] == {"match": 7.0, "mismatch": 1.0}
    assert summary["hydrate_seconds"]["all"]["count"] == 0
    assert set(summary["hydrate_seconds"]) == {"all", "forced", "moving"}
    assert summary["gauges_peak"] == {"bydconnect_worker_slots": 4.0}
    assert rw.metrics_summary(None, after, {})["loop_lag_s"]["count"] == 110


# ---------------------------------------------------------------------------
# Detections
# ---------------------------------------------------------------------------


def _raw(drives=(), charges=()):
    return {
        "drives": [
            {"vin": vin, "start_at": at(s), "end_at": at(e), "km": km, "start_odo": so, "end_odo": eo}
            for vin, s, e, km, so, eo in drives
        ],
        "charges": [
            {"vin": vin, "start_at": at(s), "end_at": at(e), "start_soc": a, "end_soc": b, "energy_kwh": kwh}
            for vin, s, e, a, b, kwh in charges
        ],
    }


def test_canonical_detections_are_sorted_lists_in_replay_seconds():
    raw = _raw(
        drives=[(VIN1, 3000, 3600, 12.3, 100.0, 112.3), (VIN1, 1000, 1500, 5.0, 95.0, 100.0)],
        charges=[(VIN2, 2000, 4000, 40.0, 80.0, 30.25)],
    )
    canonical = rw.canonical_detections(raw, ORIGIN)
    assert canonical["drives"] == {
        VIN1: [
            {"start": 1000.0, "end": 1500.0, "km": 5.0, "start_odo": 95.0, "end_odo": 100.0},
            {"start": 3000.0, "end": 3600.0, "km": 12.3, "start_odo": 100.0, "end_odo": 112.3},
        ]
    }
    assert canonical["charges"] == {VIN2: [{"start": 2000.0, "end": 4000.0, "start_soc": 40.0, "end_soc": 80.0, "energy_kwh": 30.25}]}
    assert rw.detection_counts(canonical) == {"drives": 2, "charges": 1}
    fast = dict(ORIGIN, speed=2.0)
    assert rw.replay_seconds(at(700), fast) == 800.0
    assert rw.replay_seconds(None, ORIGIN) is None


BASE = rw.canonical_detections(
    _raw(
        drives=[(VIN1, 1000, 1500, 5.0, 95.0, 100.0), (VIN1, 3000, 3600, 12.3, 100.0, 112.3)],
        charges=[(VIN2, 2000, 4000, 40.0, 80.0, 30.2)],
    ),
    ORIGIN,
)


def _variant(**edits: Any) -> dict[str, Any]:
    copy = json.loads(json.dumps(BASE))
    for path, value in edits.items():
        kind, vin, index, name = path.split(".")
        copy[kind][vin][int(index)][name] = value
    return copy


def test_same_detections_have_no_differences():
    assert rw.compare_detections(BASE, json.loads(json.dumps(BASE))) == []


def test_differences_within_tolerance_are_not_differences():
    other = _variant(**{
        f"drives.{VIN1}.0.start": 1000 + rw.TOLERANCES["start"],
        f"drives.{VIN1}.0.end": 1500 - rw.TOLERANCES["end"],
        f"drives.{VIN1}.1.km": 12.3 + 0.04,
        f"charges.{VIN2}.0.end_soc": 80.4,
    })  # fmt: skip
    assert rw.compare_detections(BASE, other) == []


def test_every_field_beyond_its_tolerance_is_listed():
    other = _variant(**{
        f"drives.{VIN1}.0.end": 1500 + rw.TOLERANCES["end"] + 1,
        f"drives.{VIN1}.1.km": 12.4,
        f"drives.{VIN1}.1.end_odo": None,
        f"charges.{VIN2}.0.start_soc": 41.0,
        f"charges.{VIN2}.0.energy_kwh": 30.3,
    })  # fmt: skip
    differences = rw.compare_detections(BASE, other)
    assert [(d["kind"], d["vin"], d["type"], d["field"], d["reference"], d["other"]) for d in differences] == [
        ("drives", VIN1, "field", "end", 1500.0, 1591.0),
        ("drives", VIN1, "field", "km", 12.3, 12.4),
        ("drives", VIN1, "field", "end_odo", 112.3, None),
        ("charges", VIN2, "field", "start_soc", 40.0, 41.0),
        ("charges", VIN2, "field", "energy_kwh", 30.2, 30.3),
    ]
    assert all(d["tolerance"] == rw.TOLERANCES[d["field"]] for d in differences)


def test_counts_missing_and_extra_items_are_listed():
    other = json.loads(json.dumps(BASE))
    del other["drives"][VIN1][0]  # missing
    other["charges"][VIN2].append({"start": 9000.0, "end": 9500.0, "start_soc": 50.0, "end_soc": 60.0, "energy_kwh": 7.0})  # extra
    other["drives"]["TESTVIN0000000009"] = [{"start": 5.0, "end": 50.0, "km": 1.0, "start_odo": 1.0, "end_odo": 2.0}]
    differences = rw.compare_detections(BASE, other)
    kinds = [(d["kind"], d["vin"], d["type"]) for d in differences]
    assert kinds == [
        ("drives", VIN1, "count"),
        ("drives", VIN1, "missing"),
        ("drives", "TESTVIN0000000009", "count"),
        ("drives", "TESTVIN0000000009", "extra"),
        ("charges", VIN2, "count"),
        ("charges", VIN2, "extra"),
    ]
    assert differences[0]["reference"] == 2 and differences[0]["other"] == 1
    assert differences[1]["reference"]["start"] == 1000.0
    assert differences[5]["other"]["start"] == 9000.0


def test_a_start_moved_beyond_tolerance_is_missing_plus_extra():
    other = _variant(**{f"drives.{VIN1}.0.start": 1000 + rw.TOLERANCES["start"] + 1})
    types = [d["type"] for d in rw.compare_detections(BASE, other)]
    assert types == ["missing", "extra"]


def test_compare_runs_and_aggregate():
    rows = [
        _row("python_cipher", 1, BASE, cpu=10.0),
        _row("rust_cipher", 1, BASE, cpu=4.0),
        _row("python_cipher", 2, _variant(**{f"drives.{VIN1}.1.km": 13.0}), cpu=12.0),
    ]
    comparison = rw.compare_runs(rows)
    # Every pair of runs, and which pairs are two repeats of the same variant.
    assert [(c["reference"], c["other"], c["same_variant"], c["equal"]) for c in comparison] == [
        ("python_cipher#1", "rust_cipher#1", False, True),
        ("python_cipher#1", "python_cipher#2", True, False),
        ("rust_cipher#1", "python_cipher#2", False, False),
    ]
    summary = rw.aggregate(rows)
    assert summary["python_cipher"]["runs"] == 2
    assert summary["python_cipher"]["figures"]["measured.cpu_s"]["median"] == 11.0
    assert summary["rust_cipher"]["figures"]["measured.cpu_s"]["n"] == 1
    problems, warnings = rw.check({"rows": rows, "comparison": comparison})
    assert problems == []
    assert any("python_cipher#2 differ" in w for w in warnings)


def test_pairing_keeps_neighbours_apart():
    def items(*starts):
        return [{"start": start, "end": None if start is None else start + 600.0} for start in starts]

    # Greedy nearest-first would give 1020 to 1000 and leave 1250 and 750 unpaired.
    assert rw._match(items(1000.0, 1250.0), items(750.0, 1020.0), 300.0) == [(0, 0), (1, 1)]
    # An extra item is left alone; of two equally near candidates the earlier one is taken.
    assert rw._match(items(100.0, 500.0), items(90.0, 110.0, 480.0), 300.0) == [(0, 0), (None, 1), (1, 2)]
    assert rw._match(items(100.0, 500.0), items(60.0, 110.0, 480.0), 300.0) == [(None, 0), (0, 1), (1, 2)]
    assert rw._match(items(100.0), items(900.0), 300.0) == [(0, None), (None, 0)]
    assert rw._match(items(), items(5.0), 300.0) == [(None, 0)]
    # An item without a start is never paired.
    assert rw._match(items(None, 50.0), items(60.0), 300.0) == [(0, None), (1, 0)]


# ---------------------------------------------------------------------------
# Replay round trip
# ---------------------------------------------------------------------------


def _state(t, **values):
    base = {
        "type": "state", "vin": VIN1, "t": t, "online": True, "soc": 71.0, "odometer_km": 20431.4, "speed_kmh": 0.0,
        "vehicle_on": False, "plugged": False, "charging": False, "connect_state": 0, "charge_power_kw": None,
        "minutes_to_full": None, "hvac_on": False, "fuel_percent": None, "gps": True,
    }  # fmt: skip
    base.update(values)
    return base


def _scenario(lines):
    timeline: dict[str, list[dict[str, Any]]] = {}
    for line in lines:
        timeline.setdefault(line["vin"], []).append(line)
    return rw.Scenario(Path("x.jsonl"), {"type": "scenario"}, ["acct-0001"], {VIN1: {}}, {}, timeline)


SCENARIO = _scenario(
    [
        _state(0, online=False),
        _state(1000, soc=70.0, odometer_km=20431.4, speed_kmh=40.0, vehicle_on=True),
        _state(1060, soc=69.0, odometer_km=20432.1, speed_kmh=55.0),
        {"type": "error", "vin": VIN1, "t": 2000, "code": "1008"},
        _state(2100, soc=68.0, charging=True),
        {"type": "error", "vin": VIN1, "t": 2200, "code": "1005"},
    ]
)


def _poll(t, **payload):
    return {"vin": VIN1, "received_at": at(t), "payload": payload}


def test_roundtrip_matches_and_rounding():
    polls = [
        _poll(1030, online=True, currentSoc=70, odometer=20431, speed=40.0),
        _poll(1061, online=True, currentSoc=70.0, odometer=20431.4, speed=40.0),  # just after a change: either state
        _poll(2150, online=True, currentSoc=68.0, charging={"isCharging": True}),
        _poll(2300, online=True, currentSoc=68.0),  # 1005 leaves the state in force
    ]
    result = rw.roundtrip_check(polls, SCENARIO, ORIGIN)
    assert result["polls_checked"] == 4 and result["polls_skipped"] == 0
    assert result["match_rate"] == 1.0, result["mismatches"]
    assert result["by_field"]["charging.isCharging"] == {"checked": 1, "matched": 1, "endpoint": "charging"}


def test_roundtrip_reports_each_mismatch_with_its_endpoint():
    polls = [_poll(1200, online=True, currentSoc=55.0, odometer=20432.1, charging={"isCharging": True})]
    result = rw.roundtrip_check(polls, SCENARIO, ORIGIN)
    assert result["values_checked"] == 4 and result["values_matched"] == 2
    assert [(m["field"], m["endpoint"], m["stored"], m["expected"]) for m in result["mismatches"]] == [
        ("currentSoc", "realtime", 55.0, [69.0]),
        ("charging.isCharging", "charging", True, [False]),
    ]


def test_roundtrip_skips_errors_and_compares_only_online_while_asleep():
    polls = [
        _poll(500, online=False, currentSoc=12.0, odometer=0),  # asleep: soc and odometer are not compared
        _poll(500, online=True),  # but online is, and it is wrong
        _poll(2010, online=True, currentSoc=1.0),  # 1008 in force: not checked
    ]
    result = rw.roundtrip_check(polls, SCENARIO, ORIGIN)
    assert result["polls_skipped"] == 1
    assert result["by_field"]["online"] == {"checked": 2, "matched": 1, "endpoint": "realtime"}
    assert result["by_field"]["currentSoc"]["checked"] == 0


def test_values_match():
    assert rw.values_match(20431, 20431.4)
    assert not rw.values_match(20431, 20431.6)
    assert rw.values_match(20431.4, 20431.4)
    assert not rw.values_match(20431.5, 20431.4)
    assert rw.values_match(True, True) and not rw.values_match(True, False)
    assert not rw.values_match(1, True) and not rw.values_match(None, 1.0)
    assert rw.values_match(None, None)


def test_read_scenario_keeps_counts_and_never_the_lines(tmp_path):
    path = tmp_path / "s.jsonl"
    lines = [
        {"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 60, "accounts": 1, "cars": 1, "seed": "s", "stats": {}},
        {"type": "account", "account": "acct-0001", "cars": [VIN1]},
        {"type": "car", "vin": VIN1, "account": "acct-0001", "model": "seal", "energy": "ev"},
        _state(0),
        {"type": "push", "vin": VIN1, "t": 5, "event": "remoteControl"},
        {"type": "error", "vin": VIN1, "t": 10, "code": "6002"},
    ]
    path.write_text("\n".join(json.dumps(line) for line in lines) + "\n", encoding="utf-8")
    scenario = rw.read_scenario(path)
    assert scenario.accounts == ["acct-0001"]
    assert [line["type"] for line in scenario.timeline[VIN1]] == ["state", "error"]
    assert scenario.summary() == {"file": "s.jsonl", "header": lines[0], "counts": {"account": 1, "car": 1, "state": 1, "push": 1, "error": 1}}
    path.write_text(json.dumps(dict(lines[0], version=2)) + "\n", encoding="utf-8")
    with pytest.raises(SystemExit):
        rw.read_scenario(path)


def test_child_environment_leaves_the_developers_settings_behind(tmp_path, monkeypatch):
    monkeypatch.setenv("DATABASE_URL", "postgresql://prod")
    monkeypatch.setenv("SENTRY_DSN", "https://key@sentry.invalid/1")
    monkeypatch.setenv("STRIPE_SECRET_KEY", "sk_live_x")
    env = rw.child_environment(tmp_path, {"BYD_SETTINGS_ENV_FILE": "", "DATABASE_URL": "sqlite+aiosqlite:///x.db"})
    assert env["DATABASE_URL"] == "sqlite+aiosqlite:///x.db"
    assert "SENTRY_DSN" not in env and "STRIPE_SECRET_KEY" not in env
    assert env["BYD_SETTINGS_ENV_FILE"] == "" and env["TEMP"].startswith(str(tmp_path))


# ---------------------------------------------------------------------------
# The report
# ---------------------------------------------------------------------------


def _row(variant: str, repeat: int, detections: dict[str, Any], cpu: float = 5.0) -> dict[str, Any]:
    lag = {"count": 60, "mean": 0.01, "p50": 0.004, "p99": 0.05, "max_bound": ["<=", 0.1]}
    return {
        "variant": variant,
        "label": rw.VARIANTS[variant].label,
        "repeat": repeat,
        "accounts": 3,
        "measured": {"cpu_s": cpu, "rss_peak_mib": 150.0, "threads_peak": 12},
        "derived": {
            "cpu_s_per_account": cpu / 3,
            "cpu_ms_per_account_minute": cpu * 1000 / 3 / 3,
            "memory_per_account_kib": 50000.0,
            "memory_growth_per_account_kib": 1000.0,
        },
        "metrics": {"loop_lag_s": lag, "hydrate_seconds": {"all": {"count": 9, "mean": 0.4}}},
        "replay": {"stats": {"requests": 120, "logins": 3, "by_endpoint": {"/app/account/login": 3}, "by_code": {"0": 117, "1008": 3}, "mqtt": {"pushes_sent": 10, "pushes_missed": 0}}},
        "store": {"polls_ok": 30, "mqtt_rows": 10, "open_at_end": {"drives": 1, "charges": 0}},
        "detections": detections,
        "detection_counts": rw.detection_counts(detections),
        "roundtrip": rw.roundtrip_check([_poll(1030, online=True, currentSoc=71.0)], SCENARIO, ORIGIN),
        "guard": {"blocked_total": 1, "blocked": [{"via": "getaddrinfo", "host": "nominatim.openstreetmap.org", "port": "443", "count": 1}]},
        "problems": [],
    }


def test_report_renders_a_replay_result():
    rows = [_row("python_cipher", 1, BASE), _row("rust_cipher", 1, _variant(**{f"drives.{VIN1}.1.km": 13.0}), cpu=2.0)]
    result = {
        "benchmark": "replay_worker",
        "quick": True,
        "date": "2026-11-19T22:00:00+02:00",
        "machine": {"cpu": "cpu", "cores_logical": 8, "memory_gib": 32, "os": "os", "python": "3.12", "rust": "rustc"},
        "versions": {"openbyd_commit": "abc", "openbyd_dirty": False, "pybyd_commit_installed": None, "pybyd_commit_pinned": "def"},
        "settings": {
            "start_at": 600, "speed": 1.0, "minutes": 3, "repeats": 1, "store": "local", "tolerances": rw.TOLERANCES,
            "poll_sample": 2000, "roundtrip_window_s": rw.ROUNDTRIP_WINDOW_S,
        },  # fmt: skip
        "scenario": {"file": "demo.jsonl", "header": {"source": "synthetic", "duration_s": 21600}, "counts": {"account": 3, "car": 4}},
        "backend": {"backend_commit": "0123456789abc", "backend_branch": "bench/replay-worker", "packages": {"pybyd": "0.0.85", "openbyd": "0.1.1"}},
        "rows": rows,
        "variants": rw.aggregate(rows),
        "comparison": rw.compare_runs(rows),
    }
    text = "\n".join(report.render_replay_worker(result))
    assert "## Replay" in text
    assert "rust_cipher#1 vs python_cipher#1: **1 difference(s)**" in text
    assert "| drives | TESTVIN0000000001 | field | km (start 3000) | 12.3 | 13 | 0.05 |" in text
    assert "nominatim.openstreetmap.org" in text
    assert "`/app/account/login`" in text


# ---------------------------------------------------------------------------
# The launcher's network guard (each case runs in a fresh interpreter)
# ---------------------------------------------------------------------------

GUARD_CASES = textwrap.dedent(
    r"""
    import asyncio, json, socket, sys
    sys.path.insert(0, sys.argv[1])
    import launcher
    guard = launcher.install_guard()
    results = {}

    def attempt(name, fn, needs=None):
        if needs:
            try:
                __import__(needs)
            except ImportError:
                results[name] = "skipped"
                return
        before = sum(guard.blocked.values())
        try:
            fn()
            results[name] = "NOT BLOCKED"
        except Exception as exc:  # noqa: BLE001
            # Blocked means: it failed, and the guard is what refused it.
            results[name] = "blocked" if sum(guard.blocked.values()) > before else f"{type(exc).__name__}: {exc}"

    def raw_connect(host):
        s = socket.socket(socket.AF_INET6 if ":" in host else socket.AF_INET)
        s.settimeout(3)
        try:
            s.connect((host, 80))
        finally:
            s.close()

    def connect_ex():
        s = socket.socket()
        s.settimeout(3)
        code = s.connect_ex(("192.0.2.1", 80))
        s.close()
        raise OSError(f"connect_ex returned {code}")

    def udp():
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            s.sendto(b"x", ("192.0.2.1", 53))
        finally:
            s.close()

    def loop_connect(loop_factory, host):
        async def go():
            await asyncio.wait_for(asyncio.open_connection(host, 80), 3)
        loop = loop_factory()
        try:
            loop.run_until_complete(go())
        finally:
            loop.close()

    def aiohttp_get(url):
        import aiohttp
        async def go():
            async with aiohttp.ClientSession() as session:
                async with session.get(url, timeout=aiohttp.ClientTimeout(total=3)) as response:
                    await response.read()
        asyncio.run(go())

    def httpx_get(url):
        import httpx
        httpx.get(url, timeout=3)

    def httpx_async(url):
        import httpx
        async def go():
            async with httpx.AsyncClient(timeout=3) as client:
                await client.get(url)
        asyncio.run(go())

    attempt("getaddrinfo name", lambda: socket.getaddrinfo("example.com", 443))
    attempt("getaddrinfo ip", lambda: socket.getaddrinfo("192.0.2.1", 443))
    attempt("gethostbyname", lambda: socket.gethostbyname("example.com"))
    attempt("create_connection name", lambda: socket.create_connection(("example.com", 80), timeout=3))
    attempt("socket.connect name", lambda: raw_connect("example.com"))
    attempt("socket.connect ipv4", lambda: raw_connect("192.0.2.1"))
    attempt("socket.connect ipv6", lambda: raw_connect("2001:db8::1"))
    attempt("socket.connect mapped", lambda: raw_connect("::ffff:192.0.2.1"))
    attempt("socket.connect_ex", connect_ex)
    attempt("socket.sendto", udp)
    attempt("selector loop ip", lambda: loop_connect(asyncio.SelectorEventLoop, "192.0.2.1"))
    if sys.platform == "win32":
        attempt("proactor loop ip", lambda: loop_connect(asyncio.ProactorEventLoop, "192.0.2.1"))
    attempt("aiohttp name", lambda: aiohttp_get("https://example.com/"), "aiohttp")
    attempt("aiohttp ip", lambda: aiohttp_get("http://192.0.2.1/"), "aiohttp")
    attempt("httpx name", lambda: httpx_get("https://example.com/"), "httpx")
    attempt("httpx ip", lambda: httpx_get("http://192.0.2.1/"), "httpx")
    attempt("httpx async ip", lambda: httpx_async("http://[2001:db8::1]/"), "httpx")
    print(json.dumps({"results": results, "report": guard.report()}))
    """
)


def test_guard_refuses_every_path_off_the_machine(tmp_path):
    script = tmp_path / "cases.py"
    script.write_text(GUARD_CASES, encoding="utf-8")
    done = subprocess.run(
        [sys.executable, str(script), str(LAUNCHER.parent)], capture_output=True, text=True, timeout=120, check=False
    )
    assert done.returncode == 0, done.stderr
    out = json.loads(done.stdout.strip().splitlines()[-1])
    not_blocked = {name: result for name, result in out["results"].items() if result not in ("blocked", "skipped")}
    assert not_blocked == {}
    vias = {item["via"] for item in out["report"]["blocked"]}
    assert {"getaddrinfo", "gethostbyname", "socket.connect", "socket.connect_ex", "socket.sendto"} <= vias
    if sys.platform == "win32":
        assert "proactor.connect" in vias
    hosts = {item["host"] for item in out["report"]["blocked"]}
    assert {"example.com", "192.0.2.1", "2001:db8::1"} <= hosts
    assert out["report"]["blocked_total"] >= len(out["results"]) - list(out["results"].values()).count("skipped")


LOOPBACK_CASES = textwrap.dedent(
    r"""
    import asyncio, json, socket, sys, threading
    sys.path.insert(0, sys.argv[1])
    import launcher
    guard = launcher.install_guard()
    server = socket.socket()
    server.bind(("127.0.0.1", 0))
    server.listen(4)
    port = server.getsockname()[1]
    threading.Thread(target=lambda: [server.accept()[0].close() for _ in range(3)], daemon=True).start()
    socket.create_connection(("127.0.0.1", port), timeout=3).close()
    socket.create_connection(("localhost", port), timeout=3).close()
    async def go():
        reader, writer = await asyncio.open_connection("127.0.0.1", port)
        writer.close()
    asyncio.run(go())
    assert socket.getaddrinfo(None, 8081, flags=socket.AI_PASSIVE)
    assert socket.getaddrinfo("0.0.0.0", 8081)
    print(json.dumps(guard.report()))
    """
)


def test_guard_lets_loopback_through(tmp_path):
    script = tmp_path / "loopback.py"
    script.write_text(LOOPBACK_CASES, encoding="utf-8")
    done = subprocess.run([sys.executable, str(script), str(LAUNCHER.parent)], capture_output=True, text=True, timeout=60, check=False)
    assert done.returncode == 0, done.stderr
    assert json.loads(done.stdout.strip().splitlines()[-1])["blocked_total"] == 0


def test_address_rules():
    sys.path.insert(0, str(LAUNCHER.parent))
    import launcher

    for host in ("127.0.0.1", "127.8.9.10", "::1", "[::1]", "localhost", "LOCALHOST", "0.0.0.0", "::", "::ffff:127.0.0.1", b"127.0.0.1"):
        assert launcher.address_is_local(host), host
    for host in ("192.0.2.1", "10.0.0.1", "2001:db8::1", "::ffff:192.0.2.1", "example.com", "dilinkappoversea-eu.byd.auto", "", None, "127.0.0.1.nip.io"):
        assert not launcher.address_is_local(host), host
    assert launcher.lookup_is_local(None) and launcher.lookup_is_local("") and not launcher.lookup_is_local("example.com")


MODULE_WAITS = textwrap.dedent(
    """
    import signal, socket, sys, time
    stop = []
    signal.signal(signal.SIGINT, lambda *_: stop.append(1))
    try:
        socket.create_connection(("example.com", 443), timeout=3)
    except OSError as exc:
        print("connect failed:", exc, file=sys.stderr)
    print("WAITING", flush=True)
    deadline = time.monotonic() + 30
    while not stop and time.monotonic() < deadline:
        time.sleep(0.05)
    print("STOPPED" if stop else "TIMED OUT", flush=True)
    """
)


def test_launcher_runs_a_module_stops_it_on_stdin_and_reports(tmp_path):
    (tmp_path / "waiting_module.py").write_text(MODULE_WAITS, encoding="utf-8")
    report_file = tmp_path / "guard.json"
    proc = subprocess.Popen(
        [sys.executable, str(LAUNCHER), "--guard-report", str(report_file), "--stdin-stop", "-m", "waiting_module"],
        cwd=tmp_path,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert proc.stdout is not None and proc.stdin is not None
    first = proc.stdout.readline()
    assert first.startswith("LAUNCHER pid=") and "module=waiting_module" in first
    assert proc.stdout.readline().strip() == "WAITING"
    proc.stdin.write("stop\n")
    proc.stdin.flush()
    out, err = proc.communicate(timeout=30)
    assert "STOPPED" in out, err
    line = next(line for line in out.splitlines() if line.startswith("GUARD_REPORT "))
    report_data = json.loads(line[len("GUARD_REPORT ") :])
    assert report_data == json.loads(report_file.read_text(encoding="utf-8"))
    assert report_data["blocked"] == [{"via": "getaddrinfo", "host": "example.com", "port": "443", "count": 1}]
    assert report_data["exit_code"] == 0 and proc.returncode == 0


def test_launcher_refuses_speed_above_one(tmp_path):
    (tmp_path / "noop_module.py").write_text("print('ran')\n", encoding="utf-8")
    done = subprocess.run(
        [sys.executable, str(LAUNCHER), "--speed", "4", "-m", "noop_module"], cwd=tmp_path, capture_output=True, text=True, timeout=30, check=False
    )
    assert done.returncode != 0 and "ran" not in done.stdout


def test_results_of_a_real_scenario_are_published_without_any_car(tmp_path, monkeypatch):
    """Per-car lists stay in the private directory; bench/results gets counts only."""
    drive = {"start": 100.0, "end": 900.0, "km": 12.3, "start_odo": 45001.2, "end_odo": 45013.5}
    other = {**drive, "end": 1300.0}
    mismatch = {"vin": "TESTVIN0000000001", "t": 5.0, "field": "odometer", "endpoint": "realtime", "stored": 45001.2, "expected": [1.0]}

    clock = iter(range(1000, 2000))

    def row(variant, repeat, item):
        return {
            "variant": variant, "label": variant, "repeat": repeat, "problems": [],
            "detections": {"drives": {"TESTVIN0000000001": [item]}, "charges": {}},
            "detection_counts": {"drives": 1, "charges": 0},
            "roundtrip": {"match_rate": 0.5, "mismatches": [mismatch], "by_field": {}},
            "guard": {"blocked_total": 0, "blocked": []},
            "derived": {name: 1.0 for _group, name in rw.AGGREGATED if _group == "derived"},
            "measured": {name: 1.0 for _group, name in rw.AGGREGATED if _group == "measured"},
            "metrics": {"loop_lag_s": {"p99": 0.01}},
            "origin": {"wall_unix_ms": next(clock), "start_at": 0, "speed": 1.0},
            "log": "worker-export-x.log",
            "store": {"ingest": [
                {"source": "poll", "kind": "error", "detail": "1008", "count": 2},
                {"source": "poll", "kind": "skip", "detail": "min_gap", "count": 1},
                {"source": "poll", "kind": "error", "detail": "Timeout talking to 10.0.0.9", "count": 1},
                {"source": "poll", "kind": "ok", "detail": None, "count": 5},
            ]},
        }  # fmt: skip

    def document(rows, source="export"):
        return {
            "date": "2026-01-05T00:00:00+00:00",
            "settings": {
                "minutes": 30, "speed": 1.0, "start_at": 0, "server_delay_ms": None, "corpus": "parity/corpus", "store": "docker",
                "start_when_claimed": True, "tolerances": rw.TOLERANCES, "variant_table": {r["variant"]: {} for r in rows},
            },
            "scenario": {"file": "export-x.jsonl", "header": {"version": 1, "source": source, "start": "2026-01-05T00:00:00Z", "duration_s": 60, "accounts": 1, "cars": 1, "stats": {"state": 3}}, "counts": {"car": 1}},
            "backend": {"backend_commit": "abc"},
            "rows": rows,
            "variants": rw.aggregate(rows),
            "comparison": rw.compare_runs(rows),
        }  # fmt: skip

    first = document([row("python_cipher", 1, drive), row("rust_cipher", 1, other)])
    second = document([row("python_cipher", 1, drive), row("rust_cipher", 1, drive)])
    merged = rw.merge_results([first, second], ["a.json", "b.json"])
    assert [(r["variant"], r["repeat"]) for r in merged["rows"]] == [
        ("python_cipher", 1), ("rust_cipher", 1), ("python_cipher", 2), ("rust_cipher", 2),
    ]  # fmt: skip
    assert merged["settings"]["repeats"] == 2 and merged["variants"]["rust_cipher"]["runs"] == 2
    assert [(entry["other"], entry["same_variant"], entry["equal"]) for entry in merged["comparison"]] == [
        ("rust_cipher#1", False, False), ("python_cipher#2", True, True), ("rust_cipher#2", False, True),
        ("python_cipher#2", False, False), ("rust_cipher#2", True, False), ("rust_cipher#2", False, True),
    ]  # fmt: skip
    with pytest.raises(SystemExit, match="packages"):
        rw.merge_results([first, {**second, "backend": {"backend_commit": "abc", "packages": {"pybyd": "9"}}}], ["a.json", "b.json"])
    with pytest.raises(SystemExit, match="same run appears twice"):
        rw.merge_results([first, first], ["a.json", "a.json"])
    with pytest.raises(SystemExit, match="settings differ"):
        rw.merge_results([first, {**second, "settings": {**second["settings"], "minutes": 5}}], ["a.json", "b.json"])

    published = rw.publishable(merged)
    text = json.dumps(published)
    assert rw.is_private(merged) and published["redacted"]
    for secret in ("TESTVIN0000000001", "45001.2", "45013.5", "12.3", "export-x", "2026-01-05T00:00:00Z", "10.0.0.9", "a.json"):
        assert secret not in text
    assert [item["detail"] for item in published["rows"][0]["store"]["ingest"]] == ["1008", "min_gap", "other", None]
    assert published["rows"][0]["detection_counts"] == {"drives": 1, "charges": 0}
    assert published["rows"][0]["cars_with_detections"] == {"drives": 1, "charges": 0}
    assert published["comparison"][0]["difference_summary"] == {"drives field end": 1}
    assert published["comparison"][0]["cars_differing"] == 1 and published["comparison"][0]["differences"] == []
    with pytest.raises(SystemExit, match="redacted"):
        rw.merge_results([published], ["p.json"])
    assert not rw.is_private(document([], source="synthetic"))

    # The warnings a run prints name fields, never a car's values.
    _problems, warnings = rw.check(merged)
    assert any("fields ['odometer']" in warning for warning in warnings) and "45001.2" not in " ".join(warnings)

    # Written: the complete file in the private directory, the published one in bench/results.
    results_dir = tmp_path / "results"
    monkeypatch.setattr(rw.common, "RESULTS", results_dir)
    monkeypatch.setattr(rw.common, "machine_info", lambda: {})
    monkeypatch.setattr(rw.common, "versions_info", lambda: {})
    private = tmp_path / "private"
    written: list[Path] = []
    original = rw.common.write_results

    def recording(name, quick, payload, *, directory=None):
        path = original(name, quick, payload, directory=directory)
        written.append(path)
        return path

    monkeypatch.setattr(rw.common, "write_results", recording)
    path = rw.write(merged, argparse.Namespace(quick=False, scratch=tmp_path), private)
    assert path.parent == results_dir and "TESTVIN" not in path.read_text(encoding="utf-8")
    (kept,) = private.glob("replay_worker-*.json")
    assert "45001.2" in kept.read_text(encoding="utf-8")
    # The complete document went straight to the private directory; only the public one to bench/results.
    assert [item.parent for item in written] == [private, results_dir]
    with pytest.raises(SystemExit, match="inside the repository"):
        rw.write(merged, argparse.Namespace(quick=False, scratch=tmp_path), rw.common.ROOT / "bench" / "results")


def test_stats_delta_counts_the_measured_window():
    before = {"requests": 10, "by_code": {"0": 9, "1008": 1}, "logins": 3, "mqtt": {"open": 3, "pushes_sent": 2}, "replay": {"accounts": 3}}
    after = {"requests": 25, "by_code": {"0": 20, "1008": 1, "6002": 4}, "logins": 3, "mqtt": {"open": 2, "pushes_sent": 9}, "replay": {"accounts": 3}}
    assert rw.stats_delta(before, after) == {
        "requests": 15, "by_code": {"0": 11, "1008": 0, "6002": 4}, "logins": 0,
        "mqtt": {"open": 2, "pushes_sent": 7}, "replay": {"accounts": 3},
    }  # fmt: skip
    assert rw.stats_delta(None, {"requests": 4}) == {"requests": 4}
