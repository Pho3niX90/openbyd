"""pybyd/_state_engine.py and pybyd/_validators.py

The filters are plain functions of ``(previous, incoming)``: each call takes
the raw payloads of both sides (``null`` for no previous value) and answers
the filtered model's ``model_dump(mode="json")``. Realtime payloads take a
``context`` and ``previous_context`` in the ``models_realtime`` spelling
(``{"energy_type": {"EnergyType": 2}}``).

``state.VehicleStateEngine`` runs a sequence of operations on a fresh engine::

    {"vin": "TESTVIN0000000001", "vehicle": {...raw Vehicle...},
     "projection_ttl": 30.0, "start": 1000.0, "ops": [...]}

``start`` is the monotonic clock at the start (``time.monotonic`` is patched
to the running clock). The operations:

- ``{"op": "update_realtime", "raw": ..., "context": ..., "source": "http"|"mqtt"}``
  validates the payload as ``VehicleRealtimeData`` and calls
  ``update_realtime``. HTTP polls and MQTT ``vehicleInfo`` pushes take the same
  path in pyBYD; ``source`` only labels the case.
- ``update_hvac``, ``update_gps``, ``update_charging``, ``update_energy``,
  ``update_charging_schedule`` with ``"raw"`` (and ``"source"``) likewise.
- ``{"op": "patch_realtime", "raw": ..., "context": ...}`` validates the payload as
  ``VehicleRealtimeData`` and calls ``patch_realtime`` with it as the partial.
- ``{"op": "patch_status_push", "realtime": ..., "hvac": ..., "charging": ..., "context": ...}``
  validates each payload that is not ``null`` (realtime, then HVAC, then
  charging) and calls ``patch_status_push``. ``status_push_op`` builds one from
  a ``vehicleStatusPush`` payload the way the client does.
- ``{"op": "register_projections", "specs": [{"section", "field_name", "expected_value"}, ...]}``
- ``{"op": "rollback_projections", "command_id": "cmd-1"}``
- ``{"op": "advance", "seconds": 5.0}`` moves the monotonic clock forward.
- ``{"op": "read"}`` changes nothing; the step still reports the state.

The answer is ``{"initial": <snapshot>, "steps": [...]}`` where a snapshot is
every section's dump and each step is::

    {"changes": {...}, "active": [...], "held": 2, "notified": 1,
     "result": "cmd-1",         # register_projections only
     "error": "ValidationError"} # the payload did not validate; the engine was not called

``changes`` holds, per section that changed, ``null`` (gone), its whole dump
(it appeared) or the fields whose JSON text changed. Diffing keeps the
recorded vectors small while comparing every snapshot exactly. ``active`` is
``active_projections`` as ``asdict``; ``held`` the number of projections the
engine holds (expired ones linger until the next rebuild); ``notified`` the
number of ``on_state_changed`` calls the step made.

``state._build_section_patch`` takes ``section`` (``realtime``, ``hvac`` or
``charging``), the raw ``base`` and ``partial`` (realtime with
``base_context`` and ``context``) and answers the patched model's dump, or
``null`` when the partial carries nothing. pyBYD reads which fields a partial
carries from pydantic's ``model_fields_set``; openbyd reads it off the values
(see ``crates/openbyd-protocol/src/state/patch.rs``), so these cases lean on
fields given at their default value, sentinels, key aliases, field names used
as keys and unknown keys.
"""

from __future__ import annotations

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

from pybyd._mqtt import VehicleStatusPushDto, flatten_vehicle_status_push
from pybyd._state_engine import (
    FieldProjection,
    ProjectionSpec,
    VehicleSnapshot,
    VehicleStateEngine,
    _build_section_patch,
)
from pybyd._validators import (
    _REALTIME_FIELD_FILTERS,
    _apply_model_field_filters,
    _guard_consumption_legs,
    _guard_tire_press_unit,
    _has_valid_coordinates,
    _MISSING,
    apply_gps_filters,
    apply_hvac_filters,
    apply_realtime_filters,
    guard_gps_coordinates,
)
from pybyd.exceptions import BydError
from pybyd.models.charging import ChargingStatus
from pybyd.models.energy import EnergyConsumption
from pybyd.models.gps import GpsInfo
from pybyd.models.hvac import HvacOverallStatus, HvacStatus, HvacWindPosition
from pybyd.models.realtime import (
    LockState,
    SeatHeatVentState,
    StearingWheelHeat,
    TirePressureUnit,
    VehicleRealtimeData,
)
from pybyd.models.smart_charging import SmartChargingSchedule
from pybyd.models.vehicle import Vehicle

from parity.framework import ROOT, Group, random_text
from parity.groups.models_energy import group as energy_group
from parity.models_support import answer_is_representable
from parity.groups.models_hvac import cases as hvac_cases
from parity.groups.models_realtime import CONTEXTS, ENERGY_CONTEXTS, energy, python_context
from parity.groups.models_realtime import cases as realtime_cases
from parity.groups.models_vehicle import VEHICLE_SAMPLES
from parity.groups.mqtt import AIR, CHARGE, SECTION_FIELDS, STATUS_PUSHES, random_status_push
from parity.models_support import (
    SENTINELS,
    UNKNOWN_KEYS,
    ModelCases,
    json_safe,
    jsonable,
    property_names,
    random_json,
    random_value,
)

group = Group("state")

# Exceptions model_validate raises that pydantic does not wrap; they escape as themselves.
_ERRORS = (BydError, ValueError, AttributeError, OverflowError, OSError)

SECTIONS = ("realtime", "hvac", "gps", "charging", "energy", "charging_schedule")
_MODELS = {
    "realtime": VehicleRealtimeData,
    "hvac": HvacStatus,
    "gps": GpsInfo,
    "charging": ChargingStatus,
    "energy": EnergyConsumption,
    "charging_schedule": SmartChargingSchedule,
}

HYBRID = energy(2)
EV = energy(0)
ICE = energy(1)

# ---------------------------------------------------------------------------
# Oracles
# ---------------------------------------------------------------------------


def _realtime(raw: Any, context: Any = None) -> VehicleRealtimeData:
    return VehicleRealtimeData.model_validate(raw, context=python_context(context))


def _optional(cls: type, raw: Any, context: Any = None) -> Any:
    if raw is None:
        return None
    if cls is VehicleRealtimeData:
        return _realtime(raw, context)
    return cls.model_validate(raw)


def _dump(model: Any) -> Any:
    return None if model is None else jsonable(model.model_dump(mode="json"))


@group.oracle("state.apply_realtime_filters", errors=_ERRORS)
def realtime_filters(previous: Any, incoming: Any, previous_context: Any = None, context: Any = None) -> Any:
    return _dump(apply_realtime_filters(_optional(VehicleRealtimeData, previous, previous_context),
                                        _realtime(incoming, context)))


@group.oracle("state._apply_model_field_filters", errors=_ERRORS)
def model_field_filters(previous: Any, incoming: Any, previous_context: Any = None, context: Any = None) -> Any:
    updates = _apply_model_field_filters(_realtime(previous, previous_context), _realtime(incoming, context),
                                         _REALTIME_FIELD_FILTERS)
    return jsonable(updates)


@group.oracle("state._guard_tire_press_unit", errors=_ERRORS)
def tire_press_unit(previous: Any, incoming: Any, previous_context: Any = None, context: Any = None) -> Any:
    unit = _guard_tire_press_unit(_realtime(previous, previous_context), _realtime(incoming, context))
    return {"missing": True} if unit is _MISSING else {"unit": jsonable(unit)}


@group.oracle("state._guard_consumption_legs", errors=_ERRORS)
def consumption_legs(previous: Any, incoming: Any, previous_context: Any = None, context: Any = None) -> Any:
    return jsonable(_guard_consumption_legs(_realtime(previous, previous_context), _realtime(incoming, context)))


@group.oracle("state.apply_hvac_filters", errors=_ERRORS)
def hvac_filters(previous: Any, incoming: Any) -> Any:
    return _dump(apply_hvac_filters(_optional(HvacStatus, previous), HvacStatus.model_validate(incoming)))


def _gps_answer(function: Any, previous: Any, incoming: Any) -> dict[str, Any]:
    old = _optional(GpsInfo, previous)
    new = _optional(GpsInfo, incoming)
    kept = function(old, new)
    source = None if kept is None else "incoming" if kept is new else "previous"
    return {"result": _dump(kept), "from": source}


@group.oracle("state.guard_gps_coordinates", errors=_ERRORS)
def gps_guard(previous: Any, incoming: Any) -> Any:
    return _gps_answer(guard_gps_coordinates, previous, incoming)


@group.oracle("state.apply_gps_filters", errors=_ERRORS)
def gps_filters(previous: Any, incoming: Any) -> Any:
    return _gps_answer(apply_gps_filters, previous, incoming)


@group.oracle("state._has_valid_coordinates", errors=_ERRORS)
def has_valid_coordinates(gps: Any) -> bool:
    return _has_valid_coordinates(GpsInfo.model_validate(gps))


_PATCHED = {"realtime": VehicleRealtimeData, "hvac": HvacStatus, "charging": ChargingStatus}


@group.oracle("state._build_section_patch", errors=_ERRORS)
def section_patch(section: str, base: Any, partial: Any, base_context: Any = None, context: Any = None) -> Any:
    cls = _PATCHED[section]
    if cls is VehicleRealtimeData:
        return _dump(_build_section_patch(_realtime(base, base_context), _realtime(partial, context)))
    return _dump(_build_section_patch(cls.model_validate(base), cls.model_validate(partial)))


