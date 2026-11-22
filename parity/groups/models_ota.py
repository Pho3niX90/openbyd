"""pybyd/models/ota.py"""

from __future__ import annotations

import copy

from pybyd.models.ota import OtaStatus, OtaUpgradeInfo, OtaUpgradeResult

from parity.framework import Group
from parity.models_support import TIMESTAMP_VALUES, json_safe, register_enum, register_model, timestamp_safe

group = Group("models_ota")

register_enum(group, "models.ota.OtaStatus", OtaStatus)

#: The idle reply of pyBYD's OTA tests (captured from a car, nothing identifying).
CAPTURED_IDLE = {
    "padTimeZone": "Europe/Madrid",
    "updateVersion": "",
    "currentUpdateTime": 1795276224815,
    "autoUpgradeTime": "3:00",
    "autoUpgradeSwitch": "1",
    "functionOptimization": "1. Optimize UI.",
    "acknowledgements": "",
    "currentVersion": "V1.3.0",
    "upgradeStatus": 0,
    "estimateUpgradeTime": 7200,
    "bookingTimeStamp": 0,
    "functionDescription": "",
    "functionAddition": "1. Add AC charging current limit feature.",
}

RESULT_SAMPLES = [
    {"status": "0", "statusCode": "200", "message": "ok"},
    {"status": 1, "status_code": 500, "message": None},
    {},
]

register_model(group, "models.ota.OtaUpgradeResult", OtaUpgradeResult, samples=RESULT_SAMPLES)

INFO_SAMPLES = [
    {"vin": "TESTVIN0000000001", **CAPTURED_IDLE},
    {
        "vin": "TESTVIN0000000001", **CAPTURED_IDLE, "updateVersion": "V1.4.0", "upgradeStatus": 1,
        "bookingTimeStamp": 1795280000000, "upgradeResult": RESULT_SAMPLES[0],
    },
    {"vin": "TESTVIN0000000001", "upgradeStatus": 999},
    {"upgradeStatus": "100", "autoUpgradeSwitch": 1, "updateVersion": "  ", "raw": {"a": 1}},
]

info_cases = register_model(group, "models.ota.OtaUpgradeInfo", OtaUpgradeInfo, samples=INFO_SAMPLES)


@group.recorded
def recorded():
    call = "models.ota.OtaUpgradeInfo"
    base = INFO_SAMPLES[0]
    for status in [-1, 0, 1, 2, 3, 4, 5, 6, 7, 99, 100, 101, "1", 1.0, 1.5, True, None, "--"]:
        yield call, {"raw": {**base, "upgradeStatus": status}, "context": None}
    for switch in ["1", "0", "", " 1", 1, None, True]:
        yield call, {"raw": {**base, "autoUpgradeSwitch": switch}, "context": None}
    for version in ["", " ", "　", "V2", " V2 ", None, 2]:
        yield call, {"raw": {**base, "updateVersion": version}, "context": None}
    for stamp in [stamp for stamp in TIMESTAMP_VALUES if timestamp_safe(stamp)][:14]:
        yield call, {"raw": {**base, "bookingTimeStamp": stamp, "currentUpdateTime": stamp}, "context": None}
    for result in [*RESULT_SAMPLES, None, "x", [], {"statusCode": [1]}]:
        yield call, {"raw": {**base, "upgradeResult": result}, "context": None}
    for estimate in [7200, "7200", 7200.0, 1.5, -1, None, "--"]:
        yield call, {"raw": {**base, "estimateUpgradeTime": estimate}, "context": None}


@group.fuzz
def fuzz(rng):
    while True:
        payload = info_cases.fuzz(rng)
        if isinstance(payload, dict) and rng.random() < 0.3:
            payload["upgradeResult"] = copy.deepcopy(rng.choice(RESULT_SAMPLES))
        if json_safe(payload):
            yield "models.ota.OtaUpgradeInfo", {"raw": payload, "context": None}
