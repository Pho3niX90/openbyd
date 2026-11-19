"""pybyd/models/control.py: command enums, results, acknowledgements and command parameters.

Reply models go through ``register_model``. The command parameter classes
are input models: a call takes the constructor's keyword arguments as JSON
(``{"kwargs": {...}}``) and answers ``model_dump(mode="json")`` and
``to_control_params_map()`` (plus ``to_dedicated_control_params_map()`` for
battery heat), or the error class. ``SeatClimateParams`` also takes a list of
``with_change`` calls to apply, and ``from_current_state`` has its own call
taking the seat readings of an ``HvacStatus`` and a ``VehicleRealtimeData``.
"""

from __future__ import annotations

import random
from collections.abc import Iterator
from typing import Any

from pydantic import TypeAdapter
from pydantic.alias_generators import to_camel
from pybyd.models.control import (
    BatteryHeatParams,
    ChargeChangeResult,
    ClimateScheduleParams,
    ClimateStartParams,
    CommandAck,
    CommandAckDiagnostics,
    CommandAckEvent,
    CommandLifecycleEvent,
    CommandLifecycleStatus,
    ControlParams,
    ControlState,
    RemoteCommand,
    RemoteControlResult,
    SeatClimateParams,
    VerifyControlPasswordResponse,
)
from pybyd.models.hvac import HvacStatus
from pybyd.models.realtime import SeatHeatVentState, StearingWheelHeat, VehicleRealtimeData

from parity.framework import Case, Group, random_text
from parity.models_support import (
    BOOL_AWKWARD,
    BOOL_VALUES,
    FLOAT_AWKWARD,
    FLOAT_VALUES,
    INT_AWKWARD,
    INT_VALUES,
    MODEL_INT_LIMIT,
    SENTINELS,
    STR_AWKWARD,
    STR_VALUES,
    UNKNOWN_KEYS,
    json_safe,
    jsonable,
    ModelCases,
    model_int_safe,
    property_names,
    random_json,
    random_numeric_text,
    random_scalar,
    register_model,
)

group = Group("models_control")
VIN = "TESTVIN0000000001"

# ---------------------------------------------------------------------------
# Enums: how a model field reads them (pydantic lax mode)
# ---------------------------------------------------------------------------

ENUMS = {
    "models.control.RemoteCommand": RemoteCommand,
    "models.control.ControlState": ControlState,
    "models.control.CommandLifecycleStatus": CommandLifecycleStatus,
}


def _register_enum(call: str, cls: Any) -> None:
    adapter = TypeAdapter(cls)

    @group.oracle(call)
    def oracle(value: Any) -> dict[str, Any]:
        member = adapter.validate_python(value)
        return {"name": member.name, "value": jsonable(member.value)}


for _call, _cls in ENUMS.items():
    _register_enum(_call, _cls)


def _enum_values(cls: Any) -> list[Any]:
    members = [member.value for member in cls]
    awkward: list[Any] = [None, [], {}, "", " ", True, False, 1.5, "x", 2**63, -1, 3]
    if all(isinstance(value, int) for value in members):
        awkward += [str(value) for value in members] + [f" {members[0]} ", 1.0, "1.0", "1_0", "+1", "２", 2**64 - 1]
    else:
        awkward += [value.lower() for value in members] + [f" {members[0]}", f"{members[0]} ", members[0][:-1], 1, 0]
    return members + awkward


@group.recorded
def enum_cases() -> Iterator[Case]:
    for call, cls in ENUMS.items():
        for value in _enum_values(cls):
            yield call, {"value": value}


@group.fuzz
def enum_fuzz(rng: random.Random) -> Iterator[Case]:
    while True:
        call, cls = rng.choice(list(ENUMS.items()))
        value = rng.choice([lambda: rng.choice(_enum_values(cls)), lambda: random_scalar(rng), lambda: random_json(rng)])()
        if json_safe(value):
            yield call, {"value": value}


# ---------------------------------------------------------------------------
# Reply models
# ---------------------------------------------------------------------------

RESULT_CALL = "models.control.RemoteControlResult"

