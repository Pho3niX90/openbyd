"""Export one car's in-car-app telemetry and its BYD cloud events, anonymised, for local pairing.

The API pod runs this module from one composed stdin script (``build_pod_script``): the
script carries ``bench/replay/export_prod.py`` and this file, because the pod has no
checkout. ``fetch`` composes that script, runs it through kubectl and checks every line
again before it keeps the file. Keep the top-level imports in the standard library and
``export_prod``. See bench/mapping/README.md for the line format and the rules.
"""

from __future__ import annotations

import argparse
import asyncio
import bisect
import hashlib
import json
import logging
import math
import re
import secrets
import signal
import subprocess
import sys
import uuid
from collections import Counter
from collections.abc import AsyncIterator, Callable, Iterable, Iterator, Mapping
from contextlib import aclosing
from datetime import UTC, datetime, time, timedelta
from decimal import Decimal
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any, NamedTuple, TextIO

from bench.replay import export_prod as ep

VERSION = 1
SYNTHETIC_VIN = "TESTVIN0000000001"
# One day less than each table's retention (30 and 14 days): the oldest rows are deleted while an export runs.
MAX_CAR_DAYS = 29
MAX_CLOUD_DAYS = 13
MAX_T = 30 * 86400
LONG_AGO = 10 * 366 * 86400
FIX_WINDOW_S = 600
EARTH_RADIUS_M = 6_371_008.8
MAX_DISTANCE_M = 20_100_000
MAX_COUNT = 10**16
MAX_LAST_AGE_S = 30 * 86400
APP_VERSION = re.compile(r"[0-9]{1,3}(?:\.[0-9]{1,3}){1,3}(?:-debug)?")
# The fixed strings backend/app/live_refresh.py and ingest_log.py write as a detail.
DETAIL_WORDS = ("session_expired", "received", "min_gap", "busy", "lock", "not_selected", "car_unlinked",
                "missing_vin", "exception", "upstream")
DETAIL = re.compile(r"[0-9]{1,5}|" + "|".join(DETAIL_WORDS))
# Keys the backend writes but the export does not carry: counted by name. Any other dropped key is "other".
# car_app_routes.HeartbeatReadings fields with no car-line key:
CAR_DROPPED = frozenset(("motorRpm", "motorRpmFront", "motorRpmRear", "slopeRaw", "acceleratorPct", "brakePct"))
# ingest_log.slim_snapshot / slim_mqtt_patch keys with no cloud-line key:
CLOUD_DROPPED = frozenset(("fuelPercent", "last50kmFuel", "placeKind", "lastStationaryPollAt", "openDriveStartAt"))
PARTITION = re.compile(r"car_telemetry_p[0-9]{8}")
# What a plan shows instead of a daily partition's name, which carries a real date.
PARTITION_NAME = "car_telemetry_partition"
TABLES = frozenset(("car_telemetry", "ingest_events", "cars", "car_app_devices"))
PLAN_RELATIONS = TABLES | {PARTITION_NAME}


# --- the line schema -----------------------------------------------------------------------------


class Num(NamedTuple):
    """A JSON number in [low, high]; ``integer`` refuses fractions, ``nullable`` allows null."""

    low: float
    high: float
    integer: bool = False
    nullable: bool = False


class Choice(NamedTuple):
    """One of a fixed set of strings."""

    values: frozenset[str]
    nullable: bool = False


class Flag(NamedTuple):
    """A JSON boolean (never a number)."""

    nullable: bool = False


class Text(NamedTuple):
    """A string matching a pattern, with no VIN, UUID or e-mail in it."""

    pattern: re.Pattern[str]
    nullable: bool = False


class Obj(NamedTuple):
    """A nested object with only these keys."""

    fields: Mapping[str, Any]
    required: frozenset[str] = frozenset()
    nullable: bool = False


class Names(NamedTuple):
    """A flat object of counts whose keys come from a fixed set (the statistics)."""

    allowed: frozenset[str]
    nullable: bool = False

    def accepts(self, key: str) -> bool:
        """Whether a key may appear."""
        return key in self.allowed


def _choice(*values: str, nullable: bool = False) -> Choice:
    return Choice(frozenset(values), nullable)


WHEELS = ("fl", "fr", "rl", "rr")
DOOR = _choice("open", "closed")
COUNT = Num(0, MAX_COUNT, integer=True)

CAR_COLUMNS = ("cadence", "speed_kph", "soc_pct", "twelve_volt", "odometer_km", "power_level", "gear")
CAR_DATA = {
    "alarm": "alarm", "alarmRaw": "alarm_raw", "twelveVoltLevel": "twelve_volt_level",
    "averageSpeedKph": "average_speed_kph", "outsideTempC": "outside_temp_c", "remainingKwh": "remaining_kwh",
    "lifetimeKwh": "lifetime_kwh", "lock": "lock", "chargingStateRaw": "charging_state_raw",
    "chargingGunRaw": "charging_gun_raw", "chargingPowerKw": "charging_power_kw",
    "chargingTypeRaw": "charging_type_raw", "chargerStateRaw": "charger_state_raw",
    "chargeRemainingMin": "charge_remaining_min", "chargedKwh": "charged_kwh", "motorPowerKw": "motor_power_kw",
}
CAR_NESTED = {
    "doors": ("doors", {key: key for key in ("fl", "fr", "rl", "rr", "hood", "trunk")}),
    "tyrePressureKpa": ("tyre_kpa", {wheel: wheel for wheel in WHEELS}),
    "tyreTempC": ("tyre_temp_c", {wheel: wheel for wheel in WHEELS}),
}
# Read from the location only to compute distances in the pod; never written.
LOCATION_PRIVATE = frozenset(("lat", "lng", "altitudeM", "headingDeg"))
LOCATION_FIELDS = {"accuracyM": "fix_accuracy_m", "ageS": "fix_age_s", "speedKph": "fix_speed_kph"}
CLOUD_FIELDS = {
    "odometer": "odometer_km", "currentSoc": "soc", "online": "online", "vehicleOn": "vehicle_on",
    "speed": "speed_kph", "chargePowerKw": "charge_power_kw", "minutesToFull": "minutes_to_full",
    "asleep": "asleep", "vehicleUnreachable": "vehicle_unreachable", "last50kmEnergy": "last50km_energy_kwh",
}
CLOUD_NESTED = {
    "energyDistribution": ("energy_distribution", {key: key for key in ("drive", "electric", "hvac", "other")}),
    "charging": ("charging", {
        "isCharging": "is_charging", "isConnected": "is_connected", "connectState": "connect_state",
        "soc": "soc", "minutesToFull": "minutes_to_full",
    }),
}
# Every name PairExport can count in stats.invalid_values: the keys it writes values under, each
# "<object>_<field>" of a nested object, and the whole values it refuses (data, payload, a location, ...).
INVALID_NAMES = frozenset((
    *CAR_COLUMNS, *CAR_DATA.values(), *LOCATION_FIELDS.values(), *CLOUD_FIELDS.values(),
    *(name for name, _ in (*CAR_NESTED.values(), *CLOUD_NESTED.values())),
    *(f"{name}_{field}" for name, fields in (*CAR_NESTED.values(), *CLOUD_NESTED.values())
      for field in fields.values()),
    "data", "payload", "has_fix", "gps_present", "gps_fresh", "hvac_on", "last_updated_t", "first_seen_t",
))

