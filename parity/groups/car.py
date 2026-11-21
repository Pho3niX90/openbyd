"""pybyd/car.py: ``BydCar`` driven through sequences of operations with a scripted client.

Every ``car.<name>`` call (``car.BydCar`` and one per public method and
property of ``BydCar``) runs a sequence on a fresh car; the recorded cases
filed under a name exercise that method. The arguments and the answer are
documented in ``crates/openbyd-client/src/car/parity.rs``. In short::

    {"vin": ..., "vehicle": {...}, "capabilities": {...} | null, "latest_config": {...},
     "projection_ttl": 30.0, "start": 1000.0,
     "ops": [{"op": "call", "target": "lock", "method": "unlock", "args": {}, "replies": [...]},
             {"op": "advance", "seconds": 31.0}, {"op": "read"}]}

A reply is ``{"result": raw, "context": ..., "pushes": [...]}`` or
``{"error": <scripted error>, "pushes": [...]}``; a push is ``{"kind":
"realtime" | "charging" | "energy", "raw": ..., "context": ...}`` or
``{"kind": "status_push", "raw": ..., "hvac": ..., "charging": ..., "context":
...}`` (the partial models of a ``vehicleStatusPush``; ``status_push_of``
builds one from a push payload the way the client does).

pyBYD's side runs the real ``BydCar`` on an event loop, with
``time.monotonic`` patched to the sequence's clock and a fake client that
records each call, delivers the reply's pushes through
``car.handle_mqtt_*`` (yielding once so their tasks start) and then answers.
After each operation the loop runs until the car's tasks are done.
"""

from __future__ import annotations

import asyncio
import copy
import dataclasses
import json
import random
import time
import warnings
from collections.abc import Iterator
from pathlib import Path
from typing import Any
from unittest import mock

from pybyd.car import BydCar
from pybyd.exceptions import BydError
from pybyd.models.charging import ChargingStatus
from pybyd.models.control import ChargeChangeResult, ClimateScheduleParams, RemoteControlResult
from pybyd.models.energy import EnergyConsumption
from pybyd.models.gps import GpsInfo
from pybyd.models.hvac import HvacStatus
from pybyd.models.latest_config import VehicleCapabilities, VehicleLatestConfig
from pybyd.models.realtime import VehicleRealtimeData
from pybyd.models.smart_charging import AcCurrentLimit, SmartChargingSchedule
from pybyd.models.vehicle import Vehicle
from pybyd._capabilities.seat import SeatLevel, SeatPosition

from parity.car_support import COMMAND_ERRORS, ScriptExhausted, describe, scripted_error
from parity.framework import ROOT, Group
from parity.groups.capabilities import HAN_HVAC, REALTIME_SEATS, RUNNING, SEATS_ON, params_json, random_error
from parity.groups.models_latest_config import LATEST_SAMPLES
from parity.groups.models_realtime import ENERGY_CONTEXTS, energy, python_context
from parity.groups.mqtt import STATUS_PUSHES, random_status_push
from parity.groups.state import (
    ASLEEP_HTTP,
    CHARGING_BASE,
    CHARGING_SAMPLES,
    ENERGY_CORPUS,
    GPS_VALID,
    GPS_VALID_B,
    HVAC_BASE,
    HVAC_CORPUS,
    HVAC_PLACEHOLDER,
    MQTT_PUSH,
    REALTIME_CHARGING,
    REALTIME_CORPUS,
    SCHEDULE_SAMPLES,
    _snapshot_dump,
    changes,
    charging_cases,
    random_energy,
    random_gps,
    random_hvac,
    random_partial,
    random_payload,
    random_realtime,
    status_push_op,
)
from parity.models_support import json_safe, jsonable

group = Group("car", crate="client")

VIN = "TESTVIN0000000001"
DRAIN = 20

CALLS = [
    "car.BydCar", "car.vin", "car.state", "car.capabilities", "car.update_realtime", "car.update_hvac",
    "car.update_gps", "car.update_charging", "car.start_charging", "car.stop_charging", "car.update_energy",
    "car.get_ac_current_limit", "car.save_charging_schedule", "car.handle_mqtt_realtime",
    "car.handle_mqtt_status_push", "car.handle_mqtt_charging", "car.handle_mqtt_energy", "car.close",
]

# ---------------------------------------------------------------------------
# The scripted client
# ---------------------------------------------------------------------------

_PUSHES = {"realtime": ("handle_mqtt_realtime", VehicleRealtimeData), "charging": ("handle_mqtt_charging", ChargingStatus),
           "energy": ("handle_mqtt_energy", EnergyConsumption)}


def parse_push(kind: str, raw: Any, context: Any) -> Any:
    cls = _PUSHES.get(kind, _PUSHES["energy"])[1]
    if cls is VehicleRealtimeData:
        return cls.model_validate(raw, context=python_context(context))
    return cls.model_validate(raw)


def deliver(car: BydCar, push: dict[str, Any]) -> None:
    """Validates ``push`` as the client does and hands it to the car's handler for its kind."""
    kind = push.get("kind", "")
    if kind == "status_push":
        data = parse_push("realtime", push.get("raw"), push.get("context"))
        hvac = None if push.get("hvac") is None else HvacStatus.model_validate(push["hvac"])
        charging = None if push.get("charging") is None else ChargingStatus.model_validate(push["charging"])
        car.handle_mqtt_status_push(data, hvac=hvac, charging=charging)
        return
    data = parse_push(kind, push.get("raw"), push.get("context"))
    getattr(car, _PUSHES.get(kind, _PUSHES["energy"])[0])(data)


