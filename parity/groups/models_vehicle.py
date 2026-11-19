"""pybyd/models/vehicle.py, plus the enum-call helpers the realtime and hvac groups share.

An enum call takes ``{"value": <any JSON>}`` and answers what ``EnumClass(value)``
gives in Python: ``{"name": member.name, "value": member.value}`` plus the
result of each public method (``to_command_level``). ``_missing_`` decides
for values no member equals.
"""

from __future__ import annotations

import copy
import random
from collections.abc import Callable, Iterator
from typing import Any

from pybyd.models.vehicle import EmpowerRange, EnergyType, Vehicle

from parity.framework import Group, random_text
from parity.models_support import (
    SENTINELS,
    TIMESTAMP_VALUES,
    ModelCases,
    json_safe,
    random_json,
    random_scalar,
    random_value,
    register_model,
    timestamp_safe,
)

group = Group("models_vehicle")

# ---------------------------------------------------------------------------
# Shared helpers (used by models_realtime and models_hvac too)
# ---------------------------------------------------------------------------


def enum_values(cls: type) -> list[Any]:
    """Values worth calling an enum with: every member, near misses, other types."""
    members = [member.value for member in cls]
    extra = [
        -1, 0, 1, 2, 3, 15, 999, -999, 2**63 - 1, -(2**63), 2**64 - 1,
        *[float(value) for value in members], 2.0, 1.5, -0.0, 1e300,
        *[str(value) for value in members], "2", " 1 ", "1.0", "x", "",
        True, False, None, [], {}, [1], {"a": 1},
    ]
    return members + extra


def register_enum(group: Group, call: str, cls: type, methods: tuple[str, ...] = ()) -> None:
    """Register the oracle and cases of one ``BydEnum`` call."""

    @group.oracle(call)
    def oracle(value: Any) -> dict[str, Any]:
        member = cls(value)
        answer = {"name": member.name, "value": int(member.value)}
        for method in methods:
            answer[method] = getattr(member, method)()
        return answer

    @group.recorded
    def recorded() -> Iterator[tuple[str, dict[str, Any]]]:
        for value in enum_values(cls):
            yield call, {"value": value}

    @group.fuzz
    def fuzz(rng: random.Random) -> Iterator[tuple[str, dict[str, Any]]]:
        values = enum_values(cls)
        members = [member.value for member in cls]
        while True:
            value = rng.choice([
                lambda: rng.choice(values),
                lambda: rng.randint(-5, 20),
                lambda: random_scalar(rng),
                lambda: random_json(rng),
                lambda: rng.choice(members) + rng.choice([0.0, 0.5, -0.5]),
                lambda: rng.choice([str, lambda v: f" {v} ", lambda v: f"{v}.0"])(rng.choice(members)),
            ])()
            if json_safe(value):
                yield call, {"value": value}