CAR_LINE: dict[str, Any] = {
    "type": _choice("car"), "t": Num(0, MAX_T),
    "cadence": _choice("moving", "stationary", "charging", "event"),
    "speed_kph": Num(0, 400), "soc_pct": Num(0, 100), "twelve_volt": Num(0, 20), "odometer_km": Num(0, 2_000_000),
    "power_level": _choice("off", "acc", "on", "ok"), "gear": _choice("P", "R", "N", "D", "M", "S"),
    "doors": Obj({key: DOOR for key in ("fl", "fr", "rl", "rr", "hood", "trunk")}),
    "lock": _choice("locked", "unlocked"), "alarm": _choice("off", "armed", "alarming"),
    "alarm_raw": Num(0, 255, integer=True), "twelve_volt_level": _choice("low", "normal"),
    "average_speed_kph": Num(0, 400), "outside_temp_c": Num(-60, 80),
    "remaining_kwh": Num(0, 300), "lifetime_kwh": Num(0, 10_000_000),
    "tyre_kpa": Obj({wheel: Num(0, 1000) for wheel in WHEELS}),
    "tyre_temp_c": Obj({wheel: Num(-40, 150) for wheel in WHEELS}),
    "charging_state_raw": Num(0, 255, integer=True), "charging_gun_raw": Num(0, 255, integer=True),
    "charging_power_kw": Num(0, 1000), "charging_type_raw": Num(0, 254, integer=True),
    "charger_state_raw": Num(0, 254, integer=True), "charge_remaining_min": Num(0, 6000, integer=True),
    "charged_kwh": Num(0, 300), "motor_power_kw": Num(-500, 1000),
    "has_fix": Flag(), "fix_accuracy_m": Num(0, 10_000), "fix_age_s": Num(0, 3600), "fix_speed_kph": Num(0, 400),
}
CLOUD_LINE: dict[str, Any] = {
    "type": _choice("cloud"), "t": Num(0, MAX_T),
    "source": _choice("poll", "mqtt"),
    "kind": _choice("ok", "error", "skip", "vehicleInfo", "smartCharge", "energyConsumption", "remoteControl", "other"),
    "detail": Text(DETAIL),
    "last_updated_t": Num(-LONG_AGO, MAX_T + 86400, nullable=True),
    "odometer_km": Num(0, 10_000_000, nullable=True), "soc": Num(0, 100, nullable=True),
    "online": Flag(True), "vehicle_on": Flag(True), "speed_kph": Num(0, 400, nullable=True),
    "charge_power_kw": Num(-1000, 1000, nullable=True), "minutes_to_full": Num(-1, 100_000, nullable=True),
    "asleep": Flag(True), "vehicle_unreachable": Flag(True),
    "last50km_energy_kwh": Num(-1000, 1000, nullable=True),
    "energy_distribution": Obj({key: Num(-1000, 1000, nullable=True) for key in ("drive", "electric", "hvac", "other")},
                               nullable=True),
    "charging": Obj({
        "is_charging": Flag(True), "is_connected": Flag(True), "connect_state": Num(-1, 255, True, True),
        "soc": Num(0, 100, nullable=True), "minutes_to_full": Num(-1, 100_000, nullable=True),
    }, nullable=True),
    "hvac_on": Flag(True),
    "gps_present": Flag(True), "gps_fresh": Flag(True), "gps_zero": Flag(),
    "gps_dist_m": Num(0, MAX_DISTANCE_M, integer=True), "gps_dt_s": Num(-FIX_WINDOW_S, FIX_WINDOW_S),
    "gps_car_accuracy_m": Num(0, 10_000, nullable=True), "gps_car_speed_kph": Num(0, 400, nullable=True),
    "gps_step_m": Num(0, MAX_DISTANCE_M, integer=True),
    "gps_last_dist_m": Num(0, MAX_DISTANCE_M, integer=True), "gps_last_age_s": Num(0, MAX_LAST_AGE_S),
}
# Nothing is measured from a cloud position of exactly (0, 0): it would put the car on a known circle.
ZERO_FORBIDDEN = ("gps_dist_m", "gps_dt_s", "gps_car_accuracy_m", "gps_car_speed_kph", "gps_step_m",
                  "gps_last_dist_m", "gps_last_age_s")
STATS_FIELDS: dict[str, Any] = {
    "car_rows": COUNT,
    "cloud_rows": Obj({key: COUNT for key in ep.ROW_KINDS}, required=frozenset(ep.ROW_KINDS)),
    "dropped_car_keys": Names(CAR_DROPPED | {"other"}), "dropped_cloud_keys": Names(CLOUD_DROPPED | {"other"}),
    "invalid_values": Names(INVALID_NAMES),
    "dropped_details": COUNT, "unparsable_last_updated": COUNT,
    "cloud_fixes_matched": COUNT, "cloud_fixes_unmatched": COUNT,
}
META_LINE: dict[str, Any] = {
    "type": _choice("meta"), "version": Num(VERSION, VERSION, integer=True), "start": Text(ep.START),
    "car_days": Num(1, MAX_CAR_DAYS, integer=True), "cloud_days": Num(1, MAX_CLOUD_DAYS, integer=True),
    "vin": _choice(SYNTHETIC_VIN), "model": Text(ep.MODEL), "energy": _choice("ev", "hybrid"),
    "device_source": _choice("car_app"), "app_version": Text(APP_VERSION),
    "first_seen_t": Num(-LONG_AGO, MAX_T + 86400),
    "stats": Obj(STATS_FIELDS, required=frozenset(STATS_FIELDS)),
}
LINES: dict[str, Obj] = {
    "car": Obj(CAR_LINE, frozenset(("type", "t"))),
    "cloud": Obj(CLOUD_LINE, frozenset(("type", "t", "source", "kind"))),
    "meta": Obj(META_LINE, frozenset(META_LINE) - {"app_version", "first_seen_t"}),
}
RULES = (
    "vin", "uuid", "email", "precise_number", "invalid_json", "invalid_line", "duplicate_key", "unexpected_type",
    "unexpected_key", "missing_key", "unexpected_null", "wrong_type", "out_of_range", "unexpected_string",
    "missing_meta", "unexpected_meta", "count_mismatch",
)


class _Reject(Exception):
    def __init__(self, rule: str) -> None:
        super().__init__(rule)
        self.rule = rule


class _Float(str):
    """A JSON number with a fraction or an exponent, kept as spelled so its decimals can be counted."""


def _pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result = dict(pairs)
    if len(result) != len(pairs):
        raise _Reject("duplicate_key")
    return result


def _constant(name: str) -> Any:
    raise _Reject("invalid_json")


def _leak_free(value: str) -> bool:
    return next(ep._patterns(value), None) is None


def _check(spec: Any, value: Any) -> str | None:
    """The rule a value breaks under its spec, or None."""
    if value is None:
        return None if spec.nullable else "unexpected_null"
    if isinstance(spec, Num):
        if isinstance(value, _Float):
            if spec.integer:
                return "wrong_type"
            # The spelling of a JSON number always parses, finite: NaN and Infinity are refused earlier.
            number = Decimal(value)
            if number.as_tuple().exponent < -3:
                return "precise_number"
        elif type(value) is int:
            number = Decimal(value)
        else:
            return "wrong_type"
        return None if spec.low <= number <= spec.high else "out_of_range"
    if isinstance(spec, Flag):
        return None if type(value) is bool else "wrong_type"
    if isinstance(spec, (Choice, Text)):
        if type(value) is not str:
            return "wrong_type"
        if isinstance(spec, Choice):
            return None if value in spec.values else "unexpected_string"
        if not spec.pattern.fullmatch(value) or not _leak_free(value):
            return "unexpected_string"
        if spec.pattern is ep.START:
            try:
                datetime.fromisoformat(value)
            except ValueError:
                return "unexpected_string"
        return None
    if type(value) is not dict:
        return "wrong_type"
    if isinstance(spec, Names):
        for key, count in value.items():
            if not spec.accepts(key) or not _leak_free(key):
                return "unexpected_key"
            rule = _check(COUNT, count)
            if rule is not None:
                return rule
        return None
    for key, item in value.items():
        if key not in spec.fields:
            return "unexpected_key"
        rule = _check(spec.fields[key], item)
        if rule is not None:
            return rule
    return None if spec.required <= value.keys() else "missing_key"


