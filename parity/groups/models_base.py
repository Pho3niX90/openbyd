"""pybyd/models/_base.py: sentinels, key aliases, timestamps, units and the base model."""

from __future__ import annotations

from functools import cache
from typing import Any

from pydantic import TypeAdapter
from pydantic.alias_generators import to_camel as pydantic_to_camel
from pybyd.models import _base
from pybyd.models._base import BydBaseModel, BydEnum, BydTimestamp

from parity.framework import Group, random_text
from parity.models_support import (
    SENTINELS,
    TIMESTAMP_VALUES,
    WINDOWS_MAX_EPOCH_SECONDS,
    field_specs,
    jsonable,
    json_safe,
    random_float,
    random_iso_text,
    random_json,
    random_numeric_text,
    register_model,
    timestamp_safe,
)

group = Group("models_base")
_TIMESTAMP = TypeAdapter(BydTimestamp)


@group.oracle("models.base.parse_byd_timestamp")
def parse_byd_timestamp(value: Any) -> str | None:
    result = _base.parse_byd_timestamp(value)
    return None if result is None else result.isoformat()


@group.oracle("models.base.BydTimestamp")
def byd_timestamp(value: Any) -> Any:
    return jsonable(_TIMESTAMP.dump_python(_TIMESTAMP.validate_python(value), mode="json"))


@group.oracle("models.base.normalize_unit")
def normalize_unit(text: str | None) -> str | None:
    return _base.normalize_unit(text)


@group.oracle("models.base.is_negative")
def is_negative(value: float) -> bool:
    return _base.is_negative(value)


@group.oracle("models.base.is_temp_sentinel")
def is_temp_sentinel(value: float) -> bool:
    return _base.is_temp_sentinel(value)


@group.oracle("models.base.to_camel")
def to_camel(name: str) -> str:
    return pydantic_to_camel(name)


@cache
def _byd_enum(members: tuple[int, ...]) -> type[BydEnum]:
    """A ``BydEnum`` subclass with these values: ``UNKNOWN`` for -1, ``M<index>`` for the rest."""
    return BydEnum("Probe", [("UNKNOWN" if value == -1 else f"M{index}", value) for index, value in enumerate(members)])


@group.oracle("models.base.BydEnum")
def byd_enum(members: list[int], value: Any) -> dict[str, Any]:
    """``Probe(value)``: the member equal to ``value``, else ``UNKNOWN``, else the first member."""
    member = _byd_enum(tuple(members))(value)
    return {"name": member.name, "value": int(member.value)}


@group.oracle("models.base.clean_dict")
def clean_dict(values: dict[str, Any], aliases: dict[str, str]) -> dict[str, Any]:
    return BydBaseModel._clean_dict(values, aliases)


register_model(
    group,
    "models.base.BydBaseModel",
    BydBaseModel,
    samples=[{"vin": "TESTVIN0000000001", "soc": 80}],
    contexts=[None, {"energy_type": 1}],
)

# ---------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------

UNITS = [
    None, "", "kW·h/100km", "kWh/100km", "KWH/100 KM", "kw.h/100km", "度/百公里", "升/百公里", "L/100km",
    "l/100mi", "kWh/100mile", "kWh/100miles", "kWh/百英里", "kW·h", "kWh", "km", "L", "mi", "/", "//", "kWh/",
    "/100km", " k W h / 1 0 0 k m ", "Kwh/100km", "kwh/100mileſ", "KWH　/\x1c100KM", "kW··h/100km",
    "kW·h/100km+7.2L/100km", "%", "mpg", "ß", "ﬁ",
]
FIELD_NAMES = sorted({
    spec.name
    for module in ("charging", "gps", "realtime", "hvac", "energy", "vehicle", "smart_charging", "booking",
                   "message_center", "push_notification", "control", "latest_config", "token")
    for cls in vars(__import__(f"pybyd.models.{module}", fromlist=["x"])).values()
    if isinstance(cls, type) and hasattr(cls, "model_fields")
    for spec in field_specs(cls)
}) + ["a__b", "__x", "_private", "Already", "x2y", "lr_3rd_heat", "rr_seat2_heat", "a_1b", "a_B", "ABC", "a", "x_"]
NUMBERS = [0, 1, -1, -0.0, 0.5, -0.5, -129, -129.0, -129.5, -128.9999, 2**63 - 1, -(2**63), 2**64 - 1, 1e300, -1e300]


