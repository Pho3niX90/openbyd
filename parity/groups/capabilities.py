"""pybyd/_capabilities/

``capabilities.seat.SeatLevel`` and ``capabilities.seat.SeatPosition`` look a
member up by value (``{"value": ...}``) and answer its name, value and, for
a level, its command and status values.

``capabilities.<file>.<Class>`` builds the capability with a scripted
client and a bare ``execute_command`` (record the specs, run the command)::

    {"vin": "TESTVIN0000000001", "kwargs": {"available": true},
     "state": {"hvac": {...}, "realtime": {...}, "context": ...},
     "method": "start", "args": {"temperature": 21.0, "duration": 10},
     "reply": null, "refresh": {"result": {...}}, "open_fn": true}

``kwargs`` are the constructor's availability flags (absent for the
default); ``state`` is the snapshot ``get_state`` returns (``null``: none;
only the HVAC capability may have none); ``method`` (``null`` for none) is
called with ``args``; ``reply`` says what every client method does
(``null``: succeed, ``{"error": <scripted error>}``: raise); ``refresh`` is
the HVAC capability's ``refresh_fn`` (``null``: none); ``open_fn: false``
builds the windows capability without an open command. The answer::

    {"properties": {...}, "specs": [...], "calls": [{"method", "vin", "params"}], "error": {...}}

``specs`` appears once the command reached ``execute_command``, ``error``
when the method raised (see ``parity/car_support.py``). A refresh shows in
``calls`` as ``{"method": "refresh"}``; ``params`` holds the parameters'
``model_dump(mode="json")`` and ``to_control_params_map()``.
"""

from __future__ import annotations

import copy
import dataclasses
import random
from collections.abc import Iterator
from typing import Any

from pybyd._capabilities.battery_heat import BatteryHeatCapability
from pybyd._capabilities.finder import FinderCapability
from pybyd._capabilities.hvac import HvacCapability
from pybyd._capabilities.lock import LockCapability
from pybyd._capabilities.seat import SeatCapability, SeatLevel, SeatPosition
from pybyd._capabilities.steering import SteeringCapability
from pybyd._capabilities.trunk import TrunkCapability
from pybyd._capabilities.windows import WindowsCapability
from pybyd._state_engine import VehicleSnapshot
from pybyd.exceptions import BydError
from pybyd.models.control import ClimateScheduleParams
from pybyd.models.hvac import HvacStatus
from pybyd.models.realtime import VehicleRealtimeData
from pybyd.models.vehicle import Vehicle

from parity.car_support import COMMAND_ERRORS, ERROR_CLASSES, describe, drive, scripted_error
from parity.framework import Group, random_text
from parity.groups.models_realtime import ENERGY_CONTEXTS, python_context
from parity.groups.state import HVAC_CORPUS, REALTIME_CORPUS, random_hvac, random_realtime
from parity.models_support import jsonable

group = Group("capabilities")

VIN = "TESTVIN0000000001"
_ERRORS = (BydError, ValueError, TypeError, AttributeError)

# ---------------------------------------------------------------------------
# Enums
# ---------------------------------------------------------------------------


@group.oracle("capabilities.seat.SeatLevel")
def seat_level(value: Any) -> Any:
    level = SeatLevel(value)
    return {"name": level.name, "value": int(level), "command_value": level.to_command_value(),
            "status_value": int(level.to_status_value())}


@group.oracle("capabilities.seat.SeatPosition")
def seat_position(value: Any) -> Any:
    position = SeatPosition(value)
    return {"name": position.name, "value": position.value}


# ---------------------------------------------------------------------------
# Capability classes
# ---------------------------------------------------------------------------


def snapshot(vin: str, state: dict[str, Any] | None) -> VehicleSnapshot | None:
    if state is None:
        return None
    hvac = None if state.get("hvac") is None else HvacStatus.model_validate(state["hvac"])
    realtime = None
    if state.get("realtime") is not None:
        realtime = VehicleRealtimeData.model_validate(state["realtime"], context=python_context(state.get("context")))
    return VehicleSnapshot(vehicle=Vehicle.model_validate({"vin": vin}), hvac=hvac, realtime=realtime)