def check_line(line: str) -> tuple[str, dict[str, Any]]:
    """Check one output line against the strict schema; return its type and value.

    Raises ``_ExportError("<rule> on <type> line")``, which names no data, on the first
    problem: a VIN (other than the synthetic one), UUID or e-mail anywhere in the text, a
    number with more than three decimals anywhere (so no coordinate can pass), invalid or
    duplicate-keyed JSON, a key outside the line type's whitelist, a string outside its
    enum or pattern, a number outside its range, or a value of the wrong JSON type.
    """
    kind = "unknown"
    try:
        if not line.endswith("\n") or "\n" in line[:-1] or "\r" in line:
            raise _Reject("invalid_line")
        row: Any = None
        parse_rule = None
        try:
            row = json.loads(line, object_pairs_hook=_pairs, parse_float=_Float, parse_constant=_constant)
        except _Reject as exc:
            parse_rule = exc.rule
        except (ValueError, RecursionError):
            parse_rule = "invalid_json"
        if type(row) is dict and type(row.get("type")) is str and row["type"] in LINES:
            kind = row["type"]
        # The raw text is scanned whether or not it parsed: strings, keys and numbers alike.
        rule = next(ep._patterns(line.replace(f'"{SYNTHETIC_VIN}"', '""')), None)
        if rule is not None:
            raise _Reject(rule)
        if next(ep._precise_numbers(line), None) is not None:
            raise _Reject("precise_number")
        if parse_rule is not None:
            raise _Reject(parse_rule)
        if type(row) is not dict:
            raise _Reject("invalid_line")
        if kind == "unknown":
            raise _Reject("unexpected_type")
        rule = _check(LINES[kind], row)
        if rule is not None:
            raise _Reject(rule)
        if kind == "cloud" and row.get("gps_zero") is True and any(key in row for key in ZERO_FORBIDDEN):
            raise _Reject("unexpected_key")
        return kind, row
    except _Reject as exc:
        raise ep._ExportError(f"{exc.rule} on {kind} line") from None


# --- mapping rows to lines -----------------------------------------------------------------------

_INVALID = object()

PUSH_KINDS = frozenset(("vehicleInfo", "smartCharge", "energyConsumption", "remoteControl"))


def _convert(spec: Any, value: Any) -> Any:
    """The value as it may be written under ``spec``: floats rounded to 3 decimals, types kept; else _INVALID."""
    if value is None:
        return None if spec.nullable else _INVALID
    if isinstance(spec, Num):
        if type(value) is int:
            number: int | float = value
        elif type(value) is float and not spec.integer and math.isfinite(value):
            number = round(value, 3)
        else:
            return _INVALID
        return number if spec.low <= number <= spec.high else _INVALID
    if isinstance(spec, Choice):
        return value if type(value) is str and value in spec.values else _INVALID
    if isinstance(spec, Flag):
        return value if type(value) is bool else _INVALID
    return _INVALID


def _stat_name(name: Any, known: frozenset[str]) -> str:
    return name if name in known else "other"


def _json_object(value: Any) -> Any:
    # The JSON columns are selected as text, so a document the driver could not decode is counted here
    # instead of failing the query: invalid JSON, a number past int's digit limit (both ValueError, which
    # covers UnicodeDecodeError too) or nesting too deep (RecursionError).
    if isinstance(value, (str, bytes)):
        try:
            return json.loads(value)
        except (ValueError, RecursionError):
            return _INVALID
    return value


def _utc(stamp: datetime) -> datetime:
    return stamp.replace(tzinfo=UTC) if stamp.tzinfo is None else stamp.astimezone(UTC)


def _coordinates(place: Mapping[str, Any], lat_key: str, lng_key: str) -> tuple[float, float] | None:
    lat, lng = place.get(lat_key), place.get(lng_key)
    try:
        # A huge JSON integer cannot become a float: OverflowError.
        if not all(type(value) in (int, float) and math.isfinite(value) for value in (lat, lng)):
            return None
    except OverflowError:
        return None
    if abs(lat) > 90 or abs(lng) > 180:
        return None
    return float(lat), float(lng)


def haversine_m(first: tuple[float, float], second: tuple[float, float]) -> int:
    """Great-circle distance in whole metres."""
    lat1, lng1, lat2, lng2 = map(math.radians, (*first, *second))
    a = math.sin((lat2 - lat1) / 2) ** 2 + math.cos(lat1) * math.cos(lat2) * math.sin((lng2 - lng1) / 2) ** 2
    return int(round(2 * EARTH_RADIUS_M * math.asin(min(1.0, math.sqrt(a)))))


def _parse_stamp(value: Any) -> datetime | None:
    if type(value) is not str or len(value) > 64:
        return None
    try:
        stamp = datetime.fromisoformat(value)
        # "0001-01-01T00:00:00+01:00" overflows on the way to UTC.
        return stamp.astimezone(UTC) if stamp.tzinfo is not None else None
    except (ValueError, OverflowError):
        return None


def _bucket(source: Any, kind: Any) -> str:
    """The ROW_KINDS bucket of an ingest row, as export_prod counts it."""
    if source == "poll":
        return f"poll_{kind}" if kind in ("ok", "error", "skip") else "poll_other"
    if source == "mqtt":
        return f"mqtt_{kind}" if kind in PUSH_KINDS else "mqtt_other"
    return "other"


class CarMetadata(NamedTuple):
    """What the pod keeps about the one car; user_id and vin never leave the pod."""

    user_id: Any
    vin: str
    model: str
    energy: str
    device_source: str | None
    app_version: str | None
    first_seen_at: datetime | None


def car_metadata(row: Mapping[str, Any]) -> CarMetadata:
    """Reduce the cars / car_app_devices row to the values the export may use."""
    model = row.get("model_key")
    if not isinstance(model, str) or not ep.MODEL.fullmatch(model) or not _leak_free(model):
        model = "unknown"
    powertrain = row.get("powertrain")
    hybrid = row.get("is_hybrid") is True or (
        isinstance(powertrain, str) and powertrain.strip().lower() in ep.HYBRID_TYPES
    )
    source = row.get("device_source")
    version = row.get("app_version")
    first_seen = row.get("first_seen_at")
    return CarMetadata(
        user_id=row["user_id"], vin=row["vin"], model=model, energy="hybrid" if hybrid else "ev",
        device_source=source if source in ("car_app", "simulator") else None,
        app_version=version if isinstance(version, str) and APP_VERSION.fullmatch(version) else None,
        first_seen_at=_utc(first_seen) if isinstance(first_seen, datetime) else None,
    )