def _random_alias_map(rng):
    keys = ["a", "b", "c", "elecPercent", "soc", "time", "updateTime"]
    return {rng.choice(keys): rng.choice(keys) for _ in range(rng.randint(0, 3))}


def _random_dict(rng):
    keys = ["a", "b", "c", "elecPercent", "soc", "time", "updateTime", "raw"]
    return {rng.choice(keys): rng.choice([*SENTINELS, 1, "x", " -- ", [], {"a": ""}]) for _ in range(rng.randint(0, 5))}


def _timestamp_values():
    values = TIMESTAMP_VALUES + [None, [], {}, "", " ", "　", "Z", "2024-01-01T10:00Z", "2024-01-01Z10:00",
                                 "2024-01-01T10:00:00+01:00Z", "2024-01-01\x00", "2024-01-01T10:00Z\x00"]
    return [value for value in values if timestamp_safe(value)]


ENUM_MEMBERS = [[-1], [0], [-1, 0, 1], [0, 1, 2], [1, 2, -1], [5, -1, 7, 9], [-1, 0, 1, 2, 3], [-2, 3]]
ENUM_VALUES: list[Any] = [-1, 0, 1, 2, 3, 5, 7, 9, -2, 1.0, 2.0, 1.5, -0.0, True, False, None, "1", "", "x", [], {}, [1],
                          {"a": 1}, 2**63 - 1, 2**64 - 1, -(2**63), 1e300]


@group.recorded
def recorded():
    for members in ENUM_MEMBERS:
        for value in ENUM_VALUES:
            yield "models.base.BydEnum", {"members": members, "value": value}
    for value in _timestamp_values():
        yield "models.base.parse_byd_timestamp", {"value": value}
        yield "models.base.BydTimestamp", {"value": value}
    for text in UNITS:
        yield "models.base.normalize_unit", {"text": text}
    for number in NUMBERS:
        yield "models.base.is_negative", {"value": number}
        yield "models.base.is_temp_sentinel", {"value": number}
    for name in FIELD_NAMES:
        yield "models.base.to_camel", {"name": name}
    yield "models.base.clean_dict", {"values": {}, "aliases": {}}
    yield "models.base.clean_dict", {
        "values": {"elecPercent": 1, "time": "", "a": None, "b": " -- ", "c": "\tnan ", "d": "NAN", "e": 0},
        "aliases": {"elecPercent": "soc", "time": "updateTime"},
    }
    yield "models.base.clean_dict", {"values": {"elecPercent": 1, "soc": 2, "x": 3}, "aliases": {"elecPercent": "soc"}}
    yield "models.base.clean_dict", {"values": {"a": 1, "b": 2, "c": 3}, "aliases": {"a": "z", "z": "y", "c": "a"}}


@group.fuzz
def fuzz(rng):
    while True:
        members = rng.sample(range(-3, 12), k=rng.randint(1, 6))
        enum_value = rng.choice([rng.choice(members), rng.randint(-5, 14), rng.choice(ENUM_VALUES), random_json(rng),
                                 rng.choice(members) + rng.choice([0.0, 0.5]), str(rng.choice(members))])
        if json_safe(enum_value):
            yield "models.base.BydEnum", {"members": members, "value": enum_value}
        value = rng.choice([
            lambda: random_iso_text(rng),
            lambda: random_numeric_text(rng),
            lambda: rng.randint(-10, WINDOWS_MAX_EPOCH_SECONDS * 1000),
            lambda: random_float(rng),
            lambda: random_json(rng),
            lambda: rng.choice(_timestamp_values()),
        ])()
        if json_safe(value) and timestamp_safe(value):
            yield rng.choice(["models.base.parse_byd_timestamp", "models.base.BydTimestamp"]), {"value": value}
        unit = rng.choice(UNITS) or ""
        yield "models.base.normalize_unit", {"text": rng.choice([unit, unit.upper(), random_text(rng, 12),
                                                                unit.replace("/", rng.choice(["/", " / ", "", "//"]))])}
        number = rng.choice([random_float(rng), rng.randint(-200, 200), rng.choice(NUMBERS)])
        yield rng.choice(["models.base.is_negative", "models.base.is_temp_sentinel"]), {"value": number}
        name = "".join(rng.choice("abcxyz_0123ABZ") for _ in range(rng.randint(1, 12)))
        yield "models.base.to_camel", {"name": rng.choice([name, rng.choice(FIELD_NAMES)])}
        yield "models.base.clean_dict", {"values": _random_dict(rng), "aliases": _random_alias_map(rng)}