class FakeClient:
    """The ``BydClient`` methods ``BydCar`` calls, answering from a script."""

    def __init__(self) -> None:
        self.replies: list[dict[str, Any]] = []
        self.calls: list[dict[str, Any]] = []
        self.car: BydCar | None = None

    async def _answer(self, call: dict[str, Any]) -> tuple[Any, Any]:
        self.calls.append(call)
        if not self.replies:
            raise ScriptExhausted("no scripted reply left")
        reply = self.replies.pop(0)
        pushes = reply.get("pushes") or []
        if pushes:
            for push in pushes:
                try:
                    deliver(self.car, push)
                except Exception:  # noqa: BLE001 - the client drops a push it cannot parse
                    continue
            await asyncio.sleep(0)
        if "error" in reply:
            raise scripted_error(reply["error"])
        return reply.get("result"), reply.get("context")

    async def _model(self, cls: type, call: dict[str, Any]) -> Any:
        raw, context = await self._answer(call)
        if cls is VehicleRealtimeData:
            return cls.model_validate(raw, context=python_context(context))
        return cls.model_validate(raw)

    async def _command(self, method: str, vin: str, params: Any = None) -> Any:
        call: dict[str, Any] = {"method": method, "vin": vin}
        if params is not None:
            call["params"] = params_json(params)
        return await self._model(RemoteControlResult, call)

    async def lock(self, vin: str, **_: Any) -> Any:
        return await self._command("lock", vin)

    async def unlock(self, vin: str, **_: Any) -> Any:
        return await self._command("unlock", vin)

    async def start_climate(self, vin: str, *, params: Any, **_: Any) -> Any:
        return await self._command("start_climate", vin, params)

    async def stop_climate(self, vin: str, **_: Any) -> Any:
        return await self._command("stop_climate", vin)

    async def schedule_climate(self, vin: str, *, params: Any, **_: Any) -> Any:
        return await self._command("schedule_climate", vin, params)

    async def set_seat_climate(self, vin: str, *, params: Any, **_: Any) -> Any:
        return await self._command("set_seat_climate", vin, params)

    async def set_battery_heat(self, vin: str, *, params: Any, **_: Any) -> Any:
        return await self._command("set_battery_heat", vin, params)

    async def find_car(self, vin: str, **_: Any) -> Any:
        return await self._command("find_car", vin)

    async def flash_lights(self, vin: str, **_: Any) -> Any:
        return await self._command("flash_lights", vin)

    async def close_windows(self, vin: str, **_: Any) -> Any:
        return await self._command("close_windows", vin)

    async def open_windows(self, vin: str, **_: Any) -> Any:
        return await self._command("open_windows", vin)

    async def open_trunk(self, vin: str, **_: Any) -> Any:
        return await self._command("open_trunk", vin)

    async def close_trunk(self, vin: str, **_: Any) -> Any:
        return await self._command("close_trunk", vin)

    async def get_vehicle_realtime(self, vin: str, **_: Any) -> Any:
        return await self._model(VehicleRealtimeData, {"method": "get_vehicle_realtime", "vin": vin})

    async def get_hvac_status(self, vin: str) -> Any:
        return await self._model(HvacStatus, {"method": "get_hvac_status", "vin": vin})

    async def get_gps_info(self, vin: str, **_: Any) -> Any:
        return await self._model(GpsInfo, {"method": "get_gps_info", "vin": vin})

    async def get_charging_homepage(self, vin: str) -> Any:
        raw, _ = await self._answer({"method": "get_charging_homepage", "vin": vin})
        status = ChargingStatus.model_validate(raw)
        schedule = SmartChargingSchedule.model_validate({"vin": vin, **raw})
        return status, schedule

    async def start_charging(self, vin: str, **_: Any) -> Any:
        return await self._model(ChargeChangeResult, {"method": "start_charging", "vin": vin})

    async def stop_charging(self, vin: str, **_: Any) -> Any:
        return await self._model(ChargeChangeResult, {"method": "stop_charging", "vin": vin})

    async def get_energy_consumption(self, vin: str) -> Any:
        return await self._model(EnergyConsumption, {"method": "get_energy_consumption", "vin": vin})

    async def get_ac_current_limit(self, vin: str, **_: Any) -> Any:
        return await self._model(AcCurrentLimit, {"method": "get_ac_current_limit", "vin": vin})

    async def save_charging_schedule(self, vin: str, *, start_charge_time: str, end_charge_time: str,
                                     charge_way: str, enabled: bool = True, **_: Any) -> Any:
        return await self._model(ChargeChangeResult, {
            "method": "save_charging_schedule", "vin": vin, "start_charge_time": start_charge_time,
            "end_charge_time": end_charge_time, "charge_way": charge_way, "enabled": enabled})


# ---------------------------------------------------------------------------
# The oracle
# ---------------------------------------------------------------------------


def views(car: BydCar) -> dict[str, Any]:
    return {
        "lock": {"available": car.lock.available},
        "hvac": {"available": car.hvac.available, "temp_offset": car.hvac.temp_offset,
                 "temp_range": list(car.hvac.temp_range)},
        "seat": {"modes": {p.value: {"heat": car.seat._is_mode_available(p, "heat"),
                                     "vent": car.seat._is_mode_available(p, "vent")} for p in SeatPosition}},
        "steering": {"available": car.steering.available},
        "battery": {"available": car.battery.available},
        "finder": {"find_available": car.finder.find_available, "flash_available": car.finder.flash_available},
        "windows": {"close_available": car.windows.close_available, "open_available": car.windows.open_available},
        "trunk": {"open_available": car.trunk.open_available, "close_available": car.trunk.close_available},
    }