@group.oracle("state.ProjectionSpec")
def projection_spec(section: str, field_name: str, expected_value: Any) -> Any:
    return jsonable(dataclasses.asdict(ProjectionSpec(section, field_name, expected_value)))


@group.oracle("state.FieldProjection")
def field_projection(section: str, field_name: str, expected_value: Any, command_id: str, created_at: float,
                     ttl: float, now: float) -> Any:
    projection = FieldProjection(section, field_name, expected_value, command_id, float(created_at), float(ttl))
    with mock.patch.object(time, "monotonic", lambda: float(now)):
        expired = projection.is_expired
    return {"projection": jsonable(dataclasses.asdict(projection)), "is_expired": expired}


def _snapshot_dump(snapshot: VehicleSnapshot) -> dict[str, Any]:
    return {"vehicle": _dump(snapshot.vehicle), **{name: _dump(getattr(snapshot, name)) for name in SECTIONS}}


@group.oracle("state.VehicleSnapshot", errors=_ERRORS)
def vehicle_snapshot(vehicle: Any, context: Any = None, **sections: Any) -> Any:
    models = {name: _optional(_MODELS[name], raw, context) for name, raw in sections.items()}
    return _snapshot_dump(VehicleSnapshot(vehicle=Vehicle.model_validate(vehicle), **models))


def _run(coroutine: Any) -> None:
    """Drive an engine coroutine without an event loop: the uncontended lock never suspends."""
    try:
        coroutine.send(None)
    except StopIteration:
        return
    coroutine.close()
    raise RuntimeError("the engine waited on something")


def _text(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False)


def changes(before: dict[str, Any], after: dict[str, Any]) -> dict[str, Any]:
    """What changed between two snapshot dumps; see the module docstring."""
    changed: dict[str, Any] = {}
    for name in SECTIONS:
        old, new = before[name], after[name]
        if _text(old) == _text(new):
            continue
        if isinstance(old, dict) and isinstance(new, dict):
            changed[name] = {key: value for key, value in new.items() if key not in old or _text(old[key]) != _text(value)}
        else:
            changed[name] = new
    return changed


_UPDATES = {
    "update_realtime": ("update_realtime", VehicleRealtimeData),
    "update_hvac": ("update_hvac", HvacStatus),
    "update_gps": ("update_gps", GpsInfo),
    "update_charging": ("update_charging", ChargingStatus),
    "update_energy": ("update_energy", EnergyConsumption),
    "update_charging_schedule": ("update_charging_schedule", SmartChargingSchedule),
}


@group.oracle("state.VehicleStateEngine", errors=_ERRORS)
def engine_sequence(vin: str, vehicle: Any, ops: list[dict[str, Any]], start: float,
                    projection_ttl: float | None = None) -> Any:
    clock = [float(start)]
    notified: list[str] = []
    ttl = 30.0 if projection_ttl is None else float(projection_ttl)
    cache: dict[str, tuple[Any, Any]] = {}

    def snapshot_dump() -> dict[str, Any]:
        # A section that is the same object as last time has the same dump.
        snapshot = engine.snapshot
        result = {"vehicle": _dump(snapshot.vehicle)}
        for name in SECTIONS:
            section = getattr(snapshot, name)
            if name not in cache or cache[name][0] is not section:
                cache[name] = (section, _dump(section))
            result[name] = cache[name][1]
        return result

    # A projected value of the wrong type makes pydantic warn while dumping; the dump is what is compared.
    with mock.patch.object(time, "monotonic", lambda: clock[0]), warnings.catch_warnings():
        warnings.simplefilter("ignore")
        engine = VehicleStateEngine(vin, Vehicle.model_validate(vehicle),
                                    on_state_changed=lambda v, s: notified.append(v), projection_ttl=ttl)
        initial = before = snapshot_dump()
        steps = []
        for op in ops:
            notified.clear()
            step: dict[str, Any] = {}
            kind = op["op"]
            if kind in _UPDATES:
                method, cls = _UPDATES[kind]
                try:
                    data = _optional(cls, op["raw"], op.get("context"))
                    if data is None:
                        cls.model_validate(None)
                except _ERRORS as exc:
                    step["error"] = type(exc).__name__
                else:
                    _run(getattr(engine, method)(data))
            elif kind == "patch_realtime":
                try:
                    partial = _realtime(op["raw"], op.get("context"))
                except _ERRORS as exc:
                    step["error"] = type(exc).__name__
                else:
                    _run(engine.patch_realtime(partial))
            elif kind == "patch_status_push":
                try:
                    partials = {
                        "realtime": _optional(VehicleRealtimeData, op.get("realtime"), op.get("context")),
                        "hvac": _optional(HvacStatus, op.get("hvac")),
                        "charging": _optional(ChargingStatus, op.get("charging")),
                    }
                except _ERRORS as exc:
                    step["error"] = type(exc).__name__
                else:
                    _run(engine.patch_status_push(**partials))
            elif kind == "register_projections":
                specs = [ProjectionSpec(s["section"], s["field_name"], s["expected_value"]) for s in op["specs"]]
                step["result"] = engine.register_projections(specs)
            elif kind == "rollback_projections":
                engine.rollback_projections(op["command_id"])
            elif kind == "advance":
                clock[0] += op["seconds"]
            elif kind != "read":
                raise KeyError(f"unknown op {kind!r}")
            after = snapshot_dump()
            step["changes"] = changes(before, after)
            step["active"] = [jsonable(dataclasses.asdict(p)) for p in engine.active_projections]
            step["held"] = len(engine._projections)
            step["notified"] = len(notified)
            steps.append(step)
            before = after
    return {"initial": initial, "steps": steps}


# ---------------------------------------------------------------------------
# Payloads
# ---------------------------------------------------------------------------


def _corpus(name: str) -> list[dict[str, Any]]:
    path = ROOT / "parity" / "corpus" / f"{name}.json"
    return [entry["payload"] for entry in json.loads(Path(path).read_text(encoding="utf-8"))]


REALTIME_CORPUS = _corpus("VehicleRealtimeData")
GPS_CORPUS = _corpus("GpsInfo")
HVAC_CORPUS = _corpus("HvacStatus")
CHARGING_CORPUS = _corpus("ChargingStatus")
ENERGY_CORPUS = _corpus("EnergyConsumption")

VEHICLE = VEHICLE_SAMPLES[0]
VIN = "TESTVIN0000000001"

# Real captures from the upstream tests (test_validators.py).
SHARK_BR = {
    "totalConsumption": "(6.7度+7.2升)/百公里",
    "totalConsumptionEn": "(6.7kW·h+7.2L)/100km",
    "totalEnergy": "6.7kW·h/100km+7.2L/100km",
}
SEAL_UK = {
    "energyConsumption": "19.4",
    "nearestEnergyConsumption": "31.2",
    "nearestEnergyConsumptionUnit": "kW·h/100miles",
    "recent50kmEnergy": "31.2kW·h/100miles",
    "totalConsumption": "19.9度/百公里",
    "totalConsumptionEn": "19.9kW·h/100km",
    "totalEnergy": "32.0kW·h/100miles",
}
ASLEEP_HTTP = {
    "powerSystem": 0, "totalEnergy": "--", "elecPercent": 0, "enduranceMileageV2Unit": "--",
    "nearestEnergyConsumptionUnit": "--", "nearestEnergyConsumption": "--", "recent50kmEnergy": "--",
    "totalMileageV2Unit": "--", "onlineState": 0,
}
HVAC_PLACEHOLDER = {
    "tempInCar": 0, "tempOutCar": 0, "pm": 0, "acSwitch": 0, "status": 0, "mainSettingTemp": 0.0,
    "refrigeratorState": -1,
}
PSI_TYRES = {"tirePressUnit": int(TirePressureUnit.PSI), "leftFrontTirepressure": 38.4,
             "rightFrontTirepressure": 37.2, "leftRearTirepressure": 37.9, "rightRearTirepressure": 37.7}
KPA_ZERO_TYRES = {"tirePressUnit": int(TirePressureUnit.KPA), "leftFrontTirepressure": 0,
                  "rightFrontTirepressure": 0, "leftRearTirepressure": 0, "rightRearTirepressure": 0}
KPA_TYRES = {"tirePressUnit": int(TirePressureUnit.KPA), "leftFrontTirepressure": 250.0,
             "rightFrontTirepressure": 245.0, "leftRearTirepressure": 252.0, "rightRearTirepressure": 248.0}
MQTT_PUSH = {
    "onlineState": 1, "elecPercent": 81, "enduranceMileage": 420, "totalMileage": 12345.6, "leftFrontDoorLock": 2,
    "rightFrontDoorLock": 2, "leftRearDoorLock": 2, "rightRearDoorLock": 2, "slidingDoorLock": 2,
    **PSI_TYRES, **SHARK_BR, "recent50kmEnergy": "6.1kW·h/100km+8.4L/100km", "time": 1700000000,
}

