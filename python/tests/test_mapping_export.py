"""Exercise the paired exporter with fabricated rows, a fake database and a composed pod script."""

from __future__ import annotations

import argparse
import asyncio
import io
import json
import math
import os
import re
import subprocess
import sys
from collections import Counter
from datetime import UTC, datetime, timedelta
from pathlib import Path
from types import SimpleNamespace
from uuid import UUID

import pytest

try:
    from sqlalchemy import text
except ImportError:  # the statements are still recorded as plain strings
    text = None

from bench.mapping import export_pairs as pairs
from bench.replay import export_prod as ep

VIN = "TESTVIN0000000001"
PRIVATE_VIN = "ABCDEFGHJK1234567"  # fabricated, but shaped like a real VIN
USER = UUID(int=0x5EED)
OTHER_USER = UUID(int=0xBEEF)
CAR_ID = "car-private-0001"
OTHER_CAR = "car-private-0002"
UNTIL = datetime(2026, 3, 1, 12, tzinfo=UTC)
BASE = UNTIL - timedelta(hours=6)
LAT, LNG = 52.123456, 4.654321
CLOUD_LAT = 52.124456  # 0.001 degrees (about 111 m) north of the car
MOVED_LAT = 52.126456  # 0.002 degrees further north
PRIVATE_DIGITS = ("123456", "654321", "124456", "126456", "123556")
OFFSET = 2_345_671


# --- the fake database, shared by the in-process tests and the pod subprocess ---------------------

FAKE_DB = r'''
import json
import re
from contextlib import asynccontextmanager
from datetime import datetime

STAMPS = ("at", "received_at", "first_seen_at", "deleted_at")
TAG = re.compile(r"/\* (\w+) \*/")
PUSH = ("vehicleInfo", "smartCharge", "energyConsumption", "remoteControl")
BUCKETS = ("poll_ok", "poll_error", "poll_skip", "poll_other", "mqtt_vehicleInfo", "mqtt_remoteControl",
           "mqtt_smartCharge", "mqtt_energyConsumption", "mqtt_other", "other")
SAFE_DETAIL = re.compile(r"[0-9]{1,5}|session_expired|received|min_gap|busy|lock|not_selected|car_unlinked|"
                         r"missing_vin|exception|upstream")
SAFE_CODE = re.compile(r"1001|1008|6051|http_[0-9]{3}|crypto")
ENDPOINTS = ("realtime", "energy", "charging", "gps", "hvac", "schedule", "vehicleInfo", "smartCharge",
             "energyConsumption", "remoteControl")
PLAN_RELATIONS = {
    "identify": "car_telemetry", "metadata": "cars", "car_rows": "car_telemetry_p20260228",
    "car_count": "car_telemetry_p20260228", "cloud_rows": "ingest_events", "cloud_count": "ingest_events",
    "raw_rows": "byd_raw_replies_p20260228", "raw_count": "byd_raw_replies_p20260228",
}
INDEXES = {"cloud_rows": "ix_ingest_events_user_vin_received", "cloud_count": "ix_ingest_events_user_vin_received"}


def load(path):
    with open(path, encoding="utf-8") as file:
        tables = json.load(file)
    for rows in tables.values():
        for row in rows:
            for key in STAMPS:
                if isinstance(row.get(key), str):
                    row[key] = datetime.fromisoformat(row[key])
    return tables


def bucket(source, kind):
    if source == "poll":
        return "poll_" + kind if kind in ("ok", "error", "skip") else "poll_other"
    if source == "mqtt":
        return "mqtt_" + kind if kind in PUSH else "mqtt_other"
    return "other"


class Result:
    def __init__(self, rows, db):
        self.rows = rows
        self.db = db

    def mappings(self):
        assert self.db.open
        return iter(self.rows)


class Session:
    def __init__(self, db, statements):
        self.db = db
        self.statements = statements

    async def execute(self, statement, params=None):
        assert self.db.open
        sql = str(statement)
        if hasattr(statement, "compile"):
            assert set(statement.compile().params) == set(params or {})
        self.statements.append((sql, dict(params or {})))
        if params is None:
            assert sql.startswith(("SET TRANSACTION READ ONLY", "SET LOCAL "))
            return None
        return Result(self.db.run(sql, params), self.db)


class FakeDB:
    """Answer the exporter's statements from fabricated tables and record every transaction."""

    def __init__(self, tables, *, json_as_text=False, fail_on=None, scans=None):
        self.tables = {name: list(tables.get(name, ()))
                       for name in ("car_telemetry", "ingest_events", "cars", "car_app_devices", "byd_raw_replies")}
        self.json_as_text = json_as_text  # the plans, as some drivers return json: the data columns are always text
        self.fail_on = fail_on
        # statement tag -> the scan node type its plan shows, or a function of the statement's parameters
        # that returns the plan's child nodes (so a plan can differ per day or for a resumed page)
        self.scans = scans or {}
        self.transactions = []
        self.open = False

    @asynccontextmanager
    async def begin(self):
        assert not self.open
        statements = []
        self.transactions.append(statements)
        self.open = True
        try:
            yield Session(self, statements)
        finally:
            self.open = False

    @staticmethod
    def _text(value):
        # CAST(... AS text): a stored text value comes back as it is, anything else as its JSON text.
        return value if value is None or isinstance(value, str) else json.dumps(value)

    def _child(self, tag, params):
        scan = self.scans.get(tag, "Index Scan")
        if callable(scan):
            return scan(params)
        index = INDEXES.get(tag, "fake_idx") if "Index" in scan else None
        return [{"Node Type": scan, "Relation Name": PLAN_RELATIONS[tag], "Index Name": index,
                 "Plan Rows": 7, "Index Cond": "52.123456"}]

    def run(self, sql, params):
        tag = TAG.search(sql).group(1)
        if tag == self.fail_on:
            raise RuntimeError("private ABCDEFGHJK1234567 at 52.123456, 4.654321")
        if sql.startswith("EXPLAIN"):
            plan = [{"Plan": {"Node Type": "Limit", "Plan Rows": 2, "Filter": "ABCDEFGHJK1234567",
                              "Plans": self._child(tag, params)}}]
            return [{"QUERY PLAN": json.dumps(plan) if self.json_as_text else plan}]
        return getattr(self, tag)(params)

    def identify(self, p):
        ids = sorted({row["car_id"] for row in self.tables["car_telemetry"]
                      if p["lower"] <= row["at"] < p["upper"] and row["car_id"] > p.get("after", "")})
        return [{"car_id": car} for car in ids[:p["limit"]]]

    def metadata(self, p):
        found = []
        for car in self.tables["cars"]:
            if car["id"] != p["car"]:
                continue
            device = next((row for row in self.tables["car_app_devices"] if row["car_id"] == car["id"]), {})
            found.append({
                "user_id": car["user_id"], "vin": car.get("vin"),
                "model_key": car.get("model_key") if car.get("model_key_source") in ("vin", "name") else None,
                "is_hybrid": car.get("is_hybrid", False), "powertrain": car.get("powertrain"),
                "deleted": car.get("deleted_at") is not None, "device_source": device.get("source"),
                "app_version": device.get("app_version"), "first_seen_at": device.get("first_seen_at"),
            })
        return found[:2]

    def _car(self, p):
        return [row for row in self.tables["car_telemetry"]
                if row["car_id"] == p["car"] and p["lower"] <= row["at"] < p["upper"]]

    def car_rows(self, p):
        rows = sorted((row for row in self._car(p) if "after" not in p or row["at"] > p["after"]),
                      key=lambda row: row["at"])
        columns = ("at", "cadence", "speed_kph", "soc_pct", "twelve_volt", "odometer_km", "power_level", "gear")
        return [{**{column: row.get(column) for column in columns}, "data": self._text(row.get("data"))}
                for row in rows[:p["limit"]]]

    def car_count(self, p):
        return [{"row_count": len(self._car(p))}]

    def _cloud(self, p):
        return [row for row in self.tables["ingest_events"]
                if row["user_id"] == p["user"] and row["vin"] == p["vin"]
                and p["lower"] <= row["received_at"] < p["upper"]]

    def cloud_rows(self, p):
        rows = sorted(self._cloud(p), key=lambda row: (row["received_at"], row["id"]))
        if "after_first" in p:
            rows = [row for row in rows if (row["received_at"], row["id"]) > (p["after_first"], p["after_second"])]
        found = []
        for row in rows[:p["limit"]]:
            detail = row.get("detail")
            safe = detail is not None and SAFE_DETAIL.fullmatch(detail) is not None
            payload = (row["source"] == "poll" and row["kind"] == "ok") or row["source"] == "mqtt"
            found.append({
                "received_at": row["received_at"], "id": row["id"], "source": row["source"], "kind": row["kind"],
                "detail": detail if safe else None, "detail_dropped": detail is not None and not safe,
                "payload": self._text(row.get("payload")) if payload else None,
            })
        return found

    def cloud_count(self, p):
        counts = dict.fromkeys(BUCKETS, 0)
        for row in self._cloud(p):
            counts[bucket(row["source"], row["kind"])] += 1
        return [{"row_count": sum(counts.values()), **counts}]

    def _raw(self, p):
        return [row for row in self.tables["byd_raw_replies"]
                if row["car_id"] == p["car"] and p["lower"] <= row["at"] < p["upper"]]

    def raw_rows(self, p):
        rows = sorted(self._raw(p), key=lambda row: (row["at"], row["endpoint"], row["seq"]))
        if "after_at" in p:
            cursor = (p["after_at"], p["after_endpoint"], p["after_seq"])
            rows = [row for row in rows if (row["at"], row["endpoint"], row["seq"]) > cursor]
        found = []
        for row in rows[:p["limit"]]:
            code = row.get("code")
            safe = code is not None and SAFE_CODE.fullmatch(code) is not None
            timeout = code == "TimeoutError"
            found.append({
                "at": row["at"], "endpoint": row["endpoint"], "seq": row["seq"], "source": row["source"],
                "code": code if safe else "timeout" if timeout else None if code is None else "other",
                "code_dropped": code is not None and not safe and not timeout,
                "raw": self._text(row.get("raw")),
            })
        return found

    def raw_count(self, p):
        counts = dict.fromkeys(ENDPOINTS + ("other",), 0)
        for row in self._raw(p):
            counts[row["endpoint"] if row["endpoint"] in ENDPOINTS else "other"] += 1
        return [{"row_count": sum(counts.values()), **counts}]
'''
_fake = {}
exec(compile(FAKE_DB, "fake_db", "exec"), _fake)
FakeDB = _fake["FakeDB"]


def _iso(stamp):
    return stamp.isoformat().replace("+00:00", "Z")


FULL_DATA = {
    "doors": {"fl": "closed", "fr": "open", "rl": "closed", "rr": "closed", "hood": "closed", "trunk": "closed"},
    "lock": "locked", "alarm": "off", "alarmRaw": 0, "twelveVoltLevel": "normal", "averageSpeedKph": 44.4,
    "outsideTempC": 18.5, "remainingKwh": 60.123456, "lifetimeKwh": 5000, "tyrePressureKpa": {
        "fl": 250.0, "fr": 251.5, "rl": 248, "rr": 249.25}, "tyreTempC": {"fl": 21.5, "fr": 22, "rl": 21, "rr": 20},
    "chargingStateRaw": 0, "chargingGunRaw": 1, "chargingPowerKw": 0.0, "chargingTypeRaw": 1,
    "chargerStateRaw": 0, "chargeRemainingMin": 0, "chargedKwh": 1.5, "motorPowerKw": -12.5,
    "motorRpm": 3000, "slopeRaw": 1.2, "acceleratorPct": 20, PRIVATE_VIN: 1, "bad key!": 2,
    "location": {"lat": LAT, "lng": LNG, "altitudeM": 12.5, "headingDeg": 270.25, "speedKph": 49.5,
                 "accuracyM": 4.5, "ageS": 1.5},
}


def _car_row(at, *, car=CAR_ID, data=None, **columns):
    return {"car_id": car, "at": at, "user_id": USER, "cadence": None, "speed_kph": None, "soc_pct": None,
            "twelve_volt": None, "odometer_km": None, "power_level": None, "gear": None, "data": data, **columns}


def _event(number, at, source="poll", kind="ok", *, detail=None, payload=None, user=USER, vin=PRIVATE_VIN):
    return {"id": UUID(int=number), "user_id": user, "vin": vin, "received_at": at, "source": source, "kind": kind,
            "detail": detail, "actor": "api-private-pod-7f9c", "payload": payload if payload is not None else {}}


POLL = {
    "lastUpdated": _iso(BASE + timedelta(seconds=20)), "odometer": 12345.6, "currentSoc": 80, "online": True,
    "vehicleOn": True, "speed": 50, "chargePowerKw": None, "minutesToFull": None, "asleep": False,
    "vehicleUnreachable": False, "last50kmEnergy": 15.25,
    "energyDistribution": {"drive": 70, "electric": 10.5, "hvac": 15, "other": None},
    "charging": {"isCharging": False, "isConnected": False, "connectState": 0, "soc": 80, "minutesToFull": None},
    "climate": {"hvacOn": True}, "gps": {"latitude": CLOUD_LAT, "longitude": LNG}, "gpsFresh": True,
    "placeKind": "home", "lastStationaryPollAt": _iso(BASE), "fuelPercent": None,
}


RAW_MS = int((BASE + timedelta(seconds=5)).timestamp()) * 1000  # an unshifted epoch time in milliseconds
RAW_S = int(BASE.timestamp())  # and in seconds
# A realtime reply as the DiLink captures show them (their key names, values of the same kind), with the
# fields the decode is for, and what the backend leaves in that may not leave the pod.
REALTIME = {
    "onlineState": 1, "connectState": -1, "vehicleState": 0, "powerGear": 3, "okLight": 1, "epb": 0, "ins": -1,
    "pwr": 2, "eps": 0, "esp": 0, "abs": 0, "svs": 0, "srs": 0, "ect": 0, "ectValue": -1, "sentryStatus": 2,
    "chargeState": 15, "chargingState": -1, "flashChargeState": 2, "flashChargeConfig": 0, "chargeEndHour": 22,
    "chargeEndMinute": 30, "lightStopStatus": 0, "trunkLid": 0, "trunkLidLower": -1, "gl": -27313.2, "rate": -999,
    "totalPower": 5.1, "chargePower": "6.3", "chargePowerUnit": "kW",
    "leftFrontDoor": 0, "rightFrontDoor": 1, "leftRearDoor": 0, "rightRearDoor": 0, "leftFrontDoorLock": 2,
    "rightFrontDoorLock": 2, "leftRearDoorLock": 2, "rightRearDoorLock": 2, "leftFrontWindow": 1,
    "rightFrontWindow": 0, "leftRearWindow": 0, "rightRearWindow": 0, "skylight": -1, "leftFrontWindowPct": 40,
    "rightFrontWindowPct": 0, "leftRearWindowPct": 0, "rightRearWindowPct": 0,
    "leftFrontTirepressure": 2.4, "rightFrontTirepressure": 2.3, "leftRearTirepressure": 2.8,
    "rightRearTirepressure": 2.7, "leftFrontTireStatus": 0, "rightFrontTireStatus": 0, "leftRearTireStatus": 0,
    "rightRearTireStatus": 0, "tirePressUnit": 1, "tirepressureSystem": 0, "rapidTireLeak": -1,
    "elecPercent": 87, "enduranceMileage": 454, "enduranceMileageV2Unit": "km", "totalMileage": 32131, "speed": 39,
    "tempInCar": 23, "mainSettingTemp": 18, "mainSettingTempNew": 18.0, "airRunState": 1, "lessOneMin": False,
    "nearestEnergyConsumption": "19.3", "nearestEnergyConsumptionUnit": "kW\u00b7h/100km",
    "repairModeSwitch": "0", "time": RAW_MS,
    # Not on the table: a time zone and a plan list with a name, a user id and a start time.
    "vehicleTimeZone": "Africa/Johannesburg",
    "standByCarList": [{"planName": "Alice morning", "userId": "98033", "startTime": "07:15"}],
}
REALTIME_FIELDS = {key: value for key, value in REALTIME.items()
                   if key not in ("vehicleTimeZone", "standByCarList", "time")}
REALTIME_FIELDS.update(chargePower=6.3, nearestEnergyConsumption=19.3, time_t=151205.0)
GPS_REPLY = {"data": {"latitude": CLOUD_LAT, "longitude": LNG, "direction": 270.25, "altitude": 12.5,
                      "speed": 50, "gpsTimeStamp": RAW_S, "satellites": 9}, "res": 2}
