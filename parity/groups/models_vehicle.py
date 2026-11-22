"""pybyd/models/vehicle.py.

The enum-call and field-mutation helpers (``register_enum``, ``mutate_fields``)
live in ``parity.models_support``.
"""

from __future__ import annotations

import copy
from typing import Any

from pybyd.models.vehicle import EmpowerDetail, EmpowerRange, EnergyType, Vehicle, collect_permission_codes

from parity.framework import Group, random_text

# enum_values, register_enum and mutate_fields moved to parity.models_support;
# they stay importable from here.
from parity.models_support import (  # noqa: F401
    TIMESTAMP_VALUES,
    enum_values,
    json_safe,
    mutate_fields,
    random_json,
    register_enum,
    register_model,
    timestamp_safe,
)

group = Group("models_vehicle")

register_enum(group, "models.vehicle.EnergyType", EnergyType)

EMPOWER_SAMPLES = [
    {"code": "1", "name": "Remote control", "childList": [{"code": "1-1", "name": "Lock"},
                                                          {"code": "1-2", "name": "Climate"}]},
    {"code": "2", "name": "Location", "children": []},
    {"code": "3", "name": "Charging", "children": [{"code": "3-1", "name": "Start", "childList": [{"code": "x"}]}]},
]

empower_cases = register_model(group, "models.vehicle.EmpowerRange", EmpowerRange, samples=EMPOWER_SAMPLES)

#: The grant tree of pyBYD's own guest permission tests: basic control (21)
#: and scheduled charging (41), no location (31) and no battery heat (43).
RANGE_TREE = [
    {"code": "2", "name": "Keys & Control", "childList": [{"code": "21", "name": "Basic control", "childList": []}]},
    {"code": "4", "name": "More control", "childList": [{"code": "41", "name": "Scheduled charging", "childList": []}]},
]
#: Trees that reach each branch of collect_permission_codes.
RANGE_TREES = [
    [],
    RANGE_TREE,
    EMPOWER_SAMPLES,
    [{"code": "", "children": [{"code": "31"}]}, {"code": "31"}],
    [{"code": 21}, {"code": 4.5}, {"code": None}],
    [{"childList": [{"childList": [{"childList": [{"code": "43"}]}]}]}],
    [{"code": "1001"}, {"code": "10020004"}, {"code": "2101"}],
    [{"code": " 21 "}, {"code": "ü"}],
    [None],
    ["x"],
    [{"children": "x"}],
]

EMPOWER_DETAIL_SAMPLES = [
    {
        "vin": "TESTVIN0000000001", "empowerControlPwdStatus": False, "empowerId": 114149, "empowerStatus": 0,
        "ownerEmail": "owner@example.com", "email": "user@example.com", "authPeriod": 4,
        "expireDate": 1799999999000, "detailList": RANGE_TREE,
    },
    {"vin": "TESTVIN0000000002", "empowerControlPwdStatus": True, "detailList": EMPOWER_SAMPLES},
]

empower_detail_cases = register_model(
    group, "models.vehicle.EmpowerDetail", EmpowerDetail, samples=EMPOWER_DETAIL_SAMPLES
)


@group.oracle("models.vehicle.collect_permission_codes")
def collect_permission_codes_oracle(ranges) -> list:
    return sorted(collect_permission_codes([EmpowerRange.model_validate(item) for item in ranges]))


@group.oracle("models.vehicle.Vehicle.has_permission")
def has_permission(raw, code: str) -> bool:
    return Vehicle.model_validate(raw).has_permission(code)


#: Permission codes worth asking about: granted, missing, parents, deep ones.
PERMISSION_CODES = ["21", "31", "41", "43", "2", "4", "", "2101", " 21"]

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
        "notUpgradeNowSwitch": "0", "supportOtaNightUpgrade": "1",
    },
    {
        "vin": "TESTVIN0000000002", "modelName": "SHARK 6", "energyType": 2, "autoAlias": "",
        "picMainUrl": "", "cfPic": {"picMainUrl": "https://example.com/cf-main.png",
                                    "picSetUrl": "https://example.com/cf-set.png"},
        "totalMileage": "--", "defaultCar": "false", "empowerType": -1, "empowerId": 123,
        "rangeDetailList": [EMPOWER_SAMPLES[2]], "autoBoughtTime": "2024-01-01T00:00:00Z",
    },
    {
        "vin": "TESTVIN0000000004", "empowerType": -1, "empowerId": 114149, "rangeDetailList": RANGE_TREE,
        "vehicleFunLearnInfo": {"otaUpgrade": 1}, "supportOtaNightUpgrade": 1, "notUpgradeNowSwitch": None,
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
    for ota in [1, 0, -1, "1", 1.0, True, None]:
        yield call, {"raw": _vehicle(vehicleFunLearnInfo={"otaUpgrade": ota}), "context": None}
    for night in ["1", "0", "", 1, 1.5, None, [1]]:
        yield call, {"raw": _vehicle(supportOtaNightUpgrade=night, notUpgradeNowSwitch=night), "context": None}
    for ranges in RANGE_TREES:
        yield "models.vehicle.collect_permission_codes", {"ranges": ranges}
        yield "models.vehicle.EmpowerDetail", {"raw": {"vin": "TESTVIN0000000001", "detailList": ranges}, "context": None}
        for empower_type in [-1, 2, 0, None]:
            for code in PERMISSION_CODES:
                yield "models.vehicle.Vehicle.has_permission", {
                    "raw": _vehicle(empowerType=empower_type, rangeDetailList=ranges), "code": code,
                }
    for flag in [True, False, 0, 1, "true", "no", None, 2]:
        yield "models.vehicle.EmpowerDetail", {"raw": {"empowerControlPwdStatus": flag}, "context": None}


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
        ranges = rng.choice(RANGE_TREES) if rng.random() < 0.5 else [mutate_fields(rng, empower_cases) for _ in range(3)]
        if json_safe(ranges):
            yield "models.vehicle.collect_permission_codes", {"ranges": ranges}
            yield "models.vehicle.Vehicle.has_permission", {
                "raw": _vehicle(empowerType=rng.choice([-1, -2, 0, 2, None, "-1"]), rangeDetailList=ranges),
                "code": rng.choice([*PERMISSION_CODES, random_text(rng, 3)]),
            }