def params_json(params: Any) -> dict[str, Any]:
    return {"dump": jsonable(params.model_dump(mode="json")), "control_params": jsonable(params.to_control_params_map())}


def _properties(cap: Any) -> dict[str, Any]:
    if isinstance(cap, SeatCapability):
        return {"modes": {p.value: {"heat": cap._is_mode_available(p, "heat"), "vent": cap._is_mode_available(p, "vent")}
                          for p in SeatPosition}}
    names = {
        FinderCapability: ["find_available", "flash_available"],
        TrunkCapability: ["open_available", "close_available"],
        WindowsCapability: ["close_available", "open_available"],
        HvacCapability: ["available", "temp_offset", "temp_range"],
    }.get(type(cap), ["available"])
    return {name: jsonable(getattr(cap, name)) for name in names}


def _call(cap: Any, method: str, args: dict[str, Any]) -> Any:
    if isinstance(cap, SeatCapability):
        return getattr(cap, method)(SeatPosition(args.get("position")), SeatLevel(args.get("level")))
    if isinstance(cap, (SteeringCapability, BatteryHeatCapability)):
        return cap.heat(on=args["on"])
    if isinstance(cap, HvacCapability) and method == "start":
        kwargs: dict[str, Any] = {key: args[key] for key in ("cycle_mode", "fan_level") if key in args}
        if args.get("temperature") is not None:
            kwargs["temperature"] = float(args["temperature"])
        if args.get("duration") is not None:
            kwargs["duration"] = args["duration"]
        return cap.start(**kwargs)
    if isinstance(cap, HvacCapability) and method == "schedule":
        return cap.schedule(ClimateScheduleParams(**args.get("params", {})))
    return getattr(cap, method)()


def _capability(name: str):
    def oracle(vin: str, kwargs: dict[str, Any] | None = None, state: dict[str, Any] | None = None,
               method: str | None = None, args: dict[str, Any] | None = None, reply: dict[str, Any] | None = None,
               refresh: dict[str, Any] | None = None, open_fn: bool = True) -> Any:
        kwargs = dict(kwargs or {})
        current = snapshot(vin, state)
        blank = VehicleSnapshot(vehicle=Vehicle.model_validate({"vin": vin}))
        out: dict[str, Any] = {}
        calls: list[dict[str, Any]] = []

        def fake(method_name: str):
            async def call(vin_: str, *, params: Any = None) -> None:
                entry: dict[str, Any] = {"method": method_name, "vin": vin_}
                if params is not None:
                    entry["params"] = params_json(params)
                calls.append(entry)
                if reply is not None and "error" in reply:
                    raise scripted_error(reply["error"])

            return call

        async def execute(command: Any, specs: list[Any]) -> None:
            out["specs"] = [jsonable(dataclasses.asdict(spec)) for spec in specs]
            await command()

        async def refresh_fn() -> HvacStatus:
            calls.append({"method": "refresh"})
            if "error" in refresh:
                raise scripted_error(refresh["error"])
            return HvacStatus.model_validate(refresh.get("result"))

        common = {"vin": vin, "execute_command": execute}
        if name == "LockCapability":
            cap = LockCapability(lock_fn=fake("lock"), unlock_fn=fake("unlock"), **common, **kwargs)
        elif name == "HvacCapability":
            cap = HvacCapability(start_fn=fake("start_climate"), stop_fn=fake("stop_climate"),
                                 schedule_fn=fake("schedule_climate"),
                                 get_state=None if current is None else (lambda: current),
                                 refresh_fn=None if refresh is None else refresh_fn, **common, **kwargs)
        elif name == "SeatCapability":
            cap = SeatCapability(set_seat_climate_fn=fake("set_seat_climate"), get_state=lambda: current or blank,
                                 **common, **kwargs)
        elif name == "SteeringCapability":
            cap = SteeringCapability(set_seat_climate_fn=fake("set_seat_climate"),
                                     get_state=lambda: current or blank, **common, **kwargs)
        elif name == "BatteryHeatCapability":
            cap = BatteryHeatCapability(set_battery_heat_fn=fake("set_battery_heat"), **common, **kwargs)
        elif name == "FinderCapability":
            cap = FinderCapability(find_fn=fake("find_car"), flash_fn=fake("flash_lights"), **common, **kwargs)
        elif name == "WindowsCapability":
            cap = WindowsCapability(close_fn=fake("close_windows"), open_fn=fake("open_windows") if open_fn else None,
                                    **common, **kwargs)
        else:
            cap = TrunkCapability(open_fn=fake("open_trunk"), close_fn=fake("close_trunk"), **common, **kwargs)

        answer: dict[str, Any] = {"properties": _properties(cap)}
        error = None
        if method is not None:
            try:
                drive(_call(cap, method, args or {}))
            except Exception as exc:  # noqa: BLE001 - every failure is part of the answer
                error = describe(exc)
        if "specs" in out:
            answer["specs"] = out["specs"]
        answer["calls"] = calls
        if error is not None:
            answer["error"] = error
        return answer

    return oracle


