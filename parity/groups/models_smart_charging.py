"""pybyd/models/smart_charging.py"""

from __future__ import annotations

import copy
import random
from collections.abc import Iterable, Iterator
from typing import Any

from pybyd.models.smart_charging import AcCurrentLimit, SmartChargeDto, SmartChargingSchedule, SmartJourneyDto

from parity.framework import Group, random_text
from parity.models_support import ModelCases, random_json, random_numeric_text, register_model

group = Group("models_smart_charging")

_CHARGE = {
    "startChargeTime": "22:00", "endChargeTime": "06:30", "chargeWay": "0,1,2,3,4", "status": "1",
    "updateTime": 1700000000, "createTime": "2024-05-07T05:56:45.000+00:00", "exeTime": 1700000000123,
}
_JOURNEY = {
    "useVehicleTime": "07:45", "startDiscountPrice": "23:30", "endDiscountPrice": "05:30", "chargeWay": "e",
    "status": 0, "updateTime": 1700000000, "createTime": 1700000000,
}

# --- Time-of-day and flag strings the generic generator would not think of ---------

TIME_TEXTS: list[Any] = [
    "08:30", "8:5", "23:59", "24:00", "00:00", "12:30:45", "12:30:60", "12:30:59:11", " 7 : 5 ", "7:5:", "07:30:00",
    "full", "FULL", " Full ", "--", "", "8", "8.5", "08-30", "+8:+30", "-0:0", "0_8:3_0", "٠٨:٣٠", "8:30 pm",
    "1e1:30", "99999999999999999999:00", "08:30:xx", "a:b", ":", "8:", ":30", "1__0:30", "08:30　", "０８：３０",
    "08:30\x1c", "\x1c8:30", "8:-5", "-1:30", "12:30:-1", "ſ:30", "FUll", 830, 8.5, True, False, [8, 30], {"a": 1},
    {"8": "30"}, ["8:30"], 0, -1,
]
STATUS_TEXTS: list[Any] = ["1", "0", " 1 ", "01", "1.0", "true", "", "--", 1, 0, 2, 1.0, 0.0, True, False, [1], {}, "١", "x"]
WAY_TEXTS: list[Any] = [
    "s", "e", "0,1,2,3,4", " 0 , 1 ", "0,,1", ",", "1,x", "x", "", "--", " ", "S", "E", "1_0,2", "٠,١", "0,1,", "-1,2",
    "99999999999999999999,1", "1.5", 5, 0, 1.5, True, [1, "a"], {"a": 1}, "6,٦,٦", "1__0", "+3,-0",
]
LIMIT_TEXTS: list[Any] = ["16", " 32 ", "", "--", 16, 16.0, 1e16, 1e-5, True, None, [], {}, "A", "16A", "٣٢", [1], "0"]
LIMIT_LISTS: list[Any] = [
    [], ["8", "16", "32"], [" 8 ", None, "", "--", 16, 1.5, True, [1], {"a": 1}, "٣"], "16", None, {}, 5, [None],
    ["nan", "NaN"], [1e16, 1e-5, -0.0, 2**64 - 1], [["a", "é"]], [{"é": "b"}],
]

_TIME_FIELDS = {
    "SmartChargeDto": ["startChargeTime", "endChargeTime", "start_time", "end_time", "startTime", "endTime"],
    "SmartJourneyDto": ["useVehicleTime", "startDiscountPrice", "endDiscountPrice", "discountStart", "discountEnd"],
}
_OTHER_FIELDS = {
    "SmartChargeDto": [("status", STATUS_TEXTS), ("chargeWay", WAY_TEXTS), ("chargeUntilFull", STATUS_TEXTS)],
    "SmartJourneyDto": [("status", STATUS_TEXTS), ("chargeWay", WAY_TEXTS)],
    "AcCurrentLimit": [("currentEvCurrentLimit", LIMIT_TEXTS), ("currentLimitList", LIMIT_LISTS)],
}

_MODELS: dict[str, tuple[type, dict[str, Any]]] = {}


def _register(name: str, cls: type, samples: list[dict[str, Any]]) -> None:
    call = f"models.smart_charging.{name}"
    cases = register_model(group, call, cls, samples=samples)
    _MODELS[name] = (cls, cases)

    @group.recorded
    def extra_recorded() -> Iterator[tuple[str, dict[str, Any]]]:
        base = samples[0]
        for key in _TIME_FIELDS.get(name, []):
            for value in TIME_TEXTS:
                yield call, {"raw": {**base, key: value}, "context": None}
        for key, values in _OTHER_FIELDS.get(name, []):
            for value in values:
                yield call, {"raw": {**base, key: value}, "context": None}
        for sample in samples:
            yield call, {"raw": {"data": sample}, "context": None}

    @group.fuzz
    def extra_fuzz(rng: random.Random) -> Iterator[tuple[str, dict[str, Any]]]:
        while True:
            payload = copy.deepcopy(cases.fuzz(rng))
            if isinstance(payload, dict):
                time_keys = _TIME_FIELDS.get(name, [])
                for key in rng.sample(time_keys, k=rng.randint(0, min(3, len(time_keys)))):
                    payload[key] = rng.choice([
                        lambda: rng.choice(TIME_TEXTS),
                        lambda: f"{rng.choice(['', ' ', '0', '00', '٠'])}{rng.randint(-1, 26)}:{rng.randint(-1, 62)}"
                        + rng.choice(["", f":{rng.randint(-1, 62)}", ":", ":x"]),
                        lambda: random_numeric_text(rng),
                        lambda: random_text(rng, 8),
                        lambda: random_json(rng),
                    ])()
                for key, values in _OTHER_FIELDS.get(name, []):
                    if rng.random() < 0.3:
                        payload[key] = rng.choice(values) if rng.random() < 0.8 else random_json(rng)
                if name == "SmartChargeDto" and rng.random() < 0.3:
                    payload["endChargeTime"] = rng.choice(["full", "FULL", " Full ", "fuLL\n", "full ?", "full:00"])
            yield call, {"raw": payload, "context": None}


