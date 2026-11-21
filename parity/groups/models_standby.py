"""pybyd/models/standby.py: preconditioning plans, their commands and schedules.

The reply models (``StandbyPlanCommand``, ``StandbyPlan``, ``StandbyPlanList``,
``StandbyPlanRunResult``) take ``{"raw", "context"}`` like every other model.
``StandbyPlan`` has two properties that raise for some plans (``seats`` when a
seat value is a list or an object, ``schedule`` when the weekly data is not a
valid schedule): such a property is answered as ``{"error": "<class>"}``, so
the rest of the plan is still compared. Its methods have calls of their own:
``models.standby.StandbyPlan.command`` and ``.climate``.

``PlanClimate``, ``PlanSeats`` and ``StandbySchedule`` are plain pydantic
models built from keyword arguments: a call takes ``{"kwargs": {...}}`` and
answers ``{"dump": model_dump(mode="json")}`` (plus ``properties`` for
``PlanSeats``), or the error class. A datetime travels as what pydantic reads
one from: ISO text or a Unix timestamp. The constructors
(``StandbySchedule.off/once/daily/weekly``, ``StandbyPlanCommand.climate/
seats/steering_wheel``) have a call each.

``controlParams`` and ``controlParamsMap`` arrive as JSON text. Text that
decodes to the container pyBYD keeps (a dict for ``controlParamsMap``, a list
for ``controlParams``) with something ``serde_json`` cannot hold (``NaN``,
``Infinity``, integers beyond 64 bits, lone surrogates, or containers nested
deeper than 127 levels: see ``pyjson.rs``) is left out.
"""

from __future__ import annotations

import copy
import json
import math
import random
from collections.abc import Callable, Iterable, Iterator
from typing import Any

from pybyd.models import standby
from pybyd.models.standby import (
    PlanClimate,
    PlanSeats,
    StandbyPlan,
    StandbyPlanCommand,
    StandbyPlanList,
    StandbyPlanRunResult,
    StandbySchedule,
)

from parity.framework import Case, Group, random_text
from parity.models_support import (
    BOOL_AWKWARD,
    BOOL_VALUES,
    FLOAT_AWKWARD,
    FLOAT_VALUES,
    INT_AWKWARD,
    INT_VALUES,
    SENTINELS,
    UNKNOWN_KEYS,
    ModelCases,
    json_safe,
    jsonable,
    model_int_safe,
    mutate_fields,
    property_names,
    random_json,
    random_numeric_text,
    random_scalar,
    register_model,
    skip_unrepresentable_integers,
)

group = Group("models_standby")

VIN = "TESTVIN0000000001"

# ---------------------------------------------------------------------------
# Payloads (the shapes app 3.6.0 sends and receives, synthetic values)
# ---------------------------------------------------------------------------

PARAMS = {
    "mainSettingTemp": 5, "cycleMode": 1, "remoteMode": 4, "airConditioningMode": 1, "airAccuracy": 1,
    "copilotSettingTemp": 5, "timeSpan": 1,
}
PARAMS_TEXT = json.dumps(PARAMS, separators=(",", ":"))
SEAT_PARAMS = {
    "mainHeat": 1, "mainVentilation": 3, "copilotHeat": 3, "copilotVentilation": 1, "lrSeatHeatState": 0,
    "rrSeatHeatState": 0, "lrSeatVentilationState": 0, "rrSeatVentilationState": 0, "lrThirdHeatState": 0,
    "rrThirdHeatState": 0, "lrThirdVentilationState": 0, "rrThirdVentilationState": 0, "chairType": 8,
}
WHEEL_PARAMS = {"steeringWheelHeatState": 1, "chairType": 5}
#: The three sections of the plan edit screen, as the app saves them.
CAPTURED_COMMANDS = [
    {"commandType": "OPENAIR", "controlParamsMap": PARAMS},
    {"commandType": "VENTILATIONHEATING", "controlParamsMap": SEAT_PARAMS},
    {"commandType": "STEERINGWHEELHEATING", "controlParamsMap": WHEEL_PARAMS},
]
COMMAND = {
    "id": "1", "planId": "1216038691305533440", "commandId": "3", "commandType": "OPENAIR",
    "controlParamsMap": PARAMS_TEXT,
}
#: The preset plan, scheduled weekly.
PLAN = {
    "id": 1216038691305533440,
    "planName": "Conducción confort",
    "planType": 1,
    "defaultPlan": 1,
    "executeType": "2",
    "startTime": "13:30",
    "weeklyDays": "4,5,6",
    "executeTime": 1795177800000,
    "recExecuteTime": 1795177800000,
    "timeZone": "Europe/Madrid",
    "controlParams": '[{"commandType":"OPENAIR","controlParamsMap":' + PARAMS_TEXT + ',"commandId":"3"}]',
    "controlParamsList": [COMMAND],
}
#: A plan made in the app and never scheduled.
NEW_PLAN = {
    "id": "2", "planName": "Test plan", "planType": 2, "defaultPlan": 0, "executeType": "0", "startTime": None,
    "weeklyDays": None, "executeTime": 0, "controlParamsList": [{"commandType": "OPENAIR", "controlParamsMap": PARAMS_TEXT}],
}
PLAN_SAMPLES: list[dict[str, Any]] = [
    PLAN,
    NEW_PLAN,
    {**PLAN, "startTime": "01:00", "weeklyDays": "", "executeTime": 0, "recExecuteTime": 0},
    {key: value for key, value in PLAN.items() if key != "controlParamsList"},
    {**PLAN, "executeType": "0", "startTime": None, "weeklyDays": "", "executeTime": 1795371000000},
    {**PLAN, "executeType": "1", "startTime": None, "weeklyDays": "", "executeTime": 1795371000000},
    {**PLAN, "startTime": "", "executeTime": 0, "recExecuteTime": 0},
    {**PLAN, "controlParamsList": [{**c, "controlParamsMap": json.dumps(c["controlParamsMap"])} for c in CAPTURED_COMMANDS]},
    {**NEW_PLAN, "planName": "比亚迪 🚗 plan", "controlParamsList": CAPTURED_COMMANDS},
    {"id": "7", "planName": "Test plan", "planType": 2, "executeType": "0"},
]
RUN_SAMPLES = [
    {"requestSerial": "SERIAL1"},
    {"requestSerial": "SERIAL1", "res": 1},
    {"vin": VIN, "requestSerial": "SERIAL1", "res": 2, "message": "ok"},
    {"res": 3, "message": "比亚迪"},
    {"res": None, "message": "x"},
]

