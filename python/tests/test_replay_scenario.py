"""Exercise the replay tools with synthetic scenarios and fabricated ingest rows."""

from __future__ import annotations

import argparse
import io
import json
import os
import re
import sys
from collections import Counter
from contextlib import asynccontextmanager
from datetime import UTC, datetime, timedelta
from pathlib import Path
from types import SimpleNamespace
from uuid import UUID

import pytest

try:
    from sqlalchemy import text
except ImportError:  # the tests of the exporter's statements need SQLAlchemy; the rest do not
    text = None

needs_sqlalchemy = pytest.mark.skipif(text is None, reason="SQLAlchemy is not installed")

from bench.replay import export_prod, generate, privacy, scenario

ROOT = Path(__file__).resolve().parents[2]
DEMO = ROOT / "bench" / "replay" / "samples" / "demo.jsonl"
VIN = "TESTVIN0000000001"
OTHER_VIN = "TESTVIN0000000002"
ACCOUNT = "acct-0001"
START = datetime(2026, 1, 5, tzinfo=UTC)


def _minimal() -> list[dict]:
    return [
        {"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z",
         "duration_s": 120, "accounts": 1, "cars": 1, "seed": "demo", "stats": {}},
        {"type": "account", "account": ACCOUNT, "cars": [VIN]},
        {"type": "car", "vin": VIN, "account": ACCOUNT, "model": "seal", "energy": "ev"},
        {"type": "state", "vin": VIN, "t": 0, **export_prod.DEFAULT_STATE},
        {"type": "push", "vin": VIN, "t": 10, "event": "remoteControl"},
    ]


def test_round_trip(tmp_path):
    path = tmp_path / "scenario.jsonl"
    rows = _minimal()
    rows[0]["stats"] = {"custom_count": 12, "state": 1}
    rows += [
        {"type": "push", "vin": VIN, "t": 10, "event": "smartCharge", "soc": None},
        {"type": "push", "vin": VIN, "t": 80, "event": "vehicleInfo", "gps": True},
        {"type": "error", "vin": VIN, "t": 120, "code": "1005"},
    ]
    scenario.write(path, iter(rows))
    assert list(scenario.read(path)) == rows
    report = scenario.validate(path)
    assert report.accounts == report.cars == 1
    assert report.pushes_by_event == {"remoteControl": 1, "vehicleInfo": 1, "smartCharge": 1}
    assert report.errors_by_code == {"1005": 1}
    assert report.warnings == ()
    rows[0].pop("seed")
    rows[0].pop("stats")
    scenario.write(path, rows)
    assert scenario.validate(path) == report


@pytest.mark.parametrize(("index", "updates", "message"), [
    (0, {"type": "car"}, "expected scenario header"),
    (0, {"version": 2}, "unsupported scenario version"),
    (0, {"version": True}, "unsupported scenario version"),
    (0, {"source": "invalid"}, "invalid scenario source"),
    (0, {"source": "export"}, "seed is only allowed"),
    (0, {"start": "2026-02-30T00:00:00Z"}, "start must be UTC"),
    (0, {"start": "2026-01-05T00:00:00.123Z"}, "start must be UTC"),
    (0, {"start": "2026-01-05T00:00:00+01:00"}, "start must be UTC"),
    (0, {"duration_s": -1}, "duration_s must be"),
    (0, {"duration_s": 120.0}, "duration_s must be"),
    (0, {"accounts": 0}, "header account count"),
    (0, {"cars": 2}, "header car count"),
    (0, {"stats": {"invalid": True}}, "stats must be"),
    (0, {"stats": {"invalid": {"count": 3}}}, "stats must be"),
    (0, {"stats": {"invalid": -1}}, "stats must be"),
    (0, {"stats": {"invalid": 1.0}}, "stats must be"),
    (0, {"stats": {"invalid_1": 1}}, "stats must be"),
    (0, {"stats": None}, "stats must be"),
    (0, {"seed": "private phrase"}, "invalid synthetic seed"),
    (0, {"other": "value"}, "unexpected keys"),
    (1, {"account": "acct-xx"}, "invalid or duplicate account"),
    (1, {"cars": VIN}, "cars must be a list"),
    (1, {"cars": [VIN, VIN]}, "VIN listed more than once"),
    (1, {"cars": [VIN, OTHER_VIN]}, "account lists an undefined car"),
    (2, {"vin": "I" * 17}, "expected a synthetic VIN"),
    (2, {"account": "acct-9999"}, "undefined account"),
    (2, {"vin": OTHER_VIN}, "VIN does not belong"),
    (2, {"model": "uppercase A"}, "invalid car model or energy"),
    (2, {"energy": "diesel"}, "invalid car model or energy"),
    (3, {"type": "something"}, "unexpected line type"),
    (3, {"type": "scenario"}, "unexpected line type"),
    (3, {"vin": OTHER_VIN}, "undefined car"),
    (3, {"t": -1}, "t must be an integer"),
    (3, {"t": 120.0}, "t must be an integer"),
    (3, {"t": True}, "t must be an integer"),
    (3, {"t": 121}, "t must be an integer"),
    (3, {"online": 0}, "invalid online"),
    (3, {"gps": None}, "invalid gps"),
    (3, {"soc": True}, "invalid soc"),
    (3, {"soc": 101}, "invalid soc"),
    (3, {"odometer_km": -1}, "invalid odometer_km"),
    (3, {"speed_kmh": "3"}, "invalid speed_kmh"),
    (3, {"vehicle_on": 1}, "invalid vehicle_on"),
    (3, {"plugged": None}, "invalid plugged"),
    (3, {"charging": "false"}, "invalid charging"),
    (3, {"connect_state": 1.0}, "invalid connect_state"),
    (3, {"charge_power_kw": -1}, "invalid charge_power_kw"),
    (3, {"minutes_to_full": -1}, "invalid minutes_to_full"),
    (3, {"hvac_on": "yes"}, "invalid hvac_on"),
    (3, {"fuel_percent": 64}, "invalid fuel_percent"),
    (3, {"altitude": 1}, "unexpected keys"),
    (4, {"event": "invalid"}, "invalid push event"),
    (4, {"event": []}, "invalid push event"),
    (4, {"soc": 50}, "unexpected keys"),
    (4, {"event": "energyConsumption", "gps": True}, "unexpected keys"),
    (4, {"event": "smartCharge", "gps": True}, "unexpected keys"),
    (4, {"event": "vehicleInfo", "online": None}, "invalid online"),
    (4, {"event": "smartCharge", "charging": None}, "invalid charging"),
])
def test_validator_rejects_invalid_fields(tmp_path, index, updates, message):
    path = tmp_path / "bad.jsonl"
    rows = _minimal()
    rows[index].update(updates)
    scenario.write(path, rows)
    with pytest.raises(scenario.ScenarioError, match=rf"line {index + 1}: .*{message}"):
        scenario.validate(path)