COMPACT_UUID = UUID(int=0x1234_5678_9ABC_DEF0_1234_5678_9ABC_DEF0).hex
# Payloads that tried to get a position, a heading, a place, an identifier or a date past the blocklist the
# first design used. Each must be dropped (or have its value refused) and counted, never written, never abort.
LEAKS = [
    ("realtime", {"gpsInfo": {"y": 52.123456, "x": 4.654321}}),
    ("realtime", {"pos": {"x": 52.123456, "y": 4.654321}}),
    ("vehicleInfo", {"mapX": 52.123456, "mapY": 4.654321}),
    ("realtime", {"pt": [52.123456, 4.654321]}),
    ("realtime", {"pairs": [[52.123456, 4.654321], [52.124456, 4.654321]]}),
    ("realtime", {"track": [[52.123456, 4.654321]]}),
    ("realtime", {"latE6": 52123456, "value": 52123456, "gl": 52123456, "totalMileage": 4654321}),
    ("realtime", {"note": "52.123:4.654", "chargePowerUnit": "52.123:4.654", "enduranceMileageV2Unit": "52.123"}),
    ("vehicleInfo", {"headingDeg": 270.25, "yaw": 270.25, "compass": "NW", "elevationM": 12.5}),
    ("realtime", {"place": "Sea Point", "city": "Cape Town", "nearestEnergyConsumptionUnit": "Sea Point"}),
    ("realtime", {"d20261101": 1, "u123456789012": 1, COMPACT_UUID: 1, PRIVATE_VIN: 1}),
    ("realtime", {"ownerRef": 7654321, "vehicleState": 7654321, "okLight": 20261120}),
    ("realtime", {"time": 20261120, "timestamp": "20-Nov-26", "dateText": "2026-11-20"}),
    ("charging", {"updateTime": "20-Nov-26", "soc": "52.123:4.654", "chargePower": "1e-05"}),
    ("gps", {"speed": {"speed": [52.123456, 4.654321]}, "data": {"speed": {"x": 52.123456}, "latitude": 52.123456,
                                                                "longitude": 4.654321, "direction": 270.25}}),
    ("gps", {"data": {"speed": [52.123456]}, "speed.speed.0": 52.123456}),
    ("hvac", {"statusNow": {"tempOutCar": "Cape Town", "pm": 52123456, "position": 52.123456}}),
    ("remoteControl", {"res": 2, "latitude": 52.123456}),
    ("privateEventName", {"okLight": 1, "lat": 52.123456}),
    ("realtime", {"gl": 99999999.9996, "totalPower": -99999999.9996, "rate": "1e-5", "speed": float("nan")}),
    ("realtime", {"repairModeSwitch": str(UUID(int=5)), "chargePowerUnit": "user@example.com", "okLight": PRIVATE_VIN}),
]
# Strings none of which may appear in any output line.
LEAK_TOKENS = ("123456", "654321", "124456", "52.123", "4.654", "52123", "Sea Point", "Cape Town", "NW", "270.25",
               "20261120", "20261101", "Nov", "d2026", "u123456789012", COMPACT_UUID, PRIVATE_VIN, "7654321",
               "99999999", "100000000", "1e-0", "example.com", str(UUID(int=5)), "Johannesburg", "Alice", "07:15",
               "98033", str(RAW_S), str(RAW_MS))


def _raw_row(at, endpoint, raw=None, *, source="poll", code=None, seq=0, car=CAR_ID):
    return {"car_id": car, "at": at, "endpoint": endpoint, "seq": seq, "source": source, "code": code, "raw": raw}


def _leak_rows():
    return [_raw_row(BASE + timedelta(seconds=20 + index), endpoint, raw,
                     source="mqtt" if endpoint in ("vehicleInfo", "remoteControl", "privateEventName") else "poll")
            for index, (endpoint, raw) in enumerate(LEAKS)]


def _raw_rows():
    return [
        _raw_row(UNTIL - timedelta(days=1, hours=2), "realtime", {"okLight": 0}),
        _raw_row(BASE + timedelta(seconds=10), "realtime", REALTIME),
        _raw_row(BASE + timedelta(seconds=10), "gps", GPS_REPLY),
        _raw_row(BASE + timedelta(seconds=10), "energy", code="1008"),
        _raw_row(BASE + timedelta(seconds=10), "charging", code="BydSecret user@example.com"),
        _raw_row(BASE + timedelta(seconds=11), "vehicleInfo", {"speed": 51, "time": RAW_MS + 1000, "yaw": 270.25},
                 source="mqtt"),
        _raw_row(BASE + timedelta(seconds=11), "vehicleInfo", {"speed": 52}, source="mqtt", seq=1),
        _raw_row(BASE + timedelta(seconds=12), "privateEventName", {"value": 3}, source="mqtt"),
        _raw_row(BASE + timedelta(seconds=13), "hvac", '{"tempInCar": '),
        _raw_row(BASE + timedelta(seconds=13), "schedule", [1, 2]),
        _raw_row(BASE + timedelta(seconds=14), "realtime", {"_truncated": True, "bytes": 70000}),
        _raw_row(BASE + timedelta(seconds=15), "realtime", {"x": 1}, source="webhook"),
        _raw_row(BASE + timedelta(seconds=16), "realtime", {"x": 2}, car=OTHER_CAR),
    ]


def _tables():
    return {
        "car_telemetry": [
            _car_row(BASE, cadence="moving", speed_kph=50.5, soc_pct=80.0, twelve_volt=12.6, odometer_km=12345.678,
                     power_level="ok", gear="D", data=FULL_DATA),
            _car_row(BASE + timedelta(seconds=120), cadence="stationary", speed_kph=0.0,
                     data={"location": {"lat": LAT + 0.0001, "lng": LNG, "accuracyM": 3, "ageS": 0}}),
            _car_row(BASE + timedelta(hours=2), cadence="event", gear="X", data={"location": "not an object"}),
            # Before the cloud window, inside the car window.
            _car_row(UNTIL - timedelta(days=1, hours=3), cadence="event", data={"lock": "unlocked"}),
        ],
        "ingest_events": [
            _event(1, BASE + timedelta(seconds=30), payload=POLL),
            _event(2, BASE + timedelta(seconds=40), "mqtt", "vehicleInfo", detail="received", payload={
                "event": "vehicleInfo", "lastUpdated": _iso(BASE + timedelta(seconds=39)), "speed": 51,
                "gps": {"latitude": MOVED_LAT, "longitude": LNG}}),
            _event(3, BASE + timedelta(seconds=90), kind="error", detail="1008", payload={"error": "private text"}),
            _event(4, BASE + timedelta(seconds=91), kind="error", detail="session_expired"),
            _event(5, BASE + timedelta(seconds=92), kind="error", detail="upstream user@example.com said no"),
            _event(6, BASE + timedelta(seconds=93), kind="skip", detail="quiet"),
            _event(7, BASE + timedelta(seconds=94), "mqtt", "remoteControl", detail="received",
                   payload={"event": "remoteControl"}),
            _event(8, BASE + timedelta(seconds=95), "poll", "weird"),
            _event(9, BASE + timedelta(hours=2), payload={"gps": {"latitude": 0, "longitude": 0}}),
            _event(10, BASE + timedelta(hours=3), payload={"gps": {"latitude": CLOUD_LAT, "longitude": LNG},
                                                           "lastUpdated": "yesterday"}),
            _event(11, BASE + timedelta(seconds=30), user=OTHER_USER, payload=POLL),
        ],
        "cars": [{"id": CAR_ID, "user_id": USER, "vin": PRIVATE_VIN, "model_key": "seal",
                  "model_key_source": "name", "is_hybrid": False, "powertrain": "ev", "deleted_at": None}],
        "car_app_devices": [{"car_id": CAR_ID, "source": "car_app", "app_version": "1.4.2",
                             "first_seen_at": UNTIL - timedelta(days=40)}],
        "byd_raw_replies": _raw_rows(),
    }


def _args(**changes):
    values = {"car_days": 2, "cloud_days": 1, "until": UNTIL, "page_rows": 2000, "pause_ms": 200,
              "max_minutes": 15, "dry_run": False, "explain": False, "out_dir": None}
    return argparse.Namespace(**{**values, **changes})


@pytest.fixture(autouse=True)
def pauses(monkeypatch):
    recorded = []

    async def sleep(seconds, result=None):
        recorded.append(seconds)

    monkeypatch.setattr(ep.asyncio, "sleep", sleep)
    return recorded


class _GuardedOutput(io.StringIO):
    """Fail if anything is written while a transaction is open."""

    def __init__(self, db):
        super().__init__()
        self.db = db

    def write(self, value):
        assert not self.db.open
        return super().write(value)


def _run(db, **changes):
    output = _GuardedOutput(db)
    asyncio.run(pairs.run_export(_args(**changes), output, db, text or str, offset_s=OFFSET))
    return output.getvalue()


def _lines(stream):
    return [json.loads(line) for line in stream.splitlines()]


# --- the line checker ----------------------------------------------------------------------------

FULL_CAR = {
    "type": "car", "t": 12.5, "cadence": "moving", "speed_kph": 50.5, "soc_pct": 80, "twelve_volt": 12.6,
    "odometer_km": 12345.678, "power_level": "ok", "gear": "D",
    "doors": {"fl": "closed", "fr": "open", "rl": "closed", "rr": "closed", "hood": "closed", "trunk": "open"},
    "lock": "locked", "alarm": "armed", "alarm_raw": 3, "twelve_volt_level": "normal", "average_speed_kph": 44.4,
    "outside_temp_c": -5.25, "remaining_kwh": 60.123, "lifetime_kwh": 5000,
    "tyre_kpa": {"fl": 250, "fr": 251.5, "rl": 248, "rr": 249}, "tyre_temp_c": {"fl": 21.5, "fr": 22, "rl": 21, "rr": 20},
    "charging_state_raw": 0, "charging_gun_raw": 1, "charging_power_kw": 0, "charging_type_raw": 1,
    "charger_state_raw": 0, "charge_remaining_min": 0, "charged_kwh": 1.5, "motor_power_kw": -12.5,
    "has_fix": True, "fix_accuracy_m": 4.5, "fix_age_s": 1.5, "fix_speed_kph": 49.5,
}
FULL_CLOUD = {
    "type": "cloud", "t": 30.0, "source": "poll", "kind": "ok", "detail": "1008", "last_updated_t": -12.5,
    "odometer_km": 12345.6, "soc": 80, "online": True, "vehicle_on": None, "speed_kph": 50,
    "charge_power_kw": None, "minutes_to_full": 30, "asleep": False, "vehicle_unreachable": False,
    "last50km_energy_kwh": 15.25, "energy_distribution": {"drive": 70, "electric": 10.5, "hvac": 15, "other": None},
    "charging": {"is_charging": False, "is_connected": True, "connect_state": 1, "soc": 80, "minutes_to_full": None},
    "hvac_on": True, "gps_present": True, "gps_fresh": True, "gps_zero": False, "gps_dist_m": 111,
    "gps_dt_s": 28.5, "gps_car_accuracy_m": 4.5, "gps_car_speed_kph": 50.5, "gps_step_m": 222,
    "gps_last_dist_m": 111, "gps_last_age_s": 28.5,
}
FULL_RAW = {
    "type": "raw", "t": 40.5, "source": "poll", "endpoint": "realtime", "code": "1008", "truncated": True,
    "fields": {"okLight": 1, "gl": -500_000, "totalPower": 1000, "rate": -999, "time_t": -30 * 366 * 86400,
               "timestamp_stale": True, "lessOneMin": False, "chargePower": "--", "chargePowerUnit": "kW",
               "nearestEnergyConsumptionUnit": "kW\u00b7h/100km", "speed": None, "leftFrontTirepressure": 2.345,
               "vehicleState": 99, "connectState": -1, "repairModeSwitch": 1},
}
FULL_META = {
    "type": "meta", "version": 2, "start": "2026-02-27T15:34:31Z", "car_days": 29, "cloud_days": 13, "vin": VIN,
    "model": "seal", "energy": "ev", "device_source": "car_app", "app_version": "1.4.2-debug", "first_seen_t": -86400.5,
    "stats": {
        "car_rows": 3, "cloud_rows": dict.fromkeys(ep.ROW_KINDS, 0), "dropped_car_keys": {"motorRpm": 2},
        "dropped_cloud_keys": {"placeKind": 1}, "invalid_values": {"gear": 1, "charging_soc": 1},
        "dropped_details": 1, "unparsable_last_updated": 0, "cloud_fixes_matched": 1, "cloud_fixes_unmatched": 0,
        "raw_rows": {"poll": 1, "mqtt": 0, "other": 2}, "raw_endpoints": dict.fromkeys(pairs.RAW_ENDPOINTS, 0),
        "raw_fields_dropped": {"realtime": 3, "gps": 4}, "raw_values_invalid": {"realtime": 1},
        "raw_invalid": {"undecodable": 1, "rejected": 1}, "raw_dropped_codes": 1,
    },
}
FULL = {"car": FULL_CAR, "cloud": FULL_CLOUD, "raw": FULL_RAW, "meta": FULL_META}


def test_full_lines_cover_every_key_and_pass():
    assert set(FULL_CAR) == set(pairs.CAR_LINE) and set(FULL_CLOUD) == set(pairs.CLOUD_LINE)
    assert set(FULL_META) == set(pairs.META_LINE) and set(FULL_RAW) == set(pairs.RAW_LINE)
    assert set(FULL_META["stats"]) == set(pairs.STATS_FIELDS)
    for kind, row in FULL.items():
        assert pairs.check_line(ep.json_line(row))[0] == kind
    minimal = {"type": "cloud", "t": 0, "source": "mqtt", "kind": "other"}
    assert pairs.check_line(ep.json_line(minimal))[0] == "cloud"
    minimal = {"type": "raw", "t": 0, "source": "mqtt", "endpoint": "other"}
    assert pairs.check_line(ep.json_line(minimal))[0] == "raw"


def _with(kind, path, value):
    row = json.loads(json.dumps(FULL[kind]))
    target = row
    *parents, last = path.split(".")
    for parent in parents:
        target = target[parent]
    if value is _DELETE:
        del target[last]
    else:
        target[last] = value
    return ep.json_line(row)


_DELETE = object()