class PairExport:
    """Turn the car's telemetry rows, then its ingest rows, into checked lines on ``output``.

    Car rows must all come first: they leave only the list of fixes (time, position,
    accuracy, speed) in memory, which the cloud rows are measured against.
    """

    def __init__(
        self, output: TextIO, *, until: datetime, car_days: int, cloud_days: int, offset_s: int,
        car: CarMetadata,
    ) -> None:
        self.output = output
        self.until = until
        self.car_days, self.cloud_days = car_days, cloud_days
        self.car_start = until - timedelta(days=car_days)
        self.cloud_start = until - timedelta(days=cloud_days)
        self.shift = timedelta(seconds=offset_s)
        self.shifted_start = min(self.car_start, self.cloud_start) + self.shift
        self.car = car
        self.fixes: list[tuple[float, float, float, Any, Any]] = []
        self.fix_times: list[float] | None = None
        self.last_cloud_fix: dict[str, tuple[float, float]] = {}
        self.car_rows = 0
        self.cloud_rows: Counter[str] = Counter(dict.fromkeys(ep.ROW_KINDS, 0))
        self.dropped_car: Counter[str] = Counter()
        self.dropped_cloud: Counter[str] = Counter()
        self.invalid: Counter[str] = Counter()
        self.counts: Counter[str] = Counter()

    def _write(self, row: dict[str, Any]) -> None:
        line = ep.json_line(row)
        check_line(line)
        self.output.write(line)

    def _t(self, stamp: datetime) -> float:
        return round(((_utc(stamp) + self.shift) - self.shifted_start).total_seconds(), 3)

    def _count(self, counter: Counter[str], name: Any) -> None:
        counter[_stat_name(name, CAR_DROPPED if counter is self.dropped_car else CLOUD_DROPPED)] += 1

    def _put(self, line: dict[str, Any], name: str, spec: Any, value: Any, stat: str | None = None) -> Any:
        converted = _convert(spec, value)
        if converted is _INVALID:
            self.invalid[stat or name] += 1
        else:
            line[name] = converted
        return converted

    def _put_object(
        self, line: dict[str, Any], name: str, spec: Obj, names: Mapping[str, str], value: Any,
        dropped: Counter[str],
    ) -> None:
        if value is None and spec.nullable:
            line[name] = None
            return
        if type(value) is not dict:
            self.invalid[name] += 1
            return
        mapped: dict[str, Any] = {}
        for key, item in value.items():
            field = names.get(key)
            if field is None:
                self._count(dropped, key)
            else:
                self._put(mapped, field, spec.fields[field], item, f"{name}_{field}")
        if mapped or spec.nullable:
            line[name] = mapped

    # car_telemetry

    def add_car(self, row: Mapping[str, Any]) -> None:
        """Write the line for one car_telemetry row and remember its fix."""
        at = _utc(row["at"])
        if not self.car_start <= at < self.until:
            raise ep._ExportError("row outside the requested window")
        line: dict[str, Any] = {"type": "car", "t": self._t(at)}
        speed = None
        for column in CAR_COLUMNS:
            if row.get(column) is not None:
                converted = self._put(line, column, CAR_LINE[column], row[column])
                if column == "speed_kph" and converted is not _INVALID:
                    speed = converted
        data = _json_object(row.get("data"))
        if data is not None and type(data) is not dict:
            self.invalid["data"] += 1
        elif data:
            for key, value in data.items():
                if key in CAR_DATA:
                    self._put(line, CAR_DATA[key], CAR_LINE[CAR_DATA[key]], value)
                elif key in CAR_NESTED:
                    name, names = CAR_NESTED[key]
                    self._put_object(line, name, CAR_LINE[name], names, value, self.dropped_car)
                elif key == "location":
                    self._location(line, value, at, speed)
                else:
                    self._count(self.dropped_car, key)
        self._write(line)
        self.car_rows += 1

    def _location(self, line: dict[str, Any], location: Any, at: datetime, speed: Any) -> None:
        if type(location) is not dict:
            line["has_fix"] = False
            self.invalid["has_fix"] += 1
            return
        place = _coordinates(location, "lat", "lng")
        if place == (0.0, 0.0):
            place = None  # never a fix to measure from: it would place the cloud position on a circle
        if place is None:
            self.invalid["has_fix"] += 1
        line["has_fix"] = place is not None
        values = {}
        for key, value in location.items():
            if key in LOCATION_FIELDS:
                name = LOCATION_FIELDS[key]
                values[name] = self._put(line, name, CAR_LINE[name], value)
            elif key not in LOCATION_PRIVATE:
                self._count(self.dropped_car, key)
        age = values.get("fix_age_s", 0)
        accuracy = values.get("fix_accuracy_m")
        if place is None or age is _INVALID:
            return
        self.fixes.append((at.timestamp() - age, *place, None if accuracy is _INVALID else accuracy, speed))

    # ingest_events

    def add_cloud(self, row: Mapping[str, Any]) -> None:
        """Write the line for one ingest_events row of the car (poll, push, error or skip)."""
        received = _utc(row["received_at"])
        if not self.cloud_start <= received < self.until:
            raise ep._ExportError("row outside the requested window")
        source, kind = row.get("source"), row.get("kind")
        bucket = _bucket(source, kind)
        self.cloud_rows[bucket] += 1
        if bucket == "other":
            return
        line: dict[str, Any] = {
            "type": "cloud", "t": self._t(received), "source": source,
            "kind": kind if bucket not in ("poll_other", "mqtt_other") else "other",
        }
        detail = row.get("detail")
        if detail is not None:
            if type(detail) is str and DETAIL.fullmatch(detail):
                line["detail"] = detail
            else:
                self.counts["dropped_details"] += 1
        if row.get("detail_dropped"):
            self.counts["dropped_details"] += 1
        payload = row.get("payload")
        if payload is not None:
            self._payload(line, _json_object(payload), source, received)
        self._write(line)

    def _payload(self, line: dict[str, Any], payload: Any, source: str, received: datetime) -> None:
        if type(payload) is not dict:
            self.invalid["payload"] += 1
            return
        for key, value in payload.items():
            if key in CLOUD_FIELDS:
                self._put(line, CLOUD_FIELDS[key], CLOUD_LINE[CLOUD_FIELDS[key]], value)
            elif key in CLOUD_NESTED:
                name, names = CLOUD_NESTED[key]
                self._put_object(line, name, CLOUD_LINE[name], names, value, self.dropped_cloud)
            elif key == "lastUpdated":
                self._last_updated(line, value)
            elif key == "climate":
                self._climate(line, value)
            elif key == "gpsFresh":
                self._put(line, "gps_fresh", CLOUD_LINE["gps_fresh"], value)
            elif key == "gps" or (key == "event" and source == "mqtt"):
                pass  # gps below; a push's event repeats its kind
            else:
                self._count(self.dropped_cloud, key)
        if "gps" in payload:
            self._cloud_fix(line, payload["gps"], source, received)

    def _last_updated(self, line: dict[str, Any], value: Any) -> None:
        if value is None:
            line["last_updated_t"] = None
            return
        stamp = _parse_stamp(value)
        try:
            t = None if stamp is None else self._t(stamp)
        except OverflowError:
            t = None
        if t is None:
            self.counts["unparsable_last_updated"] += 1
        else:
            self._put(line, "last_updated_t", CLOUD_LINE["last_updated_t"], t)

    def _climate(self, line: dict[str, Any], value: Any) -> None:
        if value is None:
            line["hvac_on"] = None
        elif type(value) is not dict:
            self.invalid["hvac_on"] += 1
        else:
            for key, item in value.items():
                if key == "hvacOn":
                    self._put(line, "hvac_on", CLOUD_LINE["hvac_on"], item)
                else:
                    self._count(self.dropped_cloud, key)

    def _cloud_fix(self, line: dict[str, Any], gps: Any, source: str, received: datetime) -> None:
        if gps is None:
            line["gps_present"] = None
            return
        place = _coordinates(gps, "latitude", "longitude") if type(gps) is dict else None
        line["gps_present"] = place is not None
        if place is None:
            self.invalid["gps_present"] += 1
            return
        line["gps_zero"] = place == (0.0, 0.0)
        if line["gps_zero"]:
            # A distance from (0, 0) would place the car on a circle: never measured.
            return
        previous = self.last_cloud_fix.get(source)
        if previous is not None:
            line["gps_step_m"] = haversine_m(previous, place)
        self.last_cloud_fix[source] = place
        when = received.timestamp()
        last = self.last_fix(when)
        if last is not None:
            line["gps_last_dist_m"] = haversine_m(last[1:3], place)
            line["gps_last_age_s"] = round(when - last[0], 1)
        match = self.nearest_fix(when)
        if match is None:
            self.counts["cloud_fixes_unmatched"] += 1
            return
        self.counts["cloud_fixes_matched"] += 1
        fix_time, lat, lng, accuracy, speed = match
        line["gps_dist_m"] = haversine_m((lat, lng), place)
        line["gps_dt_s"] = round(when - fix_time, 1)
        line["gps_car_accuracy_m"] = accuracy
        line["gps_car_speed_kph"] = speed

    def _sorted_fix_times(self) -> list[float]:
        if self.fix_times is None:
            self.fixes.sort(key=lambda fix: fix[0])
            self.fix_times = [fix[0] for fix in self.fixes]
        return self.fix_times

    def last_fix(self, when: float) -> tuple[float, float, float, Any, Any] | None:
        """The latest car fix taken at or before ``when``, however old (a parked car has none newer)."""
        index = bisect.bisect_right(self._sorted_fix_times(), when) - 1
        return self.fixes[index] if index >= 0 else None

    def nearest_fix(self, when: float) -> tuple[float, float, float, Any, Any] | None:
        """The car fix nearest in time (the earlier one on a tie), if within FIX_WINDOW_S.

        Starts from bisect_right like last_fix, so that among fixes taken at exactly ``when`` both
        pick the same (last) one.
        """
        index = bisect.bisect_right(self._sorted_fix_times(), when)
        candidates = [i for i in (index - 1, index) if 0 <= i < len(self.fixes)]
        if not candidates:
            return None
        best = min(candidates, key=lambda i: (abs(self.fix_times[i] - when), i))
        return self.fixes[best] if abs(self.fix_times[best] - when) <= FIX_WINDOW_S else None

    def finish(self) -> None:
        """Write the meta line, the last line of the pod stream."""
        start = self.shifted_start.isoformat(timespec="seconds").replace("+00:00", "Z")
        meta: dict[str, Any] = {
            "type": "meta", "version": VERSION, "start": start,
            "car_days": self.car_days, "cloud_days": self.cloud_days, "vin": SYNTHETIC_VIN,
            "model": self.car.model, "energy": self.car.energy, "device_source": self.car.device_source,
        }
        if self.car.app_version is not None:
            meta["app_version"] = self.car.app_version
        if self.car.first_seen_at is not None:
            try:
                self._put(meta, "first_seen_t", META_LINE["first_seen_t"], self._t(self.car.first_seen_at))
            except OverflowError:
                self.invalid["first_seen_t"] += 1
        meta["stats"] = {
            "car_rows": self.car_rows,
            "cloud_rows": {key: self.cloud_rows[key] for key in ep.ROW_KINDS},
            "dropped_car_keys": dict(sorted(self.dropped_car.items())),
            "dropped_cloud_keys": dict(sorted(self.dropped_cloud.items())),
            "invalid_values": dict(sorted(self.invalid.items())),
            **{key: self.counts[key] for key in (
                "dropped_details", "unparsable_last_updated", "cloud_fixes_matched", "cloud_fixes_unmatched",
            )},
        }
        self._write(meta)