register_model(
    group,
    RESULT_CALL,
    RemoteControlResult,
    samples=[
        {"controlState": 1, "requestSerial": "SERIAL0001"},
        {"res": 2, "requestSerial": "SERIAL0001"},
        {"res": 1},
        {"controlState": 0},
        {"controlState": 2, "success": False, "requestSerial": "SERIAL0002"},
    ],
)
# ``int(None)`` and ``int([])`` in the before-validator raise TypeError, which pydantic lets escape.
group.expected_errors[RESULT_CALL] = (ValueError, TypeError)

# Values for ``res`` and ``controlState``, which the before-validator reads with ``int()``.
INT_LIKE: list[Any] = [
    0, 1, 2, 3, -1, 5, 1.0, 1.9, 2.5, -0.0, True, False, "1", "2", " 2 ", "02", "+2", "2_0", "1__0", "1.0",
    "", " ", "--", "nan", "x", "٢", "２", "　2　", "\x1c2", None, [], [2], {}, {"a": 1}, 2**63, 2**64 - 1,
    -(2**63), 1e300, "9" * 30,
]


@group.recorded
def result_cases() -> Iterator[Case]:
    # Upstream: command outcomes polled as {"res": 1} then terminal.
    for value in INT_LIKE:
        yield RESULT_CALL, {"raw": {"res": value, "requestSerial": "S1"}, "context": None}
        yield RESULT_CALL, {"raw": {"controlState": value}, "context": None}
    for extra in [{"success": True}, {"success": "--"}, {"success": "no"}, {"controlState": 1}, {"control_state": 2},
                  {"controlState": 5, "success": True}, {"controlState": None}, {"raw": {"a": 1}}]:
        for res in [1, 2, 3, "x", None]:
            yield RESULT_CALL, {"raw": {"res": res, **extra}, "context": None}
    yield RESULT_CALL, {"raw": {"control_state": 1}, "context": None}
    yield RESULT_CALL, {"raw": {"control_state": "1", "controlState": "2"}, "context": None}
    yield RESULT_CALL, {"raw": {"control_state": 7}, "context": None}


@group.fuzz
def result_fuzz(rng: random.Random) -> Iterator[Case]:
    while True:
        raw: dict[str, Any] = {}
        for key in rng.sample(["res", "controlState", "control_state", "success", "requestSerial", "request_serial"],
                              k=rng.randint(0, 4)):
            raw[key] = rng.choice([lambda: rng.choice(INT_LIKE), lambda: random_scalar(rng),
                                   lambda: random_numeric_text(rng), lambda: random_json(rng)])()
        if json_safe(raw):
            yield RESULT_CALL, {"raw": raw, "context": None}


register_model(
    group,
    "models.control.CommandAck",
    CommandAck,
    samples=[
        {"vin": VIN, "result": "1", "requestSerial": "SERIAL0001"},
        {"vin": VIN, "result": 1},
        {"result": None, "requestSerial": ""},
    ],
)

register_model(
    group,
    "models.control.ChargeChangeResult",
    ChargeChangeResult,
    samples=[
        {"vin": VIN, "res": 2, "message": "Operation successful", "requestSerial": "SERIAL0001"},
        {"vin": VIN, "res": "1", "message": "--"},
        {"res": 3, "raw": {"res": 3}},
    ],
)


def _timestamp_in_domain(raw: Any) -> bool:
    """Whether ``int(timestamp)`` in the shape normalisers stays within the shared ``±2**53`` domain.

    ``CommandAckEvent`` and ``CommandLifecycleEvent`` turn a float timestamp
    into an ``int`` before pydantic sees it, so ``1e300`` becomes a model
    ``int`` far outside ``i64`` (openbyd answers ``Unsupported`` there, see
    ``parity/models_support.py``). The generic ``in_domain`` check cannot see
    that conversion; this is the same limit applied after it.
    """
    if not isinstance(raw, dict):
        return True
    value = raw.get("timestamp")
    if value is not None and not isinstance(value, bool):
        try:
            if abs(int(value)) > MODEL_INT_LIMIT:
                return False
        except (TypeError, ValueError, OverflowError):
            pass
    return _timestamp_in_domain(raw.get("ack"))