@pytest.mark.parametrize(("kind", "path", "value", "rule"), [
    ("car", "latitude", 1, "unexpected_key"),
    ("car", "doors.window", "open", "unexpected_key"),
    ("car", "t", _DELETE, "missing_key"),
    ("car", "gear", "X", "unexpected_string"),
    ("car", "doors.fl", "ajar", "unexpected_string"),
    ("car", "doors", ["open"], "wrong_type"),
    ("car", "doors", "open", "wrong_type"),
    ("car", "speed_kph", 401, "out_of_range"),
    ("car", "tyre_kpa.fl", -1, "out_of_range"),
    ("car", "t", -0.5, "out_of_range"),
    ("car", "has_fix", 1, "wrong_type"),
    ("car", "has_fix", "true", "wrong_type"),
    ("car", "speed_kph", True, "wrong_type"),
    ("car", "speed_kph", "50", "wrong_type"),
    ("car", "alarm_raw", 1.5, "wrong_type"),
    ("car", "gear", 4, "wrong_type"),
    ("car", "speed_kph", None, "unexpected_null"),
    ("car", "t", LAT, "precise_number"),
    ("car", "odometer_km", 12345.6789, "precise_number"),
    ("car", "lock", PRIVATE_VIN, "vin"),
    ("cloud", "online", 1, "wrong_type"),
    ("cloud", "kind", "privateEvent", "unexpected_string"),
    ("cloud", "source", "car_app", "unexpected_string"),
    ("cloud", "detail", "boom", "unexpected_string"),
    ("cloud", "detail", "123456", "unexpected_string"),
    ("cloud", "detail", None, "unexpected_null"),
    ("cloud", "charging.soc", 101, "out_of_range"),
    ("cloud", "charging.connect_state", 1.0, "wrong_type"),
    ("cloud", "gps_dt_s", 600.5, "out_of_range"),
    ("cloud", "gps_dist_m", 111.5, "wrong_type"),
    ("cloud", "gps_zero", None, "unexpected_null"),
    ("cloud", "gps_dt_s", 4.12345, "precise_number"),
    ("cloud", "detail", "user@example.com", "email"),
    ("cloud", "detail", str(UUID(int=99)), "uuid"),
    ("meta", "vin", PRIVATE_VIN, "vin"),
    ("meta", "vin", "TESTVIN0000000002", "unexpected_string"),
    ("meta", "model", "a" * 17, "vin"),
    ("meta", "model", "Seal U", "unexpected_string"),
    ("meta", "app_version", "1.2.3.4.5", "unexpected_string"),
    ("meta", "app_version", "1.4.2-beta", "unexpected_string"),
    ("meta", "app_version", "1.4.2-debugx", "unexpected_string"),
    ("meta", "device_source", "simulator", "unexpected_string"),
    ("cloud", "gps_last_age_s", 30 * 86400 + 1, "out_of_range"),
    ("cloud", "gps_last_dist_m", 1.5, "wrong_type"),
    ("cloud", "detail", "upstream said no", "unexpected_string"),
    ("cloud", "detail", "Received", "unexpected_string"),
    ("meta", "app_version", "build 7", "unexpected_string"),
    ("meta", "start", "2026-02-30T00:00:00Z", "unexpected_string"),
    ("meta", "start", "2026-02-27T15:34:31+00:00", "unexpected_string"),
    ("meta", "version", 1, "out_of_range"),
    ("meta", "stats.raw_rows.poll", _DELETE, "missing_key"),
    ("meta", "stats.raw_endpoints.other", _DELETE, "missing_key"),
    ("meta", "stats.raw_endpoints.privateEvent", 1, "unexpected_key"),
    ("meta", "stats.raw_fields_dropped", {"latitude": 1}, "unexpected_key"),
    ("meta", "stats.raw_values_invalid", {"okLight": 1}, "unexpected_key"),
    ("meta", "stats.raw_invalid", {"other": 1}, "unexpected_key"),
    ("raw", "endpoint", "privateEvent", "unexpected_string"),
    ("raw", "source", "webhook", "unexpected_string"),
    ("raw", "code", "BydSecretError", "unexpected_string"),
    ("raw", "code", "123456", "unexpected_string"),
    ("raw", "code", "user@example.com", "email"),
    ("raw", "endpoint", _DELETE, "missing_key"),
    ("raw", "truncated", 1, "wrong_type"),
    ("raw", "fields", [], "wrong_type"),
    ("raw", "fields", None, "unexpected_null"),
    ("raw", "extra", 1, "unexpected_key"),
    ("meta", "car_days", 30, "out_of_range"),
    ("meta", "cloud_days", 14, "out_of_range"),
    ("meta", "energy", "diesel", "unexpected_string"),
    ("meta", "device_source", "phone", "unexpected_string"),
    ("meta", "stats.car_rows", -1, "out_of_range"),
    ("meta", "stats.car_rows", 1.0, "wrong_type"),
    ("meta", "stats.cloud_rows.poll_ok", _DELETE, "missing_key"),
    ("meta", "stats.dropped_details", _DELETE, "missing_key"),
    ("meta", "stats.dropped_car_keys", {"has space": 1}, "unexpected_key"),
    ("meta", "stats.dropped_car_keys", {"customName": 1}, "unexpected_key"),
    ("meta", "stats.dropped_car_keys", {"placeKind": 1}, "unexpected_key"),
    ("meta", "stats.dropped_cloud_keys", {"motorRpm": 1}, "unexpected_key"),
    ("meta", "stats.dropped_car_keys", {"motorRpm": -1}, "out_of_range"),
    ("meta", "stats.dropped_car_keys", {"other": True}, "wrong_type"),
    ("meta", "stats.dropped_car_keys", None, "unexpected_null"),
    ("meta", "stats.dropped_car_keys", [], "wrong_type"),
    ("meta", "stats.invalid_values", {"Gear": 1}, "unexpected_key"),
    ("meta", "stats.invalid_values", {"speed": 1}, "unexpected_key"),
    ("meta", "stats.invalid_values", {"doors_window": 1}, "unexpected_key"),
    ("meta", "stats", _DELETE, "missing_key"),
    ("meta", "stats.extra", 1, "unexpected_key"),
])
def test_checker_rejects(kind, path, value, rule):
    with pytest.raises(ep._ExportError, match=rf"^{rule} on {kind} line$"):
        pairs.check_line(_with(kind, path, value))


MQTT_EVENTS = ("vehicleInfo", "smartCharge", "energyConsumption", "remoteControl")


def _raw_line(fields, endpoint="realtime", source=None):
    source = source or ("mqtt" if endpoint in MQTT_EVENTS else "poll")
    return ep.json_line({"type": "raw", "t": 1, "source": source, "endpoint": endpoint, "fields": fields})


@pytest.mark.parametrize(("endpoint", "fields", "rule"), [
    # Only the literal paths of the endpoint's table.
    ("realtime", {"latitude": 52.123}, "unexpected_key"),
    ("realtime", {"gpsInfo.y": 52.123}, "unexpected_key"),
    ("realtime", {"pt.0": 52.123}, "unexpected_key"),
    ("realtime", {"mapX": 52}, "unexpected_key"),
    ("realtime", {"headingDeg": 270}, "unexpected_key"),
    ("realtime", {"ownerRef": 7}, "unexpected_key"),
    ("realtime", {"d20261101": 1}, "unexpected_key"),
    ("realtime", {"data.speed": 1}, "unexpected_key"),
    ("realtime", {"statusNow.tempOutCar": 20}, "unexpected_key"),
    ("realtime", {"okLight_t": 1}, "unexpected_key"),
    ("realtime", {"time": 12.5}, "unexpected_key"),
    ("realtime", {"selfGraph.energyConsumption.0": 1}, "unexpected_key"),
    ("energy", {"selfGraph.energyConsumption.12": 1}, "unexpected_key"),
    ("gps", {"data.latitude": 52.123}, "unexpected_key"),
    ("gps", {"data.direction": 270}, "unexpected_key"),
    ("gps", {"speed.speed.0": 1}, "unexpected_key"),
    ("gps", {"okLight": 1}, "unexpected_key"),
    ("remoteControl", {"res": 2}, "unexpected_key"),
    ("other", {"okLight": 1}, "unexpected_key"),
    # Each path's kind and range.
    ("realtime", {"time_t": RAW_S}, "out_of_range"),
    ("realtime", {"time_t": "x"}, "wrong_type"),
    ("realtime", {"time_t": None}, "unexpected_null"),
    ("realtime", {"time_stale": False}, "wrong_type"),
    ("realtime", {"time_stale": "yes"}, "unexpected_string"),
    ("realtime", {"okLight_stale": True}, "unexpected_key"),
    ("realtime", {"gl": 500_000.001}, "out_of_range"),
    ("realtime", {"totalPower": 1000.5}, "out_of_range"),
    ("realtime", {"okLight": 100}, "out_of_range"),
    ("realtime", {"repairModeSwitch": 2}, "out_of_range"),
    ("realtime", {"okLight": 256}, "out_of_range"),
    ("realtime", {"vehicleState": 7654321}, "out_of_range"),
    ("realtime", {"okLight": 1.5}, "wrong_type"),
    ("realtime", {"okLight": True}, "wrong_type"),
    ("realtime", {"okLight": "1"}, "unexpected_string"),
    ("realtime", {"gl": 1_000_000.001}, "out_of_range"),
    ("realtime", {"gl": 52123456}, "out_of_range"),
    ("realtime", {"gl": 52.123456}, "precise_number"),
    ("realtime", {"chargePower": "3.1"}, "unexpected_string"),
    ("realtime", {"chargePowerUnit": "MW"}, "unexpected_string"),
    ("realtime", {"chargePowerUnit": "52.123:4.654"}, "unexpected_string"),
    ("realtime", {"nearestEnergyConsumptionUnit": "Sea Point"}, "unexpected_string"),
    ("realtime", {"chargePowerUnit": 1}, "wrong_type"),
    ("realtime", {"lessOneMin": 0}, "wrong_type"),
    ("realtime", {"okLight": [1]}, "wrong_type"),
    # The text scan still applies.
    ("realtime", {"repairModeSwitch": PRIVATE_VIN}, "vin"),
    ("realtime", {"repairModeSwitch": str(UUID(int=5))}, "uuid"),
    ("realtime", {"chargePowerUnit": "a@example.com"}, "email"),
    # Fine:
    ("realtime", {"okLight": 1, "gl": -39142.4, "time_t": 12.5, "lessOneMin": None, "chargePowerUnit": "kW",
                  "chargePower": "--", "rate": -999, "speed": 39}, None),
    ("gps", {"data.speed": 50, "data.gpsTimeStamp_t": 12.5, "speed": 1, "gpsTimeStamp_stale": True}, None),
    ("realtime", {"repairModeSwitch": "1", "time_stale": True}, None),
    ("hvac", {"statusNow.tempOutCar": 20, "tempOutCar": -129, "statusNow.status": 2}, None),
    ("energy", {"selfGraph.energyConsumption.11": 19.3, "nearestEnergyConsumption.driveDistribution": "--"}, None),
    ("smartCharge", {"chargingState": 15, "updateTime_t": 1, "smartJourneyDto.chargeWay": "s"}, None),
    ("remoteControl", {}, None),
])
def test_checker_rejects_raw_fields(endpoint, fields, rule):
    if rule is None:
        assert pairs.check_line(_raw_line(fields, endpoint))[0] == "raw"
        return
    with pytest.raises(ep._ExportError, match=rf"^{rule} on raw line$"):
        pairs.check_line(_raw_line(fields, endpoint))


@pytest.mark.parametrize("number", ["1e999999999999999999999", "1.5E+999999999999999999999", "-1e9999999999999999999"])
def test_an_unrepresentable_decimal_is_an_ordinary_rejection(number):
    for line in (f'{{"type":"car","t":{number}}}\n',
                 f'{{"type":"raw","t":1,"source":"poll","endpoint":"realtime","fields":{{"gl":{number}}}}}\n'):
        with pytest.raises(ep._ExportError, match=r"^out_of_range on (car|raw) line$"):
            pairs.check_line(line)


def test_raw_tables_follow_the_exact_source_and_endpoint():
    assert pairs.raw_table("poll", "gps") is pairs.GPS_FIELDS and pairs.raw_table("mqtt", "gps") == {}
    assert pairs.raw_table("mqtt", "vehicleInfo") is pairs.REALTIME_FIELDS
    assert pairs.raw_table("poll", "vehicleInfo") == {}
    for source, endpoint in (("mqtt", "gps"), ("poll", "smartCharge"), ("mqtt", "realtime")):
        with pytest.raises(ep._ExportError, match="^unexpected_key on raw line$"):
            pairs.check_line(_raw_line({"speed": 1}, endpoint, source))
    export = _bare_export()
    export.add_raw(_raw_row(BASE, "gps", {"data": {"speed": 50, "gpsTimeStamp": RAW_S}}, source="mqtt"))
    export.add_raw(_raw_row(BASE, "vehicleInfo", {"speed": 50}, source="poll"))
    export.add_raw(_raw_row(BASE, "gps", {"data": {"speed": 50, "gpsTimeStamp": 0}}))
    export.finish()
    rows = _lines(export.output.getvalue())
    # A push named gps and a poll named vehicleInfo are "other" and keep nothing.
    assert rows[0] == {"type": "raw", "t": 151200.0, "source": "mqtt", "endpoint": "other", "fields": {}}
    assert rows[1] == {"type": "raw", "t": 151200.0, "source": "poll", "endpoint": "other", "fields": {}}
    # BYD's 0 for "no fix time" is written as a marker, so the reply is never paired at its receive time.
    assert rows[2]["fields"] == {"data.speed": 50, "data.gpsTimeStamp_stale": True}
    assert rows[-1]["stats"]["raw_endpoints"]["other"] == 2


def test_every_path_on_the_tables_passes_at_its_bounds():
    for (source, endpoint), table in pairs.RAW_TABLES.items():
        for value in ("low", "high"):
            fields = {}
            for path, spec in table.items():
                if spec.kind == "time":
                    fields[f"{path}_t"] = -pairs.RAW_T_LIMIT if value == "low" else pairs.RAW_T_LIMIT
                elif spec.kind == "bool":
                    fields[path] = value == "high"
                elif spec.kind == "enum":
                    fields[path] = sorted(spec.values)[0 if value == "low" else -1]
                else:
                    fields[path] = getattr(spec, value)
            assert pairs.check_line(_raw_line(fields, endpoint, source))[0] == "raw"
    # No location, heading, identifier or place path is on any table.
    paths = {path.lower() for table in pairs.RAW_TABLES.values() for path in table}
    for word in ("lat", "lng", "lon", "direction", "heading", "altitude", "zone", "vin", "serial", "user", "plan",
                 "name", "url", "price", "position", "address"):
        assert not [path for path in paths if word in path.split(".")[-1] and not path.endswith(
            ("ventilationstate", "windposition"))], word
    assert not [path for table in pairs.RAW_TABLES.values() for path in table if path.endswith(("Id", "ID", ".id"))]


@pytest.mark.parametrize("value", [99999999.9996, -99999999.9996, 1e-5, 1e300, float("inf"), "1e-5", "52.123:4.5"])
def test_the_pod_and_the_check_agree_on_bounds(value):
    """Round first, then bound, with the bound the check uses: a value either fits both or neither."""
    spec = pairs.RawField("float", -100_000_000, 100_000_000)
    converted = pairs.raw_value(spec, value, 0)
    if converted is pairs._INVALID:
        assert not isinstance(value, float) or abs(value) > 1e8 or value != value
        return
    assert pairs._check_raw_value(spec, json.loads(json.dumps(converted), parse_float=pairs._Float)) is None
    assert abs(converted) <= 100_000_000
    narrow = pairs.RawField("float", -1_000_000, 1_000_000)
    assert pairs.raw_value(narrow, value, 0) is pairs._INVALID or abs(value) < 1


@pytest.mark.parametrize("key", pairs.ZERO_FORBIDDEN)
def test_checker_refuses_any_measure_from_a_zero_cloud_position(key):
    row = {"type": "cloud", "t": 1, "source": "poll", "kind": "ok", "gps_present": True, "gps_zero": True}
    pairs.check_line(ep.json_line(row))
    with pytest.raises(ep._ExportError, match="^unexpected_key on cloud line$"):
        pairs.check_line(ep.json_line({**row, key: FULL_CLOUD[key]}))
    pairs.check_line(ep.json_line({**row, "gps_zero": False, key: FULL_CLOUD[key]}))


@pytest.mark.parametrize("word", pairs.DETAIL_WORDS + ("0", "99999"))
def test_checker_accepts_the_backends_fixed_details(word):
    pairs.check_line(ep.json_line({"type": "cloud", "t": 1, "source": "poll", "kind": "skip", "detail": word}))


def test_value_checks_hold_on_their_own():
    # check_line's text scan refuses these first; the schema check refuses them too.
    assert pairs._check(pairs.Num(0, 1), pairs._Float("0.1234")) == "precise_number"
    assert pairs._check(pairs.Num(0, 1), pairs._Float("1.5e-4")) == "precise_number"
    assert pairs._convert(pairs.Text(pairs.DETAIL), "1008") is pairs._INVALID
    assert pairs._convert(pairs.Num(0, 10, integer=True), 5.0) is pairs._INVALID
    assert pairs._convert(pairs.Num(0, 10), 5.00049) == 5.0
    assert pairs.haversine_m((0.0, 0.0), (0.0, 180.0)) == round(math.pi * pairs.EARTH_RADIUS_M)


@pytest.mark.parametrize(("line", "rule", "kind"), [
    ('{"type":"car","t":1}', "invalid_line", "unknown"),
    ('{"type":"car","t":1}\r\n', "invalid_line", "unknown"),
    ('{"type":"car",\n"t":1}\n', "invalid_line", "unknown"),
    ("[]\n", "invalid_line", "unknown"),
    ("{bad\n", "invalid_json", "unknown"),
    ('{"type":"car","t":NaN}\n', "invalid_json", "unknown"),
    ('{"type":"car","t":Infinity}\n', "invalid_json", "unknown"),
    ('{"type":"car","t":1,"t":2}\n', "duplicate_key", "unknown"),
    ('{"type":"other","t":1}\n', "unexpected_type", "unknown"),
    ('{"type":1,"t":1}\n', "unexpected_type", "unknown"),
    ('{"t":1}\n', "unexpected_type", "unknown"),
    ('{"type":"car","t":1e-05}\n', "precise_number", "car"),
    ('{"type":"car","t":1.2E-4}\n', "precise_number", "car"),
    ('{"type":"car","t":1E+20}\n', "out_of_range", "car"),
    ('{"type":"car","t":1.5e2,"speed_kph":1.25e1}\n', None, "car"),
    (f'{{"type":"car","t":1,"{PRIVATE_VIN}":1}}\n', "vin", "car"),
    ('{"type":"car","t":1,"me@example.com":1}\n', "email", "car"),
    ('{"type":"car","t":1,"lock":"52.1234567"}\n', "precise_number", "car"),
    ('{"type":"car","t":1,"t":52.123456}\n', "precise_number", "unknown"),
    ('{"type":"car","t":1,"lock":"locked"} 52.123456\n', "precise_number", "unknown"),
])
def test_checker_rejects_raw_text(line, rule, kind):
    if rule is None:
        assert pairs.check_line(line)[0] == kind
        return
    with pytest.raises(ep._ExportError, match=rf"^{rule} on {kind} line$"):
        pairs.check_line(line)


