"""pybyd/models/command_gating.py: which commands a vehicle may be sent.

``evaluate_command_gate`` is called with the command's value, the
capabilities as a raw ``VehicleCapabilities`` dict, and the optional
``control_params`` and ``learn_info`` mappings. Recorded cases cover every
command against fixtures with no, one or all ``functionNo`` values, failed
capability resolution, malformed capability data, every seat target and
every shape of learn info; the fuzz source draws random capability sets.
"""

from __future__ import annotations

import random
from collections.abc import Iterator
from typing import Any

from pybyd.models.command_gating import (
    CommandGateRule,
    CommandGateVerdict,
    _evaluate_rule,
    command_gate_rules,
    evaluate_all_command_gates,
    evaluate_command_gate,
    known_command_function_nos,
)
from pybyd.models.control import RemoteCommand
from pybyd.models.latest_config import VehicleCapabilities, registered_latest_config_function_nos

from parity.framework import Case, Group, random_text
from parity.models_support import json_safe, jsonable, random_json, random_scalar, register_model

group = Group("models_command_gating")
VIN = "TESTVIN0000000001"
ERRORS = (ValueError, TypeError)

register_model(
    group,
    "models.command_gating.CommandGateRule",
    CommandGateRule,
    samples=[
        {"gateId": "lock", "command": "LOCKDOOR", "requiredFunctionNos": ["1005"]},
        {"gateId": "open_windows", "command": "OPENWINDOW", "requiredFunctionNos": ["1026"],
         "requiredLearnInfoKeys": ["openWindowLearnInfo"]},
        {"gate_id": "x", "command": "BATTERYHEAT", "required_function_nos": []},
    ],
)

register_model(
    group,
    "models.command_gating.CommandGateVerdict",
    CommandGateVerdict,
    samples=[
        {"command": "LOCKDOOR", "gateId": "lock", "supported": True, "reason": "supported",
         "requiredFunctionNos": ["1005"], "matchedFunctionNos": ["1005"], "counterpartFunctionNos": [],
         "requiredLearnInfoKeys": [], "matchedLearnInfoKeys": []},
        {"command": "VENTILATIONHEATING", "gateId": "seat_target", "supported": False,
         "reason": "seat_target_unknown", "requiredFunctionNos": ["10030001"], "matchedFunctionNos": [],
         "counterpartFunctionNos": ["10030001"]},
    ],
)


@group.oracle("models.command_gating.command_gate_rules")
def rules() -> list[Any]:
    return [jsonable(rule.model_dump(mode="json")) for rule in command_gate_rules()]


@group.oracle("models.command_gating.known_command_function_nos")
def known() -> list[str]:
    return sorted(known_command_function_nos())


@group.oracle("models.command_gating.evaluate_command_gate", errors=ERRORS)
def evaluate(command: Any, capabilities: Any, control_params: Any = None, learn_info: Any = None) -> Any:
    member = RemoteCommand(command)
    caps = VehicleCapabilities.model_validate(capabilities)
    verdict = evaluate_command_gate(member, caps, control_params=control_params, learn_info=learn_info)
    return jsonable(verdict.model_dump(mode="json"))


@group.oracle("models.command_gating.evaluate_all_command_gates", errors=ERRORS)
def evaluate_all(capabilities: Any, learn_info: Any = None) -> Any:
    caps = VehicleCapabilities.model_validate(capabilities)
    return [jsonable(verdict.model_dump(mode="json"))
            for verdict in evaluate_all_command_gates(caps, learn_info=learn_info)]


@group.oracle("models.command_gating.evaluate_rule", errors=ERRORS)
def evaluate_rule(rule: Any, function_nos: list[str], learn_info: Any = None) -> Any:
    verdict = _evaluate_rule(CommandGateRule.model_validate(rule), VehicleCapabilities(),
                             function_nos=set(function_nos), learn_info=learn_info)
    return jsonable(verdict.model_dump(mode="json"))


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

COMMANDS = [member.value for member in RemoteCommand]
BAD_COMMANDS: list[Any] = ["", "lockdoor", " LOCKDOOR", "LOCK", "SMARTCHARGE", None, 1, ["LOCKDOOR"]]
COMMAND_FUNCTION_NOS = sorted(known_command_function_nos())
ALL_FUNCTION_NOS = sorted(registered_latest_config_function_nos())