def register_event_model(call: str, cls: Any, samples: list[dict[str, Any]]) -> ModelCases:
    """``register_model`` with the generated payloads kept inside the timestamp domain."""
    cases = ModelCases(cls, samples)
    names = property_names(cls)

    @group.oracle(call)
    def oracle(raw: Any, context: Any = None) -> dict[str, Any]:
        model = cls.model_validate(raw, context=context)
        return {"dump": jsonable(model.model_dump(mode="json")),
                "properties": {name: jsonable(getattr(model, name)) for name in names}}

    @group.recorded
    def recorded() -> Iterator[Case]:
        for payload in cases.recorded():
            if _timestamp_in_domain(payload):
                yield call, {"raw": payload, "context": None}

    @group.fuzz
    def fuzz(rng: random.Random) -> Iterator[Case]:
        while True:
            payload = cases.fuzz(rng)
            if _timestamp_in_domain(payload):
                yield call, {"raw": payload, "context": None}

    return cases


ACK_EVENT_CALL = "models.control.CommandAckEvent"
register_event_model(
    ACK_EVENT_CALL,
    CommandAckEvent,
    samples=[
        {"vin": VIN, "requestSerial": "SERIAL0001", "raw_uuid": "uuid-1", "result": "2", "success": True,
         "timestamp": 1700000000123, "raw": {"res": 2}},
        {"vin": VIN, "request_serial": "SERIAL0002", "result": 2, "success": "true", "timestamp": "1700000000"},
        {"vin": "", "requestSerial": "", "request_serial": 5, "timestamp": 1.5, "raw": "x"},
    ],
)

LIFECYCLE_CALL = "models.control.CommandLifecycleEvent"
register_event_model(
    LIFECYCLE_CALL,
    CommandLifecycleEvent,
    samples=[
        {"status": "matched", "vin": VIN, "requestSerial": "SERIAL0001", "command": "LOCKDOOR",
         "timestamp": 1700000000, "ack": {"vin": VIN, "requestSerial": "SERIAL0001", "success": True}},
        {"status": "expired", "vin": VIN, "request_serial": "SERIAL0002", "command": 7, "timestamp": None,
         "reason": 404},
        {"status": "uncorrelated", "timestamp": "12", "reason": "no pending command"},
    ],
)

# Values the shape normalisers turn into text or integers.
TEXT_LIKE: list[Any] = [
    "x", "", " ", "--", 0, 1, -1, 1.5, -0.0, 1e16, 1e-5, 0.1, 1e300, True, False, None, [], [1, "a"], {},
    {"a": "é", "b": None}, ["\n", "'", '"', "\x00", "​", "🚗"], 2**64 - 1,
]
TIMESTAMP_LIKE: list[Any] = [
    None, 0, 1, -5, True, False, 1.5, -2.5, "12", " 12 ", "1_2", "x", "", "--", "١٢", [], {}, [1], 1700000000123,
    2**53, -(2**53), "9007199254740992",
]


@group.recorded
def event_cases() -> Iterator[Case]:
    for value in TEXT_LIKE:
        yield ACK_EVENT_CALL, {"raw": {"vin": VIN, "result": value, "requestSerial": value}, "context": None}
        yield LIFECYCLE_CALL, {"raw": {"status": "matched", "command": value, "reason": value,
                                       "request_serial": value}, "context": None}
    for value in TIMESTAMP_LIKE:
        yield ACK_EVENT_CALL, {"raw": {"timestamp": value, "raw_uuid": value, "success": value}, "context": None}
        yield LIFECYCLE_CALL, {"raw": {"status": "registered", "timestamp": value}, "context": None}
    for first in ["", "S1", 0, None, False, [], 5]:
        for second in ["S2", "", None, 7, "--"]:
            yield ACK_EVENT_CALL, {"raw": {"vin": VIN, "requestSerial": first, "request_serial": second},
                                   "context": None}
            yield LIFECYCLE_CALL, {"raw": {"status": "matched", "requestSerial": first, "request_serial": second},
                                   "context": None}
    yield ACK_EVENT_CALL, {"raw": {"rawUuid": 5, "raw_uuid": "u"}, "context": None}
    yield ACK_EVENT_CALL, {"raw": {"rawUuid": "u1", "raw_uuid": None}, "context": None}
    yield LIFECYCLE_CALL, {"raw": {"status": "matched", "ack": {"timestamp": "x", "raw": None}}, "context": None}
    yield LIFECYCLE_CALL, {"raw": {"status": "MATCHED", "timestamp": 1}, "context": None}
    yield LIFECYCLE_CALL, {"raw": {"timestamp": 1}, "context": None}