# ---------------------------------------------------------------------------
# What both sides can hold
# ---------------------------------------------------------------------------


#: Deepest container nesting serde_json reads (matches ``MAX_NESTING`` in ``pyjson.rs``).
MAX_NESTING = 127


def _decodes_safely(text: str, wanted: type) -> bool:
    """Whether ``json.loads(text)`` fails, is discarded, or gives something serde_json holds too."""
    try:
        decoded = json.loads(text)
    except ValueError:
        return True
    except RecursionError:
        expected = "{" if wanted is dict else "["
        return not text.lstrip(" \t\n\r").startswith(expected)
    if not isinstance(decoded, wanted):
        return True
    # A decoded command list carries each command's own map as text, decoded in turn.
    return _holdable(decoded) and in_domain(decoded, wanted=None)


def _holdable(value: Any, depth: int = 0) -> bool:
    if isinstance(value, bool) or value is None:
        return True
    if isinstance(value, float):
        return math.isfinite(value)
    if isinstance(value, int):
        return -(2**63) <= value < 2**64
    if isinstance(value, str):
        return not any(0xD800 <= ord(ch) <= 0xDFFF for ch in value)
    if depth >= MAX_NESTING:
        return False
    if isinstance(value, list):
        return all(_holdable(item, depth + 1) for item in value)
    return all(_holdable(key, depth + 1) and _holdable(item, depth + 1) for key, item in value.items())


#: The keys whose text pyBYD decodes as JSON (by alias and field name) and the container type it keeps.
JSON_KEYS: dict[str, type] = {
    "controlParams": list,
    "controlParamsMap": dict,
    "control_params": list,
    "control_params_map": dict,
}


def in_domain(value: Any, *, wanted: type | None = dict) -> bool:
    """Whether the JSON text pyBYD keeps in ``value`` gives something both sides hold.

    A string on its own is taken as a ``controlParamsMap`` (wanted type
    ``dict``). In a dict only the keys pyBYD decodes are checked against the
    container type pyBYD keeps: any other text, a plan called ``Infinity``
    say, is in.
    """
    if isinstance(value, str):
        return wanted is None or _decodes_safely(value, wanted)
    if isinstance(value, list):
        return all(in_domain(item, wanted=None) for item in value)
    if isinstance(value, dict):
        return all(in_domain(item, wanted=JSON_KEYS.get(key)) for key, item in value.items())
    return True


# ---------------------------------------------------------------------------
# Reply models
# ---------------------------------------------------------------------------


@group.oracle("models.standby.constants")
def constants() -> dict[str, Any]:
    names = [
        "COMMAND_CLIMATE", "COMMAND_SEATS", "COMMAND_STEERING_WHEEL", "EXECUTE_ONCE", "EXECUTE_DAILY",
        "EXECUTE_WEEKLY", "PLAN_TYPE_PRESET", "PLAN_TYPE_USER",
    ]
    return {name: getattr(standby, name) for name in names}


command_cases = register_model(
    group,
    "models.standby.StandbyPlanCommand",
    StandbyPlanCommand,
    samples=[
        COMMAND,
        *CAPTURED_COMMANDS,
        {"commandType": "OPENAIR", "commandId": 3, "controlParamsMap": '{"a": 1.0, "b": [1, {"c": null}]}'},
        {"command_type": "OPENAIR", "control_params_map": PARAMS},
    ],
)
list_cases = register_model(
    group,
    "models.standby.StandbyPlanList",
    StandbyPlanList,
    samples=[
        {"vin": VIN, "data": [PLAN]},
        {"vin": VIN, "data": [PLAN, NEW_PLAN, PLAN_SAMPLES[-1]]},
        {"vin": VIN, "data": []},
        {"vin": VIN, "plans": [NEW_PLAN], "data": [PLAN]},
        {"data": [PLAN_SAMPLES[3]]},
    ],
)
register_model(group, "models.standby.StandbyPlanRunResult", StandbyPlanRunResult, samples=RUN_SAMPLES)

plan_cases = ModelCases(StandbyPlan, PLAN_SAMPLES)
PLAN_PROPERTIES = property_names(StandbyPlan)


def plan_answer(plan: StandbyPlan) -> dict[str, Any]:
    """The dump and every property; a property that raises is ``{"error": "<class>"}``."""
    properties: dict[str, Any] = {}
    for name in PLAN_PROPERTIES:
        try:
            properties[name] = jsonable(getattr(plan, name))
        except (ValueError, TypeError) as error:
            properties[name] = {"error": type(error).__name__}
    return {"dump": jsonable(plan.model_dump(mode="json")), "properties": properties}


@group.oracle("models.standby.StandbyPlan")
def standby_plan(raw: Any, context: Any = None) -> dict[str, Any]:
    return plan_answer(StandbyPlan.model_validate(raw, context=context))


@group.oracle("models.standby.StandbyPlan.command")
def plan_command(raw: Any, command_type: Any) -> Any:
    found = StandbyPlan.model_validate(raw).command(command_type)
    return None if found is None else jsonable(found.model_dump(mode="json"))


@group.oracle("models.standby.StandbyPlan.climate")
def plan_climate(raw: Any, temp_offset: float | None = None) -> Any:
    plan = StandbyPlan.model_validate(raw)
    found = plan.climate() if temp_offset is None else plan.climate(temp_offset=temp_offset)
    return None if found is None else jsonable(found.model_dump(mode="json"))