NUMERIC_ZERO_DROP = [
    ("elec_percent", 73.0, 72.0), ("left_front_tire_pressure", 2.4, 2.3), ("right_front_tire_pressure", 2.5, 2.4),
    ("left_rear_tire_pressure", 2.6, 2.5), ("right_rear_tire_pressure", 2.7, 2.6),
    ("endurance_mileage", 320.0, 315.0), ("ev_endurance", 310.0, 300.0), ("endurance_mileage_v2", 305.0, 299.0),
    ("total_mileage", 18234.0, 18240.0), ("total_mileage_v2", 18234.0, 18240.0),
    ("oil_endurance", 420.0, 410.0), ("oil_percent", 74.0, 72.0),
]
LOCKS = ["left_front_door_lock", "right_front_door_lock", "left_rear_door_lock", "right_rear_door_lock",
         "sliding_door_lock"]
PRESERVED = [
    ("recent_50km_energy", "14.7kW·h/100km", "14.9kW·h/100km"),
    ("total_energy", "11.1kW·h/100km", "11.2kW·h/100km"),
    ("total_consumption", "11.1kW·h/100km", "11.3kW·h/100km"),
    ("total_consumption_en", "11.1kW·h/100km", "11.3kW·h/100km"),
]
LEG_KEYS = [("recent_50km_energy", "recent50kmEnergy"), ("total_energy", "totalEnergy"),
            ("total_consumption", "totalConsumption"), ("total_consumption_en", "totalConsumptionEn")]

GPS_VALID = {"latitude": 52.0, "longitude": 4.0, "speed": 60.0}
GPS_VALID_B = {"latitude": 52.5, "longitude": 4.5, "speed": 30.0}
GPS_EDGES = [
    {}, {"latitude": None, "longitude": None}, {"latitude": 0.001, "longitude": 0.002}, {"latitude": 52.0},
    {"longitude": 4.0}, {"latitude": 0.1, "longitude": 0.1}, {"latitude": -33.5, "longitude": 151.0},
    {"latitude": 0.05, "longitude": 32.5}, {"latitude": 0.0, "longitude": 0.0}, {"latitude": -0.0, "longitude": 0},
    {"latitude": 0.0999999, "longitude": -0.0999999}, {"latitude": "NAN", "longitude": "nan"},
    {"latitude": "inf", "longitude": 0.0}, {"latitude": "--", "longitude": 4.0}, {"latitude": "52.0", "longitude": "4"},
    {"data": {"latitude": 52.0, "longitude": 4.0}}, {"data": {"latitude": 0, "longitude": 0}},
    {"latitude": 0.0, "longitude": 4.0}, {"latitude": 1e-300, "longitude": -1e-300},
]


def _engine(ops: list[dict[str, Any]], *, ttl: float = 30.0, start: float = 1000.0,
            vehicle: Any = None) -> tuple[str, dict[str, Any]]:
    return "state.VehicleStateEngine", {"vin": VIN, "vehicle": VEHICLE if vehicle is None else vehicle,
                                        "projection_ttl": ttl, "start": start, "ops": ops}


def realtime_op(raw: Any, source: str = "http", context: Any = None) -> dict[str, Any]:
    return {"op": "update_realtime", "raw": raw, "context": context, "source": source}


def update(section: str, raw: Any, source: str = "http") -> dict[str, Any]:
    return {"op": f"update_{section}", "raw": raw, "source": source}


def patch_realtime(raw: Any, context: Any = None) -> dict[str, Any]:
    return {"op": "patch_realtime", "raw": raw, "context": context}


def patch_status_push(realtime: Any = None, hvac: Any = None, charging: Any = None,
                      context: Any = None) -> dict[str, Any]:
    return {"op": "patch_status_push", "realtime": realtime, "hvac": hvac, "charging": charging, "context": context}


def status_push_op(payload: dict[str, Any], context: Any = None) -> dict[str, Any]:
    """The ``patch_status_push`` the client makes of a push: realtime from every
    realtime section, HVAC and charging each from their own, nothing at all
    when no known section is there."""
    flat = flatten_vehicle_status_push(payload)
    if not flat:
        return patch_status_push(context=context)
    air = flatten_vehicle_status_push(payload, (VehicleStatusPushDto.AIR_STATUS,))
    charge = flatten_vehicle_status_push(payload, (VehicleStatusPushDto.CHARGE_STATUS,))
    return patch_status_push(flat, air or None, charge or None, context)


def register(*specs: tuple[str, str, Any]) -> dict[str, Any]:
    return {"op": "register_projections",
            "specs": [{"section": s, "field_name": f, "expected_value": v} for s, f, v in specs]}


def rollback(command_id: str) -> dict[str, Any]:
    return {"op": "rollback_projections", "command_id": command_id}


def advance(seconds: float) -> dict[str, Any]:
    return {"op": "advance", "seconds": seconds}


READ = {"op": "read"}

# ---------------------------------------------------------------------------
# Recorded cases
# ---------------------------------------------------------------------------


def _pair(call: str, previous: Any, incoming: Any, previous_context: Any = None, context: Any = None):
    return call, {"previous": previous, "incoming": incoming, "previous_context": previous_context,
                  "context": context}


def _upstream_validators() -> Iterator[tuple[str, dict[str, Any]]]:
    """The assertions of tests/test_validators.py, as cases."""
    rt = "state.apply_realtime_filters"
    # _has_valid_coordinates
    for gps in [GPS_VALID, {}, {"latitude": 0.001, "longitude": 0.002}, {"latitude": 48.8566},
                {"longitude": 2.3522}, {"latitude": 0.1, "longitude": 0.1}, {"latitude": -33.8688, "longitude": 151.2093},
                {"latitude": 0.05, "longitude": 32.5}, {"latitude": 0.0, "longitude": 0.0}]:
        yield "state._has_valid_coordinates", {"gps": gps}
    # guard_gps_coordinates, first poll and later polls; apply_gps_filters delegates.
    invalid = [{}, {"latitude": 0.001, "longitude": 0.002}, {"latitude": 48.8566}, {"longitude": 2.3522}]
    for call in ("state.guard_gps_coordinates", "state.apply_gps_filters"):
        for previous in (None, GPS_VALID):
            yield call, {"previous": previous, "incoming": GPS_VALID_B}
            yield call, {"previous": previous, "incoming": None}
            for incoming in invalid:
                yield call, {"previous": previous, "incoming": incoming}
    # Zero-drop of numeric fields: kept, first payload, replaced.
    for name, old, new in NUMERIC_ZERO_DROP:
        yield _pair(rt, {name: old}, {name: 0})
        yield _pair(rt, None, {name: 0})
        yield _pair(rt, {name: old}, {name: new})
    # Door locks: missing is not kept, UNAVAILABLE keeps, first payload drops, a real state replaces.
    for name in LOCKS:
        yield _pair(rt, {name: int(LockState.LOCKED)}, {})
        yield _pair(rt, {name: int(LockState.LOCKED)}, {name: int(LockState.UNAVAILABLE)})
        yield _pair(rt, None, {name: int(LockState.UNAVAILABLE)})
        yield _pair(rt, {name: int(LockState.LOCKED)}, {name: int(LockState.UNLOCKED)})
    # Preserve-when-none.
    for name, old, new in PRESERVED:
        yield _pair(rt, {name: old}, {name: "--"})
        yield _pair(rt, None, {name: "--"})
        yield _pair(rt, {name: old}, {name: new})
    # Consumption legs.
    for _name, key in LEG_KEYS:
        both = "6.7kW·h/100km+7.2L/100km"
        yield _pair(rt, {key: both}, {key: "--"}, HYBRID, HYBRID)
        yield _pair(rt, {key: both}, {"elecPercent": 80}, HYBRID, HYBRID)
        yield _pair(rt, {key: both}, {key: "6.8kW·h/100km+7.3L/100km"}, HYBRID, HYBRID)
        yield _pair(rt, None, {key: "--"}, None, HYBRID)
    yield _pair(rt, SHARK_BR, ASLEEP_HTTP, HYBRID, HYBRID)
    yield _pair(rt, SEAL_UK, ASLEEP_HTTP, EV, EV)
    yield _pair(rt, SHARK_BR, {"totalEnergy": "6.8kW·h/100km"}, HYBRID, HYBRID)
    yield _pair(rt, SHARK_BR, {"totalEnergy": "7.5L/100km"}, HYBRID, HYBRID)
    yield _pair(rt, {"totalConsumptionEn": "3.5L/100km"}, {"totalConsumptionEn": "3.6L/100km"}, ICE, ICE)
    for previous, incoming, context in [
        (SHARK_BR, {"totalEnergy": "N/A"}, HYBRID), (SHARK_BR, {"totalEnergy": "(--+--)/100km"}, HYBRID),
        ({"totalEnergy": "6.7kW·h/100km+7.2L/100km"}, {"totalEnergy": "--+--"}, HYBRID),
        ({"totalEnergy": "7.2L/100km"}, {"totalEnergy": "N/A"}, ICE),
    ]:
        yield _pair(rt, previous, incoming, context, context)
    # Tyre pressure unit.
    yield _pair(rt, PSI_TYRES, KPA_ZERO_TYRES)
    yield _pair(rt, PSI_TYRES, KPA_TYRES)
    yield _pair(rt, None, KPA_ZERO_TYRES)
    yield _pair(rt, {"tirePressUnit": 2, "leftFrontTirepressure": 38.4},
                {"tirePressUnit": 3, "leftFrontTirepressure": 260.0, "rightFrontTirepressure": 0,
                 "leftRearTirepressure": 0, "rightRearTirepressure": 0})
    # HVAC placeholder.
    hv = "state.apply_hvac_filters"
    sentinel_placeholder = {**HVAC_PLACEHOLDER, "tempInCar": -129}
    yield hv, {"previous": None, "incoming": HVAC_PLACEHOLDER}
    yield hv, {"previous": None, "incoming": sentinel_placeholder}
    yield hv, {"previous": {"tempInCar": 22.0, "tempOutCar": 14.0, "pm": 8.0}, "incoming": HVAC_PLACEHOLDER}
    yield hv, {"previous": {"tempInCar": -129, "tempOutCar": 14.0, "pm": 8.0}, "incoming": HVAC_PLACEHOLDER}
    yield hv, {"previous": {}, "incoming": HVAC_PLACEHOLDER}
    yield hv, {"previous": {"tempInCar": 22.0, "tempOutCar": 14.0, "pm": 8.0},
               "incoming": {"tempInCar": 5.0, "tempOutCar": 0, "pm": 3.0}}
    yield hv, {"previous": None, "incoming": {"tempInCar": 0.5, "tempOutCar": 0, "pm": 2.0}}
    yield hv, {"previous": {"tempInCar": 5.0, "tempOutCar": 0, "pm": 3.0}, "incoming": HVAC_PLACEHOLDER}
    yield hv, {"previous": {"tempInCar": 22.0, "tempOutCar": 14.0, "pm": 8.0, "mainSettingTemp": 24.0},
               "incoming": HVAC_PLACEHOLDER}