# --- rows to lines -------------------------------------------------------------------------------


def _export(tables=None, **changes):
    db = FakeDB(tables or _tables(), **changes)
    return db, _run(db)


def test_rows_become_lines_without_coordinates_or_identifiers():
    db, stream = _export()
    for secret in (*PRIVATE_DIGITS, PRIVATE_VIN, CAR_ID, OTHER_CAR, str(USER), str(OTHER_USER), "api-private",
                   "private text", "example.com", "quiet", "home", "270.25", str(OFFSET), str(RAW_S), str(RAW_MS),
                   str(RAW_S)[:6], "2026-03-01", "Johannesburg", "Alice", "privateEventName", "BydSecret",
                   "webhook", "612345678", str(UUID(int=7)), "V1234", "kWh/100km"):
        assert secret not in stream
    data_lines = stream.rsplit("\n", 2)[0]  # the meta line counts dropped keys by name, like slopeRaw
    assert "altitude" not in stream and "heading" not in stream and "slope" not in data_lines
    for line in stream.splitlines(keepends=True):
        pairs.check_line(line)
    rows = _lines(stream)
    cars = [row for row in rows if row["type"] == "car"]
    assert [row["t"] for row in cars] == [75600.0, 151200.0, 151320.0, 158400.0]
    full = cars[1]
    assert full == {
        "type": "car", "t": 151200.0, "cadence": "moving", "speed_kph": 50.5, "soc_pct": 80.0, "twelve_volt": 12.6,
        "odometer_km": 12345.678, "power_level": "ok", "gear": "D",
        "doors": FULL_DATA["doors"], "lock": "locked", "alarm": "off", "alarm_raw": 0,
        "twelve_volt_level": "normal", "average_speed_kph": 44.4, "outside_temp_c": 18.5, "remaining_kwh": 60.123,
        "lifetime_kwh": 5000, "tyre_kpa": {"fl": 250.0, "fr": 251.5, "rl": 248, "rr": 249.25},
        "tyre_temp_c": {"fl": 21.5, "fr": 22, "rl": 21, "rr": 20}, "charging_state_raw": 0, "charging_gun_raw": 1,
        "charging_power_kw": 0.0, "charging_type_raw": 1, "charger_state_raw": 0, "charge_remaining_min": 0,
        "charged_kwh": 1.5, "motor_power_kw": -12.5, "has_fix": True, "fix_speed_kph": 49.5,
        "fix_accuracy_m": 4.5, "fix_age_s": 1.5,
    }
    assert cars[2] == {"type": "car", "t": 151320.0, "cadence": "stationary", "speed_kph": 0.0, "has_fix": True,
                       "fix_accuracy_m": 3, "fix_age_s": 0}
    assert cars[3] == {"type": "car", "t": 158400.0, "cadence": "event", "has_fix": False}
    cloud = {row["t"]: row for row in rows if row["type"] == "cloud"}
    poll = cloud[151230.0]
    # 0.001 degrees of latitude is 111.195 m; the car's fix was taken 1.5 s before its row.
    assert poll["gps_dist_m"] == round(6_371_008.8 * math.radians(0.001)) == 111
    assert poll["gps_dt_s"] == 31.5 and poll["gps_car_accuracy_m"] == 4.5 and poll["gps_car_speed_kph"] == 50.5
    assert poll["gps_present"] is True and poll["gps_zero"] is False and poll["gps_fresh"] is True
    assert "gps_step_m" not in poll
    # The last fix at or before the poll is the same first fix here.
    assert poll["gps_last_dist_m"] == 111 and poll["gps_last_age_s"] == 31.5
    push = cloud[151240.0]
    assert push["source"] == "mqtt" and push["kind"] == "vehicleInfo" and push["detail"] == "received"
    # A push's step is from the last push, not from the poll; its nearest car fix is the first one.
    assert "gps_step_m" not in push and push["gps_dist_m"] == 334 and push["gps_dt_s"] == 41.5
    assert push["gps_last_dist_m"] == 334 and push["gps_last_age_s"] == 41.5
    assert cloud[151290.0]["detail"] == "1008" and set(cloud[151290.0]) == {"type", "t", "source", "kind", "detail"}
    assert cloud[151291.0]["detail"] == "session_expired" and cloud[151294.0]["detail"] == "received"
    assert "detail" not in cloud[151292.0] and "detail" not in cloud[151293.0]
    assert cloud[151295.0]["kind"] == "other"
    zero = cloud[158400.0]
    assert zero["gps_zero"] is True and not set(pairs.ZERO_FORBIDDEN) & set(zero)
    late = cloud[162000.0]
    assert late["gps_step_m"] == 0 and "gps_dist_m" not in late and "last_updated_t" not in late
    # Parked for hours: no fix within 600 s, but the last one (0.0009 degrees south, 2h58m earlier) is measured.
    assert late["gps_last_dist_m"] == 100 and late["gps_last_age_s"] == 10680.0
    meta = rows[-1]
    assert meta["stats"] == {
        "car_rows": 4,
        "cloud_rows": {**dict.fromkeys(ep.ROW_KINDS, 0), "poll_ok": 3, "poll_error": 3, "poll_skip": 1,
                       "poll_other": 1, "mqtt_vehicleInfo": 1, "mqtt_remoteControl": 1},
        "dropped_car_keys": {"acceleratorPct": 1, "motorRpm": 1, "other": 2, "slopeRaw": 1},
        "dropped_cloud_keys": {"fuelPercent": 1, "lastStationaryPollAt": 1, "placeKind": 1},
        "invalid_values": {"gear": 1, "has_fix": 1},
        "dropped_details": 2, "unparsable_last_updated": 1, "cloud_fixes_matched": 2, "cloud_fixes_unmatched": 1,
        **RAW_STATS,
    }
    assert meta["model"] == "seal" and meta["energy"] == "ev" and meta["device_source"] == "car_app"
    assert meta["app_version"] == "1.4.2" and meta["vin"] == VIN


RAW_STATS = {
    "raw_rows": {"poll": 8, "mqtt": 3, "other": 1},
    "raw_endpoints": {**dict.fromkeys(pairs.RAW_ENDPOINTS, 0), "realtime": 3, "energy": 1, "charging": 1, "gps": 1,
                      "hvac": 1, "schedule": 1, "vehicleInfo": 2, "other": 1},
    # realtime: the time zone and the plan's three leaves, and the truncated reply's size; gps: the position,
    # direction, altitude, satellites and res; vehicleInfo: a heading; other: everything.
    "raw_fields_dropped": {"realtime": 5, "gps": 6, "vehicleInfo": 1, "other": 1},
    "raw_values_invalid": {},
    "raw_invalid": {"not_object": 1, "undecodable": 1},
    "raw_dropped_codes": 1,
}


def test_raw_replies_keep_only_their_table_paths():
    _, stream = _export()
    raw = [row for row in _lines(stream) if row["type"] == "raw"]
    # Oldest first, by the primary key's order; the other car's and the webhook row are not written.
    assert [(row["t"], row["endpoint"]) for row in raw] == [
        (79200.0, "realtime"), (151210.0, "charging"), (151210.0, "energy"), (151210.0, "gps"),
        (151210.0, "realtime"), (151211.0, "vehicleInfo"), (151211.0, "vehicleInfo"), (151212.0, "other"),
        (151213.0, "hvac"), (151213.0, "schedule"), (151214.0, "realtime")]
    first, charging, energy, gps, realtime, push, second_push, other, hvac, schedule, truncated = raw
    assert first == {"type": "raw", "t": 79200.0, "source": "poll", "endpoint": "realtime", "fields": {"okLight": 0}}
    # The failure's BYD code is kept; a free-text code never leaves SQL: it is "other" (and counted).
    assert charging == {"type": "raw", "t": 151210.0, "source": "poll", "endpoint": "charging", "code": "other"}
    assert energy == {"type": "raw", "t": 151210.0, "source": "poll", "endpoint": "energy", "code": "1008"}
    # The gps endpoint keeps speed and its fix time, shifted like every t (BASE is t 151200).
    assert gps["fields"] == {"data.speed": 50, "data.gpsTimeStamp_t": 151200}
    # Every decode target of a realistic reply is kept, numbers sent as strings become numbers.
    assert realtime["fields"] == REALTIME_FIELDS
    for target in ("okLight", "epb", "ins", "pwr", "chargeState", "flashChargeState", "chargeEndHour",
                   "chargeEndMinute", "lightStopStatus", "trunkLidLower", "gl", "rate", "totalPower", "powerGear",
                   "vehicleState", "leftFrontTireStatus", "leftFrontWindow", "leftFrontWindowPct", "rightFrontDoor",
                   "leftFrontDoorLock", "leftFrontTirepressure", "tirePressUnit", "skylight", "sentryStatus"):
        assert target in realtime["fields"], target
    assert push == {"type": "raw", "t": 151211.0, "source": "mqtt", "endpoint": "vehicleInfo",
                    "fields": {"speed": 51, "time_t": 151206.0}}
    assert second_push["fields"] == {"speed": 52}
    # An event without a table keeps nothing; its fields are only counted.
    assert other == {"type": "raw", "t": 151212.0, "source": "mqtt", "endpoint": "other", "fields": {}}
    assert "fields" not in hvac and "fields" not in schedule
    assert truncated == {"type": "raw", "t": 151214.0, "source": "poll", "endpoint": "realtime", "truncated": True,
                         "fields": {}}
    assert {key: _lines(stream)[-1]["stats"][key] for key in RAW_STATS} == RAW_STATS


def _leak_tables():
    tables = _tables()
    tables["byd_raw_replies"] = _leak_rows()
    return tables


def test_leak_payloads_are_dropped_and_counted_never_written():
    _, stream = _export(_leak_tables())
    for token in LEAK_TOKENS:
        assert token not in stream, token
    for line in stream.splitlines(keepends=True):
        pairs.check_line(line)
    raw = [row for row in _lines(stream) if row["type"] == "raw"]
    assert len(raw) == len(LEAKS)
    kept = {key: value for row in raw for key, value in row.get("fields", {}).items()}
    # None of these replies has a value that fits its path; a time path with none is only marked stale.
    assert kept == {"time_stale": True, "timestamp_stale": True, "updateTime_stale": True}
    stats = _lines(stream)[-1]["stats"]
    assert sum(stats["raw_fields_dropped"].values()) > 30 and sum(stats["raw_values_invalid"].values()) >= 20
    assert stats["raw_invalid"] == {}


def test_leak_payloads_never_abort_the_composed_pod_script(tmp_path):
    result, _ = _pod(tmp_path, _leak_tables())
    assert result.returncode == 0, result.stderr
    stream = result.stdout.decode("ascii")
    for token in LEAK_TOKENS:
        assert token not in stream, token
    assert [pairs.check_line(line)[0] for line in stream.splitlines(keepends=True)].count("raw") == len(LEAKS)


def test_a_line_the_check_refuses_is_written_without_its_fields(monkeypatch):
    export = _bare_export()
    # Simulate a table value the check would refuse: the pod retries without fields and counts it.
    monkeypatch.setattr(pairs, "raw_value", lambda spec, value, start_s: 52.123456)
    export.add_raw(_raw_row(BASE, "realtime", {"okLight": 1}))
    export.finish()
    rows = _lines(export.output.getvalue())
    assert rows[0] == {"type": "raw", "t": 151200.0, "source": "poll", "endpoint": "realtime"}
    assert rows[-1]["stats"]["raw_invalid"] == {"rejected": 1}


def test_flatten_reads_literal_paths_and_converts_times():
    raw = {"time": "2026-03-01T06:00:00+02:00", "okLight": 2.0, "gl": "12.3456", "rate": "-999", "speed": True,
           "chargePower": "--", "chargePowerUnit": "kW", "lessOneMin": 1, "statusNow": {"tempOutCar": 20},
           "leftFrontDoor": [0], "extra": {"a": [1, 2, {"b": 3}]}}
    fields, dropped, invalid = pairs.flatten_raw(raw, "poll", "realtime", start_s=RAW_S)
    assert fields == {"time_t": -7200.0, "okLight": 2, "gl": 12.346, "rate": -999, "chargePower": "--",
                      "chargePowerUnit": "kW"}
    # speed (a boolean) and lessOneMin (a number) are refused; statusNow.tempOutCar, the door list's item and
    # extra's three leaves are not on the realtime table.
    assert (dropped, invalid) == (5, 2)
    pairs.check_line(_raw_line(fields))
    assert pairs.flatten_raw({"time": str(RAW_MS), "timestamp": 0}, "poll", "realtime", start_s=RAW_S) == (
        {"time_t": 5.0, "timestamp_stale": True}, 0, 1)
    assert pairs.flatten_raw({"time": None}, "poll", "realtime", start_s=RAW_S) == ({"time_stale": True}, 0, 1)
    assert pairs.flatten_raw({"updateTime": RAW_S + 0.25}, "mqtt", "smartCharge", start_s=RAW_S)[0] == {
        "updateTime_t": 0.25}
    graph = {"selfGraph": {"energyConsumption": [str(index) for index in range(14)]}}
    fields, dropped, _ = pairs.flatten_raw(graph, "mqtt", "energyConsumption", start_s=RAW_S)
    assert len(fields) == pairs.RAW_GRAPH_ITEMS and dropped == 2
    assert pairs.flatten_raw({"okLight": 1}, "mqtt", "remoteControl", start_s=0) == ({}, 1, 0)
    assert pairs.flatten_raw({"repairModeSwitch": 1}, "poll", "realtime", start_s=0)[0] == {"repairModeSwitch": 1}


def test_a_raw_reply_too_large_to_read_is_counted_not_parsed():
    export = _bare_export()
    export.add_raw(_raw_row(BASE, "realtime", '{"a": "' + "x" * pairs.RAW_MAX_TEXT + '"}'))
    export.add_raw(_raw_row(BASE, "realtime", "[" * 60_000 + "]" * 60_000))  # RecursionError
    export.add_raw(_raw_row(BASE, "realtime", '"text"'))
    export.add_raw(_raw_row(BASE, "realtime", {"okLight": 1}, code="TimeoutError"))
    export.add_raw(_raw_row(BASE, "realtime", None, code="free text"))
    with pytest.raises(ep._ExportError, match=f"^{pairs.OUTSIDE}$"):
        export.add_raw(_raw_row(UNTIL, "realtime"))
    export.finish()
    rows = _lines(export.output.getvalue())
    assert [set(row) for row in rows[:3]] == [{"type", "t", "source", "endpoint"}] * 3
    assert rows[3]["code"] == "timeout" and rows[3]["fields"] == {"okLight": 1} and rows[4]["code"] == "other"
    stats = rows[-1]["stats"]
    assert stats["raw_invalid"] == {"too_large": 1, "undecodable": 1, "not_object": 1}
    assert stats["raw_dropped_codes"] == 1 and stats["raw_rows"] == {"poll": 5, "mqtt": 0, "other": 0}


def test_nearest_fix_prefers_the_closer_then_the_earlier_and_stops_at_600_seconds():
    export = pairs.PairExport(io.StringIO(), until=UNTIL, car_days=2, cloud_days=1, offset_s=0,
                              car=pairs.car_metadata({"user_id": USER, "vin": PRIVATE_VIN}))
    zero = BASE.timestamp()
    export.add_car(_car_row(BASE, data={"location": {"lat": LAT, "lng": LNG}}))
    export.add_car(_car_row(BASE + timedelta(seconds=1000), speed_kph=12.0,
                            data={"location": {"lat": LAT, "lng": LNG, "ageS": 30, "accuracyM": 7}}))
    export.add_car(_car_row(BASE + timedelta(seconds=2000), data={"location": {"lat": LAT, "lng": LNG, "ageS": -1}}))
    export.add_car(_car_row(BASE + timedelta(seconds=3000), data={"location": {"lat": 91, "lng": LNG}}))
    export.add_car(_car_row(BASE + timedelta(seconds=3100), data={"location": {"lat": True, "lng": LNG}}))
    export.add_car(_car_row(BASE + timedelta(seconds=3200), data={"location": {"lat": 0, "lng": 0.0}}))
    export.add_car(_car_row(BASE + timedelta(seconds=3300), data={"location": {"lat": 10**400, "lng": LNG}}))
    assert export.nearest_fix(zero + 400)[0] == zero
    assert export.nearest_fix(zero + 485)[0] == zero  # a tie goes to the earlier fix
    assert export.nearest_fix(zero + 486)[:5] == (zero + 970, LAT, LNG, 7, 12.0)
    assert export.nearest_fix(zero - 600)[0] == zero
    assert export.nearest_fix(zero - 600.5) is None
    assert export.nearest_fix(zero + 1570)[0] == zero + 970
    assert export.nearest_fix(zero + 1570.5) is None  # the invalid ageS left no fix at 2000
    assert len(export.fixes) == 2 and export.invalid == {"fix_age_s": 1, "has_fix": 4}
    assert export.last_fix(zero - 0.1) is None and export.last_fix(zero)[0] == zero
    assert export.last_fix(zero + 969.9)[0] == zero and export.last_fix(zero + 970)[0] == zero + 970
    assert export.last_fix(zero + 10 * 86400)[0] == zero + 970  # however old
    empty = pairs.PairExport(io.StringIO(), until=UNTIL, car_days=2, cloud_days=1, offset_s=0,
                             car=pairs.car_metadata({"user_id": USER, "vin": PRIVATE_VIN}))
    assert empty.nearest_fix(zero) is None and empty.last_fix(zero) is None