def _dump(model: Any) -> Any:
    return jsonable(model.model_dump(mode="json"))


async def _call(car: BydCar, target: str, method: str, args: dict[str, Any]) -> Any:
    if target == "car":
        if method == "vin":
            return car.vin
        if method == "state":
            return _snapshot_dump(car.state)
        if method == "capabilities":
            return _dump(car.capabilities)
        if method == "close":
            return car.close()
        if method.startswith("handle_mqtt_"):
            return deliver(car, {**args, "kind": method[len("handle_mqtt_"):]})
        if method == "save_charging_schedule":
            enabled = True if args.get("enabled") is None else args["enabled"]
            return _dump(await car.save_charging_schedule(
                start_charge_time=args["start_charge_time"], end_charge_time=args["end_charge_time"],
                charge_way=args["charge_way"], enabled=enabled))
        return _dump(await getattr(car, method)())
    feature = getattr(car, target)
    if target == "seat":
        await getattr(feature, method)(SeatPosition(args.get("position")), SeatLevel(args.get("level")))
    elif target in ("steering", "battery"):
        await feature.heat(on=args["on"])
    elif target == "hvac" and method == "start":
        kwargs: dict[str, Any] = {key: args[key] for key in ("cycle_mode", "fan_level") if key in args}
        if args.get("temperature") is not None:
            kwargs["temperature"] = float(args["temperature"])
        if args.get("duration") is not None:
            kwargs["duration"] = args["duration"]
        await feature.start(**kwargs)
    elif target == "hvac" and method == "schedule":
        await feature.schedule(ClimateScheduleParams(**args.get("params", {})))
    else:
        await getattr(feature, method)()
    return None


async def _steps(car: BydCar, client: FakeClient, ops: list[dict[str, Any]], clock: list[float],
                 notified: list[str]) -> dict[str, Any]:
    initial = _snapshot_dump(car.state)
    before_views = views(car)
    answer: dict[str, Any] = {"capabilities": _dump(car.capabilities), "views": before_views, "initial": initial}
    before = initial
    steps = []
    for op in ops:
        notified.clear()
        client.calls = []
        client.replies = copy.deepcopy(list(op.get("replies") or []))
        step: dict[str, Any] = {}
        outcome = None
        kind = op["op"]
        if kind == "call":
            try:
                outcome = ("result", await _call(car, op["target"], op["method"], op.get("args") or {}))
            except Exception as exc:  # noqa: BLE001 - every failure is part of the answer
                outcome = ("error", describe(exc))
        elif kind == "advance":
            clock[0] += op["seconds"]
        elif kind != "read":
            raise KeyError(f"unknown op {kind!r}")
        for _ in range(DRAIN):
            await asyncio.sleep(0)
        step["calls"] = client.calls
        if outcome is not None:
            step[outcome[0]] = outcome[1]
        after = _snapshot_dump(car.state)
        step["changes"] = changes(before, after)
        step["active"] = [jsonable(dataclasses.asdict(p)) for p in car._engine.active_projections]
        step["held"] = len(car._engine._projections)
        step["notified"] = len(notified)
        after_views = views(car)
        changed = {name: view for name, view in after_views.items() if before_views.get(name) != view}
        if changed:
            step["views"] = changed
        if client.replies:
            step["unused"] = len(client.replies)
        steps.append(step)
        before, before_views = after, after_views
    answer["steps"] = steps
    return answer


_LOOP: asyncio.AbstractEventLoop | None = None


def _loop() -> asyncio.AbstractEventLoop:
    global _LOOP
    if _LOOP is None or _LOOP.is_closed():
        _LOOP = asyncio.SelectorEventLoop()
    return _LOOP


def car_sequence(vin: str, vehicle: Any, ops: list[dict[str, Any]], start: float, capabilities: Any = None,
                 latest_config: Any = None, projection_ttl: float | None = None) -> Any:
    model = Vehicle.model_validate(vehicle)
    if latest_config is not None:
        caps = VehicleCapabilities.from_latest_config(vin, VehicleLatestConfig.model_validate(latest_config))
    elif capabilities is not None:
        caps = VehicleCapabilities.model_validate(capabilities)
    else:
        caps = None
    clock = [float(start)]
    notified: list[str] = []
    ttl = 30.0 if projection_ttl is None else float(projection_ttl)
    # A projected enum makes pydantic warn while dumping; the dump is what is compared.
    with mock.patch.object(time, "monotonic", lambda: clock[0]), warnings.catch_warnings():
        warnings.simplefilter("ignore")
        client = FakeClient()
        try:
            car = BydCar(client, vin, model, capabilities=caps,  # type: ignore[arg-type]
                         on_state_changed=lambda v, s: notified.append(v), projection_ttl=ttl)
        except Exception as exc:  # noqa: BLE001 - a car that cannot be built is the answer
            return {"error": describe(exc)}
        client.car = car
        loop = _loop()
        try:
            return loop.run_until_complete(_steps(car, client, ops, clock, notified))
        finally:
            for task in asyncio.all_tasks(loop):
                task.cancel()


for _name in CALLS:
    group.oracle(_name, errors=(BydError, ValueError))(car_sequence)

# ---------------------------------------------------------------------------
# Case builders
# ---------------------------------------------------------------------------

EV, HYBRID = energy(0), energy(2)
REMOTE_OK = {"controlState": 1, "res": 2}
CHARGE_OK = {"res": 2, "message": "Operation successful"}
LOCKED = {"leftFrontDoorLock": 2, "rightFrontDoorLock": 2, "leftRearDoorLock": 2, "rightRearDoorLock": 2}
UNLOCKED = {key: 1 for key in LOCKED}
CAPS_FIXTURE = {"vin": VIN, "source": "test_fixture", "functionNos": ["1026"], "openWindows": True,
                "closeWindows": True}