register_model(
    group,
    "models.control.CommandAckDiagnostics",
    CommandAckDiagnostics,
    samples=[
        {"pending": 1, "matched": 4, "expired": 0, "uncorrelated": 2, "pendingByVin": {VIN: 1}},
        {"pending_by_vin": {VIN: "2", "TESTVIN0000000002": 0}},
    ],
)

register_model(
    group,
    "models.control.VerifyControlPasswordResponse",
    VerifyControlPasswordResponse,
    samples=[
        {"vin": VIN, "ok": True},
        {"vin": VIN, "ok": "true"},
        {"vin": VIN, "ok": 1},
        {"ok": None},
    ],
)

# ---------------------------------------------------------------------------
# Command parameters
# ---------------------------------------------------------------------------


def _params_answer(params: Any) -> dict[str, Any]:
    return {"dump": jsonable(params.model_dump(mode="json")), "params": jsonable(params.to_control_params_map())}


def _apply_changes(params: SeatClimateParams, changes: list[list[Any]] | None) -> SeatClimateParams:
    for key, value in changes or []:
        params = params.with_change(key, value)
    return params


@group.oracle("models.control.ControlParams")
def control_params(kwargs: dict[str, Any]) -> dict[str, Any]:
    return _params_answer(ControlParams(**kwargs))


@group.oracle("models.control.ClimateStartParams")
def climate_start(kwargs: dict[str, Any]) -> dict[str, Any]:
    return _params_answer(ClimateStartParams(**kwargs))


@group.oracle("models.control.ClimateScheduleParams")
def climate_schedule(kwargs: dict[str, Any]) -> dict[str, Any]:
    return _params_answer(ClimateScheduleParams(**kwargs))


@group.oracle("models.control.SeatClimateParams")
def seat_climate(kwargs: dict[str, Any], changes: list[list[Any]] | None = None) -> dict[str, Any]:
    return _params_answer(_apply_changes(SeatClimateParams(**kwargs), changes))


SEAT_ATTRS = [
    "main_seat_heat_state", "main_seat_ventilation_state", "copilot_seat_heat_state",
    "copilot_seat_ventilation_state", "lr_seat_heat_state", "lr_seat_ventilation_state", "rr_seat_heat_state",
    "rr_seat_ventilation_state", "steering_wheel_heat_state",
]


def _state(cls: Any, readings: dict[str, int | None] | None) -> Any:
    """A model holding exactly these enum members (``model_construct`` skips validation on purpose)."""
    if readings is None:
        return None
    values: dict[str, Any] = {}
    for name, value in readings.items():
        enum = StearingWheelHeat if name == "steering_wheel_heat_state" else SeatHeatVentState
        member = None if value is None else enum(value)
        if member is not None and member.value != value:
            raise AssertionError(f"{value} is not a member value of {enum.__name__}")
        values[name] = member
    return cls.model_construct(**values)


@group.oracle("models.control.SeatClimateParams.from_current_state")
def seat_from_current_state(hvac: dict[str, int | None] | None, realtime: dict[str, int | None] | None,
                            changes: list[list[Any]] | None = None) -> dict[str, Any]:
    params = SeatClimateParams.from_current_state(_state(HvacStatus, hvac), _state(VehicleRealtimeData, realtime))
    return _params_answer(_apply_changes(params, changes))


@group.oracle("models.control.BatteryHeatParams")
def battery_heat(kwargs: dict[str, Any]) -> dict[str, Any]:
    params = BatteryHeatParams(**kwargs)
    return {**_params_answer(params), "dedicated": jsonable(params.to_dedicated_control_params_map())}


PARAM_CLASSES = {
    "models.control.ControlParams": ControlParams,
    "models.control.ClimateStartParams": ClimateStartParams,
    "models.control.ClimateScheduleParams": ClimateScheduleParams,
    "models.control.SeatClimateParams": SeatClimateParams,
    "models.control.BatteryHeatParams": BatteryHeatParams,
}