def _rule_cases() -> Iterator[tuple[str, dict[str, Any]]]:
    """Each rule where it applies, where it does not, and on the first payload."""
    rt = "state.apply_realtime_filters"
    # Zero-drop: -0.0 and "0" are zero too; a NaN is not; a sentinel -1 clears to None before the filter.
    for value in (-0.0, "0", " 0.0 ", "NAN", -1, 0.001, False):
        yield _pair(rt, {"elecPercent": 55}, {"elecPercent": value})
    # The private helpers, with a baseline and with real previous values.
    empty: dict[str, Any] = {}
    for call in ("state._apply_model_field_filters", "state._guard_tire_press_unit", "state._guard_consumption_legs"):
        yield _pair(call, empty, ASLEEP_HTTP, None, HYBRID)
        yield _pair(call, MQTT_PUSH, ASLEEP_HTTP, HYBRID, HYBRID)
        yield _pair(call, MQTT_PUSH, MQTT_PUSH, HYBRID, HYBRID)
        yield _pair(call, MQTT_PUSH, {**KPA_ZERO_TYRES, "leftFrontDoorLock": 0, "recent50kmEnergy": "N/A"}, HYBRID, EV)
        yield _pair(call, SEAL_UK, SHARK_BR, EV, HYBRID)
        yield _pair(call, {"totalEnergy": "6.7kW·h/100km"}, {"totalEnergy": "--+7.2L/100km"}, HYBRID, HYBRID)
    # Tyre unit: one pressure None and the others zero still counts as all zero; a NaN pressure does not.
    yield _pair(rt, PSI_TYRES, {"tirePressUnit": 3, "leftFrontTirepressure": 0, "rightFrontTirepressure": "--"})
    yield _pair(rt, PSI_TYRES, {"tirePressUnit": 3, "leftFrontTirepressure": "NAN"})
    yield _pair(rt, PSI_TYRES, {"tirePressUnit": 3})
    yield _pair(rt, {}, {"tirePressUnit": 1})
    # Validation errors on either side.
    yield _pair(rt, [], {})
    yield _pair(rt, {}, "x")
    yield _pair(rt, {}, {}, None, 5)
    yield "state.apply_hvac_filters", {"previous": [], "incoming": {}}
    yield "state.apply_hvac_filters", {"previous": None, "incoming": None}
    # HVAC: one sensor real; NaN; missing triad; negative zero.
    for incoming in ({"pm": 1}, {"tempInCar": "NAN", "tempOutCar": 0, "pm": 0}, {}, {"status": 1},
                     {"tempInCar": -0.0, "tempOutCar": "0", "pm": "--"}, {"tempOutCar": -129, "pm": 0}):
        yield "state.apply_hvac_filters", {"previous": {"tempInCar": 21.0, "pm": 4}, "incoming": incoming}
        yield "state.apply_hvac_filters", {"previous": None, "incoming": incoming}
    # GPS edges in both positions.
    for edge in GPS_EDGES:
        yield "state.guard_gps_coordinates", {"previous": GPS_VALID, "incoming": edge}
        yield "state.guard_gps_coordinates", {"previous": None, "incoming": edge}
        yield "state.guard_gps_coordinates", {"previous": edge, "incoming": None}
        yield "state._has_valid_coordinates", {"gps": edge}
    yield "state.guard_gps_coordinates", {"previous": None, "incoming": "x"}
    yield "state._has_valid_coordinates", {"gps": None}
    # Data classes.
    yield "state.ProjectionSpec", {"section": "hvac", "field_name": "status", "expected_value": 1}
    yield "state.ProjectionSpec", {"section": "x", "field_name": "", "expected_value": {"a": [1, None]}}
    for created, ttl, now in [(0.0, 30.0, 30.0), (0.0, 30.0, 30.000001), (10.5, 0.0, 10.5), (10.5, 0.0, 10.6),
                              (100.0, 5.0, 50.0), (0.1, 0.2, 0.30000000000000004)]:
        yield "state.FieldProjection", {"section": "realtime", "field_name": "left_front_door_lock",
                                        "expected_value": 2, "command_id": "cmd-1", "created_at": created,
                                        "ttl": ttl, "now": now}
    yield "state.VehicleSnapshot", {"vehicle": VEHICLE}
    yield "state.VehicleSnapshot", {"vehicle": VEHICLE, "realtime": SHARK_BR, "context": HYBRID,
                                    "hvac": HVAC_CORPUS[2], "gps": GPS_VALID, "charging": CHARGING_CORPUS[0],
                                    "energy": {"time": 1700000000}, "charging_schedule": {"soc": 80}}
    yield "state.VehicleSnapshot", {"vehicle": "x"}


LOCK_SPECS = [("realtime", name, int(LockState.LOCKED)) for name in LOCKS[:4]]
UNLOCK_SPECS = [("realtime", name, int(LockState.UNLOCKED)) for name in LOCKS[:4]]
CLIMATE_OFF = [("hvac", "status", int(HvacOverallStatus.OFF)),
               ("hvac", "main_seat_heat_state", int(SeatHeatVentState.OFF)),
               ("hvac", "steering_wheel_heat_state", int(StearingWheelHeat.OFF)),
               ("realtime", "main_seat_heat_state", int(SeatHeatVentState.OFF)),
               ("realtime", "steering_wheel_heat_state", int(StearingWheelHeat.OFF))]


def _every_field_cases() -> Iterator[tuple[str, dict[str, Any]]]:
    """One value projected onto every field of a section at once.

    How a value written without validation is dumped depends on the field's type (a bool
    in an int field reads 0 or 1, in an enum field stays a bool), so this fails for any
    field whose type changed upstream.
    """
    bases = {
        "realtime": realtime_op(MQTT_PUSH, "mqtt"),
        "hvac": update("hvac", {"status": 2, "pm": 5}),
        "gps": update("gps", GPS_VALID),
        "charging": update("charging", {"soc": 80, "chargingState": 1}),
        "energy": update("energy", {}),
        "charging_schedule": update("charging_schedule", {}, "http"),
    }
    for section in SECTIONS:
        for value in (True, False, 2, 2.5, "x"):
            specs = [(section, field, value) for field in _FIELDS[section] if field != "raw"]
            yield _engine([bases[section], register(*specs), READ, bases[section], READ])