CLASSES = {
    "lock": ("LockCapability", ["lock", "unlock"], ["available"]),
    "hvac": ("HvacCapability", ["start", "stop", "schedule"], ["available"]),
    "seat": ("SeatCapability", ["heat", "ventilation"],
             ["driver_heat_available", "driver_ventilation_available", "passenger_heat_available",
              "passenger_ventilation_available", "rear_left_heat_available", "rear_left_ventilation_available",
              "rear_right_heat_available", "rear_right_ventilation_available"]),
    "steering": ("SteeringCapability", ["heat"], ["available"]),
    "battery_heat": ("BatteryHeatCapability", ["heat"], ["available"]),
    "finder": ("FinderCapability", ["find", "flash_lights"], ["find_available", "flash_available"]),
    "windows": ("WindowsCapability", ["close", "open"], ["close_available", "open_available"]),
    "trunk": ("TrunkCapability", ["open", "close"], ["open_available", "close_available"]),
}

for _file, (_class, _methods, _flags) in CLASSES.items():
    group.oracle(f"capabilities.{_file}.{_class}", errors=_ERRORS)(_capability(_class))

# ---------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------

HAN_HVAC = {"vdeTempRangeMin": 17, "vdeTempRangeMax": 33, "pm": 5}
RUNNING = {"status": 1, "cycleChoice": 1, "windPosition": 3, "pm": 5}
SEATS_ON = {"status": 1, "mainSeatHeatState": 3, "copilotSeatHeatState": 2, "mainSeatVentilationState": 1,
            "copilotSeatVentilationState": 0, "lrSeatHeatState": 2, "rrSeatVentilationState": 3,
            "stearingWheelHeatState": -1, "pm": 4}
REALTIME_SEATS = {"mainSeatHeatState": 2, "copilotSeatHeatState": 3, "lrSeatHeatState": 1, "rrSeatHeatState": 2,
                  "lrSeatVentilationState": 3, "stearingWheelHeatState": 2}


def case(file: str, **args: Any) -> tuple[str, dict[str, Any]]:
    return f"capabilities.{file}.{CLASSES[file][0]}", {"vin": VIN, **args}


def _start(**kwargs: Any) -> dict[str, Any]:
    return {"temperature": 21.0, "duration": 10, **kwargs}