# --- the queries ---------------------------------------------------------------------------------

READ_SETTINGS = (ep.READ_ONLY, ep.TIMEOUT, ep.IDLE_TIMEOUT, ep.LOCK_TIMEOUT)
CAR_WINDOW = "at >= :lower AND at < :upper"
CLOUD_WINDOW = "user_id = :user AND vin = :vin AND received_at >= :lower AND received_at < :upper"
SAFE_DETAIL = "(detail ~ '^[0-9]{1,5}$' OR detail IN (" + ", ".join(f"'{word}'" for word in DETAIL_WORDS) + "))"


# At most two distinct car ids per day: one is the car, a second is enough to stop.
IDENTIFY_LIMIT = 2
IDENTIFY_SQL = f"""/* identify */
        SELECT DISTINCT car_id FROM car_telemetry
        WHERE {CAR_WINDOW}
        ORDER BY car_id LIMIT :limit"""


METADATA_SQL = """/* metadata */
        SELECT cars.user_id, cars.vin,
               CASE WHEN cars.model_key_source IN ('vin', 'name') THEN cars.model_key END AS model_key,
               cars.is_hybrid, cars.powertrain, cars.deleted_at IS NOT NULL AS deleted,
               devices.source AS device_source, devices.app_version, devices.first_seen_at
        FROM cars LEFT JOIN car_app_devices AS devices ON devices.car_id = cars.id
        WHERE cars.id = :car
        LIMIT 2"""


def car_rows_sql(resume: bool) -> str:
    """One page of the car's telemetry in one day (one partition), by its primary key."""
    cursor = "AND at > :after" if resume else ""
    return f"""/* car_rows */
        SELECT at, cadence, speed_kph, soc_pct, twelve_volt, odometer_km, power_level, gear,
               CAST(data AS text) AS data
        FROM car_telemetry
        WHERE car_id = :car AND {CAR_WINDOW} {cursor}
        ORDER BY at LIMIT :limit"""


CAR_COUNT_SQL = f"""/* car_count */
        SELECT COUNT(*) AS row_count FROM car_telemetry
        WHERE car_id = :car AND {CAR_WINDOW}"""


def cloud_rows_sql(resume: bool) -> str:
    """One page of the car's ingest rows in one day; free-text details and error payloads stay in SQL."""
    cursor = "AND (received_at, id) > (:after_first, :after_second)" if resume else ""
    return f"""/* cloud_rows */
        SELECT received_at, id, source, kind,
               CASE WHEN {SAFE_DETAIL} THEN detail END AS detail,
               (detail IS NOT NULL AND NOT {SAFE_DETAIL}) AS detail_dropped,
               CASE WHEN (source = 'poll' AND kind = 'ok') OR source = 'mqtt' THEN CAST(payload AS text) END
                   AS payload
        FROM ingest_events
        WHERE {CLOUD_WINDOW} {cursor}
        ORDER BY received_at, id LIMIT :limit"""


CLOUD_COUNT_SQL = f"""/* cloud_count */
        SELECT COUNT(*) AS row_count {ep.COUNTS}
        FROM ingest_events
        WHERE {CLOUD_WINDOW}"""
PLAN_SHAPES = ("identify", "metadata", "car_rows", "car_rows_resumed", "car_count", "cloud_rows",
               "cloud_rows_resumed", "cloud_count")
# ingest_events holds every customer's events, so this car's rows must come through its composite index.
INGEST_INDEX = "ix_ingest_events_user_vin_received"
INGEST_SCANS = frozenset(("Index Scan", "Index Only Scan", "Bitmap Heap Scan"))
INGEST_SHAPES = ("cloud_rows", "cloud_rows_resumed", "cloud_count")


def ingest_plan_ok(nodes: Iterable[Mapping[str, Any]]) -> bool:
    """Whether a plan of an ingest_events statement reads only this car's rows, through INGEST_INDEX.

    Fails closed: every relation is ingest_events (or none); every node on it is an index or bitmap
    heap scan; every index it names, including a Bitmap Index Scan's, is INGEST_INDEX. A sequential
    scan, another index (such as ix_ingest_events_received_at) or a relation it does not recognise
    (a child or partition) refuses.
    """
    for node in nodes:
        relation, kind, index = node["relation"], node["node"], node["index"]
        if relation not in (None, "ingest_events"):
            return False
        if relation == "ingest_events" and kind not in INGEST_SCANS:
            return False
        if kind in ("Index Scan", "Index Only Scan", "Bitmap Index Scan") and index != INGEST_INDEX:
            return False
    return True


def day_windows(start: datetime, until: datetime) -> Iterator[tuple[datetime, datetime]]:
    """[start, until) cut at UTC midnights, so each window falls in one daily partition."""
    lower = start
    while lower < until:
        midnight = datetime.combine(lower.astimezone(UTC).date() + timedelta(days=1), time(), tzinfo=UTC)
        upper = min(midnight, until)
        yield lower, upper
        lower = upper