@pytest.mark.parametrize(("edit", "line", "message"), [
    (lambda rows: rows.clear(), 1, "expected scenario header"),
    (lambda rows: rows[0].pop("accounts"), 1, "missing keys"),
    (lambda rows: rows[1].pop("cars"), 2, "missing keys"),
    (lambda rows: rows.insert(2, rows[1].copy()), 3, "duplicate account"),
    (lambda rows: rows.insert(3, rows[2].copy()), 4, "duplicate car"),
    (lambda rows: rows.append(rows[2]), 6, "must precede the timeline"),
    (lambda rows: rows.append(rows[1]), 6, "must precede the timeline"),
    (lambda rows: rows.reverse(), 1, "expected scenario header"),
    (lambda rows: rows[3].pop("gps"), 4, "missing keys"),
    (lambda rows: rows[3].update(t=12), 5, "timeline t decreases"),
    (lambda rows: rows.append({"type": "error", "vin": VIN, "t": 20, "code": 1008}), 6, "error code"),
    (lambda rows: rows.append({"type": "error", "vin": VIN, "t": 20, "code": "10A"}), 6, "error code"),
])
def test_validator_rejects_invalid_structure(tmp_path, edit, line, message):
    path = tmp_path / "bad.jsonl"
    rows = _minimal()
    edit(rows)
    scenario.write(path, rows)
    with pytest.raises(scenario.ScenarioError, match=rf"line {line}: .*{message}"):
        scenario.validate(path)


@pytest.mark.parametrize("bad", [
    b"", b"[]\n", b"\xff\n", b"{bad\n", b"\n", b'{"type":"car","type":"car"}\n',
    b'{"type":"car","bad":NaN}\n',
])
def test_validator_reports_malformed_json_line(tmp_path, bad):
    path = tmp_path / "bad.jsonl"
    scenario.write(path, _minimal()[:1])
    with path.open("ab") as output:
        output.write(bad or b"\n")
    with pytest.raises(scenario.ScenarioError, match="line 2:"):
        scenario.validate(path)


def test_unrecovered_error_is_a_warning(tmp_path):
    rows = _minimal()
    path = tmp_path / "scenario.jsonl"
    rows.append({"type": "error", "vin": VIN, "t": 110, "code": "1008"})
    scenario.write(path, rows)
    assert scenario.validate(path).warnings == ("WARNING line 6: car ends with an unrecovered error",)
    rows.append({"type": "error", "vin": VIN, "t": 111, "code": "1005"})
    scenario.write(path, rows)
    assert scenario.validate(path).warnings == ("WARNING line 6: car ends with an unrecovered error",)
    rows.append({**rows[3], "t": 112})
    scenario.write(path, rows)
    assert scenario.validate(path).warnings == ()


@pytest.mark.parametrize("profile", ["normal", "busy"])
@pytest.mark.parametrize(("seed", "hours"), [("one", 0.01), ("two", 3), ("three", 25)])
def test_generator_is_deterministic_and_safe(tmp_path, profile, seed, hours):
    first = tmp_path / "first.jsonl"
    second = tmp_path / "second.jsonl"
    for path in (first, second):
        generate.generate(path, 6, hours, seed, profile)
        report = scenario.validate(path)
        assert report.accounts == 6 and report.cars == 7 and not report.warnings
        with path.open(encoding="utf-8") as stream:
            assert list(privacy.find_leaks(stream)) == []
    assert first.read_bytes() == second.read_bytes()


def test_committed_demo_is_complete_and_reproducible(tmp_path):
    path = tmp_path / "demo.jsonl"
    generate.generate(path, 3, 6, "demo", "busy")
    assert path.read_bytes() == DEMO.read_bytes()
    assert path.stat().st_size < 400_000
    rows = list(scenario.read(path))
    report = scenario.validate(path)
    assert report.accounts == 3 and report.cars == 4 and not report.warnings
    assert Counter(row["energy"] for row in rows if row["type"] == "car") == {"ev": 3, "hybrid": 1}
    assert set(report.errors_by_code) == {"1008", "6002", "1001", "1005"}
    assert set(report.pushes_by_event) == export_prod.PUSH_FIELDS.keys()
    for car in (row["vin"] for row in rows if row["type"] == "car"):
        states = [row for row in rows if row["type"] == "state" and row["vin"] == car]
        first_hour = [row for row in states if row["t"] < 3600]
        assert any(row["speed_kmh"] > 0 for row in first_hour)
        assert any(row["charging"] for row in first_hour)
        assert any(row["plugged"] and not row["charging"] and row["soc"] == 100 for row in first_hour)
        assert sum(previous["speed_kmh"] > 0 and not current["vehicle_on"]
                   for previous, current in zip(states, states[1:])) >= 2
        errors = [row for row in rows if row["type"] == "error" and row["vin"] == car and row["code"] == "1008"]
        assert len(errors) > 1 and errors[-1]["t"] - errors[0]["t"] >= 120
        assert any(row["t"] == errors[-1]["t"] + 60 for row in states)