def _engine_cases() -> Iterator[tuple[str, dict[str, Any]]]:
    on, off = int(HvacOverallStatus.ON), int(HvacOverallStatus.OFF)
    # tests/test_issue_74.py: a newer projection replaces the older one on the same field.
    yield _engine([update("hvac", {"status": 2, "pm": 5}), register(("hvac", "status", on)),
                   register(("hvac", "status", off)), update("hvac", {"status": 2, "pm": 5})])
    yield _engine([update("hvac", {"vdeTempRangeMin": 17, "vdeTempRangeMax": 33, "pm": 5})])
    # Lock: an HTTP poll contradicting the projection is held off (guard window); the MQTT push confirms it.
    yield _engine([realtime_op(MQTT_PUSH, "mqtt", HYBRID), register(*UNLOCK_SPECS),
                   realtime_op({**MQTT_PUSH, "leftFrontDoorLock": 2}, "http", HYBRID),
                   realtime_op({**MQTT_PUSH, **{k: 1 for k in ("leftFrontDoorLock", "rightFrontDoorLock",
                                                               "leftRearDoorLock", "rightRearDoorLock")}},
                               "mqtt", HYBRID), READ])
    # A bool projected onto a plain int field is dumped as 0 or 1, onto an enum field as it is
    # (air_run_state is an int since pyBYD #94, repair_mode_switch an int or a string since #96).
    for value in (True, False, 1.0, "x", None):
        yield _engine([realtime_op(MQTT_PUSH, "mqtt"), update("hvac", {"status": 2, "pm": 5}),
                       register(("realtime", "air_run_state", value), ("realtime", "repair_mode_switch", value),
                                ("realtime", "trunk_lid_lower", value), ("realtime", "online_state", value),
                                ("hvac", "air_run_state", value), ("hvac", "cycle_choice", value)), READ])
    # The command failed: rollback; an unknown id changes nothing.
    yield _engine([realtime_op(MQTT_PUSH, "mqtt"), register(*LOCK_SPECS), rollback("cmd-9"), rollback("cmd-1"),
                   rollback("cmd-1")])
    # Expiry: still active exactly at the TTL, gone just after; a read does not clean, a rebuild does.
    yield _engine([update("hvac", {"status": 2, "pm": 5}), register(("hvac", "status", on)), advance(30.0), READ,
                   advance(0.25), READ, update("gps", GPS_VALID), advance(100.0), READ])
    yield _engine([register(("hvac", "status", on)), advance(5.0), update("hvac", {"status": 2, "pm": 5})], ttl=5.0)
    yield _engine([update("hvac", {"status": 2}), register(("hvac", "status", on)), READ, advance(0.0),
                   update("hvac", {"status": 2})], ttl=0.0, start=0.0)
    # Interleaved HTTP and MQTT, both orders, with the asleep placeholder.
    yield _engine([realtime_op(MQTT_PUSH, "mqtt", HYBRID), realtime_op(ASLEEP_HTTP, "http", HYBRID),
                   realtime_op(KPA_ZERO_TYRES, "http", HYBRID), realtime_op(MQTT_PUSH, "mqtt", HYBRID)])
    yield _engine([realtime_op(ASLEEP_HTTP, "http", EV), realtime_op(SEAL_UK, "mqtt", EV),
                   realtime_op(ASLEEP_HTTP, "http", EV), realtime_op(ASLEEP_HTTP, "http", EV)])
    # Repeated identical payloads, then a partial one.
    yield _engine([realtime_op(REALTIME_CORPUS[4], "mqtt", HYBRID)] * 3
                  + [realtime_op({"elecPercent": 64}, "mqtt", HYBRID), realtime_op({}, "http")])
    yield _engine([realtime_op(PSI_TYRES, "mqtt"), realtime_op(KPA_ZERO_TYRES, "http"),
                   realtime_op(KPA_TYRES, "mqtt"), realtime_op(KPA_ZERO_TYRES, "http")])
    # HVAC: placeholder after good, good after placeholder.
    yield _engine([update("hvac", HVAC_CORPUS[2]), update("hvac", HVAC_PLACEHOLDER), update("hvac", HVAC_CORPUS[1]),
                   update("hvac", HVAC_CORPUS[2])])
    yield _engine([update("hvac", HVAC_PLACEHOLDER), update("hvac", HVAC_PLACEHOLDER),
                   update("hvac", {"tempInCar": 21.0, "tempOutCar": 0, "pm": 3})])
    # GPS: nothing until a usable fix; bad fixes keep the last good one.
    yield _engine([update("gps", {}), update("gps", {"latitude": 0.0, "longitude": 0.0}), update("gps", GPS_VALID),
                   update("gps", {"latitude": 0.01, "longitude": 0.02}), update("gps", {"latitude": 52.1}),
                   update("gps", GPS_VALID_B), update("gps", {"data": {"latitude": 0, "longitude": 0}})])
    yield _engine([register(("gps", "speed", 0)), update("gps", {"latitude": 0.0, "longitude": 0.0, "speed": 0}),
                   update("gps", {**GPS_VALID, "speed": 1}), update("gps", {"latitude": None, "speed": 0}),
                   update("gps", {**GPS_VALID, "speed": 0})])
    # Climate stop projects onto both hvac and realtime; seat and steering confirm separately.
    yield _engine([update("hvac", HVAC_CORPUS[2]), realtime_op(REALTIME_CORPUS[4], "mqtt"), register(*CLIMATE_OFF),
                   realtime_op({"mainSeatHeatState": 1}, "mqtt"), update("hvac", {"status": 2, "pm": 4}),
                   update("hvac", {"status": 2, "mainSeatHeatState": 1, "stearingWheelHeatState": 1, "pm": 4}),
                   realtime_op({"stearingWheelHeatState": 1}, "mqtt")])
    # Charging, energy and the schedule, with their own projections.
    yield _engine([update("charging", CHARGING_CORPUS[0], "mqtt"), register(("charging", "soc", 80)),
                   update("charging", {"soc": 79, "chargingState": 1}), update("charging", {"soc": "80"}),
                   update("energy", ENERGY_CORPUS[0]), register(("energy", "timestamp", None)),
                   update("energy", {}), update("charging_schedule", {"soc": 80, "smartChargeDto": {"status": "1"}}),
                   register(("charging_schedule", "soc", True), ("charging_schedule", "charge", {"status": 1})),
                   update("charging_schedule", {"soc": 1})])
    # Odd projections: unknown section, unknown field, a property, values the field's type would not take.
    yield _engine([realtime_op(MQTT_PUSH, "mqtt", HYBRID), update("hvac", HVAC_CORPUS[2]),
                   register(("nowhere", "x", 1), ("realtime", "no_such_field", 2), ("realtime", "is_locked", True),
                            ("realtime", "elec_percent", 90), ("realtime", "main_setting_temp", True),
                            ("realtime", "total_energy", 5), ("realtime", "timestamp", "2024"),
                            ("hvac", "temp_in_car", False), ("hvac", "status", "on"), ("realtime", "raw", [1])),
                   realtime_op({**MQTT_PUSH, "elecPercent": 90}, "mqtt", HYBRID),
                   update("hvac", HVAC_CORPUS[2]), advance(31.0), READ])
    # Two specs for one field in one command: the later wins; is_locked is a property.
    yield _engine([realtime_op(MQTT_PUSH, "mqtt"), register(("realtime", "elec_percent", 10),
                                                            ("realtime", "elec_percent", 20)),
                   register(("realtime", "is_locked", False)), realtime_op({"elecPercent": 20}, "http"),
                   realtime_op({"leftFrontDoorLock": 1}, "http")])
    # Payloads that do not validate are reported and leave the state alone.
    yield _engine([update("hvac", []), realtime_op("x"), realtime_op({}, "mqtt", 5), update("gps", None),
                   update("energy", 5), update("charging", {"soc": "x"}), update("charging_schedule", [1]), READ])
    yield _engine([READ], vehicle="x")
    yield _engine([], vehicle={"vin": VIN})


def _patch(section: str, base: Any, partial: Any, base_context: Any = None, context: Any = None):
    return "state._build_section_patch", {"section": section, "base": base, "partial": partial,
                                           "base_context": base_context, "context": context}


CHARGING_BASE = {"vin": VIN, "soc": 70, "chargingState": 1, "connectState": 1, "fullHour": 1, "fullMinute": 25,
                 "chargePower": 5.1, "chargePowerUnit": "kW", "updateTime": 1700000000}
REALTIME_CHARGING = {**MQTT_PUSH, "chargeState": 1, "chargingState": 1, "chargePower": 5.1, "chargePowerUnit": "kW",
                     "tempInCar": 22.5, "leftFrontDoor": 0, "stearingWheelHeatState": 1, "mainSeatHeatState": 3}
HVAC_BASE = {"status": 2, "cycleChoice": 1, "mainSettingTempNew": 21.0, "tempInCar": 21.0, "tempOutCar": 14.0, "pm": 8,
             "mainSeatHeatState": 3, "stearingWheelHeatState": 1, "airRunState": 0}