CAPS_NONE = {"vin": VIN, "source": "unknown:unavailable"}
CAPS_ALL = {"vin": VIN, "source": "test_fixture", **{name: True for name in (
    "lock", "unlock", "climate", "batteryHeat", "steeringWheelHeat", "driverSeatHeat", "driverSeatVentilation",
    "passengerSeatHeat", "passengerSeatVentilation", "rearLeftSeatHeat", "rearRightSeatHeat",
    "rearLeftSeatVentilation", "rearRightSeatVentilation", "findCar", "flashLights", "closeWindows", "openWindows",
    "openTrunk", "closeTrunk")}}
VEHICLE = {"vin": VIN, "modelName": "SEAL", "energyType": "0"}
LEARN_CLOSED = {"vin": VIN, "vehicleFunLearnInfo": {"openWindowLearnInfo": 0, "openWindow499LearnInfo": 1}}
LEARN_OPEN = {"vin": VIN, "vehicleFunLearnInfo": {"openWindowLearnInfo": 1, "openWindow499LearnInfo": 1}}


def _corpus(name: str) -> list[Any]:
    path = ROOT / "parity" / "corpus" / f"{name}.json"
    return [entry["payload"] for entry in json.loads(Path(path).read_text(encoding="utf-8"))]


VEHICLE_CORPUS = _corpus("Vehicle")
AC_LIMIT_CORPUS = _corpus("AcCurrentLimit")


def sequence(name: str, ops: list[dict[str, Any]], *, vehicle: Any = None, capabilities: Any = None,
             latest_config: Any = None, ttl: float = 30.0, start: float = 1000.0) -> tuple[str, dict[str, Any]]:
    args: dict[str, Any] = {"vin": VIN, "vehicle": VEHICLE if vehicle is None else vehicle,
                            "capabilities": capabilities, "projection_ttl": ttl, "start": start, "ops": ops}
    if latest_config is not None:
        args["latest_config"] = latest_config
    return f"car.{name}", args


def ok(result: Any = None, *, pushes: list[dict[str, Any]] | None = None, context: Any = None) -> dict[str, Any]:
    reply: dict[str, Any] = {"result": REMOTE_OK if result is None else result}
    if context is not None:
        reply["context"] = context
    if pushes:
        reply["pushes"] = pushes
    return reply