def test_privacy_finds_every_leak_class_with_redacted_excerpts():
    vin = "A" * 17
    email = "replay" + "@example.com"
    lat = 52 + 123456 / 1_000_000
    lon = 4 + 654321 / 1_000_000
    lines = [
        json.dumps({**_minimal()[2], "model": vin.lower()}),
        json.dumps({**_minimal()[2], "model": email}),
        json.dumps({**_minimal()[3], "private": {"a": lat, "b": lon}}),
        json.dumps({**_minimal()[4], "soc": 90}),
        json.dumps({**_minimal()[2], "energy": "secret"}),
        f"{lat} : {lon}",
        json.dumps({**_minimal()[3], "odometer_km": int("1" * 17)}),
        r'{"type":"error","vin":"TESTVIN0000000001","t":1,"code":"replay\u0040example.com"}',
        json.dumps({**_minimal()[3], "private": [lat, lon]}),
        f"{lat},",
        f"{lon}",
        json.dumps({**_minimal()[0], "stats": {email: 1}}),
        json.dumps({**_minimal()[3], "online": "ev"}),
        json.dumps({**_minimal()[3], "odometer_km": 17.12345, "speed_kmh": 2.12345}),
        json.dumps({**_minimal()[3], "odometer_km": 1e-6, "speed_kmh": 2e-6}),
    ]
    findings = list(privacy.find_leaks("\n".join(lines)))
    expected = {
        (1, "vin"), (2, "email"), (3, "coordinate_pair"), (4, "unexpected_key"),
        (5, "unexpected_string"), (6, "coordinate_pair"), (7, "vin"), (8, "email"),
        (9, "coordinate_pair"), (11, "coordinate_pair"), (12, "email"),
        (13, "unexpected_string"), (14, "coordinate_pair"), (15, "coordinate_pair"),
    }
    assert expected <= {(finding.line, finding.rule) for finding in findings}
    assert all(finding.excerpt == "[redacted]" for finding in findings)
    assert not list(privacy.find_leaks(json.dumps({**_minimal()[2], "model": "a" * 18})))
    assert not list(privacy.find_leaks(json.dumps({**_minimal()[3], "odometer_km": lat})))


def test_privacy_flags_uuid_and_underscored_coordinates_but_uses_decimal_exponents():
    uid = str(UUID(int=3)).upper()
    uuid_line = json.dumps({**_minimal()[2], "model": f"x{uid}"})
    assert "uuid" in {leak.rule for leak in privacy.find_leaks(uuid_line)}
    lat, lon = 52 + 123456 / 1_000_000, 4 + 654321 / 1_000_000
    assert "coordinate_pair" in {leak.rule for leak in privacy.find_leaks(f"{lat}_{lon}")}
    state_text = export_prod.json_line(_minimal()[3]).replace(
        '"odometer_km":null,"speed_kmh":null', '"odometer_km":1.2345e5,"speed_kmh":2.3456e3'
    )
    assert not list(privacy.find_leaks(state_text))
    precise = state_text.replace("1.2345e5", "1.2345e-5").replace("2.3456e3", "2.3456e-3")
    assert "coordinate_pair" in {leak.rule for leak in privacy.find_leaks(precise)}


@pytest.mark.parametrize(("stats", "rule"), [
    ({"nested": {"count": 3}}, "unexpected_stats_value"),
    ({"bad": -1}, "unexpected_stats_value"),
    ({"bad": True}, "unexpected_stats_value"),
    ({"bad": 1.0}, "unexpected_stats_value"),
    ({"upperCase": 1}, "unexpected_key"),
    ({"a" * 17: 1}, "vin"),
    ({("a" * 41): 1}, "unexpected_key"),
    (None, "unexpected_stats_value"),
])
def test_privacy_requires_flat_safe_stats(stats, rule):
    row = {**_minimal()[0], "stats": stats}
    assert rule in {leak.rule for leak in privacy.find_leaks(export_prod.json_line(row))}


class _PrivateGPS(dict):
    """Fail the test if production mapping ever inspects coordinate values."""

    def __getitem__(self, key):
        raise AssertionError("read a GPS member")

    get = items = values = keys = __getitem__


UIDS = tuple(UUID(int=index + 100) for index in range(5))
PAIRS = tuple((uid, chr(65 + index) * 17) for index, uid in enumerate(UIDS))


def _event(t, source="poll", kind="ok", *, pair=PAIRS[0], detail=None, payload=None):
    return {
        "user_id": pair[0], "vin": pair[1], "received_at": START + timedelta(seconds=t),
        "source": source, "kind": kind, "detail": detail, "payload": payload if payload is not None else {},
        "actor": "discard private actor",
    }


def _transformed(rows, *, salt=b"first", offset_s=4567, max_accounts=None):
    inventory = {pair: {"model_key": "not a model key"} for pair in PAIRS}
    inventory[PAIRS[0]] = {"model_key": "seal-u-dm-i", "powertrain": "phev"}
    output = io.StringIO()
    export = export_prod.Export(output, inventory, START, START + timedelta(days=1),
                                salt=salt, offset_s=offset_s, max_accounts=max_accounts)
    for row in rows:
        export.add(row)
    export.finish()
    return export, output.getvalue()