def _upstream() -> Iterator[tuple[str, dict[str, Any]]]:
    """The assertions of tests/test_rear_seat_climate.py and tests/test_issue_74.py, as cases."""
    blank = {"hvac": None, "realtime": None}
    yield case("seat", kwargs={"rear_left_heat_available": True}, state=blank, method="heat",
               args={"position": "rear_left", "level": 2})
    yield case("seat", state=blank, method="ventilation", args={"position": "rear_right", "level": 1})
    for position in ("rear_left", "rear_right"):
        for method in ("heat", "ventilation"):
            yield case("seat", kwargs={f"{position}_heat_available": True, f"{position}_ventilation_available": True},
                       state=blank, method=method, args={"position": position, "level": 1})
    # HAN scale and the temperature range.
    yield case("hvac", state={"hvac": None})
    yield case("hvac", state={"hvac": HAN_HVAC})
    yield case("hvac", state={"hvac": HAN_HVAC}, method="start", args=_start(temperature=33))
    yield case("hvac", state={"hvac": HAN_HVAC}, method="start", args=_start(cycle_mode=1))
    yield case("hvac", state=blank, method="start", args=_start())
    yield case("hvac", state=blank, method="start", args=_start(fan_level=4))
    yield case("hvac", state=blank, method="start", args=_start(fan_level=8))
    # A running session keeps its settings (#64) and adjusts only the setpoint (#20).
    yield case("hvac", state={"hvac": RUNNING}, method="start", args=_start(temperature=24))
    yield case("hvac", state={"hvac": RUNNING}, method="start", args=_start(temperature=24, cycle_mode=2, fan_level=6))
    yield case("hvac", state={"hvac": {**RUNNING, "status": 2, "windPosition": 6}}, method="start",
               args=_start(temperature=24))
    yield case("hvac", state={"hvac": RUNNING}, method="start", args=_start(temperature=22))
    yield case("hvac", state={"hvac": RUNNING}, method="start", args=_start(temperature=22, cycle_mode=1, fan_level=3))
    yield case("hvac", state={"hvac": RUNNING}, method="start", args=_start(temperature=22, cycle_mode=2))
    yield case("hvac", state={"hvac": {"status": 2, "pm": 5}}, method="start", args=_start(temperature=22))
    # The range is fetched when no HVAC status is known; a failed fetch falls back to 15-31.
    yield case("hvac", state=blank, method="start", args=_start(),
               refresh={"result": {"status": 2, "vdeTempRangeMin": 17, "vdeTempRangeMax": 33, "pm": 5}})
    yield case("hvac", state=blank, method="start", args=_start(), refresh={"error": {"class": "BydError"}})