class PairQueries:
    """Bounded reads in short read-only transactions, through export_prod's query runner."""

    def __init__(self, sessions: Any, statement: Callable[[str], Any], page_rows: int, pause_ms: int) -> None:
        self.runner = ep.ExportQueries(sessions, statement, page_rows, pause_ms)
        self.page_rows = page_rows

    async def _query(self, sql: str, params: Mapping[str, Any]) -> list[Mapping[str, Any]]:
        return await self.runner._query(sql, params)

    async def _plan(self, sql: str, params: Mapping[str, Any]) -> list[dict[str, Any]]:
        result = await self._query("EXPLAIN (FORMAT JSON) " + sql, params)
        return ep._plan_nodes(result[0]["QUERY PLAN"])

    async def _gated(self, sql: str, params: Mapping[str, Any]) -> list[Mapping[str, Any]]:
        """Run an ingest_events statement only after its own plan, with these same parameters, passes."""
        if not ingest_plan_ok(await self._plan(sql, params)):
            raise ep._ExportError(INDEX_GATE)
        return await self._query(sql, params)

    async def _paged(
        self, windows: Iterable[tuple[datetime, datetime]], build: Callable[[bool], str],
        params: Mapping[str, Any], cursor: Callable[[Mapping[str, Any]], dict[str, Any]], *, gated: bool = False,
    ) -> AsyncIterator[Mapping[str, Any]]:
        run = self._gated if gated else self._query
        for lower, upper in windows:
            after: dict[str, Any] | None = None
            while True:
                page = await run(build(after is not None),
                                 {**params, "lower": lower, "upper": upper, "limit": self.page_rows, **(after or {})})
                for row in page:
                    yield row
                if len(page) < self.page_rows:
                    break
                after = cursor(page[-1])
                del page

    async def car_id(self, start: datetime, until: datetime) -> Any:
        """The one car id with telemetry in the window. Kept in the pod, never written.

        Stops at the first sign of a second car, before reading another page or day, and looks
        nothing up about it.
        """
        found = None
        for lower, upper in day_windows(start, until):
            rows = await self._query(IDENTIFY_SQL, {"lower": lower, "upper": upper, "limit": IDENTIFY_LIMIT})
            if len(rows) > 1 or (rows and found is not None and rows[0]["car_id"] != found):
                raise ep._ExportError(SEVERAL_CARS)
            if rows:
                found = rows[0]["car_id"]
        if found is None:
            raise ep._ExportError(NO_CAR)
        return found

    async def metadata(self, car: Any) -> Mapping[str, Any]:
        """The car's cars row joined with its device row."""
        rows = await self._query(METADATA_SQL, {"car": car})
        if not rows:
            raise ep._ExportError(NO_CAR_ROW)
        if len(rows) > 1:
            raise ep._ExportError(DUPLICATE_CAR_ROW)
        if rows[0]["deleted"]:
            raise ep._ExportError(DELETED_CAR)
        if not isinstance(rows[0]["vin"], str) or not rows[0]["vin"].strip():
            raise ep._ExportError(NO_VIN)
        return rows[0]

    def car_rows(self, car: Any, start: datetime, until: datetime) -> AsyncIterator[Mapping[str, Any]]:
        """The car's telemetry rows, oldest first, a page at a time."""
        return self._paged(day_windows(start, until), car_rows_sql, {"car": car}, lambda row: {"after": row["at"]})

    def cloud_rows(self, user: Any, vin: str, start: datetime, until: datetime) -> AsyncIterator[Mapping[str, Any]]:
        """The car's ingest rows, oldest first, a page at a time."""
        return self._paged(day_windows(start, until), cloud_rows_sql, {"user": user, "vin": vin},
                           lambda row: {"after_first": row["received_at"], "after_second": row["id"]}, gated=True)

    async def car_counts(self, car: Any, start: datetime, until: datetime) -> tuple[int, int]:
        """Telemetry rows of the car in the window, and the number of UTC days that have any."""
        total = days = 0
        for lower, upper in day_windows(start, until):
            rows = await self._query(CAR_COUNT_SQL, {"car": car, "lower": lower, "upper": upper})
            count = int(rows[0]["row_count"])
            total += count
            days += count > 0
        return total, days

    async def cloud_counts(self, user: Any, vin: str, start: datetime, until: datetime) -> tuple[dict[str, int], int]:
        """Ingest rows of the car by ROW_KINDS bucket, and the number of UTC days that have any."""
        totals: Counter[str] = Counter(dict.fromkeys(ep.ROW_KINDS, 0))
        days = 0
        for lower, upper in day_windows(start, until):
            rows = await self._gated(CLOUD_COUNT_SQL, {"user": user, "vin": vin, "lower": lower, "upper": upper})
            totals.update({key: int(rows[0][key]) for key in ep.ROW_KINDS})
            days += int(rows[0]["row_count"]) > 0
        return {key: totals[key] for key in ep.ROW_KINDS}, days

    async def explain(
        self, car: Any, user: Any, vin: str, car_window: tuple[datetime, datetime],
        cloud_window: tuple[datetime, datetime],
    ) -> dict[str, list[dict[str, Any]]]:
        """Plan nodes (type, relation, index, estimated rows) of first and resumed pages. No ANALYZE.

        A daily partition's name carries a real date, so it is replaced by PARTITION_NAME.
        """
        car_params = {"lower": car_window[0], "upper": car_window[1]}
        cloud_params = {"user": user, "vin": vin, "lower": cloud_window[0], "upper": cloud_window[1]}
        statements = {
            "identify": (IDENTIFY_SQL, {**car_params, "limit": IDENTIFY_LIMIT}),
            "metadata": (METADATA_SQL, {"car": car}),
            "car_rows": (car_rows_sql(False), {"car": car, **car_params, "limit": self.page_rows}),
            "car_rows_resumed": (car_rows_sql(True), {"car": car, **car_params, "limit": self.page_rows,
                                                      "after": car_window[0]}),
            "car_count": (CAR_COUNT_SQL, {"car": car, **car_params}),
            "cloud_rows": (cloud_rows_sql(False), {**cloud_params, "limit": self.page_rows}),
            "cloud_rows_resumed": (cloud_rows_sql(True), {**cloud_params, "limit": self.page_rows,
                                                          "after_first": cloud_window[0],
                                                          "after_second": uuid.UUID(int=0)}),
            "cloud_count": (CLOUD_COUNT_SQL, cloud_params),
        }
        plans = {}
        for shape in PLAN_SHAPES:
            nodes = await self._plan(*statements[shape])
            for node in nodes:
                if isinstance(node["relation"], str) and PARTITION.fullmatch(node["relation"]):
                    node["relation"] = PARTITION_NAME
                # A partition's own indexes are named after it (car_telemetry_p20260228_pkey).
                if isinstance(node["index"], str):
                    node["index"] = PARTITION.sub(PARTITION_NAME, node["index"])
            plans[shape] = nodes
        return plans


def ingest_plans_ok(plans: Mapping[str, list[dict[str, Any]]]) -> bool:
    """Whether the printed plans of every ingest_events statement pass ingest_plan_ok."""
    return all(ingest_plan_ok(plans[shape]) for shape in INGEST_SHAPES)


# --- data-free failures and checked summaries ------------------------------------------------------

NO_CAR = "no car has car-app telemetry in the window"
NO_CAR_ROW = "the car with telemetry has no cars row"
DUPLICATE_CAR_ROW = "the car with telemetry matches more than one cars row"
DELETED_CAR = "the car with telemetry is deleted"
NO_VIN = "the car with telemetry has no VIN"
NO_DEVICE = "the car with telemetry has no car_app device row"
SIMULATOR = "the car with telemetry is a simulator"
SEVERAL_CARS = "more than one car has car-app telemetry in the window; ask the owner which car to export"
INDEX_GATE = "refusing to export: a query plan does not read ingest_events through this car's index"
OUTSIDE = "row outside the requested window"
INVALID_COUNTS = "invalid dry-run counts"
INVALID_PLANS = "invalid export plans"
INVALID_NODE = "invalid export plan node"
SAFE_MESSAGES = frozenset(
    message.encode() for message in (
        NO_CAR, NO_CAR_ROW, DUPLICATE_CAR_ROW, DELETED_CAR, NO_VIN, NO_DEVICE, SIMULATOR, SEVERAL_CARS, INDEX_GATE,
        OUTSIDE, INVALID_COUNTS, INVALID_PLANS, INVALID_NODE,
    )
)


_SAFE_RULE = re.compile(rb"(?:" + b"|".join(rule.encode() for rule in RULES) + rb") on (?:car|cloud|meta|unknown) line")
_SAFE_FAILURE = re.compile(rb"mapping export failed: ([A-Za-z_][A-Za-z_0-9]{0,79})")


def _pod_error(stderr: bytes, status: int) -> ep._ExportError:
    """Repeat a pod failure only when it is one of this exporter's own data-free messages."""
    for line in stderr[:4096].splitlines():
        line = line.rstrip(b"\r")
        failure = _SAFE_FAILURE.fullmatch(line)
        if failure is not None and not _leak_free(failure.group(1).decode("ascii")):
            continue  # a class name shaped like a VIN is not repeated
        if line in SAFE_MESSAGES or _SAFE_RULE.fullmatch(line) or failure is not None:
            return ep._ExportError(line.decode("ascii"))
    return ep._ExportError(f"kubectl export failed (exit {status})")


def _is_count(value: Any) -> bool:
    return type(value) is int and 0 <= value < MAX_COUNT