def test_transform_is_anonymous_and_salted():
    export, stream = _transformed([_event(1.54), _event(3.8, pair=PAIRS[1])])
    same, again = _transformed([_event(1.54), _event(3.8, pair=PAIRS[1])])
    other, other_stream = _transformed([], salt=b"other", offset_s=-2468)
    limited, _ = _transformed([], max_accounts=2)
    assert same.accounts == export.accounts and same.cars == export.cars and again == stream
    assert other.accounts != export.accounts and other.cars != export.cars
    assert json.loads(other_stream.splitlines()[-1])["start"] == "2026-01-04T23:18:52Z"
    assert list(limited.accounts) == list(export.accounts)[:2]
    for uid, real_vin in PAIRS:
        assert str(uid) not in stream and real_vin not in stream
    entries = [json.loads(line) for line in stream.splitlines()]
    header = entries[-1]
    assert header["start"] == "2026-01-05T01:16:07Z" and header["duration_s"] == 86400
    assert "seed" not in header and not list(privacy.find_leaks(stream))
    assert [row["t"] for row in entries if row["type"] == "state"] == [2, 4]
    cars = [row for row in entries if row["type"] == "car"]
    assert sum(car["model"] == "unknown" for car in cars) == 4
    assert sum(car["energy"] == "hybrid" for car in cars) == 1
    assert len({car["vin"] for car in cars}) == len(PAIRS)


def test_export_projects_patches_and_recovers_from_errors(tmp_path):
    lat = 52 + 134789 / 1_000_000
    lon = 4 + 987431 / 1_000_000
    gps = _PrivateGPS(latitude=lat, longitude=lon)
    initial = {
        "currentSoc": 71.123456, "odometer": 10000.12899, "speed": 3000, "online": True,
        "vehicleOn": False, "charging": {"isCharging": False, "isConnected": False, "connectState": 1},
        "chargePowerKw": 3.54521, "minutesToFull": 7, "climate": {"hvacOn": False},
        "fuelPercent": 34.123456, "gps": gps, "gpsFresh": True, "privateField": "drop this text",
    }
    _, stream = _transformed([
        _event(0, payload=initial),
        _event(1, payload={"currentSoc": 71.12}),
        _event(2, kind="error", detail="1008"),
        _event(3, payload={"vehicleOn": 1, "speed": False, "minutesToFull": 2.0, "online": "yes"}),
        _event(4, kind="error", detail="session_expired"),
        _event(5, payload={}),
        _event(6, kind="error", detail="1001"),
        _event(7, "mqtt", "vehicleInfo", payload={"currentSoc": 71.12, "ignored": "drop me"}),
        _event(8, kind="error", detail="6002"),
        _event(9, "mqtt", "remoteControl", payload={"online": True, "name": "drop me"}),
        _event(10, "mqtt", "smartCharge", payload={
            "currentSoc": None, "charging": {"isConnected": False, "isCharging": 0, "connectState": None},
        }),
        _event(11, "mqtt", "smartCharge", payload={
            "currentSoc": "bad", "charging": {"soc": 80.331, "isCharging": True, "isConnected": True},
        }),
        _event(12, "mqtt", "vehicleInfo", payload={"speed": 12, "gps": gps, "gpsFresh": False}),
        _event(13, "mqtt", "energyConsumption", payload={"anotherKey": "drop me"}),
        _event(14, kind="error", detail="exception"),
        _event(15, kind="error", detail="a code and a message"),
        _event(16, kind="error", detail="123456"),
        _event(17, "mqtt", "arbitrary private event"),
        _event(18, kind="skip"),
        _event(19, kind="error", detail="9"),
        _event(20, payload={"speed": float("inf"), "fuelPercent": float("nan")}),
    ])
    assert not any(value in stream for value in ("134789", "987431", "discard", "drop me", "private", "exception"))
    rows = [json.loads(line) for line in stream.splitlines()]
    states = {row["t"]: row for row in rows if row["type"] == "state"}
    pushes = {row["t"]: row for row in rows if row["type"] == "push"}
    assert list(states) == [0, 3, 20]
    assert states[0]["soc"] == 71.12 and states[0]["odometer_km"] == 10000.13
    assert states[0]["fuel_percent"] == 34.12 and states[0]["charge_power_kw"] == 3.55
    assert states[0]["gps"] is True and states[0]["hvac_on"] is False
    assert states[0]["speed_kmh"] is None and states[3]["minutes_to_full"] == 7
    assert states[3] == {**states[0], "t": 3}
    assert pushes[7]["soc"] == 71.12 and set(pushes[9]) == export_prod.LINE_KEYS["push"]
    assert set(pushes[10]) == export_prod.LINE_KEYS["push"] | {"plugged"}
    assert pushes[11]["charging"] is True and states[20]["soc"] == 80.33
    assert states[20]["gps"] is False and states[20]["speed_kmh"] == 12
    assert states[20]["odometer_km"] == 10000.13 and states[20]["fuel_percent"] == 34.12
    assert [row["code"] for row in rows if row["type"] == "error"] == ["1008", "1005", "1001", "6002", "9"]
    assert rows[-1]["stats"]["dropped_errors"] == 3 and rows[-1]["stats"]["dropped_events"] == 1
    assert rows[-1]["stats"]["dropped_fields"] > 10 and rows[-1]["stats"]["skipped_polls"] == 1
    pod = tmp_path / "stream.jsonl"
    pod.write_text(stream, encoding="utf-8")
    folded = tmp_path / "scenario.jsonl"
    export_prod._fold(pod, folded)
    assert not scenario.validate(folded).warnings
    assert not list(privacy.find_leaks(folded.read_text(encoding="utf-8")))