def _rule_cases() -> Iterator[tuple[str, dict[str, Any]]]:
    states = [None, {"hvac": None, "realtime": None}, {"hvac": SEATS_ON, "realtime": REALTIME_SEATS},
              {"hvac": None, "realtime": REALTIME_SEATS}, {"hvac": HVAC_CORPUS[2], "realtime": REALTIME_CORPUS[4]}]
    # Every class, method and availability, with success and each command error.
    for file, (_, methods, flags) in CLASSES.items():
        for available in (True, False, None):
            kwargs = {flag: available for flag in flags}
            yield case(file, kwargs=kwargs, state=states[2])
            for method in methods:
                args = {"on": True} if file in ("steering", "battery_heat") else {}
                if file == "seat":
                    args = {"position": "driver", "level": 1}
                if file == "hvac" and method == "start":
                    args = _start()
                yield case(file, kwargs=kwargs, state=states[2], method=method, args=args)
        yield case(file, kwargs={}, state=states[2])
    for error in COMMAND_ERRORS:
        yield case("lock", state=None, method="unlock", reply={"error": error})
        yield case("seat", state=states[2], method="heat", args={"position": "copilot", "level": 0},
                   reply={"error": error})
    # Seats: every position, mode and level against every kind of state.
    flags = {name: True for name in CLASSES["seat"][2]}
    for state in states[1:]:
        for position in ("driver", "copilot", "rear_left", "rear_right"):
            for level in (0, 1, 2):
                for method in ("heat", "ventilation"):
                    yield case("seat", kwargs=flags, state=state, method=method,
                               args={"position": position, "level": level})
    for position, level in [("DRIVER", 1), ("driver", 3), ("driver", "1"), (None, 1), ("driver", True),
                            ("copilot", 2.0), ("front", 0)]:
        yield case("seat", kwargs=flags, state=states[2], method="heat", args={"position": position, "level": level})
    for state in states[1:]:
        for on in (True, False):
            yield case("steering", state=state, method="heat", args={"on": on})
            yield case("battery_heat", state=state, method="heat", args={"on": on})
    # HVAC: stop, schedule, durations, temperatures, cycle modes, running states.
    yield case("hvac", state=states[2], method="stop")
    yield case("hvac", state=None, method="stop", reply={"error": COMMAND_ERRORS[1]})
    for params in ({"booking_id": 1, "booking_time": 1700000000, "temperature": 22.0},
                   {"temperature": 40.0}, {}, {"nonsense": 1}):
        yield case("hvac", state=None, method="schedule", args={"params": params})
    for duration in (10, 15, 20, 25, 30, 0, 11, -10, 60):
        yield case("hvac", state=None, method="start", args={"duration": duration})
    for temperature in (15.0, 15.5, 31.0, 31.5, 14.9, 17, 33.0, -40.0, 21.25):
        yield case("hvac", state=None, method="start", args={"temperature": temperature})
        yield case("hvac", state={"hvac": HAN_HVAC}, method="start", args={"temperature": temperature})
    for cycle_mode in (None, 0, 1, 2, 3, -1):
        for fan_level in (None, 0, 1, 3, 7, 8, -1):
            yield case("hvac", state={"hvac": RUNNING}, method="start",
                       args={"cycle_mode": cycle_mode, "fan_level": fan_level})
    for running in ({"status": 1}, {"status": 1, "cycleChoice": 0, "windPosition": 0},
                    {"status": 1, "cycleChoice": 2, "windPosition": 7}, {"status": 1, "cycleChoice": -1,
                                                                          "windPosition": 9},
                    {"status": -1, "cycleChoice": 1, "windPosition": 3}, {"status": "1", "windPosition": "5"}):
        yield case("hvac", state={"hvac": running}, method="start", args={})
        yield case("hvac", state={"hvac": running}, method="start", args={"cycle_mode": 1})
    # Refresh: the reply, a non-pyBYD error, an invalid payload; none when the state is known.
    yield case("hvac", state=None, method="start", args={}, refresh={"result": HAN_HVAC})
    yield case("hvac", state={"hvac": None}, method="start", args={}, refresh={"error": {"class": "ValueError"}})
    yield case("hvac", state={"hvac": None}, method="start", args={}, refresh={"result": []})
    yield case("hvac", state={"hvac": None}, method="start", args={"fan_level": 9}, refresh={"result": HAN_HVAC})
    yield case("hvac", state={"hvac": RUNNING}, method="start", args={}, refresh={"result": HAN_HVAC})
    for error in COMMAND_ERRORS[:4]:
        yield case("hvac", state={"hvac": None}, method="start", args={}, refresh={"error": error})
    # Windows without an open command; finder and trunk directions.
    yield case("windows", open_fn=False)
    yield case("windows", open_fn=False, method="open")
    yield case("windows", open_fn=False, method="close")
    for file in ("finder", "trunk", "windows"):
        for method in CLASSES[file][1]:
            yield case(file, kwargs={CLASSES[file][2][0]: False}, method=method)
            yield case(file, kwargs={CLASSES[file][2][1]: None}, method=method, reply={"error": COMMAND_ERRORS[0]})
    # Invalid state payloads.
    yield case("seat", state={"hvac": [], "realtime": None})
    yield case("hvac", state={"hvac": None, "realtime": "x"})
    # Enums.
    for value in (0, 1, 2, 3, -1, True, False, 1.0, 2.5, "1", None, [1], -0.0):
        yield "capabilities.seat.SeatLevel", {"value": value}
    for value in ("driver", "copilot", "rear_left", "rear_right", "DRIVER", "", None, 1, ["driver"]):
        yield "capabilities.seat.SeatPosition", {"value": value}


@group.recorded
def recorded() -> Iterator[tuple[str, dict[str, Any]]]:
    yield from _upstream()
    yield from _rule_cases()
    rng = random.Random("capabilities-recorded")
    for _ in range(60):
        yield random_case(rng)


# ---------------------------------------------------------------------------
# Fuzz
# ---------------------------------------------------------------------------