def test_a_car_fix_at_zero_zero_is_never_measured_from():
    tables = _tables()
    tables["car_telemetry"] = [_car_row(BASE, data={"location": {"lat": 0.0, "lng": 0, "accuracyM": 3}})]
    tables["ingest_events"] = [_event(1, BASE + timedelta(seconds=5), payload={
        "gps": {"latitude": CLOUD_LAT, "longitude": LNG}})]
    _, stream = _export(tables)
    car, cloud, *raw, meta = _lines(stream)
    assert car["has_fix"] is False and meta["stats"]["invalid_values"] == {"has_fix": 1}
    assert cloud["gps_present"] is True and cloud["gps_zero"] is False
    assert not set(pairs.ZERO_FORBIDDEN) & set(cloud) and meta["stats"]["cloud_fixes_unmatched"] == 1


def test_cloud_lines_keep_null_apart_from_absent_and_keep_types():
    tables = _tables()
    tables["ingest_events"] = [_event(1, BASE, payload={
        "currentSoc": None, "speed": 1, "odometer": 12000, "online": True, "chargePowerKw": 7.25,
        "charging": {"soc": None, "isCharging": True, "connectState": 2, "extra": 1},
        "climate": None, "gps": None, "gpsFresh": None, "lastUpdated": None, "energyDistribution": None,
        "minutesToFull": 1.0,
    }), _event(2, BASE + timedelta(seconds=1), payload={
        "charging": None, "climate": {"hvacOn": None, "fanLevel": 2}, "online": 1, "speed": True,
        "currentSoc": "80", "gps": {"latitude": "52.1", "longitude": LNG}, "energyDistribution": {"drive": "x"},
        "asleep": None,
    }), _event(3, BASE + timedelta(seconds=2), payload='"not an object"'),
        _event(4, BASE + timedelta(seconds=3), payload={"climate": "on"})]
    _, stream = _export(tables)
    first, second, third, fourth = [row for row in _lines(stream) if row["type"] == "cloud"]
    assert first == {
        "type": "cloud", "t": 151200.0, "source": "poll", "kind": "ok", "soc": None, "speed_kph": 1,
        "odometer_km": 12000, "online": True, "charge_power_kw": 7.25,
        "charging": {"soc": None, "is_charging": True, "connect_state": 2}, "hvac_on": None,
        "gps_fresh": None, "last_updated_t": None, "energy_distribution": None, "minutes_to_full": 1.0,
        "gps_present": None,
    }
    assert '"speed_kph":1,' in stream and '"odometer_km":12000,' in stream and '"minutes_to_full":1.0' in stream
    assert second == {"type": "cloud", "t": 151201.0, "source": "poll", "kind": "ok", "charging": None,
                      "hvac_on": None, "energy_distribution": {}, "asleep": None, "gps_present": False}
    assert set(third) == set(fourth) == {"type", "t", "source", "kind"}
    stats = _lines(stream)[-1]["stats"]
    assert stats["invalid_values"] == {"energy_distribution_drive": 1, "hvac_on": 1, "online": 1, "payload": 1,
                                       "soc": 1, "speed_kph": 1, "gear": 1, "has_fix": 1, "gps_present": 1}
    # Names the backend does not write are never repeated: they are all "other".
    assert stats["dropped_cloud_keys"] == {"other": 2}


def test_json_columns_given_as_text_give_the_same_lines():
    _, objects = _export()
    _, texts = _export(json_as_text=True)
    assert texts == objects


def test_one_random_offset_shifts_every_time_and_is_never_written():
    tables = _tables()
    tables["ingest_events"] = [_event(1, BASE + timedelta(seconds=30), payload={
        "lastUpdated": _iso(BASE - timedelta(days=3, seconds=0.25))})]
    db = FakeDB(tables)
    output = io.StringIO()
    asyncio.run(pairs.run_export(_args(car_days=29, cloud_days=13), output, db, text or str, offset_s=OFFSET))
    rows = _lines(output.getvalue())
    meta = rows[-1]
    real_start = UNTIL - timedelta(days=29)
    assert meta["start"] == _iso(real_start + timedelta(seconds=OFFSET)) == "2026-02-27T15:34:31Z"
    assert real_start.date().isoformat() not in output.getvalue() and str(OFFSET) not in output.getvalue()
    car_t = [row["t"] for row in rows if row["type"] == "car"]
    assert car_t[1] == (BASE - real_start).total_seconds() == 28 * 86400 + 18 * 3600
    cloud = next(row for row in rows if row["type"] == "cloud")
    assert cloud["t"] == car_t[1] + 30
    assert cloud["last_updated_t"] == car_t[1] - 3 * 86400 - 0.25
    assert meta["first_seen_t"] == (UNTIL - timedelta(days=40) - real_start).total_seconds() == -11 * 86400
    # A different offset moves only the published start.
    other = io.StringIO()
    asyncio.run(pairs.run_export(_args(car_days=29, cloud_days=13), other, FakeDB(tables), text or str,
                                 offset_s=-OFFSET))
    assert _lines(other.getvalue())[:-1] == rows[:-1]
    assert _lines(other.getvalue())[-1]["start"] == _iso(real_start - timedelta(seconds=OFFSET))


def test_the_offset_is_random_and_within_thirty_days(monkeypatch):
    draws = []

    def randbelow(limit):
        draws.append(limit)
        return 0

    monkeypatch.setattr(pairs.secrets, "randbelow", randbelow)
    output = io.StringIO()
    asyncio.run(pairs.run_export(_args(), output, FakeDB(_tables()), text or str))
    assert draws == [2 * 30 * 86400 + 1]
    assert _lines(output.getvalue())[-1]["start"] == _iso(UNTIL - timedelta(days=2) - timedelta(days=30))


@pytest.mark.parametrize("page_rows", [2000, 1])  # identification does not page: LIMIT 2 per day
def test_a_second_car_stops_the_scan_at_once_and_nothing_is_looked_up(page_rows):
    tables = _tables()
    # Both cars have rows on 2026-02-28; the scan must not go on to 2026-03-01.
    tables["car_telemetry"].append(_car_row(UNTIL - timedelta(days=1, hours=2), car=OTHER_CAR, user_id=OTHER_USER))
    tables["car_app_devices"].append({"car_id": OTHER_CAR, "source": "car_app"})
    db = FakeDB(tables)
    output = io.StringIO()
    with pytest.raises(ep._ExportError) as exc:
        asyncio.run(pairs.run_export(_args(page_rows=page_rows), output, db, text or str, offset_s=OFFSET))
    assert str(exc.value) == pairs.SEVERAL_CARS and output.getvalue() == ""
    assert "2" not in pairs.SEVERAL_CARS and str(pairs._pod_error(pairs.SEVERAL_CARS.encode() + b"\n", 1)) == (
        pairs.SEVERAL_CARS)
    statements = [transaction[-1] for transaction in db.transactions]
    assert all("/* identify */" in sql for sql, _ in statements)
    assert not any("car_app_devices" in sql or "FROM cars" in sql for sql, _ in statements)
    # Day one is empty; on day two the second car ends the scan with the statement that shows it.
    assert len(statements) == 2
    assert statements[-1][1]["upper"] == datetime(2026, 3, 1, tzinfo=UTC)
    assert all(params["limit"] == 2 and set(params) == {"lower", "upper", "limit"} for _, params in statements)


def test_a_second_car_on_a_later_day_also_stops_the_scan():
    tables = _tables()
    # car-private-0002 sorts after the car, alone on the last day: one row, but not the id already found.
    tables["car_telemetry"] = [_car_row(UNTIL - timedelta(days=1, hours=3)), _car_row(BASE, car=OTHER_CAR)]
    db = FakeDB(tables)
    with pytest.raises(ep._ExportError, match=f"^{pairs.SEVERAL_CARS}$"):
        asyncio.run(pairs.run_export(_args(), io.StringIO(), db, text or str, offset_s=OFFSET))
    assert len(db.transactions) == 3 and all("/* identify */" in t[4][0] for t in db.transactions)


@pytest.mark.parametrize(("change", "message"), [
    (lambda tables: tables["car_telemetry"].clear(), pairs.NO_CAR),
    (lambda tables: tables["cars"].clear(), pairs.NO_CAR_ROW),
    (lambda tables: tables["cars"].append(dict(tables["cars"][0])), pairs.DUPLICATE_CAR_ROW),
    (lambda tables: tables["cars"][0].update(deleted_at=BASE), pairs.DELETED_CAR),
    (lambda tables: tables["cars"][0].update(vin=None), pairs.NO_VIN),
    (lambda tables: tables["car_app_devices"].clear(), pairs.NO_DEVICE),
    (lambda tables: tables["car_app_devices"][0].update(source="phone"), pairs.NO_DEVICE),
    (lambda tables: tables["car_app_devices"][0].update(source="simulator"), pairs.SIMULATOR),
])
def test_missing_or_ambiguous_car_fails_before_any_row(change, message):
    tables = _tables()
    change(tables)
    output = io.StringIO()
    with pytest.raises(ep._ExportError, match=f"^{re.escape(message)}$"):
        asyncio.run(pairs.run_export(_args(), output, FakeDB(tables), text or str, offset_s=OFFSET))
    assert output.getvalue() == ""
    assert str(pairs._pod_error(message.encode(), 1)) == message


def test_car_metadata_keeps_only_safe_values():
    base = {"user_id": USER, "vin": PRIVATE_VIN}
    for model in ("a" * 17, str(UUID(int=3)), "Seal U", None, 7):
        assert pairs.car_metadata({**base, "model_key": model}).model == "unknown"
    assert pairs.car_metadata({**base, "powertrain": " PHEV "}).energy == "hybrid"
    assert pairs.car_metadata({**base, "is_hybrid": True}).energy == "hybrid"
    assert pairs.car_metadata({**base, "is_hybrid": 1}).energy == "ev"
    assert pairs.car_metadata({**base, "app_version": "1.4.2 (private build)"}).app_version is None
    assert pairs.car_metadata({**base, "app_version": "1.4.2-beta"}).app_version is None
    assert pairs.car_metadata({**base, "app_version": "1.4.2-debug"}).app_version == "1.4.2-debug"
    assert pairs.car_metadata({**base, "device_source": "simulator"}).device_source == "simulator"
    naive = pairs.car_metadata({**base, "first_seen_at": datetime(2026, 1, 1)}).first_seen_at
    assert naive == datetime(2026, 1, 1, tzinfo=UTC)


def test_rows_outside_the_window_stop_the_export():
    export = pairs.PairExport(io.StringIO(), until=UNTIL, car_days=2, cloud_days=1, offset_s=0,
                              car=pairs.car_metadata({"user_id": USER, "vin": PRIVATE_VIN}))
    with pytest.raises(ep._ExportError, match=f"^{pairs.OUTSIDE}$"):
        export.add_car(_car_row(UNTIL))
    with pytest.raises(ep._ExportError, match=f"^{pairs.OUTSIDE}$"):
        export.add_cloud(_event(1, UNTIL - timedelta(days=1, seconds=1)))


def test_other_sources_and_odd_values_are_counted_not_written():
    car = {"user_id": USER, "vin": PRIVATE_VIN, "app_version": "2.0", "device_source": "car_app",
           "first_seen_at": datetime(1, 1, 1, tzinfo=UTC)}
    # An offset of -1 s makes the year-1 stamps overflow.
    export = pairs.PairExport(io.StringIO(), until=UNTIL, car_days=2, cloud_days=1, offset_s=-1,
                              car=pairs.car_metadata(car))
    export.add_cloud(_event(1, BASE, "webhook", "ok", payload={"secret": PRIVATE_VIN}))
    export.add_cloud(_event(2, BASE, payload={"lastUpdated": "0001-01-01T00:00:00+00:00"}))
    export.add_cloud(_event(3, BASE, payload={"lastUpdated": "2026-01-01T00:00:00"}))
    export.add_cloud(_event(4, BASE, payload={"lastUpdated": "2000-01-01T00:00:00Z", "online": float("nan")}))
    # The SQL already nulls free-text details; the pod drops one again if it ever arrives.
    export.add_cloud({**_event(5, BASE, kind="error"), "detail": "upstream said no", "payload": None})
    export.add_cloud(_event(6, BASE, payload={"lastUpdated": 1772323200}))
    # Malformed stored values are counted, never fatal: an offset that overflows on the way to UTC,
    # and a JSON integer too large for a float.
    export.add_cloud(_event(7, BASE, payload={"lastUpdated": "0001-01-01T00:00:00+01:00"}))
    export.add_cloud(_event(8, BASE, payload={"gps": {"latitude": 10**400, "longitude": LNG}}))
    export.add_car(_car_row(BASE, data={"location": {"lat": 10**400, "lng": LNG}, "brakePct": 3}))
    export.add_car(_car_row(BASE, data={"location": {"lat": LAT, "lng": LNG, "satellites": 9}}))
    export.add_car(_car_row(BASE, data="not json {"))
    export.add_car(_car_row(BASE, data=[1, 2]))
    export.add_car(_car_row(BASE, speed_kph=float("inf"), data={"doors": None, "tyreTempC": {"fl": 200, "xx": 1}}))
    for index in range(105):
        export.add_car(_car_row(BASE, data={f"key{index}": 1}))
    export.finish()
    rows = _lines(export.output.getvalue())
    assert [row["type"] for row in rows].count("cloud") == 7
    stats = rows[-1]["stats"]
    assert stats["cloud_rows"]["other"] == 1 and stats["unparsable_last_updated"] == 4
    assert stats["dropped_details"] == 1
    assert stats["invalid_values"] == {"data": 2, "doors": 1, "first_seen_t": 1, "gps_present": 1, "has_fix": 1,
                                       "last_updated_t": 1, "online": 1, "speed_kph": 1, "tyre_temp_c_fl": 1}
    # Only names the backend writes are kept; satellites, xx and key0..key104 are all "other".
    assert stats["dropped_car_keys"] == {"brakePct": 1, "other": 107}
    assert "first_seen_t" not in rows[-1] and rows[-1]["app_version"] == "2.0"
    assert PRIVATE_VIN not in export.output.getvalue()


def _bare_export(**car):
    return pairs.PairExport(io.StringIO(), until=UNTIL, car_days=2, cloud_days=1, offset_s=car.pop("offset_s", 0),
                            car=pairs.car_metadata({"user_id": USER, "vin": PRIVATE_VIN, "device_source": "car_app",
                                                    **car}))


def test_every_invalid_value_name_the_mapper_can_produce_is_in_the_fixed_set():
    bad = "bad"
    # An offset of -1 s makes a year-1 first_seen_at overflow.
    export = _bare_export(offset_s=-1, first_seen_at=datetime(1, 1, 1, tzinfo=UTC))
    export.add_car(_car_row(BASE, **dict.fromkeys(pairs.CAR_COLUMNS, bad), data={
        **dict.fromkeys(pairs.CAR_DATA, bad),
        **{key: dict.fromkeys(fields, bad) for key, (_, fields) in pairs.CAR_NESTED.items()},
        "location": {"lat": LAT, "lng": LNG, **dict.fromkeys(pairs.LOCATION_FIELDS, bad)},
    }))
    export.add_car(_car_row(BASE, data={**dict.fromkeys(pairs.CAR_NESTED, bad), "location": bad}))
    export.add_car(_car_row(BASE, data="{"))
    export.add_cloud(_event(1, BASE, payload={
        **dict.fromkeys(pairs.CLOUD_FIELDS, bad),
        **{key: dict.fromkeys(fields, bad) for key, (_, fields) in pairs.CLOUD_NESTED.items()},
        "gpsFresh": bad, "lastUpdated": "2000-01-01T00:00:00Z", "climate": bad, "gps": {"latitude": bad},
    }))
    export.add_cloud(_event(2, BASE, payload=dict.fromkeys(pairs.CLOUD_NESTED, bad)))
    export.add_cloud(_event(3, BASE, payload="["))
    export.finish()  # the meta line passes the check with every one of them
    assert set(export.invalid) == pairs.INVALID_NAMES
    assert set(_lines(export.output.getvalue())[-1]["stats"]["invalid_values"]) == pairs.INVALID_NAMES