def test_export_null_means_unreported_except_where_it_is_state():
    awake = {"currentSoc": 64.0, "odometer": 1200.5, "fuelPercent": 40.0, "speed": 30, "online": True,
             "vehicleOn": True, "chargePowerKw": 7.0, "climate": {"hvacOn": True}}
    asleep = {"currentSoc": None, "odometer": None, "fuelPercent": None, "speed": None, "online": False,
              "vehicleOn": None, "chargePowerKw": None, "climate": {"hvacOn": None}}
    _, stream = _transformed([
        _event(0, payload=awake),
        # A sleeping car's poll: the battery, odometer and tank keep their last value.
        _event(60, payload=asleep),
        _event(120, payload=awake),
        # A push names only what it carries; its nulls are ignored, but a null charge power clears.
        _event(180, "mqtt", "vehicleInfo", payload={"currentSoc": None, "odometer": None, "speed": None,
                                                    "vehicleOn": None, "chargePowerKw": None, "online": True}),
        _event(240, "mqtt", "vehicleInfo", payload='{"speed": 41, "gps": {}}'),
    ])
    rows = [json.loads(line) for line in stream.splitlines()]
    states = {row["t"]: row for row in rows if row["type"] == "state"}
    pushes = {row["t"]: row for row in rows if row["type"] == "push"}
    assert (states[60]["soc"], states[60]["odometer_km"], states[60]["fuel_percent"]) == (64.0, 1200.5, 40.0)
    assert states[60]["online"] is False and states[60]["speed_kmh"] is None
    assert states[60]["vehicle_on"] is None and states[60]["charge_power_kw"] is None
    assert states[60]["hvac_on"] is None
    assert set(pushes[180]) == export_prod.LINE_KEYS["push"] | {"online", "charge_power_kw"}
    assert pushes[180]["charge_power_kw"] is None
    assert states[180]["soc"] == 64.0 and states[180]["speed_kmh"] == 30 and states[180]["vehicle_on"] is True
    assert states[180]["charge_power_kw"] is None
    # Text JSON from the driver is read like an object.
    assert pushes[240]["speed_kmh"] == 41 and pushes[240]["gps"] is True and states[240]["speed_kmh"] == 41
    assert rows[-1]["stats"]["unreported_fields"] == 7


@pytest.mark.parametrize(("kind", "payload", "field"), [
    ("vehicleInfo", {"currentSoc": 77, "online": True}, "online"),
    ("smartCharge", {"charging": {"soc": 77, "isConnected": True}}, "plugged"),
])
def test_push_does_not_end_error_period(kind, payload, field):
    _, stream = _transformed([
        _event(0, kind="error", detail="1008"),
        _event(5, "mqtt", kind, payload=payload),
        _event(10, kind="skip"),
        _event(400, payload={"odometer": 1032}),
    ])
    timeline = [row for line in stream.splitlines() if (row := json.loads(line)).get("t") is not None]
    assert [(row["type"], row["t"]) for row in timeline] == [("error", 0), ("push", 5), ("state", 400)]
    assert timeline[-1]["soc"] == 77 and timeline[-1]["odometer_km"] == 1032
    assert timeline[-1][field] is timeline[1][field] is True


@pytest.mark.parametrize("model", ["A".lower() * 17, str(UUID(int=220))])
def test_export_drops_a_leak_shaped_model_key(model):
    output = io.StringIO()
    export = export_prod.Export(output, {PAIRS[0]: {"model_key": model}}, START, START + timedelta(days=1),
                                salt=b"salt", offset_s=0)
    export.finish()
    rows = [json.loads(line) for line in output.getvalue().splitlines()]
    assert rows[1]["model"] == "unknown" and rows[-1]["stats"]["dropped_models"] == 1
    assert model not in output.getvalue()


def test_export_stops_before_writing_a_leak():
    output = io.StringIO()
    with pytest.raises(export_prod._ExportError, match=r"^uuid on car line$"):
        export_prod._write_checked(output, {**_minimal()[2], "model": str(UUID(int=1))})
    assert not output.getvalue()


class _FakeSessions:
    """Record transaction setup, query parameters and page lifetimes."""

    def __init__(self, pages):
        self.pages = iter(pages)
        self.transactions = []
        self.activity = []
        self.open = False

    @asynccontextmanager
    async def begin(self):
        call = []
        self.transactions.append(call)

        class Session:
            async def execute(self, statement, params=None):
                assert owner.open
                params = params or {}
                assert set(statement.compile().params) == params.keys()
                call.append((str(statement), params.copy()))
                if not params:
                    return None

                def mappings():
                    assert owner.open
                    yield from next(owner.pages)

                return SimpleNamespace(mappings=mappings)

        owner = self
        assert not self.open
        self.open = True
        self.activity.append("enter")
        try:
            yield Session()
        finally:
            self.activity.append("exit")
            self.open = False