_register("SmartChargeDto", SmartChargeDto, [dict(_CHARGE), {"startTime": "1:2", "endTime": "full", "chargeWay": "s"},
                                             {"startChargeTime": "22:00", "endChargeTime": " full ", "status": "1"},
                                             {"start_time": "07:00", "endChargeTime": "08:00", "end_time": "09:00"}])
_register("SmartJourneyDto", SmartJourneyDto, [dict(_JOURNEY), {"discountStart": "01:00", "status": "1"},
                                               {"useVehicleTime": "--", "chargeWay": "0,6"}])
_register("AcCurrentLimit", AcCurrentLimit, [
    {"vin": "TESTVIN0000000001", "currentEvCurrentLimit": "16", "currentLimitList": ["8", "10", "13", "16"]},
    {"currentEvCurrentLimit": 32, "currentLimitList": [" 8 ", None, "", 16]},
    {"vin": "--", "currentLimitList": "notalist"},
])

# The schedule nests both DTOs; its own samples cover the aliases.
register_model(
    group,
    "models.smart_charging.SmartChargingSchedule",
    SmartChargingSchedule,
    samples=[
        {"vin": "TESTVIN0000000001", "soc": 80, "type": "1", "vehicleTimeZone": "Europe/Amsterdam",
         "smartChargeDto": dict(_CHARGE), "smartJourneyDto": dict(_JOURNEY), "updateTime": 1700000000},
        {"vin": "TESTVIN0000000001", "soc": "55", "time_zone": "UTC", "charge": {"status": "1"},
         "journey": {"status": "0"}},
        {"smartChargeDto": {"startChargeTime": "22:00", "endChargeTime": "full", "status": "1"}},
        {"smartJourneyDto": {"status": 1}, "smartChargeDto": "--", "type": 5},
    ],
)

_SCHEDULE = "models.smart_charging.SmartChargingSchedule"


@group.recorded
def schedule_recorded() -> Iterator[tuple[str, dict[str, Any]]]:
    for charge in (_CHARGE, {"endChargeTime": "FULL"}, {"status": "1"}, {"status": 0}, {}, [], 5, "x", None):
        for journey in ({"status": 1}, {"status": "0"}, {}, "x", None, 5):
            yield _SCHEDULE, {"raw": {"smartChargeDto": charge, "smartJourneyDto": journey}, "context": None}
    for key in ("smartChargeDto", "charge", "smartJourneyDto", "journey"):
        yield _SCHEDULE, {"raw": {key: {"status": "1", "startChargeTime": "bad"}}, "context": None}


@group.fuzz
def schedule_fuzz(rng: random.Random) -> Iterator[tuple[str, dict[str, Any]]]:
    charge_cases = _MODELS["SmartChargeDto"][1]
    journey_cases = _MODELS["SmartJourneyDto"][1]
    while True:
        payload: dict[str, Any] = {}
        for keys, cases in ((("smartChargeDto", "charge"), charge_cases), (("smartJourneyDto", "journey"), journey_cases)):
            if rng.random() < 0.8:
                payload[rng.choice(keys)] = cases.fuzz(rng, 1) if rng.random() < 0.8 else random_json(rng)
        if rng.random() < 0.5:
            payload["soc"] = rng.choice([80, "55", "--", -1, 1.5, "x"])
        yield _SCHEDULE, {"raw": payload, "context": None}


# ``_to_int``-style conversions do not appear here, but ``repeat_days`` can return an
# integer wider than i64 for text such as "99999999999999999999". openbyd's integers are
# i64 (docs/porting-models.md, "Representation limits"), so cases whose answer holds such
# an integer are left out; every other case, including all errors, is kept.


def _ints(value: Any) -> Iterator[int]:
    if isinstance(value, bool):
        return
    if isinstance(value, int):
        yield value
    elif isinstance(value, dict):
        for key, item in value.items():
            if key != "raw":  # the input echoed back; openbyd keeps it as JSON
                yield from _ints(item)
    elif isinstance(value, list):
        for item in value:
            yield from _ints(item)


def _representable(call: str, args: dict[str, Any]) -> bool:
    answer = group.answer(call, args)
    return all(-(2**63) < number < 2**63 for number in _ints(answer.get("ok")))


def _keep_representable(source: Any) -> Any:
    def wrapped(*arguments: Any) -> Iterable[Any]:
        for call, args in source(*arguments):
            if _representable(call, args):
                yield call, args

    return wrapped


group._recorded = [_keep_representable(source) for source in group._recorded]
group._fuzz = [_keep_representable(source) for source in group._fuzz]