TEMPERATURES: list[Any] = [15, 15.0, 21, 21.5, 22.4, 22.5, 23.5, 31, 31.0, 14.9, 14, 32, 33, 17, 0, -40, "21", " 21.5 ",
                           "inf", "-inf", "nan", 1e308, -1e308]
OFFSETS: list[Any] = [14, 14.0, 16, 16.0, 15.5, 0, -14, 1e308, -1e308, "inf", "-inf", "nan", "16"]
SMALL_INTS: list[Any] = [-2, -1, 0, 1, 2, 3, 4, 5, 6, 7, "1", " 3 ", 1.0, 2.5, True, False, "", "--"]
TEXTS: list[Any] = ["14", " 14 ", "1", "\t1\n", "　1　", "1\x1c", " ", "", "--", "比亚迪", "​1"]


def _kind(field: Any) -> str:
    annotation = str(field.annotation)
    for name in ("bool", "float", "int", "str"):
        if name in annotation:
            return name
    raise AssertionError(annotation)


def _pool(cls: Any, name: str) -> list[Any]:
    kind = _kind(cls.model_fields[name])
    if kind == "float":
        base = OFFSETS if name == "temp_offset" else TEMPERATURES
        pool = base + FLOAT_VALUES + FLOAT_AWKWARD
    elif kind == "int":
        pool = SMALL_INTS + INT_VALUES + INT_AWKWARD
    elif kind == "str":
        pool = TEXTS + STR_VALUES + STR_AWKWARD
    else:
        pool = BOOL_VALUES + BOOL_AWKWARD
    return [value for value in pool + [None, [], {}, "x"] if json_safe(value) and (kind != "int" or model_int_safe(value))]


def _keys(name: str) -> list[str]:
    alias = to_camel(name)
    return [name] if alias == name else [name, alias]


PARAM_SAMPLES: dict[str, list[dict[str, Any]]] = {
    "models.control.ControlParams": [{}],
    "models.control.ClimateStartParams": [
        {"temperature": 21, "time_span": 1},
        {"temperature": 29, "time_span": 1, "temp_offset": 16.0},
        {"temperature": 29},
        {"temperature": 22, "copilot_temperature": 24, "cycle_mode": 1, "wind_level": 3, "air_set": "14"},
        {"copilot_temperature": 22},
        {"temperature": 21, "remote_mode": 1},
        {"temperature": 21, "airSet": "14", "timeSpan": 5, "cycleMode": 1},
    ],
    "models.control.ClimateScheduleParams": [
        {"temperature": 21, "booking_time": 1700000000, "time_span": 2, "wind_mode": 2},
        {"remote_mode": 2, "booking_id": 3, "booking_time": 1700000000, "temperature": 23},
        {"remote_mode": 3, "booking_id": 3},
        {"remoteMode": 1, "acSwitch": 1, "bookingTime": 1700000000, "air_set": "14"},
    ],
    "models.control.SeatClimateParams": [
        {},
        {"chair_type": "1", "main_heat": 1},
        {"chairType": "5", "steeringWheelHeatState": 1, "mainHeat": 2, "copilotVentilation": 1},
        {"chair_type": "3", "lr_seat_heat_state": 2, "rr_third_ventilation_state": 3, "remote_mode": 4},
    ],
    "models.control.BatteryHeatParams": [
        {"on": True},
        {"on": False, "heat_type": 1},
        {"on": "yes", "heatType": "3"},
    ],
}