def checked_plans(result: Any) -> dict[str, list[dict[str, Any]]]:
    """Accept only node type, relation (the four tables or PARTITION_NAME), index and row estimate.

    A dated partition name anywhere in a relation or index is refused, even if the pod did not rewrite it.
    """
    if not isinstance(result, dict) or set(result) != set(PLAN_SHAPES):
        raise ep._ExportError(INVALID_PLANS)
    for nodes in result.values():
        if not isinstance(nodes, list) or not nodes:
            raise ep._ExportError(INVALID_PLANS)
        for node in nodes:
            if not isinstance(node, dict) or set(node) != {"node", "relation", "index", "rows"}:
                raise ep._ExportError(INVALID_NODE)
            name, relation, index = node["node"], node["relation"], node["index"]
            if (
                not isinstance(name, str) or not ep._PLAN_NODE.fullmatch(name) or not _leak_free(name)
                or not (relation is None or relation in PLAN_RELATIONS)
                or (index is not None and (not isinstance(index, str) or not ep._PLAN_INDEX.fullmatch(index)
                                           or not _leak_free(index) or PARTITION.search(index)))
                or not _is_count(node["rows"])
            ):
                raise ep._ExportError(INVALID_NODE)
    return result


DRY_RUN_KEYS = frozenset((
    "cars", "device_sources", "car_rows", "car_days_with_rows", "cloud_rows", "cloud_days_with_rows", "plans",
    "ingest_events_plan_ok",
))
DEVICE_SOURCES = frozenset(("car_app", "simulator", "other", "none"))


def checked_dry_run(result: Any) -> dict[str, Any]:
    """Accept only the dry run's counts and checked plans."""
    if (
        not isinstance(result, dict) or set(result) != DRY_RUN_KEYS
        or not isinstance(result["device_sources"], dict) or set(result["device_sources"]) != DEVICE_SOURCES
        or not isinstance(result["cloud_rows"], dict) or set(result["cloud_rows"]) != ep.ROW_KINDS.keys()
        or not all(_is_count(value) for value in (
            result["cars"], result["car_rows"], result["car_days_with_rows"], result["cloud_days_with_rows"],
            *result["device_sources"].values(), *result["cloud_rows"].values(),
        ))
        or type(result["ingest_events_plan_ok"]) is not bool
    ):
        raise ep._ExportError(INVALID_COUNTS)
    checked_plans(result["plans"])
    return result


# --- the pod -------------------------------------------------------------------------------------


async def run_export(
    args: argparse.Namespace, output: TextIO, sessions: Any, statement: Callable[[str], Any], *,
    offset_s: int | None = None, now: datetime | None = None,
) -> None:
    """Identify the one car, then write the plans, the dry-run counts or the export to ``output``."""
    until = args.until if args.until is not None else ep._default_until(now or datetime.now(UTC))
    until = _utc(until).replace(microsecond=0)
    car_start = until - timedelta(days=args.car_days)
    cloud_start = until - timedelta(days=args.cloud_days)
    queries = PairQueries(sessions, statement, args.page_rows, args.pause_ms)

    car_id = await queries.car_id(car_start, until)
    car = car_metadata(await queries.metadata(car_id))
    windows = (next(day_windows(car_start, until)), next(day_windows(cloud_start, until)))

    if args.explain or args.dry_run:
        plans = checked_plans(await queries.explain(car_id, car.user_id, car.vin, *windows))
        if args.explain:
            output.write(ep.json_line(plans))
            return
        # The counts read ingest_events too: not after a plan that fails the gate (--explain shows the plans).
        # Each count statement is also gated on its own plan right before it runs.
        if not ingest_plans_ok(plans):
            raise ep._ExportError(INDEX_GATE)
        car_rows, car_days = await queries.car_counts(car_id, car_start, until)
        cloud_rows, cloud_days = await queries.cloud_counts(car.user_id, car.vin, cloud_start, until)
        sources = dict.fromkeys(sorted(DEVICE_SOURCES), 0)
        sources[car.device_source or "none"] = 1
        output.write(ep.json_line(checked_dry_run({
            "cars": 1, "device_sources": sources, "car_rows": car_rows, "car_days_with_rows": car_days,
            "cloud_rows": cloud_rows, "cloud_days_with_rows": cloud_days, "plans": plans,
            "ingest_events_plan_ok": ingest_plans_ok(plans),
        })))
        return

    # Only the car's own app is ground truth.
    if car.device_source == "simulator":
        raise ep._ExportError(SIMULATOR)
    if car.device_source != "car_app":
        raise ep._ExportError(NO_DEVICE)
    # Every ingest_events statement is gated on its own plan right before it runs (PairQueries._gated).
    if offset_s is None:
        offset_s = secrets.randbelow(2 * ep.HALF_OFFSET + 1) - ep.HALF_OFFSET
    export = PairExport(output, until=until, car_days=args.car_days, cloud_days=args.cloud_days,
                        offset_s=offset_s, car=car)
    async with aclosing(queries.car_rows(car_id, car_start, until)) as rows:
        async for row in rows:
            export.add_car(row)
    async with aclosing(queries.cloud_rows(car.user_id, car.vin, cloud_start, until)) as rows:
        async for row in rows:
            export.add_cloud(row)
    export.finish()


async def _run_pod(args: argparse.Namespace, output: TextIO) -> None:
    from sqlalchemy import text
    from sqlalchemy.ext.asyncio import async_sessionmaker

    from app.config import get_settings
    from app.db import create_engine

    engine = create_engine(get_settings())
    try:
        await run_export(args, output, async_sessionmaker(engine), text)
    finally:
        await engine.dispose()


def _pod_with_deadline(args: argparse.Namespace) -> None:
    reconfigure = getattr(sys.stdout, "reconfigure", None)
    if reconfigure is not None:
        reconfigure(newline="\n")  # the line check refuses "\r", whatever the platform
    alarm = getattr(signal, "alarm", None)
    if alarm is not None:
        previous_handler = signal.signal(signal.SIGALRM, signal.SIG_DFL)
        alarm(args.max_minutes * 60)
    try:
        asyncio.run(_run_pod(args, sys.stdout))
        sys.stdout.flush()
    finally:
        if alarm is not None:
            alarm(0)
            signal.signal(signal.SIGALRM, previous_handler)


# --- the local side --------------------------------------------------------------------------------

POD_MODULE = "bench.mapping.export_pairs"
PRELUDE = '''\
# Composed by bench.mapping.export_pairs.build_pod_script: export_prod.py and export_pairs.py
# as modules of one stdin script, because the API pod has no openbyd checkout.
import sys
import types


def _package(name):
    module = types.ModuleType(name)
    module.__path__ = []
    sys.modules[name] = module
    parent, _, child = name.rpartition(".")
    if parent:
        setattr(sys.modules[parent], child, module)


def _load(name, source):
    module = types.ModuleType(name)
    module.__file__ = "<" + name + ">"
    module.__package__ = name.rpartition(".")[0]
    sys.modules[name] = module
    parent, _, child = name.rpartition(".")
    setattr(sys.modules[parent], child, module)
    exec(compile(source, module.__file__, "exec"), module.__dict__)


_package("bench")
_package("bench.replay")
_package("bench.mapping")
'''


def build_pod_script() -> bytes:
    """The single stdin script for the pod: a prelude, both module sources, then ``main()``.

    Deterministic: the same two source files always give the same bytes.
    """
    parts = [PRELUDE]
    for name, path in (("bench.replay.export_prod", Path(ep.__file__)), (POD_MODULE, Path(__file__))):
        source = path.read_bytes().decode("utf-8")
        digest = hashlib.sha256(source.encode("utf-8")).hexdigest()
        # ascii(): the script is pure ASCII whatever encoding the pod reads stdin with.
        parts.append(f"# {name} sha256 {digest}\n_load({name!r}, {ascii(source)})\n")
    parts.append(f"sys.modules[{POD_MODULE!r}].main()\n")
    return "".join(parts).encode("utf-8")


def _single_json(stdout: bytes) -> Any:
    try:
        text = stdout.decode("ascii")
    except UnicodeDecodeError:
        raise ep._ExportError(INVALID_COUNTS) from None
    if not text.endswith("\n") or "\n" in text[:-1] or next(ep._patterns(text), None) is not None:
        raise ep._ExportError(INVALID_COUNTS)
    try:
        return json.loads(text, object_pairs_hook=ep._unique_counts, parse_constant=_constant)
    except (ValueError, RecursionError, _Reject):
        raise ep._ExportError(INVALID_COUNTS) from None