def _section_patch_cases() -> Iterator[tuple[str, dict[str, Any]]]:
    """``_build_section_patch`` on its own: what a partial carries, field by field."""
    realtime_partials: list[dict[str, Any]] = [
        {},
        # tests/test_mqtt_status_push.py: sentinels in a partial do not erase the base.
        {"leftFrontDoor": 1, "leftFrontDoorLock": 1, "tempInCar": -129, "rightFrontDoorLock": -1},
        # Zero is a reading here (the engine's filters come after the patch).
        {"elecPercent": 0}, {"leftFrontDoorLock": 0}, {"speed": 0.0}, {"lessOneMin": False}, {"epb": 0},
        # Fields given at the value they default to, and sentinels: not carried.
        {"onlineState": -1}, {"connectState": -1, "vehicleState": -1, "chargingState": -1, "sentryStatus": -1},
        {"elecPercent": None}, {"elecPercent": "--"}, {"elecPercent": -1}, {"totalEnergy": "--"}, {"fullHour": -1},
        {"chargeState": -1}, {"leftFrontDoor": -1}, {"leftFrontDoor": 7}, {"powerGear": 0},
        # Enum members that are not UNKNOWN or NO_DATA: carried, also when the value was not a member.
        {"onlineState": 0}, {"onlineState": 5}, {"onlineState": 1}, {"sentryStatus": 4}, {"vehicleState": 2},
        # StearingWheelHeat has ON at -1 and NO_DATA at 0; a seat's NO_DATA is 0 too.
        {"stearingWheelHeatState": -1}, {"stearingWheelHeatState": 0}, {"stearingWheelHeatState": 2},
        {"stearingWheelHeatState": 7}, {"mainSeatHeatState": 0}, {"mainSeatHeatState": -1}, {"mainSeatHeatState": 1},
        # A charging state other than 1 clears the charge power, unless the partial brings one.
        {"chargeState": 15}, {"chargingState": 0}, {"chargeState": 1}, {"chargeState": 1, "chargingState": 15},
        {"chargeState": 15, "chargePower": 3.3}, {"chargeState": 15, "chargePowerUnit": "W"},
        {"chargeState": 15, "chargePower": "--"}, {"chargeState": 15, "chargePower": 0}, {"chargingState": "1"},
        # Key aliases, field names as keys, unknown keys (alone they carry nothing, raw included).
        {"backCover": 1}, {"abs": 1}, {"time": 1700000123}, {"timestamp": 1700000456, "time": 1700000123},
        {"elec_percent": 50}, {"elec_percent": 50, "elecPercent": 60}, {"left_front_door_lock": 1},
        {"foo": 1}, {"foo": 1, "elecPercent": 60}, {"zzz": 1, "elecPercent": 60, "enduranceMileage": "410"},
        {"leftFrontTirepressure": 2.4}, {"leftFrontTirePressure": 2.5, "leftFrontTirepressure": 2.4},
        # The partial's own raw.
        {"raw": {"own": 1}, "elecPercent": 5}, {"raw": {}, "elecPercent": 5}, {"raw": {"own": 1}},
        # Other types: text, both kinds of repair switch, a timestamp, consumption strings.
        {"requestSerial": "S1"}, {"repairModeSwitch": "1"}, {"repairModeSwitch": 0}, {"vehicleTimeZone": "UTC"},
        {"totalEnergy": "6.8kW·h/100km+7.3L/100km"}, {"recent50kmEnergy": "N/A"},
        {"nearestEnergyConsumption": "10.1", "nearestEnergyConsumptionUnit": "L/100km"},
        {"energyConsumption": "6.1+8.4"}, {"tirePressUnit": 2}, {"tirePressUnit": 0},
    ]
    for partial in realtime_partials:
        yield _patch("realtime", REALTIME_CHARGING, partial, HYBRID, HYBRID)
    for partial in realtime_partials[:12]:
        yield _patch("realtime", {}, partial)
    yield _patch("realtime", REALTIME_CHARGING, REALTIME_CORPUS[4], HYBRID, EV)
    yield _patch("realtime", REALTIME_CORPUS[4], ASLEEP_HTTP, EV, EV)
    hvac_partials: list[dict[str, Any]] = [
        {}, {"cycleModel": 2}, {"cycleModel": 2, "cycleChoice": 1}, {"cycleChoice": 0}, {"cycleModel": -1},
        {"status": 0}, {"status": 1}, {"status": 2}, {"acSwitch": 0}, {"acSwitch": 2}, {"airRunState": 1},
        {"airRunState": 0}, {"airRunState": -1}, {"tempInCar": -129}, {"tempInCar": 0}, {"tempOutCar": "--"},
        {"tempInCar": 0, "tempOutCar": 0, "pm": 0}, {"mainSettingTempNew": 23.0}, {"mainSettingTemp": 7},
        {"stearingWheelHeatState": 0}, {"stearingWheelHeatState": -1}, {"steeringWheelHeatState": 1},
        {"mainSeatHeatState": 0}, {"mainSeatHeatState": 2}, {"windPosition": 9}, {"windPosition": 0},
        {"statusNow": {"status": 1, "tempInCar": 19.5}}, {"statusNow": {}, "status": 1}, {"statusNow": "x", "status": 1},
        {"foo": 1}, {"foo": 1, "pm": 3}, {"raw": {"own": 1}, "pm": 3}, {"time": 1700000000, "cycleModel": 2},
        {"air_run_state": 2}, {"refrigeratorState": -1}, {"wiperHeatStatus": 0},
    ]
    for partial in hvac_partials:
        yield _patch("hvac", HVAC_BASE, partial)
    yield _patch("hvac", {}, {"cycleModel": 2})
    yield _patch("hvac", HVAC_CORPUS[2], HVAC_PLACEHOLDER)
    charging_partials: list[dict[str, Any]] = [
        {}, {"chargingState": 15, "connectState": 0, "chargeState": 15}, {"chargingState": 1}, {"chargingState": 0},
        {"chargingState": 0, "chargePower": 2.0}, {"chargingState": 0, "chargePowerUnit": "W"},
        {"chargingState": -1}, {"chargingState": "1"}, {"chargingState": True}, {"connectState": 0},
        {"vin": "TESTVIN0000000002"}, {"vin": "TESTVIN0000000002", "soc": 5}, {"vin": ""}, {"elecPercent": 60},
        {"elecPercent": 60, "soc": 61}, {"soc": 0}, {"soc": -1}, {"time": 1700000999}, {"updateTime": 0},
        {"updateTime": 1700000999, "time": 5}, {"fullHour": -1}, {"fullHour": 0, "fullMinute": 0},
        {"chargePower": "--"}, {"chargePower": 0}, {"foo": 1}, {"raw": {"own": 1}, "soc": 5}, {"wait_status": 1},
    ]
    for partial in charging_partials:
        yield _patch("charging", CHARGING_BASE, partial)
    yield _patch("charging", {}, {"chargingState": 15})
    # Payloads that do not validate, on either side; the base is validated first.
    for section in ("realtime", "hvac", "charging"):
        yield _patch(section, [], {})
        yield _patch(section, {}, "x")
        yield _patch(section, None, None)
    yield _patch("realtime", {}, {}, None, 5)
    yield _patch("realtime", {}, {"onlineState": [1]})
    yield _patch("charging", {}, {"chargingState": "x"})
    yield _patch("hvac", {"status": []}, {"status": {}})


def _status_push_cases() -> Iterator[tuple[str, dict[str, Any]]]:
    """``patch_realtime`` and ``patch_status_push`` on the engine."""
    lock = ("realtime", "left_front_door_lock", int(LockState.UNLOCKED))
    # tests/test_mqtt_status_push.py: a partial keeps the other fields, merges raw and confirms a projection.
    yield _engine([
        realtime_op({"elecPercent": 75, "enduranceMileage": 400, "tempInCar": 22.5, "leftFrontDoor": 0,
                     "leftFrontDoorLock": 2}),
        register(lock),
        patch_realtime({"leftFrontDoor": 1, "leftFrontDoorLock": 1, "tempInCar": -129, "rightFrontDoorLock": -1}),
        READ,
    ])
    # tests/test_mqtt_status_push.py: nothing before a base; then all three sections in one rebuild.
    bases = [realtime_op({"elecPercent": 70, "chargeState": 1, "chargePower": 5.1}),
             update("hvac", {"status": 2, "mainSettingTempNew": 21.0}),
             update("charging", {"soc": 70, "chargingState": 1, "connectState": 1, "chargePower": 5.1})]
    yield _engine([patch_realtime({"elecPercent": 80}), READ, *bases, status_push_op(STATUS_PUSHES[2]), READ])
    full = [realtime_op(REALTIME_CHARGING, "mqtt", HYBRID), update("hvac", HVAC_BASE), update("charging", CHARGING_BASE)]
    # Every upstream push on a full base, and on no base at all; a door-only push leaves HVAC and charging alone.
    for push in STATUS_PUSHES:
        yield _engine([*full, status_push_op(push, HYBRID), READ])
        yield _engine([status_push_op(push), READ])
    # A section without a base is skipped while the others are patched.
    for kept in range(3):
        yield _engine([full[kept], status_push_op(STATUS_PUSHES[2], HYBRID)])
    # The realtime filters run on the patched section, with the section before as the previous value:
    # a zero keeps the reading, a unit without pressures is not taken, a unit with the base's pressures is.
    yield _engine([realtime_op(REALTIME_CHARGING, "mqtt", HYBRID),
                   patch_realtime({"elecPercent": 0, "leftFrontDoorLock": 0, "totalMileage": 0, "speed": 0}),
                   patch_realtime({"tirePressUnit": 3}), patch_realtime({"totalEnergy": "--", "recent50kmEnergy": "N/A"},
                                                                         HYBRID)])
    yield _engine([realtime_op({"elecPercent": 50}), patch_realtime({"tirePressUnit": 2}),
                   patch_realtime({"tirePressUnit": 2, "leftFrontTirepressure": 38.4}),
                   patch_realtime({"leftFrontTirepressure": 0}), patch_realtime({"tirePressUnit": 3})])
    # The HVAC placeholder filter too: a patch that leaves all three sensors at zero clears them.
    yield _engine([update("hvac", {"status": 2, "pm": 4}), patch_status_push(hvac={"pm": 0}),
                   patch_status_push(hvac={"tempInCar": 21.0}), patch_status_push(hvac={"tempInCar": 0, "pm": 0}),
                   patch_status_push(hvac={"tempInCar": 0, "tempOutCar": 3, "pm": 0})])
    # The charge power goes when charging stops, in both sections.
    yield _engine([*full, patch_status_push({"chargeState": 15}, None, {"chargingState": 15}, HYBRID),
                   patch_status_push({"chargeState": 1, "chargePower": 7.0}, None, {"chargingState": 1}, HYBRID),
                   patch_status_push({"chargingState": 0, "chargePower": 1.0}, None, {"chargingState": 0, "chargePower": 1})])
    # Projections: confirmed by a patch, held against a contradicting one, expired by one.
    on, off = int(HvacOverallStatus.ON), int(HvacOverallStatus.OFF)
    yield _engine([*full, register(("hvac", "status", on), lock, ("charging", "soc", 80)),
                   patch_status_push({"leftFrontDoorLock": 2}, {"status": off}, {"soc": 79}),
                   patch_status_push({"leftFrontDoorLock": 1}, {"status": on}, {"soc": 80}), READ])
    yield _engine([*full, register(("hvac", "status", on)), advance(31.0),
                   # Nothing carried: no rebuild, so the expired projection is still held.
                   patch_status_push({}, {}, {}), patch_status_push({"foo": 1}), patch_status_push(), READ,
                   patch_status_push(charging={"soc": 71}), READ])
    yield _engine([*full, register(("realtime", "charge_power", None), ("realtime", "elec_percent", 82)),
                   status_push_op(STATUS_PUSHES[0], HYBRID), patch_realtime({"chargeState": 15})], ttl=5.0)
    # Partials that do not validate are reported and nothing is patched, not even the valid ones.
    yield _engine([*full, patch_realtime("x"), patch_realtime({}, 5), patch_realtime(None),
                   patch_status_push({"elecPercent": 1}, [], {"soc": 2}),
                   patch_status_push({"elecPercent": 1}, {"pm": 2}, {"soc": "x"}),
                   patch_status_push({"onlineState": [1]}, {"pm": 2}, {"soc": 3}), READ])
    # Patch after patch: raw keeps growing, the envelope's time moves the timestamp.
    yield _engine([*full, status_push_op({"time": 1700000100, "doorStatusDto": {"leftFrontDoor": 1, "extra": "a"}}),
                   status_push_op({"time": 1700000200, AIR: {"cycleModel": 2, "extra": "b"}}),
                   status_push_op({"timestamp": 1700000300, CHARGE: {"chargingState": 15, "time": 1700000250}}),
                   realtime_op({"elecPercent": 64}, "http"), status_push_op({"can4A505Dto": {"elecPercent": 65}})])