INVALID: dict[str, list[dict[str, Any]]] = {
    "models.control.ControlParams": [{"x": 1}, {"chairType": "1"}],
    "models.control.ClimateStartParams": [
        {"temperature": 33}, {"temperature": 15, "temp_offset": 16.0}, {"time_span": 0}, {"time_span": 6},
        {"cycle_mode": -1}, {"temperature": 21, "booking_id": 1}, {"cycle_mode": 1, "cycleMode": 2},
        {"cycleMode": 2, "cycle_mode": 1}, {"tempOffset": 16, "temperature": 30}, {"temperature": "inf", "temp_offset": "inf"},
        {"mainSettingTemp": 7},
    ],
    "models.control.ClimateScheduleParams": [
        {"remote_mode": 4}, {"remote_mode": 0}, {"booking_id": 0}, {"booking_time": 0}, {"ac_switch": 2},
        {"wind_mode": -1}, {"remote_mode": 2, "remoteMode": 2},
    ],
    "models.control.SeatClimateParams": [
        {"main_heat": 4}, {"remote_mode": 0}, {"remote_mode": 5}, {"steering_wheel_heat_state": -1},
        {"chair_type": 1}, {"temperature": 21}, {"main_heat": 1, "mainHeat": 1},
    ],
    "models.control.BatteryHeatParams": [
        {}, {"heat_type": 2}, {"batteryHeatSwitch": 1}, {"on": 2}, {"on": "maybe"}, {"on": True, "heatType": "x"},
        {"on": True, "heat_type": 1, "heatType": 1},
    ],
}

SEAT_PARAM_KEYS = [
    "main_heat", "main_ventilation", "copilot_heat", "copilot_ventilation", "lr_seat_heat_state",
    "lr_seat_ventilation_state", "rr_seat_heat_state", "rr_seat_ventilation_state", "steering_wheel_heat_state",
    "lr_third_heat_state", "lr_third_ventilation_state", "rr_third_heat_state", "rr_third_ventilation_state",
    "remote_mode", "chair_type", "mainHeat", "unknown", "",
]


@group.recorded
def params_cases() -> Iterator[Case]:
    for call, samples in PARAM_SAMPLES.items():
        for kwargs in samples + INVALID[call]:
            yield call, {"kwargs": kwargs}
    # Every value of every field, on top of the first sample; then under the alias.
    for call, cls in PARAM_CLASSES.items():
        base = PARAM_SAMPLES[call][0]
        for name in cls.model_fields:
            rest = {key: value for key, value in base.items() if key not in _keys(name)}
            for value in _pool(cls, name):
                yield call, {"kwargs": {**rest, name: value}}
            for key in _keys(name)[1:]:
                yield call, {"kwargs": {**rest, key: _pool(cls, name)[0]}}
        yield call, {"kwargs": {**base, **{key: 1 for key in UNKNOWN_KEYS}}}
    # Upstream: tests/test_rear_seat_climate.py, with_change on every seat field.
    for key in SEAT_PARAM_KEYS:
        for value in [1, 2, 3, 0, 99, -1]:
            yield "models.control.SeatClimateParams", {"kwargs": {}, "changes": [[key, value]]}
    yield "models.control.SeatClimateParams", {
        "kwargs": {"chair_type": "2"}, "changes": [["main_heat", 1], ["copilot_heat", 2], ["unknown", 5]]}


STATE_VALUES = {"seat": [None, -1, 0, 1, 2, 3], "steering_wheel_heat_state": [None, -1, 0, 1]}


def _state_values(name: str) -> list[int | None]:
    return STATE_VALUES["steering_wheel_heat_state" if name == "steering_wheel_heat_state" else "seat"]


@group.recorded
def from_current_state_cases() -> Iterator[Case]:
    call = "models.control.SeatClimateParams.from_current_state"
    yield call, {"hvac": None, "realtime": None}
    yield call, {"hvac": {}, "realtime": {}}
    for name in SEAT_ATTRS:
        for value in _state_values(name):
            yield call, {"hvac": {name: value}, "realtime": None}
            yield call, {"hvac": None, "realtime": {name: value}}
            for fallback in _state_values(name):
                yield call, {"hvac": {name: value}, "realtime": {name: fallback}}
    everything = {name: 3 if name != "steering_wheel_heat_state" else -1 for name in SEAT_ATTRS}
    yield call, {"hvac": everything, "realtime": None, "changes": [["main_heat", 2]]}
    yield call, {"hvac": {**everything, "rr_seat_heat_state": None}, "realtime": {"rr_seat_heat_state": 1},
                 "changes": [["steering_wheel_heat_state", 3]]}