#: ``controlParams`` and ``controlParamsMap`` as BYD and the awkward world send them.
MAP_VALUES: list[Any] = [
    PARAMS_TEXT, PARAMS, json.dumps(SEAT_PARAMS), json.dumps(WHEEL_PARAMS), "{}", "[]", "[1]", '"text"', "5", "null",
    "true", "", "  ", "--", "x", "{not json", PARAMS_TEXT[:-1], " " + PARAMS_TEXT + "\n", '{"a":-0}', '{"a":1,"a":2}',
    '{"比亚迪":"é","n":1.5,"l":[1,{"k":null}]}', "﻿{}", 5, 1.5, True, None, [1], [PARAMS], {}, {"mainHeat": [1]},
    {"mainSettingTemp": "5", "timeSpan": 9, "cycleMode": "2"}, {"mainSettingTemp": True, "timeSpan": True, "cycleMode": 2.0},
    {"mainSettingTemp": 2**64 - 1, "timeSpan": 5.0}, {"mainSettingTemp": 17, "timeSpan": 5, "cycleMode": 2},
    {"mainHeat": 1.0, "mainVentilation": True, "copilotHeat": "3", "copilotVentilation": 3},
    {"mainHeat": 2, "mainVentilation": 0, "copilotHeat": None, "copilotVentilation": {"a": 1}},
    {"steeringWheelHeatState": 1.0}, {"steeringWheelHeatState": 3}, {"steeringWheelHeatState": "1"},
    " -Infinity", "NaN", "9503770829.25e400", "100000000000000000000", "[NaN]", '  [1, {"a": Infinity}]',
]
COMMAND_TYPES: list[Any] = [
    "OPENAIR", "OPENAIR", "VENTILATIONHEATING", "STEERINGWHEELHEATING", "CLOSEAIR", "openair", "", "--", None, 5, " OPENAIR",
]
COMMAND_IDS: list[Any] = ["3", 3, 1.5, True, [1, "a"], {"a": 1}, "--", "", None, 1e300, 2**64 - 1, -7, "比亚迪"]
CONTROL_PARAMS: list[Any] = [
    PLAN["controlParams"], json.dumps(CAPTURED_COMMANDS), json.dumps(CAPTURED_COMMANDS)[:-2], "[]", "[1]", "{}", '{"a":1}',
    "zz", "", "--", "null", '"OPENAIR"', '[{"commandType":"OPENAIR"}]', '[{"commandType":"STEERINGWHEELHEATING"}, 5]',
    [{"commandType": "Y"}], CAPTURED_COMMANDS, [], {}, 5, None, True,
    json.dumps([{"commandType": "OPENAIR", "controlParamsMap": PARAMS_TEXT, "commandId": 9}]),
    "NaN", "1e400", '{"a": NaN}',
]
COMMAND_LISTS: list[Any] = [
    [COMMAND], CAPTURED_COMMANDS, [], None, "", "x", 5, {"a": 1}, [5], [COMMAND, "x"], [{}], 0, False,
    [CAPTURED_COMMANDS[2], CAPTURED_COMMANDS[0]], [CAPTURED_COMMANDS[1]],
    [{"commandType": "OPENAIR"}, {"commandType": "OPENAIR", "controlParamsMap": PARAMS}],
]
WEEKLY_DAYS: list[Any] = [
    "4,5,6", "6, 4 ,4", "", " ", "--", "x,1", "١,2", "²", "1,²", "1;2", "-1,2", "1.5", "+1", "1,,2", "1_0", "0x1", "７",
    "0,1,2,3,4,5,6", "7", "8,9", "99999999999999999999,1", "99999999999999999999,²", [3, 1], ["2", 1.0], [], [7], [-1],
    [1.5], ["x"], 5, {"a": 1}, True, None, "0", " 3 ", "3\n,\t4",
]
START_TIMES: list[Any] = [
    "13:30", "00:00", "23:59", "24:00", "25:00", "9:30", "10:00\n", " 10:00", "1٢:3٤", "１0:00", "2٣:00", "19:6٠", "", "--",
    None, 1000, "10-00", "10:0", "100:00", "ab:cd",
]
EXECUTE_TYPES: list[Any] = ["0", "1", "2", 0, 1, 2, 2.0, True, None, "", "7", [2], " 2", "--"]
EXECUTE_TIMES: list[Any] = [
    0, 1795177800000, 1795371000000, "1795371000000", 1795371000, None, "", "--", -1, "2026-11-22T18:10:00+00:00",
    "2026-11-22T19:10:00+01:00", "2026-11-22T18:10:00", "2026-11-22T10:00:00-00:30:15.5", "garbage", 1.9, True,
]
IDS: list[Any] = [
    1216038691305533440, "1216038691305533440", "7", 7, 0, -5, 1.5, True, [1], {"a": 1}, "", "--", None, 2**63 - 1, 2**63,
    2**64 - 1, 1e300, "١٢", "²", "abc", " 7 ", "007",
]
PLAN_POOLS: dict[str, list[Any]] = {
    "id": IDS,
    "plan_name": ["Test plan", "Conducción confort", "比亚迪", "", "nan", "--", None, 5, ["a"], " x ", "Infinity", "NaN", "[1e999]", "100000000000000000000"],
    "plan_type": [1, 2, "1", 1.0, True, None, 0, 3, "x", 2**53],
    "default_plan": [0, 1, "1", None, True, 2, "x"],
    "execute_type": EXECUTE_TYPES,
    "start_time": START_TIMES,
    "weekly_days": WEEKLY_DAYS,
    "execute_time": EXECUTE_TIMES,
    "rec_execute_time": EXECUTE_TIMES,
    "time_zone": ["Europe/Madrid", "UTC", "", None, 5],
    "commands": COMMAND_LISTS,
}
TEMP_OFFSETS: list[Any] = [None, 14, 14.0, 16, 16.5, 0, -14, 1e308, -1e308, 2.5]


def _random_map(rng: random.Random) -> Any:
    roll = rng.random()
    if roll < 0.5:
        return rng.choice(MAP_VALUES)
    values = {
        key: rng.choice([0, 1, 2, 3, 5, 17, 1.0, 2.0, "1", True, False, None, [1], {"a": 1}, 9, -1, 2**53])
        for key in rng.sample(
            ["mainSettingTemp", "timeSpan", "cycleMode", "mainHeat", "mainVentilation", "copilotHeat",
             "copilotVentilation", "steeringWheelHeatState", "chairType", random_text(rng, 6)],
            k=rng.randint(0, 6),
        )
    }
    if roll < 0.75:
        return values
    text = json.dumps(values, ensure_ascii=rng.random() < 0.5)
    return text if roll < 0.95 else text[: rng.randint(0, len(text))]