def test_fixes_taken_at_the_same_time_give_one_answer():
    export = _bare_export()
    when = BASE.timestamp()
    export.add_car(_car_row(BASE, data={"location": {"lat": LAT, "lng": LNG}}))
    # Another row whose fix, by its age, was taken at the same instant somewhere else.
    export.add_car(_car_row(BASE + timedelta(seconds=10), speed_kph=5.0,
                            data={"location": {"lat": MOVED_LAT, "lng": LNG, "ageS": 10}}))
    for moment in (when, when + 1, when + 600):
        assert export.nearest_fix(moment) == export.last_fix(moment) == (when, MOVED_LAT, LNG, None, 5.0)
    export.add_cloud(_event(1, BASE + timedelta(seconds=30), payload={"gps": {"latitude": CLOUD_LAT, "longitude": LNG}}))
    cloud = [line for line in _lines(export.output.getvalue()) if line["type"] == "cloud"][0]
    assert cloud["gps_dist_m"] == cloud["gps_last_dist_m"] == 222 and cloud["gps_dt_s"] == cloud["gps_last_age_s"]


def test_an_undecodable_json_document_is_counted_and_its_row_still_written():
    tables = _tables()
    deep = "[" * 100_000 + "]" * 100_000  # RecursionError
    huge = '{"lock": ' + "1" * 5000 + "}"  # past int's digit limit: ValueError
    tables["car_telemetry"] = [
        _car_row(BASE, cadence="moving", speed_kph=12.5, data='{"lock": "locked"'),
        _car_row(BASE + timedelta(seconds=1), cadence="event", data=huge),
        _car_row(BASE + timedelta(seconds=2), data=deep),
    ]
    tables["ingest_events"] = [
        _event(1, BASE, payload='{"speed": 1'),
        _event(2, BASE + timedelta(seconds=1), "mqtt", "vehicleInfo", detail="received",
               payload='{"speed": ' + "9" * 5000 + "}"),
        _event(3, BASE + timedelta(seconds=2), payload=deep),
    ]
    _, stream = _export(tables)
    rows = _lines(stream)
    assert [row for row in rows if row["type"] == "car"] == [
        {"type": "car", "t": 151200.0, "cadence": "moving", "speed_kph": 12.5},
        {"type": "car", "t": 151201.0, "cadence": "event"},
        {"type": "car", "t": 151202.0},
    ]
    assert [row for row in rows if row["type"] == "cloud"] == [
        {"type": "cloud", "t": 151200.0, "source": "poll", "kind": "ok"},
        {"type": "cloud", "t": 151201.0, "source": "mqtt", "kind": "vehicleInfo", "detail": "received"},
        {"type": "cloud", "t": 151202.0, "source": "poll", "kind": "ok"},
    ]
    assert rows[-1]["stats"]["invalid_values"] == {"data": 3, "payload": 3}


# --- the statements ------------------------------------------------------------------------------

STATEMENT = re.compile(r"/\* (\w+) \*/\s+SELECT\b")
FORBIDDEN = re.compile(r"\b(INSERT|UPDATE|DELETE|MERGE|CREATE|ALTER|DROP|TRUNCATE|GRANT|REVOKE|COPY|LOCK|"
                       r"VACUUM|ANALYZE|CALL|DO|INTO|FOR\s+UPDATE|FOR\s+SHARE|init_db)\b", re.IGNORECASE)


def _assert_read_only(db):
    tags = []
    for transaction in db.transactions:
        assert [sql for sql, _ in transaction[:4]] == list(pairs.READ_SETTINGS)
        assert len(transaction) == 5
        sql, params = transaction[4]
        explain = sql.startswith("EXPLAIN (FORMAT JSON) ")
        body = sql.removeprefix("EXPLAIN (FORMAT JSON) ")
        match = STATEMENT.match(body)
        assert match, sql
        tags.append(("explain " if explain else "") + match.group(1))
        # Keywords outside string literals ('lock' is one of the detail values compared against).
        assert not FORBIDDEN.search(re.sub(r"'[^']*'", "''", sql)) and ";" not in sql
        assert set(re.findall(r"\b(?:FROM|JOIN)\s+([A-Za-z_]+)", sql)) <= pairs.TABLES
        assert "latitude" not in sql and "actor" not in sql
        if "FROM car_telemetry" in sql or "FROM byd_raw_replies" in sql:
            assert "at >= :lower AND at < :upper" in sql
            assert params["upper"] - params["lower"] <= timedelta(days=1)
            assert params["lower"].date() == (params["upper"] - timedelta(microseconds=1)).date()
        if "FROM ingest_events" in sql:
            assert "user_id = :user AND vin = :vin AND received_at >= :lower AND received_at < :upper" in sql
            assert params["upper"] - params["lower"] <= timedelta(days=1)
        if "FROM byd_raw_replies" in sql:
            assert "car_id = :car AND at >= :lower AND at < :upper" in sql
        if "LIMIT :limit" in sql:
            assert 1 <= params["limit"] <= (pairs.RAW_PAGE_ROWS if "byd_raw_replies" in sql else 5000)
    return tags


@pytest.mark.skipif(text is None, reason="SQLAlchemy is not installed")
@pytest.mark.parametrize("mode", ["export", "dry_run", "explain", "two_cars"])
def test_every_statement_is_a_bounded_read_only_select(mode):
    tables = _tables()
    if mode == "two_cars":
        tables["car_telemetry"].append(_car_row(BASE, car=OTHER_CAR))
    db = FakeDB(tables)
    try:
        _run(db, dry_run=mode == "dry_run", explain=mode == "explain")
    except ep._ExportError:
        assert mode == "two_cars"
    tags = _assert_read_only(db)
    identify = ["identify"] * 3  # the 2-day window ending at noon touches three UTC days
    explains = [transaction[4][0] for transaction in db.transactions if transaction[4][0].startswith("EXPLAIN")]
    resumed = [sql for sql in explains if ":after" in sql]
    # The printed resumed-page forms are planned with their keyset cursors; no page resumes here.
    assert len(resumed) == (0 if mode in ("export", "two_cars") else 3)
    assert all("AND at > :after" in sql or "(received_at, id) > (:after_first, :after_second)" in sql
               or "(at, endpoint, seq) > (:after_at, :after_endpoint, :after_seq)" in sql for sql in resumed)
    # Identification never pages.
    assert not any(":after" in sql for sql, _ in (t[4] for t in db.transactions) if "/* identify */" in sql)
    gated_rows = ["explain cloud_rows", "cloud_rows"] * 2  # each ingest statement right after its own plan
    if mode == "export":
        assert tags == [*identify, "metadata", *["car_rows"] * 3, *gated_rows, *["raw_rows"] * 3]
    elif mode == "two_cars":
        assert tags == identify
    else:
        plans = [f"explain {shape.removesuffix('_resumed')}" for shape in pairs.PLAN_SHAPES]
        counts = (["car_count"] * 3 + ["explain cloud_count", "cloud_count"] * 2 + ["raw_count"] * 3
                  if mode == "dry_run" else [])
        assert tags == [*identify, "metadata", *plans, *counts]
        # The dry run reads no row payloads: only counts, ids and the car's metadata are selected.
        for transaction in db.transactions:
            sql = transaction[4][0]
            if not sql.startswith("EXPLAIN"):
                assert not re.search(r"\b(?:payload|data|detail|raw|code)\b", sql)
    for transaction in db.transactions:
        sql = transaction[4][0]
        if "/* cloud_rows */" in sql:
            assert ("CASE WHEN (detail ~ '^[0-9]{1,5}$' OR detail IN ('session_expired', 'received', 'min_gap', "
                    "'busy', 'lock', 'not_selected', 'car_unlinked', 'missing_vin', 'exception', 'upstream')) "
                    "THEN detail END") in sql
            assert ("CASE WHEN (source = 'poll' AND kind = 'ok') OR source = 'mqtt' THEN CAST(payload AS text) END"
                    in sql)
        if "/* car_rows */" in sql:
            assert "CAST(data AS text) AS data" in sql
        if "/* raw_rows */" in sql:
            assert ("CASE WHEN (code IN ('1001', '1002', '1005', '1008', '1009', '1010', '5005', '5006', '5011', "
                    "'6002', '6051', '8003', 'crypto') OR code ~ '^http_[0-9]{3}$') THEN code WHEN code = "
                    "'TimeoutError' THEN 'timeout'\n"
                    "                    WHEN code IS NOT NULL THEN 'other' END AS code") in sql
            assert "CAST(raw AS text) AS raw" in sql and "ORDER BY at, endpoint, seq LIMIT :limit" in sql
        if "/* metadata */" in sql:
            assert "CASE WHEN cars.model_key_source IN ('vin', 'name') THEN cars.model_key END" in sql


@pytest.mark.parametrize(("mode", "until", "statements"), [
    ("export", UNTIL, 30 + 1 + 30 + 2 * 14 + 30),
    ("dry_run", UNTIL, 30 + 1 + 11 + 30 + 2 * 14 + 30),
    ("explain", UNTIL, 30 + 1 + 11),
    ("export", datetime(2026, 3, 1, tzinfo=UTC), 29 + 1 + 29 + 2 * 13 + 29),
])
def test_the_number_of_statements_with_default_windows(mode, until, statements):
    """The counts the README gives, with one page per day: 119, 130 and 42 at the defaults."""
    db = FakeDB(_tables())
    _run(db, car_days=29, cloud_days=13, until=until, dry_run=mode == "dry_run", explain=mode == "explain")
    assert len(db.transactions) == statements


def test_pages_resume_from_the_keyset_cursor_and_pause_between_queries(pauses):
    tables = _tables()
    day = datetime(2026, 2, 28, 18, tzinfo=UTC)
    tables["car_telemetry"] = [_car_row(day + timedelta(seconds=index)) for index in range(5)]
    same = day + timedelta(minutes=5)
    tables["ingest_events"] = [_event(number, same) for number in (3, 1, 2)]
    db = FakeDB(tables)
    stream = _run(db, page_rows=2, pause_ms=250)
    queries = [transaction[4] for transaction in db.transactions if not transaction[4][0].startswith("EXPLAIN")]
    car_pages = [params for sql, params in queries if "/* car_rows */" in sql]
    assert [page.get("after") for page in car_pages] == [
        None, None, day + timedelta(seconds=1), day + timedelta(seconds=3), None]
    assert "AND at > :after" in [sql for sql, _ in queries if "/* car_rows */" in sql][2]
    cloud_pages = [params for sql, params in queries if "/* cloud_rows */" in sql]
    assert [(page.get("after_first"), page.get("after_second")) for page in cloud_pages] == [
        (None, None), (same, UUID(int=2)), (None, None)]
    assert [len(db.cloud_rows(page)) for page in cloud_pages] == [2, 1, 0]
    assert pauses == [0.25] * (len(db.transactions) - 1)
    rows = _lines(stream)
    assert [row["t"] for row in rows if row["type"] == "car"] == [108000.0 + index for index in range(5)]
    assert rows[-1]["stats"]["cloud_rows"]["poll_ok"] == 3


def test_raw_pages_resume_from_the_primary_key_cursor():
    tables = _tables()
    same = BASE + timedelta(seconds=10)
    tables["byd_raw_replies"] = [_raw_row(same, endpoint, {"speed": seq}, seq=seq)
                                 for endpoint in ("realtime", "gps") for seq in (1, 0)]
    db = FakeDB(tables)
    stream = _run(db, page_rows=3)
    pages = [params for sql, params in (t[4] for t in db.transactions) if "/* raw_rows */" in sql]
    assert [page["limit"] for page in pages] == [3] * len(pages)
    resumed = [page for page in pages if "after_at" in page]
    assert [(page["after_at"], page["after_endpoint"], page["after_seq"]) for page in resumed] == [
        (same, "realtime", 0)]
    raw = [row for row in _lines(stream) if row["type"] == "raw"]
    assert [(row["endpoint"], row["fields"]) for row in raw] == [
        ("gps", {"speed": 0}), ("gps", {"speed": 1}), ("realtime", {"speed": 0}), ("realtime", {"speed": 1})]
    assert _lines(stream)[-1]["stats"]["raw_fields_dropped"] == {}
    # A page never holds more than RAW_PAGE_ROWS raw replies, however large --page-rows is.
    db = FakeDB(_tables())
    _run(db, page_rows=5000)
    assert {params["limit"] for sql, params in (t[4] for t in db.transactions) if "/* raw_rows */" in sql} == {
        pairs.RAW_PAGE_ROWS}


def test_the_first_error_stops_the_run_and_no_query_follows():
    db = FakeDB(_tables(), fail_on="car_rows")
    output = io.StringIO()
    with pytest.raises(RuntimeError):
        asyncio.run(pairs.run_export(_args(), output, db, text or str, offset_s=OFFSET))
    assert re.search(r"/\* car_rows \*/", db.transactions[-1][-1][0]) and output.getvalue() == ""
    assert sum("/* car_rows */" in transaction[-1][0] for transaction in db.transactions) == 1


@pytest.mark.parametrize("local", [False, True])
def test_main_reports_only_the_exception_class(monkeypatch, capsys, local):
    async def fail(args, output):
        raise RuntimeError(f"{PRIVATE_VIN} {LAT}, {LNG}")

    def fail_fetch(args):
        raise RuntimeError(f"{PRIVATE_VIN} {LAT}, {LNG}")

    monkeypatch.setattr(pairs.logging, "disable", lambda level: None)
    monkeypatch.setattr(pairs, "_run_pod", fail)
    monkeypatch.setattr(pairs, "_fetch", fail_fetch)
    monkeypatch.setattr(pairs, "signal", SimpleNamespace())
    with pytest.raises(SystemExit) as exc:
        pairs.main(["fetch", "--explain"] if local else [])
    assert exc.value.code == 1
    assert capsys.readouterr() == ("", "mapping export failed: RuntimeError\n")


def test_main_prints_a_data_free_export_error(monkeypatch, capsys):
    async def fail(args, output):
        raise ep._ExportError(pairs.NO_CAR)

    monkeypatch.setattr(pairs.logging, "disable", lambda level: None)
    monkeypatch.setattr(pairs, "_run_pod", fail)
    monkeypatch.setattr(pairs, "signal", SimpleNamespace())
    with pytest.raises(SystemExit):
        pairs.main(["--car-days", "3"])
    assert capsys.readouterr() == ("", pairs.NO_CAR + "\n")


@pytest.mark.parametrize("argv", [
    ["--car-days", "30"], ["--cloud-days", "14"], ["--car-days", "0"], ["--page-rows", "5001"],
    ["--pause-ms", "199"], ["--max-minutes", "0"], ["--dry-run", "--explain"], ["fetch"],
    ["--until", "2026-03-01T12:00:00"],
])
def test_arguments_are_bounded(argv, capsys):
    with pytest.raises(SystemExit) as exc:
        pairs.main(argv)
    assert exc.value.code == 2


def test_pod_arms_a_process_alarm_and_writes_plain_newlines(monkeypatch):
    calls = []

    def handler(signum, disposition):
        calls.append((signum, disposition))
        return "old handler"

    async def run(args, output):
        calls.append("run")

    stdout = SimpleNamespace(reconfigure=lambda **options: calls.append(options), flush=lambda: None)
    monkeypatch.setattr(pairs, "_run_pod", run)
    monkeypatch.setattr(pairs.sys, "stdout", stdout)
    monkeypatch.setattr(pairs, "signal", SimpleNamespace(signal=handler, SIGALRM=14, SIG_DFL=0, alarm=calls.append))
    pairs._pod_with_deadline(argparse.Namespace(max_minutes=15))
    assert calls == [{"newline": "\n"}, (14, 0), 900, "run", 0, (14, "old handler")]