@needs_sqlalchemy
@pytest.mark.asyncio
@pytest.mark.parametrize("mode", ["inventory", "dry-run", "events"])
async def test_database_queries_are_read_only_bounded_and_paginated(monkeypatch, mode):
    pauses = []

    async def sleep(seconds):
        pauses.append(seconds)

    monkeypatch.setattr(export_prod.asyncio, "sleep", sleep)
    event1 = {**_event(0.11), "id": UUID(int=1)}
    event2 = {**_event(0.11, pair=PAIRS[1]), "id": UUID(int=2)}
    fake = _FakeSessions([[event1, event2], [], [event1]])
    query = export_prod.ExportQueries(fake, text, 2, 50)
    until = START + timedelta(hours=1, minutes=24)
    if mode == "events":
        stream = query.events(START, until, UIDS[:2])
        cursor_columns = ("received_at", "id")
    else:
        stream = query.inventory(START, until, count_only=mode == "dry-run")
        cursor_columns = ("user_id", "vin")
    yielded = []
    async for row in stream:
        assert not fake.open
        yielded.append(row)
        fake.activity.append("yield")
    assert yielded == [event1, event2, event1]
    assert fake.activity == ["enter", "exit", "yield", "yield", "enter", "exit", "enter", "exit", "yield"]
    assert pauses == [0.05, 0.05]
    for first, second, third, fourth, (sql, params) in fake.transactions:
        assert first[0] == "SET TRANSACTION READ ONLY"
        assert second[0] == "SET LOCAL statement_timeout = '30s'"
        assert third[0] == "SET LOCAL idle_in_transaction_session_timeout = '10s'"
        assert fourth[0] == "SET LOCAL lock_timeout = '2s'"
        tables = set(re.findall(r"\bFROM (ingest_events|cars|identities)\b", sql))
        assert tables == ({"ingest_events", "cars", "identities"} if mode == "inventory" else {"ingest_events"})
        sql_for_tables = re.sub(r"FROM (ingest_events|cars|identities)\b", "", sql)
        assert not re.search(r"\b(?:FROM|JOIN)\s+[A-Za-z_]+", sql_for_tables.replace("JOIN LATERAL", ""))
        assert "received_at >= :lower AND received_at < :upper" in sql and "LIMIT :limit" in sql
        assert params["limit"] == 2 and params["upper"] - params["lower"] <= timedelta(hours=1)
        assert "actor" not in sql and "latitude" not in sql and "longitude" not in sql
        if mode == "dry-run":
            assert "COUNT(*)" in sql and "payload" not in sql and "detail" not in sql
        elif mode == "events":
            assert "ORDER BY received_at, id" in sql and "CAST(:users AS uuid[])" in sql
            assert " - 'gps'" in sql and "CASE WHEN (source = 'poll' AND kind = 'ok') OR source = 'mqtt'" in sql
        else:
            assert "CASE WHEN model_key_source IN ('vin', 'name') THEN model_key END AS model_key" in sql
    second_query, second_params = fake.transactions[1][-1]
    assert f"({', '.join(cursor_columns)}) > (:after_first, :after_second)" in second_query
    assert (second_params["after_first"], second_params["after_second"]) == tuple(
        event2[key] for key in cursor_columns
    )
    assert fake.transactions[2][-1][1]["lower"] == START + timedelta(hours=1)
    assert fake.transactions[2][-1][1]["upper"] == until


@pytest.mark.parametrize("outcome", ["ok", "remote-error", "remote-leak", "leak", "malformed", "dry-run"])
def test_local_fetch_finalizes_only_verified_streams(tmp_path, monkeypatch, outcome):
    monkeypatch.setattr(export_prod, "_export_directory", lambda path: path)
    _, stream = _transformed([_event(0, payload={"online": True})])
    if outcome == "leak":
        stream = stream.replace("seal-u-dm-i", "A" * 17)
    elif outcome == "malformed":
        stream = stream.replace('"t":0', '"t":-1')
    elif outcome == "dry-run":
        stream = json.dumps({"accounts": 1, "cars": 1, "rows": dict.fromkeys(export_prod.ROW_KINDS, 0)})

    def fake_remote(command, *, stdin, stdout, stderr, check):
        assert command[:13] == ["kubectl", "-n", "bydconnect", "exec", "-i", "deploy/api", "--",
                                "nice", "-n", "19", "python", "-", "--days"]
        assert stdin.name.endswith("export_prod.py")
        stdout.write(stream.encode())
        assert stderr == export_prod.subprocess.PIPE
        diagnostics = b"VIN and private diagnostics are ignored\nvin on car line\n" if outcome == "remote-leak" else b""
        return argparse.Namespace(returncode=1 if outcome.startswith("remote-") else 0, stderr=diagnostics)

    monkeypatch.setattr(export_prod.subprocess, "run", fake_remote)
    args = argparse.Namespace(out_dir=tmp_path, days=14, page_rows=5, pause_ms=50, until=None, max_minutes=45,
                              max_accounts=None, explain=False, dry_run=outcome == "dry-run")
    if outcome in {"ok", "dry-run"}:
        report = export_prod._fetch(args)
        if outcome == "ok":
            files = list(tmp_path.glob("export-2026-01-05-14d.jsonl"))
            assert len(files) == 1 and report == scenario.validate(files[0]).as_dict()
        else:
            assert report["accounts"] == 1 and not list(tmp_path.iterdir())
    else:
        expected = "^vin on car line$" if outcome == "remote-leak" else None
        with pytest.raises(export_prod._ExportError, match=expected):
            export_prod._fetch(args)
        assert not list(tmp_path.iterdir())


def test_fetch_refuses_a_git_work_tree(tmp_path, monkeypatch):
    monkeypatch.setattr(export_prod, "EXPORT_ROOT", tmp_path)
    (tmp_path / ".git").mkdir()
    with pytest.raises(export_prod._ExportError, match="git work tree"):
        export_prod._export_directory(tmp_path / "nested")