def mutate_fields(
    rng: random.Random,
    cases: ModelCases,
    pools: dict[str, list[Any]] | None = None,
    nested: Callable[[type], dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """A realistic sample with one to three fields changed.

    Each changed field gets a value from ``pools`` (keyed by field name) when
    there is one, else a sentinel, another key spelling, or a random value of
    the field's kind.
    """
    pools = pools or {}
    payload = copy.deepcopy(rng.choice(cases.samples))
    for spec in rng.sample(cases.specs, k=min(len(cases.specs), rng.randint(1, 3))):
        roll = rng.random()
        if roll < 0.1:
            payload = {key: value for key, value in payload.items() if key not in spec.all_keys}
            continue
        key = rng.choice(spec.all_keys) if roll < 0.3 else spec.keys[0]
        if spec.name in pools and rng.random() < 0.6:
            value = rng.choice(pools[spec.name])
        elif rng.random() < 0.25:
            value = rng.choice(SENTINELS)
        else:
            value = random_value(rng, spec.kind, nested or (lambda cls: {}))
        if cases.accepts(spec, value):
            payload[key] = value
    return payload if json_safe(payload) else {}


# ---------------------------------------------------------------------------
# This file's calls
# ---------------------------------------------------------------------------

register_enum(group, "models.vehicle.EnergyType", EnergyType)

EMPOWER_SAMPLES = [
    {"code": "1", "name": "Remote control", "childList": [{"code": "1-1", "name": "Lock"},
                                                          {"code": "1-2", "name": "Climate"}]},
    {"code": "2", "name": "Location", "children": []},
    {"code": "3", "name": "Charging", "children": [{"code": "3-1", "name": "Start", "childList": [{"code": "x"}]}]},
]

empower_cases = register_model(group, "models.vehicle.EmpowerRange", EmpowerRange, samples=EMPOWER_SAMPLES)

LEARN_INFO = {"openWindowLearnInfo": 1, "openWindow499LearnInfo": 1, "batteryHeating": 0,
              "refrigeratorLearnInfo": -1, "trunkLearnInfo": 1}

# The first sample is the base the generic cases vary, so it is the small one.
VEHICLE_SAMPLES = [
    {"vin": "TESTVIN0000000003", "energyType": 1, "cfPic": {"pic_main_url": "m.png", "pic_set_url": "s.png"},
     "vehicleFunLearnInfo": {"trunkLearnInfo": 1}},
    {
        "vin": "TESTVIN0000000001", "modelName": "SEAL", "brandName": "BYD", "energyType": "0",
        "autoAlias": "My Seal", "autoPlate": "TEST-01", "picMainUrl": "https://example.com/main.png",
        "picSetUrl": "https://example.com/set.png", "outModelType": "EA1", "totalMileage": 12345.6,
        "modelId": 101, "carType": 2, "defaultCar": 1, "empowerType": 0, "permissionStatus": 1,
        "tboxVersion": "3", "vehicleState": "1", "autoBoughtTime": 1700000000000, "yunActiveTime": 1700000000,
        "empowerId": 0, "rangeDetailList": EMPOWER_SAMPLES[:2], "vehicleFunLearnInfo": LEARN_INFO,
    },
    {
        "vin": "TESTVIN0000000002", "modelName": "SHARK 6", "energyType": 2, "autoAlias": "",
        "picMainUrl": "", "cfPic": {"picMainUrl": "https://example.com/cf-main.png",
                                    "picSetUrl": "https://example.com/cf-set.png"},
        "totalMileage": "--", "defaultCar": "false", "empowerType": -1, "empowerId": 123,
        "rangeDetailList": [EMPOWER_SAMPLES[2]], "autoBoughtTime": "2024-01-01T00:00:00Z",
    },
]

vehicle_cases = register_model(group, "models.vehicle.Vehicle", Vehicle, samples=VEHICLE_SAMPLES)

# Values that reach each branch of _fill_missing_pics.
CF_PICS = [
    None, "notadict", [], {}, {"picMainUrl": "a", "picSetUrl": "b"}, {"pic_main_url": "c", "pic_set_url": "d"},
    {"picMainUrl": "", "pic_main_url": "e"}, {"picMainUrl": 0, "pic_main_url": 0}, {"picMainUrl": 5, "picSetUrl": 1.5},
    {"picMainUrl": True, "picSetUrl": [1]}, {"picMainUrl": {"a": "'"}, "picSetUrl": "比亚迪"},
    {"picMainUrl": None, "pic_main_url": "--"}, {"picSetUrl": "only-set"},
]


def _vehicle(**changes: Any) -> dict[str, Any]:
    payload = {"vin": "TESTVIN0000000001"}
    payload.update(changes)
    return {key: value for key, value in payload.items() if value is not ...}


@group.recorded
def recorded():
    call = "models.vehicle.Vehicle"
    for cf_pic in CF_PICS:
        for main, pic_set in [("", ""), ("main.png", ""), ("", "set.png"), ("main.png", "set.png"), (..., ...),
                              ("--", " ")]:
            yield call, {"raw": _vehicle(cfPic=cf_pic, picMainUrl=main, picSetUrl=pic_set), "context": None}
    # cfPic read from a caller-provided raw, not from the payload.
    yield call, {"raw": {"vin": "V", "raw": {"cfPic": {"picMainUrl": "from-raw"}}, "cfPic": {"picMainUrl": "x"}},
                 "context": None}
    yield call, {"raw": {"vin": "V", "raw": {"cfPic": "x"}}, "context": None}
    yield call, {"raw": {"vin": "V", "raw": "--", "cfPic": {"picMainUrl": "x"}}, "context": None}
    for empower_type in [-1, 0, 1, -0.0, "-1", " -2 ", None, "--", -(2**53), 1.5, "x"]:
        yield call, {"raw": _vehicle(empowerType=empower_type), "context": None}
    for energy in [*enum_values(EnergyType), "3", 3]:
        if json_safe(energy):
            yield call, {"raw": _vehicle(energyType=energy), "context": None}
    for info in [{"a": 1, "b": "2", "c": True}, {"a": "x"}, {"a": None}, {"a": 1.0}, {"a": 1.5}, {"a": 2**53}, {"a": "9007199254740993"}]:
        yield call, {"raw": _vehicle(vehicleFunLearnInfo=info), "context": None}
    for ranges in [[{"code": 1}], [{"childList": [{"childList": [{"name": 5}]}]}], [{"children": "x"}],
                   [{"children": [], "childList": [{"code": "a"}]}], [None], ["x"], [{"raw": 1}]]:
        yield call, {"raw": _vehicle(rangeDetailList=ranges), "context": None}
    for stamp in [stamp for stamp in TIMESTAMP_VALUES if timestamp_safe(stamp)][:14]:
        yield call, {"raw": _vehicle(autoBoughtTime=stamp, yunActiveTime=stamp), "context": None}


@group.fuzz
def fuzz(rng):
    pools = {"pic_main_url": ["", "--", "m.png"], "pic_set_url": ["", "s.png"], "empower_type": [-1, 0, 1, "-1"]}
    while True:
        payload = mutate_fields(rng, vehicle_cases, pools, nested=lambda cls: copy.deepcopy(rng.choice(EMPOWER_SAMPLES)))
        if rng.random() < 0.4:
            payload["cfPic"] = rng.choice(CF_PICS) if rng.random() < 0.7 else random_json(rng)
            if rng.random() < 0.5:
                payload.pop(rng.choice(["picMainUrl", "picSetUrl"]), None)
        if json_safe(payload):
            yield "models.vehicle.Vehicle", {"raw": payload, "context": None}
        empower = mutate_fields(rng, empower_cases)
        if rng.random() < 0.3:
            empower[rng.choice(["children", "childList"])] = [mutate_fields(rng, empower_cases)
                                                             for _ in range(rng.randint(0, 2))]
        if rng.random() < 0.1:
            empower[random_text(rng, 6)] = random_json(rng)
        if json_safe(empower):
            yield "models.vehicle.EmpowerRange", {"raw": empower, "context": None}