# Upstream fixtures (tests/test_latest_config.py, test_rear_seat_climate.py), no identifying values.
OVERSEAS = [
    "1001", "1002", "1003", "1004", "1005", "1006", "1007", "1008", "1009", "1012", "1013", "1014", "1020", "1021",
    "1022", "1023", "1026", "1030", "1031", "10020002", "10020003", "10020004", "10020005", "10030001", "10030002",
    "10030004", "10030005", "10030007", "10030009", "10030010", "10040001", "10130002", "10230001", "10230002",
    "10230003", "10230004", "10230005", "10230007", "10230010", "10230011", "10230012", "10300001", "10300003",
    "10300004",
]
ATTO3_EVO = [
    "1001", "1002", "10020001", "10020002", "10020003", "10020004", "10020005", "1003", "10030001", "10030002",
    "10030004", "10030005", "10030010", "10030013", "1004", "10040001", "1005", "1006", "1007", "1008", "1009", "1010",
    "10100001", "1012", "1013", "10130001", "10130002", "10130003", "1014", "1016", "1022", "1026", "1029", "1030",
    "10300001", "10300003", "10300004",
]
HAN_REAR_SEATS = ["10030006", "10030007", "10030008", "10030009"]


def caps(function_nos: Any, **extra: Any) -> dict[str, Any]:
    return {"vin": VIN, "source": "test_fixture", "functionNos": function_nos, **extra}


FIXTURES: list[dict[str, Any]] = [
    {},
    {"vin": VIN, "source": "unknown:unavailable"},
    {"vin": VIN, "source": "unknown:timeout", "lock": None},
    caps([]),
    caps(COMMAND_FUNCTION_NOS),
    caps(ALL_FUNCTION_NOS),
    caps(OVERSEAS),
    caps(ATTO3_EVO),
    caps(HAN_REAR_SEATS),
    caps(["99999999", "", " 1005", "1005 "]),
    caps([], lock=True, unlock=True, climate=True, openWindows=True),
    {"function_nos": ["1005", "1026"]},
    {"functionNos": ["1005"], "function_nos": ["1006"]},
    caps(None),
    caps("--"),
    # Malformed capability data: pydantic rejects it.
    caps("1005"),
    caps([1005]),
    caps(["1005", None]),
    caps({"1005": True}),
    "not a dict",
    None,
]

CHAIR_TYPES: list[Any] = ["1", "2", "3", "4", "5", "0", "6", "", " 1", "1 ", "01", 1, 2, 3, 4, 5, 0, 1.0, True, None,
                          [1], {"a": 1}, "seat_driver"]
LEARN_VALUES: list[Any] = [1, 0, -1, 2, 0.5, -0.5, 0.0, True, False, None, "", "1", "0", [], [1], {}, {"a": 1},
                           2**64 - 1, -(2**63)]


def _gate_args(command: Any, capabilities: Any, control_params: Any = None, learn_info: Any = None) -> dict[str, Any]:
    return {"command": command, "capabilities": capabilities, "control_params": control_params,
            "learn_info": learn_info}