def test_default_window_ends_ten_minutes_ago_on_a_minute_boundary():
    now = START + timedelta(hours=2, minutes=11, seconds=59, microseconds=987654)
    assert export_prod._default_until(now) == START + timedelta(hours=2, minutes=1)


@needs_sqlalchemy
@pytest.mark.asyncio
async def test_explain_projects_only_safe_plan_fields_in_bounded_read_only_transactions():
    private = "A" * 17
    raw = [{"Plan": {"Node Type": "Limit", "Plan Rows": 2, "Filter": private, "Plans": [
        {"Node Type": "Index Scan", "Relation Name": "ingest_events", "Index Name": "received_idx",
         "Plan Rows": 5, "Index Cond": private},
    ]}}]
    fake = _FakeSessions([[{"QUERY PLAN": raw}], [{"QUERY PLAN": json.dumps(raw)}], [{"QUERY PLAN": raw}]])
    query = export_prod.ExportQueries(fake, text, 2, 0)
    plans = await query.explain(START, START + timedelta(hours=8))
    expected = [
        {"node": "Limit", "relation": None, "index": None, "rows": 2},
        {"node": "Index Scan", "relation": "ingest_events", "index": "received_idx", "rows": 5},
    ]
    assert plans == dict.fromkeys(export_prod.PLAN_SHAPES, expected)
    assert export_prod._checked_plans(plans) == plans
    for transaction in fake.transactions:
        assert [sql for sql, _ in transaction[:4]] == [
            export_prod.READ_ONLY, export_prod.TIMEOUT, export_prod.IDLE_TIMEOUT, export_prod.LOCK_TIMEOUT,
        ]
        sql, params = transaction[-1]
        assert sql.startswith("EXPLAIN (FORMAT JSON) ") and "ANALYZE" not in sql
        assert params == {"lower": START, "upper": START + timedelta(hours=1), "limit": 2}
    assert "FROM cars" in fake.transactions[0][-1][0]
    assert "COUNT(*)" in fake.transactions[1][-1][0]
    assert "ORDER BY received_at, id" in fake.transactions[2][-1][0]


@pytest.mark.parametrize("bad_plan", [False, True])
def test_local_fetch_explain_validates_the_plan_and_needs_no_output_directory(monkeypatch, bad_plan):
    node = {"node": "Seq Scan", "relation": "ingest_events", "index": None, "rows": 99}
    if bad_plan:
        node["Filter"] = "A" * 17
    plans = dict.fromkeys(export_prod.PLAN_SHAPES, [node])

    def fake_remote(command, *, stdin, capture_output, check):
        assert "--explain" in command and "--dry-run" not in command
        assert command[command.index("--max-minutes") + 1] == "15"
        assert capture_output and not check and stdin.name.endswith("export_prod.py")
        return SimpleNamespace(returncode=0, stdout=export_prod.json_line(plans).encode(), stderr=b"")

    monkeypatch.setattr(export_prod.subprocess, "run", fake_remote)
    args = argparse.Namespace(days=14, page_rows=5, pause_ms=50, until=None, max_minutes=15,
                              max_accounts=None, explain=True, dry_run=False)
    if bad_plan:
        with pytest.raises(export_prod._ExportError, match="invalid export plan node"):
            export_prod._fetch(args)
    else:
        assert export_prod._fetch(args) == plans


def test_pod_arms_a_process_alarm_before_starting(monkeypatch):
    calls = []

    def handler(signum, disposition):
        calls.append((signum, disposition))
        return "old handler"

    async def run(args, output):
        calls.append("run")

    monkeypatch.setattr(export_prod, "_run_pod", run)
    monkeypatch.setattr(export_prod, "signal", SimpleNamespace(
        signal=handler, SIGALRM=14, SIG_DFL=0, alarm=calls.append,
    ))
    export_prod._pod_with_deadline(argparse.Namespace(max_minutes=15))
    assert calls == [(14, 0), 900, "run", 0, (14, "old handler")]


@pytest.mark.parametrize("local", [False, True])
def test_main_reports_only_unexpected_exception_class(monkeypatch, capsys, local):
    private = f"{'A' * 17} {52 + 0.123456}, {4 + 0.654321}"

    def fail(*args):
        raise RuntimeError(private)

    monkeypatch.setattr(export_prod.logging, "disable", lambda level: None)
    monkeypatch.setattr(export_prod, "_fetch" if local else "_pod_with_deadline", fail)
    argv = ["export_prod.py", "fetch", "--explain"] if local else ["export_prod.py"]
    monkeypatch.setattr(sys, "argv", argv)
    with pytest.raises(SystemExit) as exc:
        export_prod.main()
    assert exc.value.code == 1
    assert capsys.readouterr() == ("", "replay export failed: RuntimeError\n")