@group.recorded
def recorded() -> Iterator[tuple[str, dict[str, Any]]]:
    yield from _upstream_validators()
    yield from _rule_cases()
    yield from _engine_cases()
    yield from _every_field_cases()
    yield from _section_patch_cases()
    yield from _status_push_cases()
    rng = random.Random("state-recorded")
    for _ in range(25):
        yield _random_engine(rng, max_steps=12)
    for _ in range(40):
        yield _random_filter(rng)
    for _ in range(60):
        yield _random_patch(rng)


# ---------------------------------------------------------------------------
# Fuzz
# ---------------------------------------------------------------------------

_ZERO_DROP_KEYS = ["leftFrontDoorLock", "rightFrontDoorLock", "leftRearDoorLock", "rightRearDoorLock",
                   "slidingDoorLock", "elecPercent", "leftFrontTirepressure", "rightFrontTirepressure",
                   "leftRearTirepressure", "rightRearTirepressure", "enduranceMileage", "evEndurance",
                   "enduranceMileageV2", "totalMileage", "totalMileageV2", "oilEndurance", "oilPercent"]
_CONSUMPTION_KEYS = ["recent50kmEnergy", "totalEnergy", "totalConsumption", "totalConsumptionEn", "energyConsumption"]
_CONSUMPTION_VALUES = ["--", "", "N/A", "(--+--)/100km", "--+--", "6.7kW·h/100km+7.2L/100km", "6.8kW·h/100km",
                       "7.5L/100km", "(6.7度+7.2升)/百公里", "19.9kW·h/100km", "32.0kW·h/100miles", "6.1+8.4", "15.2",
                       "--kW·h/100km+7.2L/100km", None]
_ODD_VALUES = [0, 0.0, -0.0, "0", "--", "", None, -1, "-1", "NaN", 1, 2, 3, 2.5, "x", True, False, -129]


def _mutate(rng: random.Random, payload: dict[str, Any], keys: list[str], values: list[Any]) -> dict[str, Any]:
    for key in rng.sample(keys, k=rng.randint(0, min(4, len(keys)))):
        roll = rng.random()
        if roll < 0.25:
            payload.pop(key, None)
        else:
            payload[key] = rng.choice(values)
    return payload


def random_realtime(rng: random.Random) -> dict[str, Any]:
    roll = rng.random()
    if roll < 0.15:
        payload = realtime_cases.fuzz(rng)
        return payload if isinstance(payload, dict) else {}
    pool = REALTIME_CORPUS + [MQTT_PUSH, ASLEEP_HTTP, PSI_TYRES, KPA_ZERO_TYRES, KPA_TYRES, SHARK_BR, SEAL_UK, {}]
    payload = copy.deepcopy(rng.choice(pool))
    if rng.random() < 0.3:
        # A sparse push: a handful of keys of a fuller payload.
        keys = rng.sample(list(payload), k=min(len(payload), rng.randint(0, 6)))
        payload = {key: payload[key] for key in keys}
    _mutate(rng, payload, _ZERO_DROP_KEYS, _ODD_VALUES + [2.4, 38.4, 250.0, 73.0, 18234.0])
    _mutate(rng, payload, _CONSUMPTION_KEYS, _CONSUMPTION_VALUES)
    if rng.random() < 0.3:
        payload["tirePressUnit"] = rng.choice([1, 2, 3, 0, -1, "2", "--", None])
    if rng.random() < 0.15:
        pressures = rng.choice([0, 0.0, None, "--", 36.5, 250.0])
        for key in ("leftFrontTirepressure", "rightFrontTirepressure", "leftRearTirepressure", "rightRearTirepressure"):
            payload[key] = pressures
    _mutate(rng, payload, ["mainSeatHeatState", "stearingWheelHeatState", "batteryHeatState", "onlineState",
                           "chargingState", "time"], _ODD_VALUES)
    return payload if json_safe(payload) else {}


def random_gps(rng: random.Random) -> dict[str, Any]:
    roll = rng.random()
    if roll < 0.35:
        return copy.deepcopy(rng.choice(GPS_EDGES))
    payload = copy.deepcopy(rng.choice(GPS_CORPUS + [GPS_VALID, GPS_VALID_B]))
    coordinate = [52.0, 4.0, 0.0, -0.0, 0.05, -0.05, 0.1, -0.1, 0.0999, 1e-9, 90.0, -180.0, "0", "--", None, "NAN"]
    for key in ("latitude", "longitude"):
        if rng.random() < 0.4:
            if rng.random() < 0.2:
                payload.pop(key, None)
            else:
                payload[key] = rng.choice(coordinate) if rng.random() < 0.8 else rng.uniform(-0.2, 0.2)
    if rng.random() < 0.2:
        payload["speed"] = rng.choice([0, 12.5, "--", None, -1])
    if rng.random() < 0.1:
        payload["gpsTimeStamp"] = rng.choice([0, 1700000000, "--"])
    if rng.random() < 0.1:
        payload = {"data": payload}
    return payload


def random_hvac(rng: random.Random) -> dict[str, Any]:
    if rng.random() < 0.15:
        payload = hvac_cases.fuzz(rng)
        return payload if isinstance(payload, dict) else {}
    payload = copy.deepcopy(rng.choice(HVAC_CORPUS + [HVAC_PLACEHOLDER, {}]))
    _mutate(rng, payload, ["tempInCar", "tempOutCar", "pm"],
            [0, 0.0, -0.0, "0", None, "--", -129, 21.5, 14, 3, "NAN", -1, 0.5])
    _mutate(rng, payload, ["status", "acSwitch", "mainSeatHeatState", "stearingWheelHeatState", "windPosition",
                           "mainSettingTemp", "mainSettingTempNew"], _ODD_VALUES + [21.0, 24])
    return payload if json_safe(payload) else {}


CHARGING_SAMPLES = CHARGING_CORPUS + [
    {"vin": VIN, "soc": 80, "chargingState": 1, "connectState": 1, "waitStatus": 0, "fullHour": 1, "fullMinute": 25,
     "chargePower": "7.2", "chargePowerUnit": "kW", "updateTime": 1700000000},
    {"vin": VIN, "elecPercent": "55", "chargingState": 0, "connectState": 0, "fullHour": -1, "fullMinute": -1,
     "chargePower": "--", "time": 1700000000123},
]
SCHEDULE_SAMPLES = [
    {"vin": VIN, "soc": 80, "type": "1", "vehicleTimeZone": "Europe/Amsterdam",
     "smartChargeDto": {"startChargeTime": "22:00", "endChargeTime": "full", "status": "1"}, "updateTime": 1700000000},
    {"vin": VIN, "soc": "55", "time_zone": "UTC", "charge": {"status": "1"}, "journey": {"status": "0"}},
    {"smartJourneyDto": {"status": 1}, "smartChargeDto": "--", "type": 5},
]
charging_cases = ModelCases(ChargingStatus, CHARGING_SAMPLES)
energy_cases = ModelCases(EnergyConsumption, ENERGY_CORPUS)
schedule_cases = ModelCases(SmartChargingSchedule, SCHEDULE_SAMPLES)


def random_payload(rng: random.Random, cases: ModelCases) -> Any:
    if rng.random() < 0.5:
        return copy.deepcopy(rng.choice(cases.samples))
    return cases.fuzz(rng)


def random_energy(rng: random.Random) -> Any:
    while True:
        payload = random_payload(rng, energy_cases)
        if answer_is_representable(energy_group, "models.energy.EnergyConsumption", {"raw": payload, "context": None}):
            return payload


_FIELDS = {name: list(cls.model_fields) for name, cls in _MODELS.items()}
_PROPERTIES = {name: property_names(cls) for name, cls in _MODELS.items()}
_EXPECTED = [0, 1, 2, 3, -1, 1.0, 2.0, 21, 21.0, 21.5, 80, "80", True, False, None, "x", "", [1], {"status": 1},
             2**64 - 1, -(2**63), 1e300, -0.0]