def _random_command(rng: random.Random) -> Any:
    roll = rng.random()
    if roll < 0.08:
        return random_json(rng)
    if roll < 0.2:
        return mutate_fields(rng, command_cases)
    command: dict[str, Any] = {}
    if rng.random() < 0.9:
        command[rng.choice(["commandType", "commandType", "command_type"])] = rng.choice(COMMAND_TYPES)
    if rng.random() < 0.5:
        command[rng.choice(["commandId", "command_id"])] = rng.choice(COMMAND_IDS)
    if rng.random() < 0.9:
        command[rng.choice(["controlParamsMap", "controlParamsMap", "control_params_map"])] = _random_map(rng)
    if rng.random() < 0.1:
        command[rng.choice(UNKNOWN_KEYS)] = random_scalar(rng)
    return command


def _random_plan(rng: random.Random) -> Any:
    roll = rng.random()
    if roll < 0.25:
        return plan_cases.fuzz(rng)
    plan = mutate_fields(rng, plan_cases, PLAN_POOLS) if roll < 0.6 else copy.deepcopy(rng.choice(PLAN_SAMPLES))
    if not isinstance(plan, dict):
        return plan
    if rng.random() < 0.6:
        commands = [_random_command(rng) for _ in range(rng.choice([0, 1, 1, 2, 3, 4]))]
        key = rng.choice(["controlParamsList", "controlParamsList", "commands"])
        plan[key] = commands
        if rng.random() < 0.2:
            plan["controlParams"] = json.dumps(commands) if json_safe(commands) else "[]"
            if rng.random() < 0.6:
                plan[key] = rng.choice([None, [], "", 0])
    if rng.random() < 0.25:
        plan["controlParams"] = rng.choice(CONTROL_PARAMS)
        if rng.random() < 0.7:
            plan.pop("controlParamsList", None)
    for name, key in [("weekly_days", "weeklyDays"), ("start_time", "startTime"), ("execute_type", "executeType"),
                      ("execute_time", "executeTime"), ("id", "id")]:
        if rng.random() < 0.2:
            plan[key] = rng.choice(PLAN_POOLS[name])
    return plan


def _days_fit(plan: Any) -> bool:
    """Whether the days pyBYD reads out of ``weeklyDays`` text fit openbyd's integers
    (a call that does not answer them would not be left out for it otherwise)."""
    try:
        days = StandbyPlan.model_validate(plan).weekly_days
    except ValueError:
        return True
    return all(-(2**63) <= day < 2**63 for day in days)


def _usable(plan: Any) -> bool:
    return json_safe(plan) and in_domain(plan) and _days_fit(plan)


def _plan_recorded() -> Iterator[Any]:
    yield from plan_cases.recorded()
    base = PLAN_SAMPLES[0]
    for name, pool in PLAN_POOLS.items():
        spec = next(spec for spec in plan_cases.specs if spec.name == name)
        for value in pool:
            yield {**{key: item for key, item in base.items() if key not in spec.all_keys}, spec.keys[0]: value}
    without_list = {key: value for key, value in base.items() if key != "controlParamsList"}
    for value in CONTROL_PARAMS:
        yield {**without_list, "controlParams": value}
        yield {**without_list, "controlParams": value, "controlParamsList": []}
        yield {**without_list, "controlParamsList": None, "controlParams": value, "x": 1}
        yield {**without_list, "controlParams": value, "commands": [CAPTURED_COMMANDS[2]]}
    for map_value in MAP_VALUES:
        for command_type in ["OPENAIR", "VENTILATIONHEATING", "STEERINGWHEELHEATING"]:
            yield {**NEW_PLAN, "controlParamsList": [{"commandType": command_type, "controlParamsMap": map_value}]}
    for command_type in COMMAND_TYPES:
        for command_id in COMMAND_IDS[:6]:
            yield {**NEW_PLAN, "controlParamsList": [{"commandType": command_type, "commandId": command_id}]}
    # Every schedule kind, and each way the plan's own data is not a schedule.
    for execute_type in EXECUTE_TYPES:
        for start_time, weekly_days, execute_time in [
            ("13:30", "4,5,6", 1795177800000), ("13:30", "", 1795177800000), ("", "4,5,6", 0), ("25:00", "1", 0),
            ("10:00", [8], 0), ("10:00", "7", 1795177800000), (None, None, 0), (None, None, 1795371000000),
            ("1٢:3٤", [6, 0, 6], None), ("10:00", "99999999999999999999,1", 0),
        ]:
            yield {**NEW_PLAN, "executeType": execute_type, "startTime": start_time, "weeklyDays": weekly_days,
                   "executeTime": execute_time}