@group.recorded
def recorded() -> Iterator[Case]:
    call = "models.command_gating.evaluate_command_gate"
    yield "models.command_gating.command_gate_rules", {}
    yield "models.command_gating.known_command_function_nos", {}
    # Every command against every fixture.
    for command in COMMANDS + BAD_COMMANDS:
        for fixture in FIXTURES:
            yield call, _gate_args(command, fixture)
    # Every command with each functionNo it could match alone, and with all but those.
    for rule in command_gate_rules():
        for function_no in rule.required_function_nos:
            params = {"chairType": next((chair for chair, gate in _SEAT_GATES.items() if gate == rule.gate_id), None)}
            yield call, _gate_args(rule.command.value, caps([function_no]), params)
        rest = [function_no for function_no in ALL_FUNCTION_NOS if function_no not in rule.required_function_nos]
        params = {"chairType": next((chair for chair, gate in _SEAT_GATES.items() if gate == rule.gate_id), None)}
        yield call, _gate_args(rule.command.value, caps(rest), params)
    # Seat targets: every chairType against no, every single and all seat functionNos.
    seat_function_nos = sorted({fn for rule in command_gate_rules() if rule.command == RemoteCommand.SEAT_CLIMATE
                                for fn in rule.required_function_nos})
    for chair in CHAIR_TYPES:
        for function_nos in [[], seat_function_nos, *([fn] for fn in seat_function_nos)]:
            yield call, _gate_args("VENTILATIONHEATING", caps(function_nos), {"chairType": chair, "mainHeat": 1})
    for params in [None, {}, {"chair_type": "1"}, {"chairtype": "1"}, {"chairType": "1", "chair_type": "9"}]:
        yield call, _gate_args("VENTILATIONHEATING", caps(seat_function_nos), params)
    # Learn info: only OPENWINDOW reads it; every other command ignores even malformed values.
    for value in LEARN_VALUES:
        for function_nos in [["1026"], []]:
            yield call, _gate_args("OPENWINDOW", caps(function_nos), None, {"openWindowLearnInfo": value})
        yield call, _gate_args("OPENWINDOW", caps(["1026"]), None,
                               {"openWindowLearnInfo": 0, "openWindow499LearnInfo": value})
        yield call, _gate_args("CLOSEWINDOW", caps(["1026"]), None, {"openWindowLearnInfo": value})
    for learn_info in [None, {}, {"openWindow499LearnInfo": 1}, {"openwindowlearninfo": 1},
                       {"openWindowLearnInfo": 1, "openWindow499LearnInfo": 1}]:
        yield call, _gate_args("OPENWINDOW", caps(["1026"]), None, learn_info)
    for command in COMMANDS:
        yield call, _gate_args(command, caps(ALL_FUNCTION_NOS), {"chairType": "1"}, {"openWindowLearnInfo": "x"})
    # Upstream tests/test_command_gating.py: electric A/C (1015) allows every climate command.
    for command in ["OPENAIR", "CLOSEAIR", "BOOKINGAIR"]:
        yield call, _gate_args(command, caps(["1015"]))
    # evaluate_all_command_gates
    for fixture in [{}, caps([]), caps(OVERSEAS), caps(ATTO3_EVO), caps(["1005"]), caps([1])]:
        for learn_info in [None, {"openWindowLearnInfo": 0}]:
            yield "models.command_gating.evaluate_all_command_gates", {"capabilities": fixture,
                                                                       "learn_info": learn_info}
    yield "models.command_gating.evaluate_all_command_gates", {
        "capabilities": caps(["1026"]), "learn_info": {"openWindowLearnInfo": [1]}}
    # Upstream tests/test_command_gating.py, TestLearnInfoGate: _evaluate_rule with custom rules.
    open_rule = {"gateId": "t", "command": "OPENWINDOW", "requiredFunctionNos": ["1026"],
                 "requiredLearnInfoKeys": ["openWindowLearnInfo"]}
    multi_rule = {**open_rule, "requiredLearnInfoKeys": ["openWindowLearnInfo", "openWindow499LearnInfo"]}
    lock_rule = {"gateId": "t", "command": "LOCKDOOR", "requiredFunctionNos": ["1005"]}
    rule_call = "models.command_gating.evaluate_rule"
    yield rule_call, {"rule": lock_rule, "function_nos": ["1005"], "learn_info": {"someUnrelatedKey": 1}}
    for value in [1, 0, -1]:
        yield rule_call, {"rule": open_rule, "function_nos": ["1026"], "learn_info": {"openWindowLearnInfo": value}}
    yield rule_call, {"rule": multi_rule, "function_nos": ["1026"],
                      "learn_info": {"openWindowLearnInfo": 0, "openWindow499LearnInfo": 1}}
    yield rule_call, {"rule": multi_rule, "function_nos": ["1026"],
                      "learn_info": {"openWindowLearnInfo": 1, "openWindow499LearnInfo": 1}}
    yield rule_call, {"rule": open_rule, "function_nos": ["1026"], "learn_info": None}
    yield rule_call, {"rule": open_rule, "function_nos": [], "learn_info": {"openWindowLearnInfo": 1}}
    yield rule_call, {"rule": {**lock_rule, "requiredFunctionNos": ["b", "a", "a", "c"]}, "function_nos": ["c", "a"]}
    yield rule_call, {"rule": {"gateId": "t"}, "function_nos": []}


_SEAT_GATES = {"1": "seat_driver", "2": "seat_passenger", "3": "seat_rear_left", "4": "seat_rear_right",
               "5": "steering_wheel_heat"}


def _random_function_nos(rng: random.Random) -> Any:
    roll = rng.random()
    if roll < 0.05:
        return random_json(rng)
    pool = rng.choice([COMMAND_FUNCTION_NOS, ALL_FUNCTION_NOS])
    chosen = rng.sample(pool, k=rng.randint(0, min(len(pool), rng.choice([2, 5, len(pool)]))))
    if rng.random() < 0.2:
        chosen.append(rng.choice([str(rng.randint(1000, 99999999)), random_text(rng, 8), "", " 1005"]))
    rng.shuffle(chosen)
    return chosen