def random_spec(rng: random.Random, last: dict[str, Any]) -> dict[str, Any]:
    section = rng.choice(SECTIONS) if rng.random() < 0.97 else rng.choice(["bogus", ""])
    names = _FIELDS.get(section, ["status"])
    roll = rng.random()
    if roll < 0.08:
        field = rng.choice(_PROPERTIES.get(section) or ["is_on"])
    elif roll < 0.12:
        field = rng.choice(["no_such_field", "", "Status"])
    elif section == "realtime" and roll < 0.6:
        field = rng.choice(LOCKS + [n for n, _, _ in NUMERIC_ZERO_DROP] + ["main_seat_heat_state",
                                                                          "steering_wheel_heat_state",
                                                                          "battery_heat_state", "tire_press_unit"])
    elif section == "hvac" and roll < 0.6:
        field = rng.choice(["status", "main_setting_temp_new", "wind_position", "main_seat_heat_state",
                            "steering_wheel_heat_state", "temp_in_car", "pm"])
    else:
        field = rng.choice(names)
    # Often what the section last held, so that later payloads confirm it.
    current = last.get(section)
    if isinstance(current, dict) and field in current and rng.random() < 0.35:
        value = current[field]
    elif rng.random() < 0.05:
        value = random_json(rng)
    else:
        value = rng.choice(_EXPECTED)
    return {"section": section, "field_name": field, "expected_value": value}


_PATCH_CASES = {"realtime": realtime_cases, "hvac": hvac_cases, "charging": charging_cases}
_PATCH_DTOS = {"hvac": (VehicleStatusPushDto.AIR_STATUS,), "charging": (VehicleStatusPushDto.CHARGE_STATUS,)}
#: What a field reads when the payload does not have it, and BYD's ways of saying so.
_DEFAULTS_AND_SENTINELS = [None, -1, "", "--", 0, "-1", -129, "NaN", 0.0]


def random_base(rng: random.Random, section: str) -> Any:
    if section == "realtime":
        return random_realtime(rng)
    if section == "hvac":
        return random_hvac(rng)
    return random_payload(rng, charging_cases)


def sparse_partial(rng: random.Random, cases: ModelCases) -> dict[str, Any]:
    """A handful of a model's fields under any of their keys, many of them at
    the value the field defaults to or at a sentinel."""
    payload: dict[str, Any] = {}
    for spec in rng.sample(cases.specs, k=rng.randint(0, 6)):
        key = spec.keys[0] if rng.random() < 0.6 else rng.choice(spec.all_keys)
        if rng.random() < 0.3:
            value = rng.choice(_DEFAULTS_AND_SENTINELS)
        else:
            value = random_value(rng, spec.kind, lambda cls: {})
        if cases.accepts(spec, value):
            payload[key] = value
    if rng.random() < 0.15:
        payload[rng.choice(UNKNOWN_KEYS)] = random_json(rng)
    return payload if json_safe(payload) else {}


def random_partial(rng: random.Random, section: str) -> Any:
    """The raw payload of a partial model of ``section``."""
    roll = rng.random()
    if roll < 0.35:
        # As the client builds it from a push.
        dtos = _PATCH_DTOS.get(section)
        push = random_status_push(rng)
        return flatten_vehicle_status_push(push, dtos) if dtos else flatten_vehicle_status_push(push)
    if roll < 0.45:
        # One section's fields with their awkward values, without the envelope.
        fields = SECTION_FIELDS[rng.choice(list(SECTION_FIELDS))]
        keys = rng.sample(list(fields), k=rng.randint(1, min(4, len(fields))))
        return {key: copy.deepcopy(rng.choice(fields[key])) for key in keys}
    if roll < 0.75:
        return sparse_partial(rng, _PATCH_CASES[section])
    if roll < 0.92:
        return random_base(rng, section)
    if roll < 0.97:
        return {}
    return rng.choice([[], "x", 5, True])


def _random_patch(rng: random.Random) -> tuple[str, dict[str, Any]]:
    section = rng.choice(["realtime", "realtime", "hvac", "charging"])
    contexts = ENERGY_CONTEXTS + [None]
    base = random_base(rng, section) if rng.random() < 0.9 else {}
    case = _patch(section, base, random_partial(rng, section), rng.choice(contexts), rng.choice(contexts))
    return case if json_safe(case[1]) else _patch(section, {}, {})


def random_patch_op(rng: random.Random) -> dict[str, Any]:
    context = rng.choice(ENERGY_CONTEXTS + [None] * 3)
    roll = rng.random()
    if roll < 0.5:
        return status_push_op(random_status_push(rng), context)
    if roll < 0.7:
        return patch_realtime(random_partial(rng, "realtime"), context)
    partials = [random_partial(rng, section) if rng.random() < 0.6 else None for section in ("realtime", "hvac", "charging")]
    return patch_status_push(*partials, context)


def random_op(rng: random.Random, last: dict[str, Any], commands: int) -> dict[str, Any]:
    if rng.random() < 0.18:
        return random_patch_op(rng)
    roll = rng.random()
    source = rng.choice(["http", "mqtt"])
    if roll < 0.3:
        context = rng.choice(ENERGY_CONTEXTS + [None] * 3 + ([rng.choice(CONTEXTS)] if rng.random() < 0.05 else []))
        return realtime_op(random_realtime(rng), source, context)
    if roll < 0.4:
        return update("hvac", random_hvac(rng), source)
    if roll < 0.5:
        return update("gps", random_gps(rng), source)
    if roll < 0.56:
        return update("charging", random_payload(rng, charging_cases), source)
    if roll < 0.6:
        return update("energy", random_energy(rng), source)
    if roll < 0.63:
        return update("charging_schedule", random_payload(rng, schedule_cases), "http")
    if roll < 0.78:
        return {"op": "register_projections",
                "specs": [random_spec(rng, last) for _ in range(rng.choice([0, 1, 1, 2, 3, 4]))]}
    if roll < 0.84:
        return rollback(f"cmd-{rng.randint(0, commands + 1)}" if rng.random() < 0.95 else random_text(rng, 5))
    if roll < 0.96:
        return advance(rng.choice([0.0, 0.5, 5.0, 29.999, 30.0, 30.0001, 31.0, 100.0, rng.uniform(0, 40)]))
    return READ


def _random_engine(rng: random.Random, max_steps: int = 40) -> tuple[str, dict[str, Any]]:
    ops: list[dict[str, Any]] = []
    last: dict[str, Any] = {}
    commands = 0
    for _ in range(rng.randint(5, max_steps)):
        op = random_op(rng, last, commands)
        if op["op"] == "register_projections":
            commands += 1
        elif op["op"].startswith("update_") and isinstance(op["raw"], dict):
            section = op["op"][len("update_"):]
            try:
                model = _optional(_MODELS[section], op["raw"], op.get("context"))
                last[section] = model.model_dump(mode="json") if model is not None else None
            except Exception:  # noqa: BLE001 - only steers the generator
                pass
        ops.append(op)
    vehicle = rng.choice(VEHICLE_SAMPLES) if rng.random() < 0.3 else VEHICLE
    start = rng.choice([1000.0, 0.0, 12345.678, 1e9 + 0.1])
    ttl = rng.choice([30.0, 30.0, 5.0, 0.0, 1.5, 60.0])
    case = _engine(ops, ttl=ttl, start=start, vehicle=vehicle)
    return case if json_safe(case[1]) else _engine([READ])


def _random_filter(rng: random.Random) -> tuple[str, dict[str, Any]]:
    roll = rng.random()
    contexts = ENERGY_CONTEXTS + [None]
    if roll < 0.4:
        previous = None if rng.random() < 0.2 else random_realtime(rng)
        return _pair("state.apply_realtime_filters", previous, random_realtime(rng), rng.choice(contexts),
                     rng.choice(contexts))
    if roll < 0.55:
        call = rng.choice(["state._apply_model_field_filters", "state._guard_tire_press_unit",
                           "state._guard_consumption_legs"])
        return _pair(call, random_realtime(rng), random_realtime(rng), rng.choice(contexts), rng.choice(contexts))
    if roll < 0.7:
        return "state.apply_hvac_filters", {"previous": None if rng.random() < 0.3 else random_hvac(rng),
                                            "incoming": random_hvac(rng)}
    if roll < 0.9:
        call = rng.choice(["state.guard_gps_coordinates", "state.apply_gps_filters"])
        return call, {"previous": None if rng.random() < 0.3 else random_gps(rng),
                      "incoming": None if rng.random() < 0.1 else random_gps(rng)}
    if roll < 0.95:
        return "state._has_valid_coordinates", {"gps": random_gps(rng)}
    created = rng.choice([0.0, 1000.0, rng.uniform(0, 100)])
    ttl = rng.choice([0.0, 30.0, rng.uniform(0, 60)])
    return "state.FieldProjection", {"section": rng.choice(SECTIONS), "field_name": "status",
                                     "expected_value": rng.choice(_EXPECTED), "command_id": "cmd-1",
                                     "created_at": created, "ttl": ttl,
                                     "now": created + rng.choice([ttl, 0.0, rng.uniform(0, 80)])}


@group.fuzz
def fuzz_engine(rng: random.Random) -> Iterator[tuple[str, dict[str, Any]]]:
    while True:
        yield _random_engine(rng)


@group.fuzz
def fuzz_filters(rng: random.Random) -> Iterator[tuple[str, dict[str, Any]]]:
    while True:
        yield _random_filter(rng)


@group.fuzz
def fuzz_patches(rng: random.Random) -> Iterator[tuple[str, dict[str, Any]]]:
    while True:
        yield _random_patch(rng)