def test_default_window_ends_ten_minutes_ago_in_whole_seconds():
    db = FakeDB(_tables())
    output = io.StringIO()
    now = UNTIL + timedelta(minutes=10, seconds=59, microseconds=5)
    asyncio.run(pairs.run_export(_args(until=None), output, db, text or str, offset_s=0, now=now))
    assert _lines(output.getvalue())[-1]["start"] == _iso(UNTIL - timedelta(days=2))


# --- dry run and plans ---------------------------------------------------------------------------


COMPOSITE = "ix_ingest_events_user_vin_received"


def _scan(kind, relation="ingest_events", index=None, children=()):
    node = {"Node Type": kind, "Relation Name": relation, "Index Name": index, "Plan Rows": 3}
    if relation is None:
        del node["Relation Name"]
    if children:
        node["Plans"] = list(children)
    return node


BITMAP = [_scan("Bitmap Heap Scan", children=[_scan("Bitmap Index Scan", None, COMPOSITE)])]


@pytest.mark.parametrize(("nodes", "ok"), [
    ([_scan("Index Scan", index=COMPOSITE)], True),
    ([_scan("Index Only Scan", index=COMPOSITE)], True),
    (BITMAP, True),
    ([_scan("Sort", None, children=[_scan("Index Scan", index=COMPOSITE)])], True),
    ([_scan("Result", None)], True),
    ([_scan("Seq Scan")], False),
    ([_scan("Parallel Seq Scan")], False),
    ([_scan("Index Scan", index="ix_ingest_events_received_at")], False),
    ([_scan("Index Scan", index=None)], False),
    ([_scan("Bitmap Heap Scan", children=[_scan("Bitmap Index Scan", None, "ix_ingest_events_received_at")])], False),
    ([_scan("Bitmap Heap Scan", children=[
        _scan("BitmapAnd", None, children=[_scan("Bitmap Index Scan", None, COMPOSITE),
                                           _scan("Bitmap Index Scan", None, "ix_ingest_events_vin")])])], False),
    ([_scan("Append", None, children=[_scan("Seq Scan", "ingest_events_2026_03")])], False),
    ([_scan("Index Scan", "ingest_events_2026_03", COMPOSITE)], False),
    ([_scan("Index Scan", "cars", COMPOSITE)], False),
    ([_scan("Tid Scan")], False),
])
def test_the_ingest_events_plan_rule_fails_closed(nodes, ok):
    plan = [{"Plan": {"Node Type": "Limit", "Plan Rows": 2, "Plans": nodes}}]
    assert pairs.ingest_plan_ok(ep._plan_nodes(plan)) is ok


GATED = re.escape(pairs.INDEX_GATE)


@pytest.mark.parametrize(("scans", "export_blocked", "dry_blocked"), [
    ({"cloud_rows": "Seq Scan"}, True, GATED),
    ({"cloud_rows": "Parallel Seq Scan"}, True, GATED),
    ({"cloud_rows": lambda params: [_scan("Index Scan", index="ix_ingest_events_received_at")]}, True, GATED),
    # An unknown relation is refused by the plan check before the dry run's gate is reached.
    ({"cloud_rows": lambda params: [_scan("Append", None, children=[_scan("Seq Scan", "ingest_events_2026_03")])]},
     True, re.escape(pairs.INVALID_NODE)),
    # The export runs no count; the dry run does, so its count plans are gated too.
    ({"cloud_count": "Seq Scan"}, False, GATED),
    ({"cloud_rows": lambda params: BITMAP, "cloud_count": lambda params: BITMAP}, False, None),
    # A partition scanned whole is one car-day, not every customer's events.
    ({"car_rows": "Seq Scan", "car_count": "Seq Scan"}, False, None),
])
def test_ingest_events_is_read_only_through_this_cars_index(scans, export_blocked, dry_blocked):
    db = FakeDB(_tables(), scans=scans)
    output = _GuardedOutput(db)
    run = pairs.run_export(_args(), output, db, text or str, offset_s=OFFSET)
    if export_blocked:
        with pytest.raises(ep._ExportError, match=f"^{re.escape(pairs.INDEX_GATE)}$"):
            asyncio.run(run)
        # Planned, never run: every cloud_rows statement that reached the database was an EXPLAIN.
        assert not any("/* cloud_rows */" in sql and not sql.startswith("EXPLAIN")
                       for sql, _ in (transaction[4] for transaction in db.transactions))
        assert any("/* cloud_rows */" in sql for sql, _ in (transaction[4] for transaction in db.transactions))
    else:
        asyncio.run(run)
        assert _lines(output.getvalue())[-1]["type"] == "meta"
    dry_db = FakeDB(_tables(), scans=scans)
    if dry_blocked:
        # Nothing is counted after a refusing plan; --explain still shows the plans.
        with pytest.raises(ep._ExportError, match=f"^{dry_blocked}$"):
            _run(dry_db, dry_run=True)
        assert not any("_count */" in transaction[4][0] and not transaction[4][0].startswith("EXPLAIN")
                       for transaction in dry_db.transactions)
        if dry_blocked == GATED:
            plans = json.loads(_run(FakeDB(_tables(), scans=scans), explain=True))
            assert not pairs.ingest_plans_ok(plans)
    else:
        assert json.loads(_run(dry_db, dry_run=True))["ingest_events_plan_ok"] is True


SECOND_DAY = datetime(2026, 3, 1, tzinfo=UTC)


@pytest.mark.parametrize("bad", ["second day", "resumed page"])
def test_each_ingest_statement_is_gated_with_its_own_parameters(bad):
    """A plan fine for the first day's first page but not for a later one stops before that statement."""
    tables = _tables()
    day = datetime(2026, 2, 28, 18, tzinfo=UTC)
    tables["ingest_events"] = [_event(number, day + timedelta(seconds=number)) for number in (1, 2, 3)]
    tables["ingest_events"].append(_event(9, BASE))

    def plan(params):
        refuse = (params["lower"] == SECOND_DAY) if bad == "second day" else "after_first" in params
        return [_scan("Seq Scan")] if refuse else [_scan("Index Scan", index=COMPOSITE)]

    db = FakeDB(tables, scans={"cloud_rows": plan})
    output = _GuardedOutput(db)
    with pytest.raises(ep._ExportError, match=f"^{re.escape(pairs.INDEX_GATE)}$"):
        asyncio.run(pairs.run_export(_args(page_rows=2 if bad == "resumed page" else 2000), output, db,
                                     text or str, offset_s=OFFSET))
    statements = [transaction[4] for transaction in db.transactions if "/* cloud_rows */" in transaction[4][0]]
    executed = [params for sql, params in statements if not sql.startswith("EXPLAIN")]
    explained = [params for sql, params in statements if sql.startswith("EXPLAIN")]
    # Every executed statement was explained right before with the same parameters.
    assert all(statements[index - 1] == ("EXPLAIN (FORMAT JSON) " + sql, params)
               for index, (sql, params) in enumerate(statements) if not sql.startswith("EXPLAIN"))
    # The refusing statement was explained and never run; the pages before it were written.
    assert len(explained) == len(executed) + 1
    refused = explained[-1]
    if bad == "second day":
        assert refused["lower"] == SECOND_DAY and all(params["lower"] != SECOND_DAY for params in executed)
    else:
        assert "after_first" in refused and refused["after_second"] == UUID(int=2)
        assert not any("after_first" in params for params in executed)
    written = [line for line in _lines(output.getvalue()) if line["type"] == "cloud"]
    assert len(written) == (3 if bad == "second day" else 2)


def test_plan_output_hides_the_dates_in_partition_and_index_names():
    class Dated(FakeDB):
        def run(self, sql, params):
            rows = super().run(sql, params)
            if sql.startswith("EXPLAIN") and "car_telemetry" in sql:
                plan = [{"Plan": {"Node Type": "Index Scan", "Relation Name": "car_telemetry_p20260228",
                                  "Index Name": "car_telemetry_p20260228_pkey", "Plan Rows": 3}}]
                return [{"QUERY PLAN": plan}]
            return rows

    stream = _run(Dated(_tables()), explain=True)
    assert "20260228" not in stream
    plans = pairs.checked_plans(json.loads(stream))
    assert {"node": "Index Scan", "relation": "car_telemetry_partition",
            "index": "car_telemetry_partition_pkey", "rows": 3} in plans["car_rows"]


def test_the_pod_script_is_ascii_and_never_touches_migrations():
    script = pairs.build_pod_script()
    script.decode("ascii")
    source = Path(pairs.__file__).read_text(encoding="utf-8") + Path(ep.__file__).read_text(encoding="utf-8")
    assert not re.search(r"init_db|upgrade_database|import alembic|app\.migrations", source)


def test_dry_run_prints_only_counts_and_checked_plans():
    tables = _tables()
    stream = _run(FakeDB(tables), dry_run=True)
    assert stream.count("\n") == 1
    result = json.loads(stream)
    node = [{"node": "Limit", "relation": None, "index": None, "rows": 2},
            {"node": "Index Scan", "relation": None, "index": "fake_idx", "rows": 7}]
    composite = {**node[1], "index": "ix_ingest_events_user_vin_received"}
    assert result == {
        "cars": 1, "device_sources": {"car_app": 1, "none": 0, "other": 0, "simulator": 0},
        "car_rows": 4, "car_days_with_rows": 2,
        "cloud_rows": {**dict.fromkeys(ep.ROW_KINDS, 0), "poll_ok": 3, "poll_error": 3, "poll_skip": 1,
                       "poll_other": 1, "mqtt_vehicleInfo": 1, "mqtt_remoteControl": 1},
        "cloud_days_with_rows": 1,
        # The partition's dated name is not printed.
        "plans": {shape: [node[0], {**(composite if relation == "ingest_events" else node[1]), "relation": relation}]
                  for shape, relation in (
            ("identify", "car_telemetry"), ("metadata", "cars"), ("car_rows", "car_telemetry_partition"),
            ("car_rows_resumed", "car_telemetry_partition"), ("car_count", "car_telemetry_partition"),
            ("cloud_rows", "ingest_events"), ("cloud_rows_resumed", "ingest_events"),
            ("cloud_count", "ingest_events"), ("raw_rows", "byd_raw_replies_partition"),
            ("raw_rows_resumed", "byd_raw_replies_partition"), ("raw_count", "byd_raw_replies_partition"))},
        "ingest_events_plan_ok": True,
        # Every source is counted (the webhook row too); the other car is not.
        "raw_rows": {**dict.fromkeys(pairs.RAW_ENDPOINTS, 0), "realtime": 4, "energy": 1, "charging": 1, "gps": 1,
                     "hvac": 1, "schedule": 1, "vehicleInfo": 2, "other": 1},
        "raw_rows_by_day": [{}, {"realtime": 1}, {"realtime": 3, "energy": 1, "charging": 1, "gps": 1, "hvac": 1,
                                                  "schedule": 1, "vehicleInfo": 2, "other": 1}],
        "raw_days_with_rows": 2,
    }
    assert PRIVATE_VIN not in stream and "52.123456" not in stream and "20260228" not in stream
    assert "privateEventName" not in stream
    assert pairs.checked_dry_run(result) == result
    tables["car_app_devices"][0]["source"] = "simulator"
    assert json.loads(_run(FakeDB(tables), dry_run=True))["device_sources"]["simulator"] == 1
    tables["car_app_devices"].clear()
    assert json.loads(_run(FakeDB(tables), dry_run=True))["device_sources"]["none"] == 1
    explained = json.loads(_run(FakeDB(_tables(), json_as_text=True), explain=True))
    assert explained == result["plans"]


def _plans():
    node = {"node": "Seq Scan", "relation": "car_telemetry_partition", "index": None, "rows": 9}
    return {shape: [dict(node)] for shape in pairs.PLAN_SHAPES}


@pytest.mark.parametrize(("edit", "message"), [
    (lambda plans: plans.pop("identify"), pairs.INVALID_PLANS),
    (lambda plans: plans.update(identify=[]), pairs.INVALID_PLANS),
    (lambda plans: plans.update(identify="x"), pairs.INVALID_PLANS),
    (lambda plans: plans["identify"][0].update(Filter=PRIVATE_VIN), pairs.INVALID_NODE),
    (lambda plans: plans["identify"].append("x"), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(relation="users"), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(relation="car_telemetry_p2026022"), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(relation="car_telemetry_p20260228"), pairs.INVALID_NODE),
    # A dated partition anywhere in an index name, even one the pod failed to rewrite.
    (lambda plans: plans["identify"][0].update(index="car_telemetry_p20260228_pkey"), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(index="ix_car_telemetry_p20260228_at"), pairs.INVALID_NODE),
    (lambda plans: plans["raw_rows"][0].update(relation="byd_raw_replies_p20260228"), pairs.INVALID_NODE),
    (lambda plans: plans["raw_rows"][0].update(index="byd_raw_replies_p20260228_pkey"), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(node="Scan " + PRIVATE_VIN), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(index="ix_" + PRIVATE_VIN), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(relation=7), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(index="bad index"), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(index=3), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(node="Scan; " + PRIVATE_VIN), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(node=None), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(rows=1.5), pairs.INVALID_NODE),
    (lambda plans: plans["identify"][0].update(rows=-1), pairs.INVALID_NODE),
])
def test_plans_are_checked(edit, message):
    plans = _plans()
    assert pairs.checked_plans(_plans()) == plans
    for relation in ("ingest_events", "cars", "car_app_devices", "car_telemetry", None):
        pairs.checked_plans({**plans, "metadata": [{**plans["metadata"][0], "relation": relation}]})
    edit(plans)
    with pytest.raises(ep._ExportError, match=f"^{message}$"):
        pairs.checked_plans(plans)


def _dry_run():
    return {"cars": 1, "device_sources": dict.fromkeys(pairs.DEVICE_SOURCES, 0), "car_rows": 3,
            "car_days_with_rows": 1, "cloud_rows": dict.fromkeys(ep.ROW_KINDS, 0), "cloud_days_with_rows": 0,
            "plans": _plans(), "ingest_events_plan_ok": True, "raw_rows": dict.fromkeys(pairs.RAW_ENDPOINTS, 0),
            "raw_rows_by_day": [{}, {"realtime": 2}], "raw_days_with_rows": 1}


@pytest.mark.parametrize("edit", [
    lambda result: result.update(extra=1),
    lambda result: result.update(cars=True),
    lambda result: result.update(ingest_events_plan_ok=1),
    lambda result: result.pop("ingest_events_plan_ok"),
    lambda result: result.update(car_rows=-1),
    lambda result: result.update(car_rows=10**16),
    lambda result: result["device_sources"].update(phone=1),
    lambda result: result.update(device_sources=[]),
    lambda result: result["cloud_rows"].pop("other"),
    lambda result: result.update(cloud_rows=None),
    lambda result: result["cloud_rows"].update(poll_ok=1.0),
    lambda result: result.update(plans={}),
    lambda result: result.clear(),
    lambda result: result["raw_rows"].pop("other"),
    lambda result: result["raw_rows"].update(privateEvent=1),
    lambda result: result.update(raw_days_with_rows=-1),
    lambda result: result.update(raw_rows_by_day="x"),
    lambda result: result.update(raw_rows_by_day=[{"privateEvent": 1}]),
    lambda result: result.update(raw_rows_by_day=[{"realtime": 1.5}]),
    lambda result: result.update(raw_rows_by_day=[[1]]),
    lambda result: result.update(raw_rows_by_day=[{}] * 31),
])
def test_dry_run_counts_are_checked(edit):
    result = _dry_run()
    assert pairs.checked_dry_run(_dry_run()) == result
    edit(result)
    with pytest.raises(ep._ExportError, match="^invalid "):
        pairs.checked_dry_run(result)
    with pytest.raises(ep._ExportError):
        pairs.checked_dry_run([])


# --- the composed pod script ---------------------------------------------------------------------


def test_pod_script_is_deterministic_self_contained_and_compiles():
    script = pairs.build_pod_script()
    assert script == pairs.build_pod_script()
    code = compile(script, "<pod>", "exec")
    assert code is not None
    text_script = script.decode("utf-8")
    assert text_script.startswith("# Composed by bench.mapping.export_pairs.build_pod_script")
    for path in (Path(ep.__file__), Path(pairs.__file__)):
        source = path.read_bytes().decode("utf-8")
        assert ascii(source) in text_script
    assert text_script.endswith("sys.modules['bench.mapping.export_pairs'].main()\n")
    assert text_script.index("'bench.replay.export_prod'") < text_script.index("'bench.mapping.export_pairs'")