def _random_capabilities(rng: random.Random) -> Any:
    roll = rng.random()
    if roll < 0.03:
        return random_json(rng)
    if roll < 0.08:
        return {"vin": VIN, "source": "unknown:" + random_text(rng, 6)}
    payload: dict[str, Any] = {}
    if rng.random() < 0.9:
        payload[rng.choice(["functionNos", "functionNos", "function_nos"])] = _random_function_nos(rng)
    if rng.random() < 0.2:
        payload[rng.choice(["lock", "openWindows", "seatDriver", "vin"])] = random_scalar(rng)
    return payload


def _random_mapping(rng: random.Random, keys: list[str], values: list[Any]) -> Any:
    if rng.random() < 0.25:
        return None
    mapping = {}
    for key in rng.sample(keys, k=rng.randint(0, len(keys))):
        mapping[key] = rng.choice([lambda: rng.choice(values), lambda: random_scalar(rng), lambda: random_json(rng)])()
    return mapping


def _with_own_function_nos(rng: random.Random, command: Any, capabilities: Any) -> Any:
    """Often add some of the command's own functionNos, so supported verdicts are common too."""
    if not isinstance(capabilities, dict) or rng.random() < 0.4:
        return capabilities
    key = next((key for key in ("functionNos", "function_nos") if isinstance(capabilities.get(key), list)), None)
    if key is None:
        return capabilities
    own = sorted({fn for rule in command_gate_rules() if rule.command.value == command
                  for fn in rule.required_function_nos})
    if own:
        capabilities = {**capabilities, key: capabilities[key] + rng.sample(own, k=rng.randint(1, len(own)))}
    return capabilities


@group.fuzz
def fuzz(rng: random.Random) -> Iterator[Case]:
    while True:
        command = rng.choice(COMMANDS) if rng.random() < 0.95 else rng.choice(BAD_COMMANDS + [random_text(rng, 10)])
        roll = rng.random()
        if roll < 0.25:
            command = "VENTILATIONHEATING"
        elif roll < 0.4:
            command = "OPENWINDOW"
        control_params = _random_mapping(rng, ["chairType", "chair_type", "mainHeat"], CHAIR_TYPES)
        if isinstance(control_params, dict) and rng.random() < 0.5:
            control_params["chairType"] = rng.choice(["1", "2", "3", "4", "5", 1, 5])
        learn_info = _random_mapping(rng, ["openWindowLearnInfo", "openWindow499LearnInfo", "trunkLearnInfo"],
                                     LEARN_VALUES)
        if isinstance(learn_info, dict) and rng.random() < 0.5:
            learn_info["openWindowLearnInfo"] = rng.choice([1, 0, -1, 2, True, None])
        args = _gate_args(command, _with_own_function_nos(rng, command, _random_capabilities(rng)), control_params,
                          learn_info)
        if json_safe(args):
            yield "models.command_gating.evaluate_command_gate", args


@group.fuzz
def fuzz_all(rng: random.Random) -> Iterator[Case]:
    while True:
        args = {"capabilities": _random_capabilities(rng),
                "learn_info": _random_mapping(rng, ["openWindowLearnInfo", "openWindow499LearnInfo"], LEARN_VALUES)}
        if json_safe(args):
            yield "models.command_gating.evaluate_all_command_gates", args


@group.fuzz
def fuzz_rule(rng: random.Random) -> Iterator[Case]:
    keys = ["openWindowLearnInfo", "openWindow499LearnInfo", "a", "b"]
    while True:
        rule = {"gateId": random_text(rng, 6) or "g", "command": rng.choice(COMMANDS),
                "requiredFunctionNos": rng.sample(COMMAND_FUNCTION_NOS, k=rng.randint(0, 4)),
                "requiredLearnInfoKeys": rng.sample(keys, k=rng.randint(0, 3))}
        args = {"rule": rule, "function_nos": _random_function_nos(rng) if rng.random() < 0.9 else [],
                "learn_info": _random_mapping(rng, keys, LEARN_VALUES)}
        if not isinstance(args["function_nos"], list) or not all(isinstance(fn, str) for fn in args["function_nos"]):
            args["function_nos"] = []
        if json_safe(args):
            yield "models.command_gating.evaluate_rule", args