def random_error(rng: random.Random) -> dict[str, Any]:
    if rng.random() < 0.7:
        return copy.deepcopy(rng.choice(COMMAND_ERRORS))
    error: dict[str, Any] = {"class": rng.choice(ERROR_CLASSES)}
    if rng.random() < 0.6:
        error["code"] = rng.choice(["", "0", "1001", "timeout", random_text(rng, 6)])
    if rng.random() < 0.6:
        error["endpoint"] = rng.choice(["", "/control/remoteControl", random_text(rng, 10)])
    if rng.random() < 0.3:
        error["status_code"] = rng.choice([None, 200, 500, 502])
    return error


def random_state(rng: random.Random) -> dict[str, Any] | None:
    roll = rng.random()
    if roll < 0.1:
        return None
    hvac = None if rng.random() < 0.3 else random_hvac(rng)
    if hvac is not None and rng.random() < 0.5:
        hvac.update(rng.choice([RUNNING, SEATS_ON, HAN_HVAC, {"status": rng.choice([1, 2, -1, 0])}]))
    realtime = None if rng.random() < 0.4 else random_realtime(rng)
    if realtime is not None and rng.random() < 0.4:
        realtime.update(REALTIME_SEATS)
    return {"hvac": hvac, "realtime": realtime, "context": rng.choice(ENERGY_CONTEXTS + [None])}


def random_args(rng: random.Random, file: str, method: str) -> dict[str, Any]:
    if file == "seat":
        return {"position": rng.choice(["driver", "copilot", "rear_left", "rear_right"] * 4 + ["x", 1]),
                "level": rng.choice([0, 1, 2] * 4 + [3, -1, True, "2"])}
    if file in ("steering", "battery_heat"):
        return {"on": rng.choice([True, False])}
    if file == "hvac" and method == "start":
        args: dict[str, Any] = {}
        if rng.random() < 0.7:
            args["temperature"] = rng.choice([15.0, 21.0, 21.5, 24.0, 31.0, 33.0, 14.0, 17.0, rng.uniform(10, 36)])
        if rng.random() < 0.7:
            args["duration"] = rng.choice([10, 15, 20, 25, 30, 10, 20, 0, 12, 45])
        if rng.random() < 0.5:
            args["cycle_mode"] = rng.choice([None, 1, 2, 1, 2, 0, 3, -1])
        if rng.random() < 0.5:
            args["fan_level"] = rng.choice([None, 1, 3, 5, 7, 0, 8])
        return args
    if file == "hvac" and method == "schedule":
        return {"params": rng.choice([{}, {"temperature": 22.0}, {"temperature": 99.0}, {"booking_id": 3},
                                      {"x": 1}])}
    return {}


def random_case(rng: random.Random) -> tuple[str, dict[str, Any]]:
    file = rng.choice(list(CLASSES))
    _, methods, flags = CLASSES[file]
    kwargs = {flag: rng.choice([True, True, True, False, None]) for flag in flags if rng.random() < 0.8}
    method = rng.choice(methods + [None]) if rng.random() < 0.95 else None
    args: dict[str, Any] = {"kwargs": kwargs, "state": random_state(rng), "method": method,
                            "args": random_args(rng, file, method or ""),
                            "reply": {"error": random_error(rng)} if rng.random() < 0.3 else None}
    if file in ("seat", "steering") and args["state"] is None and rng.random() < 0.5:
        args["state"] = {"hvac": None, "realtime": None}
    if file == "hvac" and rng.random() < 0.6:
        roll = rng.random()
        args["refresh"] = ({"error": random_error(rng)} if roll < 0.3
                           else {"result": random_hvac(rng)} if roll < 0.9 else {"result": rng.choice([[], "x", None])})
    if file == "windows" and rng.random() < 0.2:
        args["open_fn"] = False
    return case(file, **args)


@group.fuzz
def fuzz(rng: random.Random) -> Iterator[tuple[str, dict[str, Any]]]:
    while True:
        if rng.random() < 0.05:
            yield "capabilities.seat.SeatLevel", {"value": rng.choice([0, 1, 2, 3, True, 1.0, 1.5, "0", None])}
        elif rng.random() < 0.05:
            yield "capabilities.seat.SeatPosition", {"value": rng.choice(["driver", "rear_right", "Driver", "", 0])}
        else:
            yield random_case(rng)