FAKE_MODULES = {
    "sqlalchemy/__init__.py": "class _Text(str):\n    pass\n\n\ndef text(sql):\n    return _Text(sql)\n",
    "sqlalchemy/ext/__init__.py": "",
    "sqlalchemy/ext/asyncio.py": "def async_sessionmaker(engine):\n    return engine.db\n",
    "app/__init__.py": "",
    "app/config.py": "def get_settings():\n    return object()\n",
    "app/db.py": (
        "import json\nimport os\n\nfrom fake_db import FakeDB, load\n\n\n"
        "class Engine:\n"
        "    def __init__(self):\n"
        "        self.db = FakeDB(load(os.environ['MAPPING_FAKE_DB']), json_as_text=True)\n\n"
        "    async def dispose(self):\n"
        "        with open(os.environ['MAPPING_FAKE_LOG'], 'w', encoding='utf-8') as file:\n"
        "            json.dump(self.db.transactions, file, default=str)\n\n\n"
        "def create_engine(settings):\n    return Engine()\n"
    ),
}


def _pod_env(tmp_path, tables):
    root = tmp_path / "pod"
    for name, source in {**FAKE_MODULES, "fake_db.py": FAKE_DB}.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source, encoding="utf-8")
    database = tmp_path / "tables.json"
    database.write_text(json.dumps(tables, default=str), encoding="utf-8")
    env = {key: value for key, value in os.environ.items() if key not in ("PYTHONPATH", "PYTHONHOME")}
    env.update(PYTHONPATH=str(root), MAPPING_FAKE_DB=str(database), MAPPING_FAKE_LOG=str(tmp_path / "log.json"))
    return root, env


def _pod(tmp_path, tables, *options):
    root, env = _pod_env(tmp_path, tables)
    argv = ["--until", UNTIL.isoformat(), "--car-days", "2", "--cloud-days", "1", "--pause-ms", "200",
            "--page-rows", "2", *options]
    result = subprocess.run([sys.executable, "-", *argv], input=pairs.build_pod_script(), capture_output=True,
                            cwd=root, env=env, timeout=120, check=False)
    log = tmp_path / "log.json"
    transactions = json.loads(log.read_text(encoding="utf-8")) if log.exists() else []
    return result, transactions


def test_composed_pod_script_runs_end_to_end(tmp_path):
    result, transactions = _pod(tmp_path, _tables())
    assert result.returncode == 0, result.stderr
    assert result.stderr == b""
    stream = result.stdout.decode("ascii")
    assert "\r" not in stream
    for secret in (*PRIVATE_DIGITS, PRIVATE_VIN, CAR_ID, str(USER)):
        assert secret not in stream
    kinds = [pairs.check_line(line)[0] for line in stream.splitlines(keepends=True)]
    assert kinds[-1] == "meta" and kinds.count("car") == 4 and kinds.count("cloud") == 10 and kinds.count("raw") == 11
    assert "privateEventName" not in stream and str(RAW_MS) not in stream
    pod = tmp_path / "stream.jsonl"
    pod.write_bytes(result.stdout)
    folded = tmp_path / "folded.jsonl"
    pairs.fold(pod, folded)
    meta, lines = pairs.verify(folded)
    assert lines == {"car": 4, "cloud": 10, "raw": 11} and meta["car_days"] == 2
    # The same lines as an in-process run, apart from the random start.
    local = _lines(_run(FakeDB(_tables())))
    assert _lines(stream)[:-1] == local[:-1]
    assert {key: value for key, value in _lines(stream)[-1].items() if key != "start"} == {
        key: value for key, value in local[-1].items() if key != "start"}
    for transaction in transactions:
        assert [sql for sql, _ in transaction[:4]] == list(pairs.READ_SETTINGS)
        assert re.match(r"(?:EXPLAIN \(FORMAT JSON\) )?/\* \w+ \*/\s+SELECT\b", transaction[4][0])


def test_composed_pod_script_dry_run_and_failures(tmp_path):
    result, _ = _pod(tmp_path / "dry", _tables(), "--dry-run")
    assert result.returncode == 0, result.stderr
    assert pairs.checked_dry_run(json.loads(result.stdout))["car_rows"] == 4
    tables = _tables()
    tables["car_telemetry"].append(_car_row(BASE, car=OTHER_CAR))
    result, transactions = _pod(tmp_path / "two", tables)
    assert result.returncode == 1 and result.stdout == b""
    assert result.stderr.decode().strip() == pairs.SEVERAL_CARS
    assert all("/* identify */" in transaction[4][0] for transaction in transactions)
    tables = _tables()
    tables["car_app_devices"][0]["source"] = "simulator"
    result, _ = _pod(tmp_path / "simulator", tables)
    assert result.returncode == 1 and result.stdout == b""
    assert result.stderr.decode().strip() == pairs.SIMULATOR
    result, _ = _pod(tmp_path / "bad", _tables(), "--car-days", "30")
    assert result.returncode == 2 and result.stdout == b""


# --- the local fetch -----------------------------------------------------------------------------


def _stream():
    return _run(FakeDB(_tables())).encode("ascii")


def _fake_kubectl(monkeypatch, stdout=b"", stderr=b"", status=0, calls=None, hang=False):
    real = subprocess.run

    def run(command, **options):
        if command[0] == "git":
            return real(command, **options)
        assert command[:12] == ["kubectl", "-n", "bydconnect", "exec", "-i", "deploy/api", "--",
                                "nice", "-n", "19", "python", "-"]
        assert options["input"] == pairs.build_pod_script() and not options["check"]
        assert options["timeout"] == (15 + 2) * 60
        if calls is not None:
            calls.append(command)
        if hang:
            if "stdout" in options:
                options["stdout"].write(stdout)  # a partial stream, then the deadline
            raise subprocess.TimeoutExpired(command, options["timeout"])
        if options.get("capture_output"):
            return SimpleNamespace(returncode=status, stdout=stdout, stderr=stderr)
        options["stdout"].write(stdout)
        assert options["stderr"] == subprocess.PIPE
        return SimpleNamespace(returncode=status, stderr=stderr)

    monkeypatch.setattr(pairs.subprocess, "run", run)


def test_fetch_keeps_a_checked_file_with_the_meta_line_first(tmp_path, monkeypatch):
    monkeypatch.setattr(ep, "_export_directory", lambda path: path)
    calls = []
    _fake_kubectl(monkeypatch, _stream(), calls=calls)
    report = pairs._fetch(_args(out_dir=tmp_path))
    files = [path.name for path in tmp_path.iterdir()]
    assert files == ["pairs-2026-03-26-2d.jsonl"] and report["file"] == files[0]
    assert report["car_lines"] == 4 and report["cloud_lines"] == 10 and report["raw_lines"] == 11
    assert report["start"] == "2026-03-26T15:34:31Z"
    lines = (tmp_path / files[0]).read_text(encoding="ascii").splitlines()
    assert json.loads(lines[0])["type"] == "meta" and len(lines) == 26
    assert calls[0][12:] == ["--car-days", "2", "--cloud-days", "1", "--page-rows", "2000", "--pause-ms", "200",
                             "--max-minutes", "15", "--until", UNTIL.isoformat()]
    with pytest.raises(ep._ExportError, match="already exists"):
        pairs._fetch(_args(out_dir=tmp_path))
    assert [path.name for path in tmp_path.iterdir()] == files


@pytest.mark.parametrize(("edit", "stderr", "status", "message"), [
    (lambda stream: stream, b"traceback with ABCDEFGHJK1234567\nprecise_number on car line\n", 1,
     "^precise_number on car line$"),
    (lambda stream: stream, b"mapping export failed: OperationalError\n", 1,
     "^mapping export failed: OperationalError$"),
    (lambda stream: stream, b"error: user@example.com 52.123456\n", 7, r"^kubectl export failed \(exit 7\)$"),
    (lambda stream: stream, b"mapping export failed: ABCDEFGHJK1234567\n", 1, r"^kubectl export failed \(exit 1\)$"),
    (lambda stream: stream, pairs.INDEX_GATE.encode() + b"\n", 1, f"^{re.escape(pairs.INDEX_GATE)}$"),
    # Each stream's own window: cloud lines start one day in (cloud_days 1 of 2), nothing ends after 2 days.
    (lambda stream: stream.replace(b'"t":151230.0,"source"', b'"t":86399.5,"source"'), b"", 0,
     "^out_of_range on cloud line$"),
    (lambda stream: stream.replace(b'{"type":"car","t":158400.0', b'{"type":"car","t":172800.5'), b"", 0,
     "^out_of_range on car line$"),
    (lambda stream: stream.replace(b'"speed_kph":50.5', b'"speed_kph":52.123456'), b"", 0,
     "^precise_number on car line$"),
    (lambda stream: stream.replace(b'"cadence":"moving"', f'"cadence":"{PRIVATE_VIN}"'.encode()), b"", 0,
     "^vin on car line$"),
    (lambda stream: stream.replace(b'"car_rows":4', b'"car_rows":5'), b"", 0, "^count_mismatch on meta line$"),
    (lambda stream: stream.rsplit(b"\n", 2)[0] + b"\n", b"", 0, "^missing_meta on meta line$"),
    (lambda stream: b"", b"", 0, "^missing_meta on meta line$"),
    (lambda stream: stream.rsplit(b"\n", 2)[1] + b"\n" + stream, b"", 0, "^unexpected_meta on meta line$"),
    (lambda stream: b"\xff" + stream, b"", 0, "^invalid_json on unknown line$"),
    (lambda stream: stream.replace(b"\n", b"\r\n"), b"", 0, "^invalid_line on unknown line$"),
])
def test_fetch_removes_its_scratch_file_on_failure(tmp_path, monkeypatch, edit, stderr, status, message):
    monkeypatch.setattr(ep, "_export_directory", lambda path: path)
    _fake_kubectl(monkeypatch, edit(_stream()), stderr, status)
    with pytest.raises(ep._ExportError, match=message):
        pairs._fetch(_args(out_dir=tmp_path))
    assert list(tmp_path.iterdir()) == []


def test_verify_accepts_the_window_edges(tmp_path):
    meta = {**FULL_META, "car_days": 2, "cloud_days": 1, "stats": {
        **FULL_META["stats"], "car_rows": 2, "cloud_rows": {**dict.fromkeys(ep.ROW_KINDS, 0), "poll_ok": 1},
        "raw_rows": {"poll": 0, "mqtt": 1, "other": 0}, "raw_endpoints": {**FULL_META["stats"]["raw_endpoints"],
                                                                           "remoteControl": 1}}}
    lines = [meta, {"type": "car", "t": 0}, {"type": "car", "t": 172800},
             {"type": "cloud", "t": 86400, "source": "poll", "kind": "ok"},
             {"type": "raw", "t": 0, "source": "mqtt", "endpoint": "remoteControl"}]
    path = tmp_path / "edges.jsonl"
    path.write_bytes("".join(ep.json_line(line) for line in lines).encode())
    assert pairs.verify(path)[1] == {"car": 2, "cloud": 1, "raw": 1}
    # Raw lines follow the car window: here the shorter one.
    meta = {**meta, "car_days": 1, "cloud_days": 2}
    lines = [meta, {"type": "car", "t": 86400}, {"type": "car", "t": 172800},
             {"type": "cloud", "t": 0, "source": "poll", "kind": "ok"},
             {"type": "raw", "t": 86399.5, "source": "mqtt", "endpoint": "remoteControl"}]
    path.write_bytes("".join(ep.json_line(line) for line in lines).encode())
    with pytest.raises(ep._ExportError, match="^out_of_range on raw line$"):
        pairs.verify(path)
    # And the meta line's raw counts must match the raw lines.
    lines[-1] = {**lines[-1], "t": 86400}
    lines[0] = {**meta, "stats": {**meta["stats"], "raw_rows": {"poll": 0, "mqtt": 2, "other": 0}}}
    path.write_bytes("".join(ep.json_line(line) for line in lines).encode())
    with pytest.raises(ep._ExportError, match="^count_mismatch on meta line$"):
        pairs.verify(path)


def test_fetch_stops_a_hung_kubectl_and_removes_its_scratch_file(tmp_path, monkeypatch):
    monkeypatch.setattr(ep, "_export_directory", lambda path: path)
    _fake_kubectl(monkeypatch, _stream()[:100], hang=True)
    with pytest.raises(ep._ExportError, match=f"^{pairs.KUBECTL_TIMEOUT}$"):
        pairs._fetch(_args(out_dir=tmp_path))
    assert list(tmp_path.iterdir()) == []
    _fake_kubectl(monkeypatch, hang=True)
    with pytest.raises(ep._ExportError, match=f"^{pairs.KUBECTL_TIMEOUT}$"):
        pairs._fetch(_args(dry_run=True))


def test_verify_finds_a_meta_line_and_a_non_utf8_file(tmp_path):
    empty = tmp_path / "empty.jsonl"
    empty.write_bytes(b"")
    with pytest.raises(ep._ExportError, match="^missing_meta on meta line$"):
        pairs.verify(empty)
    car = tmp_path / "car.jsonl"
    car.write_bytes(ep.json_line(FULL_CAR).encode())
    with pytest.raises(ep._ExportError, match="^missing_meta on meta line$"):
        pairs.verify(car)
    broken = tmp_path / "broken.jsonl"
    broken.write_bytes(ep.json_line(FULL_META).encode() + b"\xff\n")
    with pytest.raises(ep._ExportError, match="^invalid_json on unknown line$"):
        pairs.verify(broken)


def test_fetch_refuses_an_out_dir_outside_the_data_root(tmp_path, monkeypatch):
    _fake_kubectl(monkeypatch, status=99)
    with pytest.raises(ep._ExportError, match="outside the replay data directory"):
        pairs._fetch(_args(out_dir=tmp_path / "out"))
    assert not (tmp_path / "out").exists()


def test_fetch_refuses_an_out_dir_inside_a_git_work_tree(tmp_path, monkeypatch):
    _fake_kubectl(monkeypatch, status=99)
    monkeypatch.setattr(ep, "EXPORT_ROOT", tmp_path)
    (tmp_path / ".git").mkdir()
    with pytest.raises(ep._ExportError, match="git work tree"):
        pairs._fetch(_args(out_dir=tmp_path / "seal"))
    assert not (tmp_path / "seal").exists()


def test_fetch_uses_a_directory_under_the_data_root(tmp_path, monkeypatch):
    root = tmp_path / "data"
    root.mkdir()
    monkeypatch.setattr(ep, "EXPORT_ROOT", root)
    _fake_kubectl(monkeypatch, _stream())
    report = pairs._fetch(_args(out_dir=root / "seal"))
    assert [path.name for path in (root / "seal").iterdir()] == [report["file"]]


@pytest.mark.parametrize("mode", ["dry_run", "explain"])
def test_fetch_dry_run_and_explain_print_only_checked_json(tmp_path, monkeypatch, mode):
    good = _dry_run() if mode == "dry_run" else _plans()
    calls = []
    _fake_kubectl(monkeypatch, ep.json_line(good).encode(), calls=calls)
    assert pairs._fetch(_args(**{mode: True})) == good
    assert calls[0][-1] == "--" + mode.replace("_", "-")
    bad = json.loads(json.dumps(good))
    if mode == "dry_run":
        bad["cars"] = PRIVATE_VIN
    else:
        bad["identify"][0]["Filter"] = "x"
    for stdout, message in (
        (ep.json_line(bad).encode(), "^invalid "),
        (ep.json_line(good).encode() * 2, f"^{pairs.INVALID_COUNTS}$"),
        (ep.json_line(good).encode()[:-1], f"^{pairs.INVALID_COUNTS}$"),
        (b"\xff\n", f"^{pairs.INVALID_COUNTS}$"),
        (b"{bad\n", f"^{pairs.INVALID_COUNTS}$"),
        (b'{"cars":NaN}\n', f"^{pairs.INVALID_COUNTS}$"),
        (b'{"cars":1,"cars":2}\n', "^duplicate key"),
        (f'{{"x":"{PRIVATE_VIN}"}}\n'.encode(), f"^{pairs.INVALID_COUNTS}$"),
    ):
        _fake_kubectl(monkeypatch, stdout)
        with pytest.raises(ep._ExportError, match=message):
            pairs._fetch(_args(**{mode: True}))
    _fake_kubectl(monkeypatch, b"", b"\xff private\n", 3)
    with pytest.raises(ep._ExportError, match=r"^kubectl export failed \(exit 3\)$"):
        pairs._fetch(_args(**{mode: True}))
    assert list(tmp_path.iterdir()) == []


def test_fetch_help_works():
    result = subprocess.run([sys.executable, "-m", "bench.mapping.export_pairs", "fetch", "--help"],
                            capture_output=True, cwd=Path(__file__).resolve().parents[2], timeout=60, check=False)
    assert result.returncode == 0 and b"--out-dir" in result.stdout and b"--dry-run" in result.stdout