def err(error: dict[str, Any], *, pushes: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    reply: dict[str, Any] = {"error": error}
    if pushes:
        reply["pushes"] = pushes
    return reply


def push_of(kind: str, raw: Any, context: Any = None) -> dict[str, Any]:
    return {"kind": kind, "raw": raw, "context": context}


def call(target: str, method: str, args: dict[str, Any] | None = None, *replies: dict[str, Any]) -> dict[str, Any]:
    return {"op": "call", "target": target, "method": method, "args": args or {}, "replies": list(replies)}


def update(section: str, *replies: dict[str, Any]) -> dict[str, Any]:
    return call("car", f"update_{section}", {}, *replies)


def push(kind: str, raw: Any, context: Any = None) -> dict[str, Any]:
    return call("car", f"handle_mqtt_{kind}", {"raw": raw, "context": context})


def status_push_of(payload: dict[str, Any], context: Any = None) -> dict[str, Any] | None:
    """The ``status_push`` the client makes of a ``vehicleStatusPush`` payload,
    or ``None`` when it has no known section (the client then calls nothing)."""
    partials = status_push_op(payload, context)
    if partials["realtime"] is None:
        return None
    return {"kind": "status_push", "raw": partials["realtime"], "hvac": partials["hvac"],
            "charging": partials["charging"], "context": context}


def status_push(raw: Any, hvac: Any = None, charging: Any = None, context: Any = None) -> dict[str, Any]:
    """A ``handle_mqtt_status_push`` call with these partial payloads."""
    return call("car", "handle_mqtt_status_push", {"raw": raw, "hvac": hvac, "charging": charging, "context": context})


def push_call(pushed: dict[str, Any]) -> dict[str, Any]:
    """The ``handle_mqtt_*`` call that delivers ``pushed`` (a push of a reply)."""
    args = {key: value for key, value in pushed.items() if key != "kind"}
    return call("car", f"handle_mqtt_{pushed['kind']}", args)


def advance(seconds: float) -> dict[str, Any]:
    return {"op": "advance", "seconds": seconds}


READ = {"op": "read"}
TIMEOUT = {"class": "BydRemoteControlError", "code": "timeout", "endpoint": "/control/remoteControlResult"}
REJECTED = COMMAND_ERRORS[0]

FEATURE_COMMANDS = [
    ("lock", "lock", {}), ("lock", "unlock", {}), ("hvac", "start", {"temperature": 22.0, "duration": 10}),
    ("hvac", "stop", {}), ("hvac", "schedule", {"params": {"temperature": 22.0}}),
    ("seat", "heat", {"position": "driver", "level": 2}), ("seat", "ventilation", {"position": "copilot", "level": 1}),
    ("seat", "heat", {"position": "rear_left", "level": 1}), ("seat", "ventilation", {"position": "rear_right", "level": 0}),
    ("steering", "heat", {"on": True}), ("steering", "heat", {"on": False}), ("battery", "heat", {"on": True}),
    ("battery", "heat", {"on": False}), ("finder", "find", {}), ("finder", "flash_lights", {}),
    ("windows", "close", {}), ("windows", "open", {}), ("trunk", "open", {}), ("trunk", "close", {}),
]

# ---------------------------------------------------------------------------
# Recorded cases
# ---------------------------------------------------------------------------


def _construction() -> Iterator[tuple[str, dict[str, Any]]]:
    # tests/test_windows_open_available.py: the open gate follows the learn info.
    for vehicle in (LEARN_CLOSED, LEARN_OPEN, {"vin": VIN, "vehicleFunLearnInfo": {}}, *VEHICLE_CORPUS):
        yield sequence("BydCar", [READ], vehicle=vehicle, capabilities=CAPS_FIXTURE)
    yield sequence("BydCar", [READ], vehicle={"vin": VIN},
                   capabilities={"vin": VIN, "source": "test_fixture", "functionNos": []})
    for capabilities in (None, CAPS_NONE, CAPS_ALL):
        yield sequence("BydCar", [READ], capabilities=capabilities)
    for latest in LATEST_SAMPLES:
        yield sequence("BydCar", [READ], latest_config=latest, vehicle=LEARN_OPEN)
    yield sequence("BydCar", [READ], vehicle="x")
    yield sequence("BydCar", [READ], capabilities=[1])
    yield sequence("BydCar", [], vehicle={"vin": VIN, "vehicleFunLearnInfo": {"openWindowLearnInfo": "x"}},
                   capabilities=CAPS_FIXTURE)
    for name in ("vin", "state", "capabilities", "close"):
        yield sequence(name, [call("car", name), update("hvac", ok(HVAC_CORPUS[2])), call("car", name)])


def _features() -> Iterator[tuple[str, dict[str, Any]]]:
    base = [update("realtime", ok(MQTT_PUSH, context=HYBRID)), update("hvac", ok(SEATS_ON))]
    for target, method, args in FEATURE_COMMANDS:
        for capabilities in (CAPS_ALL, CAPS_NONE, None):
            yield sequence("BydCar", base + [call(target, method, args, ok()), READ], capabilities=capabilities)
        # Each kind of failure rolls the projection back; a timeout is a BydRemoteControlError too.
        yield sequence("BydCar", base + [call(target, method, args, err(TIMEOUT))], capabilities=CAPS_ALL)
    for error in COMMAND_ERRORS:
        yield sequence("BydCar", base + [call("lock", "unlock", {}, err(error)), READ], capabilities=CAPS_ALL)
        yield sequence("BydCar", base + [call("seat", "heat", {"position": "driver", "level": 1}, err(error))],
                       capabilities=CAPS_ALL)
    # A result that does not validate, and no reply at all.
    yield sequence("BydCar", base + [call("lock", "lock", {}, ok([1, 2])), call("lock", "lock", {})],
                   capabilities=CAPS_ALL)
    # Invalid arguments.
    for args in ({"position": "front", "level": 1}, {"position": "driver", "level": 5}):
        yield sequence("BydCar", base + [call("seat", "heat", args, ok())], capabilities=CAPS_ALL)
    yield sequence("BydCar", [call("hvac", "start", {"fan_level": 9}, ok()),
                              call("hvac", "start", {"duration": 12}, ok()),
                              call("hvac", "start", {"temperature": 40.0}, ok(HAN_HVAC), ok()),
                              call("hvac", "schedule", {"params": {"temperature": 99.0}}, ok())])


def _projections() -> Iterator[tuple[str, dict[str, Any]]]:
    rt = update("realtime", ok(MQTT_PUSH, context=HYBRID))
    # Applied, then confirmed by the next update.
    yield sequence("BydCar", [rt, call("lock", "unlock", {}, ok()), READ,
                              update("realtime", ok({**MQTT_PUSH, **UNLOCKED}, context=HYBRID)), READ])
    # Applied, contradicted (guard window), then expired.
    yield sequence("BydCar", [rt, call("lock", "unlock", {}, ok()),
                              update("realtime", ok(MQTT_PUSH, context=HYBRID)), advance(30.0), READ, advance(0.5),
                              READ, update("realtime", ok(MQTT_PUSH, context=HYBRID))])
    yield sequence("BydCar", [rt, call("lock", "lock", {}, ok()), advance(6.0), update("gps", ok(GPS_VALID))], ttl=5.0)
    # Rolled back on failure; a later success replaces an earlier projection of the same field.
    yield sequence("BydCar", [rt, call("lock", "lock", {}, ok()), call("lock", "unlock", {}, err(REJECTED)),
                              call("lock", "unlock", {}, ok()), update("realtime", ok(MQTT_PUSH, context=HYBRID))])
    # Pushes before, during and after a command.
    unlocked_push = push_of("realtime", {**MQTT_PUSH, **UNLOCKED}, HYBRID)
    locked_push = push_of("realtime", MQTT_PUSH, HYBRID)
    yield sequence("BydCar", [push("realtime", MQTT_PUSH, HYBRID), call("lock", "unlock", {}, ok(pushes=[locked_push])),
                              push("realtime", {**MQTT_PUSH, **UNLOCKED}, HYBRID)])
    yield sequence("BydCar", [push("realtime", MQTT_PUSH, HYBRID),
                              call("lock", "unlock", {}, ok(pushes=[unlocked_push]))])
    yield sequence("BydCar", [push("realtime", MQTT_PUSH, HYBRID),
                              call("lock", "unlock", {}, err(TIMEOUT, pushes=[unlocked_push, locked_push]))])
    yield sequence("BydCar", [call("lock", "unlock", {}, err(REJECTED, pushes=[unlocked_push])), READ])
    yield sequence("BydCar", [update("realtime", ok(MQTT_PUSH, pushes=[unlocked_push], context=HYBRID)),
                              update("hvac", ok(HVAC_CORPUS[2], pushes=[push_of("charging", CHARGING_SAMPLES[0]),
                                                                       push_of("energy", ENERGY_CORPUS[0])]))])
    yield sequence("BydCar", [call("lock", "lock", {}, ok(pushes=[push_of("realtime", "x"), push_of("charging", []),
                                                                  push_of("energy", 5), locked_push]))])
    # Seats read the projected state of the other seats; steering after seats; stop clears them.
    yield sequence("BydCar", [update("hvac", ok(SEATS_ON)), update("realtime", ok(REALTIME_SEATS)),
                              call("seat", "heat", {"position": "driver", "level": 2}, ok()),
                              call("seat", "heat", {"position": "copilot", "level": 1}, ok()),
                              call("steering", "heat", {"on": True}, ok()),
                              call("seat", "ventilation", {"position": "rear_left", "level": 2}, ok()),
                              call("hvac", "stop", {}, ok()),
                              call("seat", "heat", {"position": "rear_right", "level": 1}, ok())],
                   capabilities=CAPS_ALL)
    yield sequence("BydCar", [update("realtime", ok(REALTIME_SEATS)),
                              call("seat", "heat", {"position": "driver", "level": 0}, ok()),
                              call("steering", "heat", {"on": False}, ok()),
                              call("seat", "heat", {"position": "copilot", "level": 2}, err(REJECTED)),
                              call("seat", "heat", {"position": "rear_left", "level": 2}, ok())],
                   capabilities=CAPS_ALL)
    # HVAC: the refresh when no status is known, the scale it brings, the projected running state.
    yield sequence("BydCar", [call("hvac", "start", {"temperature": 33.0, "duration": 10}, ok(HAN_HVAC), ok()),
                              call("hvac", "start", {"temperature": 30.0, "duration": 10}, ok()),
                              call("hvac", "start", {"temperature": 30.0, "fan_level": 5}, ok()),
                              call("hvac", "start", {"temperature": 29.0}, ok()),
                              call("hvac", "start", {"temperature": 29.0, "cycle_mode": 1}, ok()),
                              update("hvac", ok({**HAN_HVAC, "status": 2}))])
    for refresh in (err({"class": "BydError"}), err(COMMAND_ERRORS[7]), err({"class": "ValueError"}), ok([]),
                    ok({"status": 1, "cycleChoice": 2, "windPosition": 4, "pm": 3})):
        yield sequence("BydCar", [call("hvac", "start", {}, refresh, ok()), READ])
    yield sequence("BydCar", [call("hvac", "start", {})])
    yield sequence("BydCar", [update("hvac", ok(RUNNING)), call("hvac", "start", {"temperature": 22.0}, ok()),
                              call("hvac", "start", {"temperature": 22.0, "cycle_mode": 2}, err(TIMEOUT)),
                              call("hvac", "start", {"temperature": 23.0, "fan_level": 3}, ok()),
                              call("hvac", "stop", {}, ok()), call("hvac", "start", {"temperature": 23.0}, ok())])
    yield sequence("BydCar", [update("hvac", ok(HVAC_PLACEHOLDER)), call("battery", "heat", {"on": True}, ok()),
                              update("realtime", ok({"batteryHeatState": 1}, context=EV)),
                              call("battery", "heat", {"on": False}, err(TIMEOUT))])


def _reads() -> Iterator[tuple[str, dict[str, Any]]]:
    yield sequence("update_realtime", [update("realtime", ok(REALTIME_CORPUS[0], context=EV)),
                                       update("realtime", ok(ASLEEP_HTTP, context=EV)),
                                       update("realtime", err(COMMAND_ERRORS[7])),
                                       update("realtime", ok("x")), update("realtime")])
    yield sequence("update_hvac", [update("hvac", ok(HVAC_CORPUS[0])), update("hvac", ok(HVAC_PLACEHOLDER)),
                                   update("hvac", err({"class": "BydDataUnavailableError", "code": "6051"})),
                                   update("hvac", ok(HAN_HVAC))])
    yield sequence("update_gps", [update("gps", ok({"latitude": 0.0, "longitude": 0.0})), update("gps", ok(GPS_VALID)),
                                  update("gps", ok({"latitude": 0.01})), update("gps", ok(GPS_VALID_B)),
                                  update("gps", err({"class": "BydDataUnavailableError", "code": "6051"}))])
    homepage = {**CHARGING_SAMPLES[1], **SCHEDULE_SAMPLES[0]}
    yield sequence("update_charging", [update("charging", ok(homepage)), update("charging", ok(CHARGING_SAMPLES[2])),
                                       update("charging", ok(SCHEDULE_SAMPLES[1])), update("charging", ok([1])),
                                       update("charging", err(COMMAND_ERRORS[7]))])
    yield sequence("update_energy", [update("energy", ok(ENERGY_CORPUS[0])), update("energy", ok(ENERGY_CORPUS[1])),
                                     update("energy", ok(5)), update("energy", err({"class": "BydApiError"}))])
    yield sequence("start_charging", [call("car", "start_charging", {}, ok(CHARGE_OK)),
                                      call("car", "start_charging", {}, err({"class": "BydRemoteControlError",
                                                                             "code": "3"})),
                                      call("car", "start_charging", {}, err({"class": "BydEndpointNotSupportedError",
                                                                             "code": "command_gate_blocked"}))])
    # tests/test_stop_charging.py: the client always refuses.
    yield sequence("stop_charging", [call("car", "stop_charging", {}, err(
        {"class": "BydEndpointNotSupportedError", "code": "cloud_stop_unsupported",
         "endpoint": "/control/smartCharge/changeChargeStatue"}))])
    yield sequence("get_ac_current_limit", [call("car", "get_ac_current_limit", {}, ok(raw)) for raw in AC_LIMIT_CORPUS]
                   + [call("car", "get_ac_current_limit", {}, err({"class": "BydApiError", "code": "1001"}))])
    save = {"start_charge_time": "22:00", "end_charge_time": "06:00", "charge_way": "1"}
    yield sequence("save_charging_schedule", [
        call("car", "save_charging_schedule", save, ok(CHARGE_OK), ok({**CHARGING_SAMPLES[1], **SCHEDULE_SAMPLES[0]})),
        call("car", "save_charging_schedule", {**save, "enabled": False}, ok(CHARGE_OK), err(COMMAND_ERRORS[7])),
        call("car", "save_charging_schedule", save, err({"class": "BydRemoteControlError", "code": "timeout"})),
        call("car", "save_charging_schedule", save, ok(CHARGE_OK)),
    ])
    # Charging and energy pushes: the schedule section stays as the last refresh left it.
    yield sequence("handle_mqtt_charging", [update("charging", ok({**CHARGING_SAMPLES[1], **SCHEDULE_SAMPLES[0]})),
                                            push("charging", CHARGING_SAMPLES[2]), push("charging", CHARGING_SAMPLES[0]),
                                            push("charging", "x")])
    yield sequence("handle_mqtt_energy", [push("energy", ENERGY_CORPUS[0]), push("energy", ENERGY_CORPUS[1]),
                                          push("energy", [])])
    yield sequence("handle_mqtt_realtime", [push("realtime", MQTT_PUSH, HYBRID), push("realtime", ASLEEP_HTTP, HYBRID),
                                            push("realtime", REALTIME_CORPUS[4], EV), push("realtime", "x")])


def _status_pushes() -> Iterator[tuple[str, dict[str, Any]]]:
    """``handle_mqtt_status_push``: partial pushes laid over what the car knows."""
    name = "handle_mqtt_status_push"
    known = [update("realtime", ok(REALTIME_CHARGING, context=HYBRID)), update("hvac", ok(HVAC_BASE)),
             update("charging", ok(CHARGING_BASE))]
    # The upstream pushes on a car that knows every section, and on one that knows nothing yet.
    for payload in STATUS_PUSHES:
        pushed = status_push_of(payload, HYBRID)
        ops = [READ] if pushed is None else [push_call(pushed)]
        yield sequence(name, [*known, *ops, READ])
        yield sequence(name, [*ops, READ])
    # One section known: the others are skipped.
    for kept in known:
        yield sequence(name, [kept, push_call(status_push_of(STATUS_PUSHES[2], HYBRID))])
    # Partials that carry nothing, and ones that do not validate: nothing is merged, not even the valid part.
    yield sequence(name, [*known, status_push({}), status_push({"foo": 1}, {}, {}), status_push("x"), status_push(None),
                          status_push({"elecPercent": 60}, []), status_push({"elecPercent": 60}, {"pm": 1}, {"soc": "x"}),
                          status_push({}, None, None, 5), status_push({"elecPercent": 61}, {"pm": 2}, {"soc": 71})])
    # A command's projection: confirmed by a status push, held against a contradicting one, expired by one.
    unlocked = {"kind": "status_push", "raw": UNLOCKED, "hvac": None, "charging": None, "context": None}
    locked = {**unlocked, "raw": LOCKED}
    rt = known[0]
    yield sequence(name, [rt, call("lock", "unlock", {}, ok()), push_call(locked), READ, push_call(unlocked), READ])
    yield sequence(name, [rt, call("lock", "unlock", {}, ok(pushes=[unlocked])), READ])
    yield sequence(name, [rt, call("lock", "unlock", {}, ok(pushes=[locked, push_of("realtime", "x"),
                                                                    {"kind": "status_push", "raw": "x"},
                                                                    {**unlocked, "hvac": []}])), READ])
    yield sequence(name, [rt, call("lock", "lock", {}, err(TIMEOUT, pushes=[unlocked])), READ])
    yield sequence(name, [rt, call("lock", "unlock", {}, ok()), advance(31.0), status_push({}), READ,
                          status_push({"elecPercent": 50}), READ])
    yield sequence(name, [known[1], rt, call("hvac", "stop", {}, ok()),
                          status_push({"elecPercent": 50}, {"status": 1}), status_push({}, {"status": 2}), READ],
                   capabilities=CAPS_ALL)
    # Charging stops: the charge power goes in both sections; the schedule section is not touched.
    yield sequence(name, [*known, status_push({"chargeState": 15}, None, {"chargingState": 15}, HYBRID),
                          status_push({"chargeState": 1, "chargePower": 7.0}, None, {"chargingState": 1})])
    # Pushes of every kind around a status push.
    yield sequence(name, [push("realtime", MQTT_PUSH, HYBRID), status_push({"elecPercent": 0, "leftFrontDoorLock": 0}),
                          push("charging", CHARGING_SAMPLES[0]), status_push({}, None, {"soc": 12}),
                          push("realtime", ASLEEP_HTTP, HYBRID), status_push({"elecPercent": 33, "time": 1700000500})])


@group.recorded
def recorded() -> Iterator[tuple[str, dict[str, Any]]]:
    yield from _construction()
    yield from _features()
    yield from _projections()
    yield from _reads()
    yield from _status_pushes()
    rng = random.Random("car-recorded")
    for _ in range(25):
        yield random_sequence(rng, max_steps=10)


# ---------------------------------------------------------------------------
# Fuzz
# ---------------------------------------------------------------------------


def _random_reply(rng: random.Random, result: Any) -> dict[str, Any]:
    reply = err(random_error(rng)) if rng.random() < 0.25 else ok(result)
    if rng.random() < 0.2:
        reply["pushes"] = [_random_push(rng) for _ in range(rng.randint(1, 3))]
    return reply


def _random_status_push(rng: random.Random) -> dict[str, Any]:
    context = rng.choice(ENERGY_CONTEXTS + [None])
    if rng.random() < 0.6:
        pushed = status_push_of(random_status_push(rng), context)
        if pushed is not None:
            return pushed
    partials = [random_partial(rng, section) if rng.random() < 0.5 else None for section in ("hvac", "charging")]
    return {"kind": "status_push", "raw": random_partial(rng, "realtime"), "hvac": partials[0], "charging": partials[1],
            "context": context}


def _random_push(rng: random.Random) -> dict[str, Any]:
    if rng.random() < 0.3:
        return _random_status_push(rng)
    roll = rng.random()
    if roll < 0.7:
        return push_of("realtime", random_realtime(rng), rng.choice(ENERGY_CONTEXTS + [None]))
    if roll < 0.9:
        return push_of("charging", random_payload(rng, charging_cases))
    return push_of("energy", random_energy(rng))


def _homepage(rng: random.Random) -> dict[str, Any]:
    payload = copy.deepcopy(rng.choice(CHARGING_SAMPLES))
    if rng.random() < 0.6:
        payload.update(copy.deepcopy(rng.choice(SCHEDULE_SAMPLES)))
    return payload


def random_op(rng: random.Random) -> dict[str, Any]:
    roll = rng.random()
    if roll < 0.12:
        return update("realtime", _random_reply(rng, random_realtime(rng)) | {"context": rng.choice(ENERGY_CONTEXTS)})
    if roll < 0.18:
        return update("hvac", _random_reply(rng, rng.choice([random_hvac(rng), RUNNING, SEATS_ON, HAN_HVAC])))
    if roll < 0.22:
        return update("gps", _random_reply(rng, random_gps(rng)))
    if roll < 0.26:
        return update("charging", _random_reply(rng, _homepage(rng)))
    if roll < 0.28:
        return update("energy", _random_reply(rng, random_energy(rng)))
    if roll < 0.40:
        return push_call(_random_push(rng))
    if roll < 0.75:
        target, method, args = rng.choice(FEATURE_COMMANDS)
        args = copy.deepcopy(args)
        if target == "seat":
            args = {"position": rng.choice(["driver", "copilot", "rear_left", "rear_right"]),
                    "level": rng.choice([0, 1, 2])}
        elif target == "hvac" and method == "start":
            args = {key: value for key, value in (
                ("temperature", rng.choice([None, 18.0, 21.0, 22.5, 31.0, 33.0])),
                ("duration", rng.choice([None, 10, 20, 30, 7])), ("cycle_mode", rng.choice([None, 1, 2])),
                ("fan_level", rng.choice([None, 1, 4, 7]))) if value is not None or rng.random() < 0.3}
        elif target in ("steering", "battery"):
            args = {"on": rng.random() < 0.5}
        replies = [_random_reply(rng, REMOTE_OK)]
        if target == "hvac" and method == "start" and rng.random() < 0.5:
            replies.insert(0, _random_reply(rng, rng.choice([random_hvac(rng), HAN_HVAC, RUNNING])))
        if rng.random() < 0.05:
            replies = []
        return call(target, method, args, *replies)
    if roll < 0.80:
        method = rng.choice(["start_charging", "stop_charging", "get_ac_current_limit"])
        result = rng.choice(AC_LIMIT_CORPUS) if method == "get_ac_current_limit" else CHARGE_OK
        return call("car", method, {}, _random_reply(rng, result))
    if roll < 0.82:
        return call("car", "save_charging_schedule",
                    {"start_charge_time": "22:00", "end_charge_time": "full", "charge_way": rng.choice(["1", "2"])},
                    _random_reply(rng, CHARGE_OK), _random_reply(rng, _homepage(rng)))
    if roll < 0.94:
        return advance(rng.choice([0.0, 1.0, 5.0, 29.999, 30.0, 30.5, 31.0, 100.0, rng.uniform(0, 40)]))
    return READ


def random_sequence(rng: random.Random, max_steps: int = 25) -> tuple[str, dict[str, Any]]:
    ops = [random_op(rng) for _ in range(rng.randint(3, max_steps))]
    roll = rng.random()
    kwargs: dict[str, Any] = {}
    if roll < 0.4:
        kwargs["capabilities"] = CAPS_ALL
    elif roll < 0.55:
        kwargs["capabilities"] = rng.choice([CAPS_FIXTURE, CAPS_NONE])
    elif roll < 0.75:
        kwargs["latest_config"] = rng.choice(LATEST_SAMPLES)
    vehicle = rng.choice([VEHICLE, VEHICLE, LEARN_OPEN, LEARN_CLOSED, *VEHICLE_CORPUS])
    case = sequence(rng.choice(CALLS)[len("car."):], ops, vehicle=vehicle, ttl=rng.choice([30.0, 30.0, 5.0, 0.0, 60.0]),
                    start=rng.choice([1000.0, 0.0, 12345.678]), **kwargs)
    return case if json_safe(case[1]) else sequence("BydCar", [READ])


@group.fuzz
def fuzz(rng: random.Random) -> Iterator[tuple[str, dict[str, Any]]]:
    while True:
        yield random_sequence(rng)