def _bounds(field: Any) -> tuple[int | None, int | None]:
    ge = next((item.ge for item in field.metadata if getattr(item, "ge", None) is not None), None)
    le = next((item.le for item in field.metadata if getattr(item, "le", None) is not None), None)
    return ge, le


def _valid_value(rng: random.Random, cls: Any, name: str, offset: float) -> Any:
    """A value the constructor accepts, sometimes in another lax form (string, float, bool)."""
    field = cls.model_fields[name]
    kind = _kind(field)
    if kind == "float":
        if name == "temp_offset":
            return offset
        value: Any = rng.choice([rng.randint(int(offset) + 1, int(offset) + 17),
                                 round(rng.uniform(offset + 1, offset + 17), rng.choice([0, 1, 2]))])
    elif kind == "int":
        ge, le = _bounds(field)
        low = 0 if ge is None else ge
        value = rng.randint(low, le if le is not None else low + rng.choice([5, 1000, 2**40]))
    elif kind == "str":
        return rng.choice(["1", "2", "3", "4", "5", "14", " 3 ", "x"])
    else:
        return rng.choice([True, False, 0, 1, "yes", "off"])
    form = rng.random()
    if form < 0.1:
        return str(value)
    if form < 0.15:
        return float(value)
    return value


def _random_value(rng: random.Random, cls: Any, name: str) -> Any:
    return rng.choice([
        lambda: rng.choice(_pool(cls, name)),
        lambda: rng.choice(_pool(cls, name)[:12]),
        lambda: random_numeric_text(rng),
        lambda: random_scalar(rng),
        lambda: rng.choice(SENTINELS),
        lambda: random_text(rng, 6),
    ])()


def _random_kwargs(rng: random.Random, call: str, cls: Any) -> dict[str, Any]:
    kwargs = dict(rng.choice(PARAM_SAMPLES[call])) if rng.random() < 0.6 else {}
    names = list(cls.model_fields)
    mostly_valid = rng.random() < 0.6
    offset = rng.choice([14.0, 14.0, 16.0, 15.5])
    if mostly_valid and "temp_offset" in names and offset != 14.0:
        kwargs = {key: value for key, value in kwargs.items() if "emperature" not in key}
        kwargs["temp_offset"] = offset
    for name in rng.sample(names, k=rng.randint(0, len(names))):
        if rng.random() < 0.15:
            for key in _keys(name):
                kwargs.pop(key, None)
            continue
        if mostly_valid and rng.random() < 0.9:
            value = _valid_value(rng, cls, name, offset)
        else:
            value = _random_value(rng, cls, name)
        if json_safe(value) and (_kind(cls.model_fields[name]) != "int" or model_int_safe(value)):
            kwargs[rng.choice(_keys(name))] = value
    if rng.random() < 0.05:
        kwargs[rng.choice(UNKNOWN_KEYS + ["mainSettingTemp", "batteryHeatSwitch", "raw"])] = random_json(rng)
    if cls is BatteryHeatParams and mostly_valid and "on" not in kwargs:
        kwargs["on"] = rng.choice([True, False])
    return kwargs


def _random_changes(rng: random.Random) -> list[list[Any]]:
    return [[rng.choice(SEAT_PARAM_KEYS), rng.choice([lambda: rng.randint(-5, 10),
                                                      lambda: rng.randint(-(2**63), 2**63 - 1)])()]
            for _ in range(rng.randint(0, 3))]


@group.fuzz
def params_fuzz(rng: random.Random) -> Iterator[Case]:
    while True:
        call, cls = rng.choice(list(PARAM_CLASSES.items()))
        args: dict[str, Any] = {"kwargs": _random_kwargs(rng, call, cls)}
        if cls is SeatClimateParams:
            args["changes"] = _random_changes(rng)
        yield call, args


@group.fuzz
def from_current_state_fuzz(rng: random.Random) -> Iterator[Case]:
    def readings() -> dict[str, int | None] | None:
        if rng.random() < 0.2:
            return None
        return {name: rng.choice(_state_values(name)) for name in rng.sample(SEAT_ATTRS, k=rng.randint(0, 9))}

    while True:
        yield "models.control.SeatClimateParams.from_current_state", {
            "hvac": readings(), "realtime": readings(), "changes": _random_changes(rng)}