@pytest.mark.asyncio
async def test_export_against_postgres(tmp_path):
    url = os.environ.get("REPLAY_TEST_PG_URL")
    if not url:
        pytest.skip("REPLAY_TEST_PG_URL is unset")
    if not url.startswith("postgresql+asyncpg://"):
        pytest.fail("REPLAY_TEST_PG_URL must be a postgresql+asyncpg URL")
    from sqlalchemy.ext.asyncio import async_sessionmaker, create_async_engine

    engine = create_async_engine(url)
    created = False
    private_model = "customer-car-name"
    private_error = "private-upstream-text\0"
    lat, lon = 52 + 357194 / 1_000_000, 4 + 926438 / 1_000_000
    events = [
        _event(0, payload={"currentSoc": 41, "gps": {"latitude": lat, "longitude": lon}}),
        _event(0, "mqtt", "vehicleInfo", payload={"speed": 28, "gps": None}),
        _event(0, payload={}),
        _event(60, kind="error", detail="1008", payload={"error": private_error}),
        _event(120, kind="skip", payload={"reason": private_error}),
        _event(180, payload={"currentSoc": 43}),
        _event(3600, pair=PAIRS[1], payload={"odometer": 1234}),
        _event(3610, pair=PAIRS[2], payload={"currentSoc": 65}),
    ]
    try:
        async with engine.begin() as connection:
            existing = await connection.scalar(text("""
                SELECT count(*) FROM pg_class AS c JOIN pg_namespace AS n ON c.relnamespace = n.oid
                WHERE n.nspname = current_schema() AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f')
            """))
            if existing:
                pytest.fail("REPLAY_TEST_PG_URL must point to an empty throwaway database")
            await connection.execute(text("""
                CREATE TABLE cars (
                    user_id uuid, vin varchar(32), deleted_at timestamptz,
                    model_key varchar(80), model_key_source varchar(16), is_hybrid boolean, powertrain varchar(16)
                )
            """))
            await connection.execute(text("""
                CREATE TABLE ingest_events (
                    id uuid PRIMARY KEY, user_id uuid NOT NULL, vin varchar(32) NOT NULL,
                    received_at timestamptz NOT NULL, source varchar(16) NOT NULL, kind varchar(32) NOT NULL,
                    detail varchar(64), actor varchar(80), payload json
                )
            """))
            await connection.execute(text("CREATE INDEX replay_test_received_at ON ingest_events (received_at, id)"))
            await connection.execute(text("""
                INSERT INTO cars (user_id, vin, model_key, model_key_source, is_hybrid, powertrain)
                VALUES (:user_id, :vin, :model, :source, false, 'ev')
            """), [
                {"user_id": UIDS[0], "vin": PAIRS[0][1], "model": "seal", "source": "name"},
                {"user_id": UIDS[1], "vin": PAIRS[1][1], "model": private_model, "source": "text"},
            ])
            await connection.execute(text("""
                INSERT INTO ingest_events (id,user_id,vin,received_at,source,kind,detail,actor,payload)
                VALUES (:id,:user_id,:vin,:received_at,:source,:kind,:detail,:actor,CAST(:payload AS json))
            """), [
                {**row, "id": UUID(int=index + 1), "payload": json.dumps(row["payload"])}
                for index, row in reversed(list(enumerate(events)))
            ])
        created = True

        queries = export_prod.ExportQueries(async_sessionmaker(engine), text, 2, 1)
        until = START + timedelta(hours=2)
        inventory = {(row["user_id"], row["vin"]): row
                     async for row in queries.inventory(START, until, count_only=False)}
        assert set(inventory) == set(PAIRS[:3])
        assert [inventory[pair]["model_key"] for pair in PAIRS[:3]] == ["seal", None, None]
        counts = [row async for row in queries.inventory(START, until, count_only=True)]
        assert sum(sum(row[key] for key in export_prod.ROW_KINDS) for row in counts) == len(events)
        rows = [row async for row in queries.events(START, until, None)]
        assert [row["id"].int for row in rows] == list(range(1, len(events) + 1))

        def payload(row):
            value = row["payload"]
            return json.loads(value) if isinstance(value, str) else value

        assert [payload(row) for row in rows[:5]] == [
            {"currentSoc": 41, "gps": {}}, {"speed": 28, "gps": None}, {}, None, None,
        ]
        assert rows[3]["detail"] == "1008"
        assert all("actor" not in row for row in rows)

        output = io.StringIO()
        export = export_prod.Export(output, inventory, START, until, salt=b"integration", offset_s=9876)
        for row in rows:
            export.add(row)
        export.finish()
        content = output.getvalue()
        private_values = (private_model, "private-upstream-text", "357194", "926438",
                          *(str(uid) for uid in UIDS[:3]), *(vin for _, vin in PAIRS[:3]))
        assert all(value not in content for value in private_values)
        assert not list(privacy.find_leaks(content))
        lines = [json.loads(line) for line in content.splitlines()]
        by_vin = {row["vin"]: row["model"] for row in lines if row["type"] == "car"}
        assert [by_vin[export.cars[pair][0]] for pair in PAIRS[:3]] == ["seal", "unknown", "unknown"]
        assert [row["t"] for row in lines if "t" in row] == [0, 0, 0, 60, 180, 3600, 3610]
        assert lines[-1]["stats"] == {"state": 5, "push": 1, "error": 1, "skipped_polls": 1}
        stream, folded = tmp_path / "postgres.jsonl", tmp_path / "scenario.jsonl"
        stream.write_text(content, encoding="utf-8")
        export_prod._fold(stream, folded)
        report = scenario.validate(folded)
        assert report.accounts == report.cars == 3 and not report.warnings

        limited_output = io.StringIO()
        limited = export_prod.Export(limited_output, inventory, START, until, salt=b"integration", offset_s=5,
                                     max_accounts=1)
        filtered = [row async for row in queries.events(START, until, limited.accounts)]
        assert len(limited.accounts) == 1
        assert [row["id"] for row in filtered] == [row["id"] for row in rows if row["user_id"] in limited.accounts]
        for row in filtered:
            limited.add(row)
        limited.finish()
        assert json.loads(limited_output.getvalue().splitlines()[-1])["accounts"] == 1
        plans = await queries.explain(START, until)
        assert export_prod._checked_plans(plans) == plans
        assert all(any(node["relation"] == "ingest_events" for node in nodes) for nodes in plans.values())
    finally:
        try:
            if created:
                async with engine.begin() as connection:
                    await connection.execute(text("DROP TABLE ingest_events, cars"))
        finally:
            await engine.dispose()