@group.recorded
def plan_recorded() -> Iterator[Case]:
    for index, payload in enumerate(_plan_recorded()):
        if not _usable(payload):
            continue
        yield "models.standby.StandbyPlan", {"raw": payload, "context": None}
        if index % 5 == 0:
            command_types = ["OPENAIR", "VENTILATIONHEATING", "STEERINGWHEELHEATING", "", None, "openair"]
            yield "models.standby.StandbyPlan.command", {"raw": payload, "command_type": command_types[index // 5 % 6]}
            yield "models.standby.StandbyPlan.command", {"raw": payload, "command_type": "OPENAIR"}
            yield "models.standby.StandbyPlan.climate", {"raw": payload, "temp_offset": TEMP_OFFSETS[index // 5 % 5]}
    for map_value in MAP_VALUES:
        if not in_domain(map_value):
            continue
        plan = {"controlParamsList": [{"commandType": "OPENAIR", "controlParamsMap": map_value}]}
        for temp_offset in TEMP_OFFSETS:
            yield "models.standby.StandbyPlan.climate", {"raw": plan, "temp_offset": temp_offset}
    for map_value in MAP_VALUES + COMMAND_IDS:
        if json_safe(map_value) and in_domain(map_value):
            yield "models.standby.StandbyPlanCommand", {"raw": {**COMMAND, "controlParamsMap": map_value}, "context": None}
            yield "models.standby.StandbyPlanCommand", {"raw": {**COMMAND, "commandId": map_value}, "context": None}
    for commands in COMMAND_LISTS:
        yield "models.standby.StandbyPlanList", {"raw": {"vin": VIN, "data": [{**NEW_PLAN, "controlParamsList": commands}]}, "context": None}
    for data in ["--", "x", 5, None, {}, [5], [PLAN, "x"], [[PLAN]]]:
        yield "models.standby.StandbyPlanList", {"raw": {"vin": VIN, "data": data}, "context": None}
    yield "models.standby.constants", {}


@group.fuzz
def plan_fuzz(rng: random.Random) -> Iterator[Case]:
    while True:
        plan = _random_plan(rng)
        if not _usable(plan):
            continue
        roll = rng.random()
        if roll < 0.6:
            yield "models.standby.StandbyPlan", {"raw": plan, "context": None}
        elif roll < 0.75:
            command_type = rng.choice(COMMAND_TYPES + [random_text(rng, 8)])
            yield "models.standby.StandbyPlan.command", {"raw": plan, "command_type": command_type}
        elif roll < 0.9:
            offset = rng.choice(TEMP_OFFSETS + [rng.uniform(-50, 50), rng.randint(-20, 40)])
            yield "models.standby.StandbyPlan.climate", {"raw": plan, "temp_offset": offset}
        else:
            plans = [plan] + [candidate for candidate in (_random_plan(rng) for _ in range(rng.randint(0, 2))) if _usable(candidate)]
            raw = {"vin": rng.choice([VIN, "", None, 5]), rng.choice(["data", "data", "plans"]): plans}
            yield "models.standby.StandbyPlanList", {"raw": raw, "context": None}
        command = _random_command(rng)
        if _usable(command):
            yield "models.standby.StandbyPlanCommand", {"raw": command, "context": None}


# ---------------------------------------------------------------------------
# Settings built from keyword arguments
# ---------------------------------------------------------------------------


@group.oracle("models.standby.PlanClimate")
def plan_climate_settings(kwargs: dict[str, Any]) -> dict[str, Any]:
    return {"dump": jsonable(PlanClimate(**kwargs).model_dump(mode="json"))}


@group.oracle("models.standby.PlanSeats")
def plan_seats(kwargs: dict[str, Any]) -> dict[str, Any]:
    seats = PlanSeats(**kwargs)
    return {"dump": jsonable(seats.model_dump(mode="json")), "properties": {"active": seats.active}}


@group.oracle("models.standby.StandbySchedule")
def schedule(kwargs: dict[str, Any]) -> dict[str, Any]:
    return {"dump": jsonable(StandbySchedule(**kwargs).model_dump(mode="json"))}


@group.oracle("models.standby.StandbySchedule.off")
def schedule_off() -> dict[str, Any]:
    return {"dump": jsonable(StandbySchedule.off().model_dump(mode="json"))}


@group.oracle("models.standby.StandbySchedule.once")
def schedule_once(at: Any) -> dict[str, Any]:
    return {"dump": jsonable(StandbySchedule.once(at).model_dump(mode="json"))}


@group.oracle("models.standby.StandbySchedule.daily")
def schedule_daily(at: Any) -> dict[str, Any]:
    return {"dump": jsonable(StandbySchedule.daily(at).model_dump(mode="json"))}


@group.oracle("models.standby.StandbySchedule.weekly")
def schedule_weekly(start_time: Any, weekly_days: list[int]) -> dict[str, Any]:
    return {"dump": jsonable(StandbySchedule.weekly(start_time, weekly_days).model_dump(mode="json"))}


@group.oracle("models.standby.StandbyPlanCommand.climate", errors=(ValueError, TypeError))
def command_climate(temperature: Any, duration: Any, recirculation: Any, temp_offset: Any) -> dict[str, Any]:
    command = StandbyPlanCommand.climate(temperature, duration, recirculation=recirculation, temp_offset=temp_offset)
    return {"dump": jsonable(command.model_dump(mode="json"))}


@group.oracle("models.standby.StandbyPlanCommand.seats")
def command_seats(settings: dict[str, Any]) -> dict[str, Any]:
    return {"dump": jsonable(StandbyPlanCommand.seats(PlanSeats(**settings)).model_dump(mode="json"))}


@group.oracle("models.standby.StandbyPlanCommand.steering_wheel")
def command_steering_wheel(on: Any) -> dict[str, Any]:
    return {"dump": jsonable(StandbyPlanCommand.steering_wheel(on).model_dump(mode="json"))}


#: Datetimes as pydantic reads them: aware, naive, dates, timestamps, and near misses of each.
DATETIMES: list[Any] = [
    "2026-11-22T19:10:00+01:00", "2026-11-22 19:10:00Z", "2026-11-22t19:10z", "2026-11-22_19:10:00", "2026-11-22T19:10",
    "2026-11-18T07:00:00", "2026-11-22T19", "2026-11-22", " 2026-11-22T19:10:00", "2026-11-22T19:10:00 ",
    "2026-11-22T19:10:00.1234567+0100", "2026-11-22T19:10:00,5", "2026-11-22T19:10:00.", "2026-11-22T19:10:00+01",
    "2026-11-22T19:10:00+1", "2026-11-22T19:10:00−01:00", "2026-11-22T19:10:00−0130", "2026-11-22T19:10:00−",
    "2026-11-22T19:10:00+23:59", "2026-11-22T19:10:00-23:59", "2026-11-22T19:10:00+24:00", "2026-11-22T19:10:00-00:00",
    "2026-11-22T19:10:00+00:60", "2026-11-22T19:10:00+2359", "2026-11-22T19:10:00+01:0", "0000-01-01T00:00:00",
    "0000-01-01", "0001-01-01T00:00:00+01:00", "0001-01-01", "9999-12-31T23:59:59.999999-01:00", "9999-12-31",
    "2026-02-29T00:00:00", "2024-02-29T00:00:00", "1900-02-29T00:00", "2000-02-29T00:00", "2026-13-01T00:00:00",
    "2026-04-31T00:00", "2026-11-22T24:00:00", "2026-11-22T23:60:00", "2026-11-22T23:59:60", "20261122T191000",
    "2026-11-22T19:10.5", "2026-11-22T19:10Z", "2026-11-22T19:10+01:00", "2026-11-22T19:10:00.5+01:00",
    "2026-11-22T19:10:00,5Z", "2026-11-22T19:10:00.Z", "2026-11-22T19:10:00.123456789012345678901234567890Z",
    "2026-11-22T19:10:00+01:00:30", "2026-11-22T19:10:00 +01:00", "2026-11-22T19:10:00UTC", "2026-11-22T19:10:00Zx",
    "２０２６-11-22T19:10:00", "2026-11-22T19:10:00+٠1:00", "2026-11-22é19:10:00", "2026-11-22\t19:10:00", "2026-11-22x",
    "2026-1-22", "2026/11/22T19:10:00", "2026-11-22T1:10:00", "2026-11-22T19-10-00", "2026-11-22T19:10:00é01:00",
    "1700000000", "1700000000123", "+1700000000", "-1", "0", "-0", "007", "1700000000.5", "1700000000123.5", "1e9",
    "1.5e3", "1.5E-2", "1.e3", ".5", "5.", ".", "-.5", "+.5", "-", "+", "1.5.5", "1.-5", " 1", "1 ", "--1", "1_000",
    "0x10", "１７", "", "inf", "nan", "NaN", "1.5e", "1.5e+", "1.5e400", "1.2345678", "1.9999995", "0.9999996",
    "-1.0000005", "0.0000005", "20000000000", "20000000001", "-20000000000", "-20000000001", "20000000000.5",
    "-20000000000.9", "20000000001.9999999", "253402300799", "253402300799999", "253402300800000", "253402300799999.9",
    "253402300799999.9999", "-62135596800000", "-62135596800001", "9223372036854775807", "9223372036854775808",
    1700000000, 1700000000123, 0, -1, 20000000000, 20000000001, -20000000000, -20000000001, 1.5, -1.5,
    1700000000.123456, 1700000000123.5, 0.9999996, 0.9999994, -4e-07, -0.9999996, 20000000000.5, -20000000000.5,
    253402300799.5, 253402300799999.9, 253402300799999, 253402300800000, -62135596800000, -62135596800001, 1e300, -1e300,
    2**63, 2**64 - 1, -(2**63), 2**63 - 1, 1e15, 2.5e14, 5e-324, -0.0, 48306239071696.0, -3369719350330.0693,
    True, False, [], {}, [1700000000], {"at": 1}, None,
]
REPEATS: list[Any] = ["off", "once", "daily", "weekly"]
ODD_REPEATS: list[Any] = ["OFF", " off", "Once", "", "--", 1, 0, None, True, ["off"], {"repeat": "off"}, "weekly "]
SCHEDULE_DAYS: list[Any] = [
    [4, 5], [0], [6], [0, 1, 2, 3, 4, 5, 6], [6, 0, 6], [], [7], [-1], [1.0, "2", True], [1.5], ["x"], [[1]], "12", {"1": 2},
    5, None, [0, 7], [" 3 "], [2**53],
]
SCHEDULE_SAMPLES: list[dict[str, Any]] = [
    {"repeat": "off"},
    {"repeat": "once", "at": "2026-11-22T19:10:00+01:00"},
    {"repeat": "daily", "at": "2026-11-18T23:12:00+01:00"},
    {"repeat": "weekly", "start_time": "12:55", "weekly_days": [4, 5]},
    # What pyBYD's own tests reject.
    {"repeat": "weekly", "start_time": "25:00", "weekly_days": [1]},
    {"repeat": "weekly", "start_time": "9:30", "weekly_days": [1]},
    {"repeat": "weekly", "start_time": "10:00", "weekly_days": [7]},
    {"repeat": "weekly", "start_time": "10:00", "weekly_days": []},
    {"repeat": "once"},
    {"repeat": "daily", "at": "2026-11-18T07:00:00"},
    # A schedule that is off does not look at the rest.
    {"repeat": "off", "at": "2026-11-18T07:00:00", "start_time": "zz", "weekly_days": [9, 9]},
    {"repeat": "weekly", "at": "2026-11-18T07:00:00", "start_time": "00:00", "weekly_days": [0]},
    {"repeat": "off", "extra": 1, "startTime": "x", "weeklyDays": [1]},
    {},
    {"repeat": "once", "at": "x", "weekly_days": "x"},
    {"at": "2026-11-22T19:10:00+01:00"},
]
CLIMATE_SAMPLES: list[dict[str, Any]] = [
    {"temperature": 19, "duration": 10},
    {"temperature": 21.5, "duration": 30, "recirculation": True},
    {"temperature": "21", "duration": "10", "recirculation": "yes", "x": 1},
    {},
    {"temperature": "nan", "duration": 10.0},
    {"temperature": 1e308, "duration": 2**53, "recirculation": 0},
    {"Temperature": 19, "duration": 10},
]
SEAT_SAMPLES: list[dict[str, Any]] = [
    {},
    {"driver_heat": True, "driver_ventilation": False, "passenger_heat": False, "passenger_ventilation": True},
    {"driver_heat": True, "passenger_heat": False},
    {"passenger_heat": False},
    {"driver_heat": 1, "passenger_heat": "off", "driverHeat": True},
    {"driver_heat": 2},
    {"driver_ventilation": None, "passenger_ventilation": "on"},
]
SEAT_FIELDS = ["driver_heat", "driver_ventilation", "passenger_heat", "passenger_ventilation"]
SEAT_VALUES: list[Any] = BOOL_VALUES + [None] + BOOL_AWKWARD + ["x", [], {}]
DURATIONS: list[Any] = [10, 15, 20, 25, 30, 10.0, 30.0, 11, 0, -10, 5, "10", None, True, False, 1.5, [10], {"a": 1}, 2**64 - 1, 1e300]
TEMPERATURES: list[Any] = [19, 21, "21.5", 15, 31, 33, 14.9, 22.4, 22.5, None, "x", True, 1e308, "nan", "inf", [], {}, 17, 16]
TRUTHY: list[Any] = [True, False, 0, 1, "", "x", None, [], [0], {}, {"a": 1}, 0.0, -0.0, 1.5]
CLIMATE_OFFSETS: list[Any] = [14, 14, 14.0, 16, "16", 0, None, 15.5, "inf", "x", 1e308]


def random_datetime(rng: random.Random) -> Any:
    """What ``at`` may be given as: ISO-like text, numeric text and numbers, often slightly off."""
    roll = rng.random()
    if roll < 0.2:
        return rng.choice(DATETIMES)
    if roll < 0.4:
        whole = "".join(rng.choice("0123456789") for _ in range(rng.randint(0, 16)))
        fraction = "".join(rng.choice("0123456789") for _ in range(rng.randint(0, 9)))
        exponent = rng.choice(["", "", "", "e3", "E-2", "e", "e+1", "e400"])
        return rng.choice(["", "", "-", "+"]) + whole + rng.choice(["", ".", "."]) + fraction + exponent
    if roll < 0.75:
        year = rng.choice(["2026", "0001", "9999", "0000", "1970", "2024", "1900", "2000"])
        text = f"{year}-{rng.choice(['01', '02', '11', '12', '13', '00', '04'])}-{rng.choice(['01', '29', '30', '31', '00', '22'])}"
        if rng.random() < 0.1:
            return text
        text += rng.choice(["T", "T", "t", " ", "_", "x", ""])
        text += rng.choice(["00", "19", "23", "24"]) + rng.choice([":", ":", ""]) + rng.choice(["00", "10", "59", "60"])
        if rng.random() < 0.7:
            text += rng.choice([":", ":", ""]) + rng.choice(["00", "30", "59", "60"])
            if rng.random() < 0.5:
                text += rng.choice([".", ","]) + "".join(rng.choice("0123456789") for _ in range(rng.randint(0, 9)))
        text += rng.choice([
            "", "", "Z", "z", "+00:00", "-00:00", "+05:30", "-0800", "+01", "+23:59", "+24:00", "−01:00", "−0130",
            "+00:60", "+5", "Z ", "+0:00", "-23:59", "+2360",
        ])
        if rng.random() < 0.08 and text:
            position = rng.randrange(len(text))
            text = text[:position] + text[position + 1 :]
        return text
    watershed = 20_000_000_000
    return rng.choice([
        lambda: rng.randint(-3 * watershed, 3 * watershed),
        lambda: rng.randint(-(2**63), 2**64 - 1),
        lambda: rng.randint(-62135596800005, 253402300800005),
        lambda: rng.uniform(-3 * watershed, 3 * watershed),
        lambda: rng.uniform(-1, 1) * 10.0 ** rng.randint(-10, 20),
        lambda: rng.choice([watershed, -watershed, watershed + 1, float(watershed), watershed + 0.5, 0.9999995, -1e-7]),
    ])()


def _schedule_kwargs(rng: random.Random) -> dict[str, Any]:
    kwargs = copy.deepcopy(rng.choice(SCHEDULE_SAMPLES)) if rng.random() < 0.3 else {}
    if rng.random() < 0.9:
        kwargs["repeat"] = rng.choice(REPEATS * 4 + ODD_REPEATS)
    if rng.random() < 0.7:
        kwargs["at"] = random_datetime(rng)
    if rng.random() < 0.6:
        kwargs["start_time"] = rng.choice(START_TIMES + [random_text(rng, 6), f"{rng.randint(0, 29):02}:{rng.randint(0, 69):02}"])
    if rng.random() < 0.6:
        days = rng.choice(SCHEDULE_DAYS + [[rng.randint(-1, 8) for _ in range(rng.randint(0, 5))]])
        kwargs["weekly_days"] = days
    if rng.random() < 0.05:
        kwargs[rng.choice(UNKNOWN_KEYS + ["startTime", "weeklyDays"])] = random_scalar(rng)
    return kwargs


def _ints_fit(kwargs: dict[str, Any]) -> bool:
    """Whether every day that becomes a model ``int`` stays within what openbyd holds."""
    days = kwargs.get("weekly_days")
    return not isinstance(days, list) or all(model_int_safe(day) for day in days)


@group.recorded
def settings_recorded() -> Iterator[Case]:
    for kwargs in SCHEDULE_SAMPLES:
        yield "models.standby.StandbySchedule", {"kwargs": kwargs}
    for at in DATETIMES:
        for repeat in ["once", "off"]:
            yield "models.standby.StandbySchedule", {"kwargs": {"repeat": repeat, "at": at}}
        yield "models.standby.StandbySchedule.once", {"at": at}
    for at in DATETIMES[:12] + [1700000000, None, True]:
        yield "models.standby.StandbySchedule.daily", {"at": at}
        yield "models.standby.StandbySchedule", {"kwargs": {"repeat": "weekly", "at": at, "start_time": "10:00", "weekly_days": [1]}}
    for repeat in REPEATS + ODD_REPEATS:
        yield "models.standby.StandbySchedule", {"kwargs": {"repeat": repeat}}
        yield "models.standby.StandbySchedule", {
            "kwargs": {"repeat": repeat, "at": "2026-11-22T19:10:00Z", "start_time": "10:00", "weekly_days": [1]}
        }
    for start_time in START_TIMES:
        yield "models.standby.StandbySchedule", {"kwargs": {"repeat": "weekly", "start_time": start_time, "weekly_days": [1]}}
        yield "models.standby.StandbySchedule", {"kwargs": {"repeat": "off", "start_time": start_time}}
        yield "models.standby.StandbySchedule.weekly", {"start_time": start_time, "weekly_days": [5, 4]}
    for days in SCHEDULE_DAYS:
        for repeat in ["weekly", "off"]:
            yield "models.standby.StandbySchedule", {"kwargs": {"repeat": repeat, "start_time": "10:00", "weekly_days": days}}
    for days in [[5, 4], [5, 4, 5], [], [0], [6, 0], [7], [-1, 3], [0, 1, 2, 3, 4, 5, 6, 6, 0], [2**53, 1]]:
        yield "models.standby.StandbySchedule.weekly", {"start_time": "12:55", "weekly_days": days}
    yield "models.standby.StandbySchedule.off", {}

    for kwargs in CLIMATE_SAMPLES:
        yield "models.standby.PlanClimate", {"kwargs": kwargs}
    for value in FLOAT_VALUES + FLOAT_AWKWARD + SENTINELS:
        yield "models.standby.PlanClimate", {"kwargs": {"temperature": value, "duration": 10}}
    for value in INT_VALUES + INT_AWKWARD + SENTINELS:
        if model_int_safe(value):
            yield "models.standby.PlanClimate", {"kwargs": {"temperature": 21.0, "duration": value}}
    for value in BOOL_VALUES + BOOL_AWKWARD + [None]:
        yield "models.standby.PlanClimate", {"kwargs": {"temperature": 21.0, "duration": 10, "recirculation": value}}

    for kwargs in SEAT_SAMPLES:
        yield "models.standby.PlanSeats", {"kwargs": kwargs}
        yield "models.standby.StandbyPlanCommand.seats", {"settings": kwargs}
    for name in SEAT_FIELDS:
        for value in SEAT_VALUES:
            yield "models.standby.PlanSeats", {"kwargs": {name: value}}
    for driver in [True, False, None]:
        for passenger in [True, False, None]:
            settings = {"driver_heat": driver, "driver_ventilation": passenger, "passenger_heat": passenger,
                        "passenger_ventilation": driver}
            yield "models.standby.StandbyPlanCommand.seats", {"settings": settings}

    def climate(**changes: Any) -> Case:
        base = {"temperature": 19, "duration": 10, "recirculation": False, "temp_offset": 14}
        return "models.standby.StandbyPlanCommand.climate", {**base, **changes}

    yield climate()
    yield climate(temperature=22, duration=30)
    yield climate(temperature=20, recirculation=True)
    yield climate(temperature=21, temp_offset=16)
    for duration in DURATIONS:
        yield climate(duration=duration)
        yield climate(duration=duration, temperature=99)
    for temperature in TEMPERATURES:
        yield climate(temperature=temperature)
        yield climate(temperature=temperature, temp_offset=16)
    for recirculation in TRUTHY:
        yield climate(recirculation=recirculation)
    for temp_offset in CLIMATE_OFFSETS:
        yield climate(temp_offset=temp_offset)
    for on in TRUTHY:
        yield "models.standby.StandbyPlanCommand.steering_wheel", {"on": on}


@group.fuzz
def settings_fuzz(rng: random.Random) -> Iterator[Case]:
    while True:
        kwargs = _schedule_kwargs(rng)
        if json_safe(kwargs) and _ints_fit(kwargs):
            yield "models.standby.StandbySchedule", {"kwargs": kwargs}
        at = random_datetime(rng)
        if json_safe(at):
            yield f"models.standby.StandbySchedule.{rng.choice(['once', 'daily'])}", {"at": at}
        days = [rng.choice([0, 1, 2, 3, 4, 5, 6, 6, 7, -1, 100, -(2**53)]) for _ in range(rng.randint(0, 8))]
        start_time = rng.choice(START_TIMES + [f"{rng.randint(0, 29):02}:{rng.randint(0, 69):02}", random_text(rng, 6)])
        yield "models.standby.StandbySchedule.weekly", {"start_time": start_time, "weekly_days": days}

        climate = copy.deepcopy(rng.choice(CLIMATE_SAMPLES)) if rng.random() < 0.4 else {}
        if rng.random() < 0.8:
            climate["temperature"] = rng.choice(FLOAT_VALUES + FLOAT_AWKWARD + [random_numeric_text(rng), random_scalar(rng)])
        if rng.random() < 0.8:
            climate["duration"] = rng.choice(INT_VALUES + INT_AWKWARD + [random_numeric_text(rng), random_scalar(rng)])
        if rng.random() < 0.5:
            climate["recirculation"] = rng.choice(BOOL_VALUES + BOOL_AWKWARD + [None, random_scalar(rng)])
        if json_safe(climate) and model_int_safe(climate.get("duration")):
            yield "models.standby.PlanClimate", {"kwargs": climate}

        seats = {name: rng.choice(SEAT_VALUES + [random_scalar(rng)]) for name in rng.sample(SEAT_FIELDS, k=rng.randint(0, 4))}
        if rng.random() < 0.1:
            seats[rng.choice(UNKNOWN_KEYS + ["driverHeat"])] = random_scalar(rng)
        if json_safe(seats):
            if rng.random() < 0.5:
                yield "models.standby.PlanSeats", {"kwargs": seats}
            else:
                yield "models.standby.StandbyPlanCommand.seats", {"settings": seats}

        arguments = {
            "temperature": rng.choice(TEMPERATURES + [rng.uniform(10, 36), rng.randint(14, 34), random_scalar(rng)]),
            "duration": rng.choice(DURATIONS + [10, 15, 20, 25, 30, random_scalar(rng)]),
            "recirculation": rng.choice(TRUTHY + [random_json(rng)]),
            "temp_offset": rng.choice(CLIMATE_OFFSETS + [rng.uniform(0, 20), random_scalar(rng)]),
        }
        if json_safe(arguments):
            yield "models.standby.StandbyPlanCommand.climate", arguments
        on = random_json(rng)
        if json_safe(on):
            yield "models.standby.StandbyPlanCommand.steering_wheel", {"on": on}


def skip_outside_domain(target: Group) -> None:
    """Leave out cases whose decoded plan JSON pyBYD keeps cannot fit in ``serde_json``."""

    def filtered(source: Callable[..., Iterable[Any]]) -> Callable[..., Iterable[Any]]:
        def wrapped(*arguments: Any) -> Iterable[Any]:
            for call, args in source(*arguments):
                if in_domain(args):
                    yield call, args

        return wrapped

    target._recorded = [filtered(source) for source in target._recorded]
    target._fuzz = [filtered(source) for source in target._fuzz]


skip_outside_domain(group)
skip_unrepresentable_integers(group)
