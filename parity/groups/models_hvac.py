"""pybyd/models/hvac.py"""

from __future__ import annotations

import copy
import random

from pybyd.models.hvac import (
    AcSwitch,
    AirConditioningMode,
    HvacOverallStatus,
    HvacStatus,
    HvacWindMode,
    HvacWindPosition,
)

from parity.framework import Group
from parity.models_support import SENTINELS, json_safe, mutate_fields, random_json, register_enum, register_model

group = Group("models_hvac")

for _cls in (AcSwitch, HvacOverallStatus, AirConditioningMode, HvacWindMode, HvacWindPosition):
    register_enum(group, f"models.hvac.{_cls.__name__}", _cls)

CALL = "models.hvac.HvacStatus"

FULL = {
    "acSwitch": 2, "status": 1, "airConditioningMode": 1, "windMode": 1, "windPosition": 3, "cycleChoice": 2,
    "mainSettingTemp": 7, "mainSettingTempNew": 21.0, "copilotSettingTemp": 8, "copilotSettingTempNew": "22.0",
    "tempInCar": 23.5, "tempOutCar": 12, "whetherSupportAdjustTemp": 1, "vdeTempRangeMin": 15, "vdeTempRangeMax": 31,
    "frontDefrostStatus": 0, "electricDefrostStatus": 0, "wiperHeatStatus": 0, "mainSeatHeatState": 3,
    "mainSeatVentilationState": 1, "copilotSeatHeatState": 0, "copilotSeatVentilationState": 0,
    "stearingWheelHeatState": -1, "lrSeatHeatState": 0, "lrSeatVentilationState": 0, "rrSeatHeatState": 0,
    "rrSeatVentilationState": 0, "rapidIncreaseTempState": 0, "rapidDecreaseTempState": 0, "refrigeratorState": 0,
    "refrigeratorDoorState": 0, "pm": 5, "pm25StateOutCar": 12.0,
}
# The first sample is the base the generic cases vary, so it is the small one.
SAMPLES = [
    {"status": 2, "pm": 5, "steeringWheelHeatState": 2, "windPosition": 6, "cycleChoice": 1},
    FULL,
    {"statusNow": {"status": 2, "acSwitch": 1, "tempInCar": -129, "tempOutCar": "--", "vdeTempRangeMin": 17,
                   "vdeTempRangeMax": 33, "stearingWheelHeatState": 1, "pm": 5}, "requestSerial": "SERIAL0001"},
]

cases = register_model(group, CALL, HvacStatus, samples=SAMPLES)

STATUS_NOW = [
    {}, {"status": 1}, {"tempInCar": -129, "tempOutCar": -129.0}, {"stearingWheelHeatState": 2},
    {"stearingWheelHeatState": 1, "steeringWheelHeatState": -1}, {"a": "--", "b": None, "c": " nan "},
    {"raw": {"x": 1}, "status": 1}, {"raw": "--"}, {"raw": 5}, {"statusNow": {"status": 1}}, {"status": "x"},
]
RANGES = [
    (None, None), (17, 33), (15, 31), (17, None), (None, 33), (20, 20), (20, 36), (20, 37), (33, 17), (-5, 11),
    ("17.5", "33.5"), ("NAN", 33), ("inf", "inf"), ("-inf", 0), (1e300, 1e300 + 1), (0, 16), (0.5, 16.5),
]
STEERING = [2, "2", 2.0, " 2 ", -1, "-1", 0, 1, 3, 1.5, True, False, "x", 2**63]


@group.recorded
def recorded():
    for inner in STATUS_NOW:
        yield CALL, {"raw": {"statusNow": inner, "status": 2, "pm": 1}, "context": None}
    for outer in ["notadict", [], None, "--", 5]:
        yield CALL, {"raw": {"statusNow": outer, "status": 1}, "context": None}
    for low, high in RANGES:
        yield CALL, {"raw": {"vdeTempRangeMin": low, "vdeTempRangeMax": high}, "context": None}
        yield CALL, {"raw": {"statusNow": {"vdeTempRangeMin": low, "vdeTempRangeMax": high}}, "context": None}
    for value in STEERING:
        yield CALL, {"raw": {"steeringWheelHeatState": value}, "context": None}
        yield CALL, {"raw": {"stearingWheelHeatState": value}, "context": None}
    for value in [-1, 0, 1, 2, 3, "2", None, "--"]:
        yield CALL, {"raw": {"acSwitch": value, "status": value}, "context": None}
    for value in [-129, -129.0, "-129", -128.5, -130, 0, "--", "NAN"]:
        yield CALL, {"raw": {"tempInCar": value, "tempOutCar": value}, "context": None}
    # Upstream #94: vehicleStatusPush's airStatusDto spells cycleChoice "cycleModel".
    # (airRunState is an AirRunState since upstream #100.)
    for value in [0, 1, 2, 3, -1, "2", "--", None, 1.5]:
        yield CALL, {"raw": {"cycleModel": value}, "context": None}
        yield CALL, {"raw": {"statusNow": {"cycleModel": value}}, "context": None}
    for payload in [{"cycleModel": 2, "cycleChoice": 1}, {"cycleChoice": 1, "cycleModel": 2},
                    {"cycleModel": 2, "cycle_choice": 1}, {"cycle_model": 2}, {"cycleModel": "--", "cycleChoice": 1}]:
        yield CALL, {"raw": payload, "context": None}
    for value in [0, 1, 2, 3, -1, "1", "1__0", "x", 1.0, 1.5, True, None, "--", "", [], 2**53]:
        yield CALL, {"raw": {"airRunState": value, "status": 1}, "context": None}


@group.fuzz
def fuzz(rng: random.Random):
    pools = {
        "steering_wheel_heat_state": STEERING,
        "temp_in_car": [-129, -129.0, "-129"] + SENTINELS,
        "temp_out_car": [-129, "-129.0"] + SENTINELS,
        "vde_temp_range_min": [r[0] for r in RANGES],
        "vde_temp_range_max": [r[1] for r in RANGES],
    }
    while True:
        payload = mutate_fields(rng, cases, pools)
        roll = rng.random()
        if roll < 0.3:
            payload = {"statusNow": payload, **({"status": rng.choice([1, 2, "--"])} if rng.random() < 0.5 else {})}
        elif roll < 0.4:
            payload["statusNow"] = rng.choice(STATUS_NOW + [random_json(rng)])
        if json_safe(payload):
            yield CALL, {"raw": copy.deepcopy(payload), "context": None}