def fold(stream: Path, destination: Path) -> None:
    """Copy the pod stream with its last (meta) line moved to the top."""
    try:
        with stream.open(encoding="utf-8", newline="") as source:
            total, trailer = 0, ""
            for total, trailer in enumerate(source, 1):
                pass
            if not total or check_line(trailer)[0] != "meta":
                raise ep._ExportError("missing_meta on meta line")
            source.seek(0)
            with destination.open("w", encoding="utf-8", newline="") as output:
                output.write(trailer)
                for _ in range(total - 1):
                    output.write(source.readline())
    except UnicodeError:
        raise ep._ExportError("invalid_json on unknown line") from None


def _window_end(meta: Mapping[str, Any]) -> int:
    return max(meta["car_days"], meta["cloud_days"]) * 86400


def _window_start(meta: Mapping[str, Any], kind: str) -> int:
    days = meta["car_days"] if kind == "car" else meta["cloud_days"]
    return _window_end(meta) - days * 86400


def verify(path: Path) -> tuple[dict[str, Any], Counter[str]]:
    """Check every line of a folded file; the meta line first and only there, its counts matching."""
    lines: Counter[str] = Counter()
    lows: dict[str, Decimal] = {}
    meta: dict[str, Any] | None = None
    try:
        with path.open(encoding="utf-8", newline="") as file:
            for number, line in enumerate(file, 1):
                kind, row = check_line(line)
                if number == 1:
                    if kind != "meta":
                        raise ep._ExportError("missing_meta on meta line")
                    meta = row
                elif kind == "meta":
                    raise ep._ExportError("unexpected_meta on meta line")
                else:
                    lines[kind] += 1
                    # Each stream's own window, from the meta line (which comes first).
                    t = Decimal(row["t"])
                    if t > _window_end(meta):
                        raise ep._ExportError(f"out_of_range on {kind} line")
                    lows[kind] = min(lows.get(kind, t), t)
    except UnicodeError:
        raise ep._ExportError("invalid_json on unknown line") from None
    if meta is None:
        raise ep._ExportError("missing_meta on meta line")
    for kind, low in lows.items():
        if low < _window_start(meta, kind):
            raise ep._ExportError(f"out_of_range on {kind} line")
    stats = meta["stats"]
    cloud = sum(count for key, count in stats["cloud_rows"].items() if key != "other")
    if lines["car"] != stats["car_rows"] or lines["cloud"] != cloud:
        raise ep._ExportError("count_mismatch on meta line")
    return meta, lines


def _pod_options(args: argparse.Namespace) -> list[str]:
    options = ["--car-days", str(args.car_days), "--cloud-days", str(args.cloud_days),
               "--page-rows", str(args.page_rows), "--pause-ms", str(args.pause_ms),
               "--max-minutes", str(args.max_minutes)]
    if args.until is not None:
        options.extend(("--until", args.until.isoformat()))
    if args.dry_run:
        options.append("--dry-run")
    if args.explain:
        options.append("--explain")
    return options


KUBECTL_TIMEOUT = "kubectl did not finish before the deadline; it was stopped"


def _kubectl(command: list[str], args: argparse.Namespace, **options: Any) -> Any:
    # The pod's own alarm should end it first; this bounds a hung kubectl. run() kills it on timeout.
    try:
        return subprocess.run(command, input=build_pod_script(), check=False,
                              timeout=(args.max_minutes + 2) * 60, **options)
    except subprocess.TimeoutExpired:
        raise ep._ExportError(KUBECTL_TIMEOUT) from None


def _fetch(args: argparse.Namespace) -> dict[str, Any]:
    # The script shares the API container's CPU allowance, so it runs at the lowest priority.
    command = ["kubectl", "-n", "bydconnect", "exec", "-i", "deploy/api", "--",
               "nice", "-n", "19", "python", "-", *_pod_options(args)]
    if args.explain or args.dry_run:
        result = _kubectl(command, args, capture_output=True)
        if result.returncode:
            raise _pod_error(result.stderr, result.returncode)
        parsed = _single_json(result.stdout)
        return checked_plans(parsed) if args.explain else checked_dry_run(parsed)

    directory = ep._export_directory(args.out_dir)
    with TemporaryDirectory(prefix=".mapping-", dir=directory) as scratch:
        stream = Path(scratch) / "stream.jsonl"
        with stream.open("wb") as output:
            result = _kubectl(command, args, stdout=output, stderr=subprocess.PIPE)
        if result.returncode:
            raise _pod_error(result.stderr, result.returncode)
        folded = Path(scratch) / "pairs.jsonl"
        fold(stream, folded)
        stream.unlink()
        meta, lines = verify(folded)
        destination = directory / f"pairs-{meta['start'][:10]}-{meta['car_days']}d.jsonl"
        if destination.exists():
            raise ep._ExportError("an export for this shifted start date and duration already exists")
        folded.rename(destination)
    return {"file": destination.name, "start": meta["start"], "car_lines": lines["car"],
            "cloud_lines": lines["cloud"], "stats": meta["stats"]}


def _parse(argv: list[str], local: bool) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="python -m bench.mapping.export_pairs" + (" fetch" if local else ""),
        description=("Fetch, check and keep one car's paired export (runs the pod side through kubectl)."
                     if local else "Pod side: write one car's paired export as JSON lines on stdout."),
    )
    parser.add_argument("--car-days", type=int, default=MAX_CAR_DAYS,
                        help=f"car_telemetry days ending at --until (1-{MAX_CAR_DAYS})")
    parser.add_argument("--cloud-days", type=int, default=MAX_CLOUD_DAYS,
                        help=f"ingest_events days ending at --until (1-{MAX_CLOUD_DAYS})")
    parser.add_argument("--until", type=ep._until, help="window end, ISO with a timezone (default: now - 10 min)")
    parser.add_argument("--page-rows", type=int, default=2000, help="rows per query page (1-5000)")
    parser.add_argument("--pause-ms", type=int, default=1000, help="pause between queries (200-60000)")
    parser.add_argument("--max-minutes", type=int, default=15, help="hard deadline of the pod process")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--dry-run", action="store_true", help="print counts and query plans only")
    mode.add_argument("--explain", action="store_true", help="print query plans only")
    if local:
        parser.add_argument("--out-dir", type=Path, help=f"a directory under {ep.EXPORT_ROOT} (export only)")
    args = parser.parse_args(argv)
    if local and not (args.explain or args.dry_run) and args.out_dir is None:
        parser.error("--out-dir is required for an export")
    if not 1 <= args.car_days <= MAX_CAR_DAYS or not 1 <= args.cloud_days <= MAX_CLOUD_DAYS:
        parser.error(f"car-days must be 1-{MAX_CAR_DAYS} and cloud-days 1-{MAX_CLOUD_DAYS}")
    if not 1 <= args.page_rows <= 5000 or not 200 <= args.pause_ms <= 60_000:
        parser.error("page-rows must be 1-5000 and pause-ms 200-60000")
    if not 1 <= args.max_minutes <= (2**31 - 1) // 60:
        parser.error("max-minutes must be positive and fit in the process alarm")
    return args


def main(argv: list[str] | None = None) -> None:
    """Run the pod exporter, or fetch a stream and check it before keeping it."""
    argv = list(sys.argv[1:] if argv is None else argv)
    local = argv[:1] == ["fetch"]
    args = _parse(argv[1:] if local else argv, local)
    logging.disable(logging.CRITICAL)
    try:
        if local:
            print(ep.json_line(_fetch(args)), end="")
        else:
            _pod_with_deadline(args)
    except ep._ExportError as exc:
        print(exc, file=sys.stderr)
        raise SystemExit(1) from None
    except BaseException as exc:  # noqa: BLE001 - nothing but the class name may be printed
        # SQLAlchemy exceptions and Kubernetes diagnostics can contain row data.
        print(f"mapping export failed: {type(exc).__name__}", file=sys.stderr)
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
