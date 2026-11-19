"""pybyd/models/realtime.py

``VehicleRealtimeData`` reads ``info.context["energy_type"]`` and uses it only
when it is an ``EnergyType`` member. JSON cannot carry one, so a parity
context spells a member ``{"energy_type": {"EnergyType": <value>}}``, which
the oracle turns into ``EnergyType(<value>)``; any other context is passed to
pyBYD unchanged (a plain ``2`` therefore reads as EV, as it does in pyBYD).
"""

from __future__ import annotations

import copy
import random
from collections.abc import Iterator
from typing import Any

from pybyd.models.realtime import (
    AirCirculationMode,
    ChargingState,
    ConnectState,
    DoorOpenState,
    LockState,
    OnlineState,
    PowerGear,
    SeatHeatVentState,
    SentryStatus,
    StearingWheelHeat,
    TirePressureUnit,
    VehicleRealtimeData,
    VehicleState,
    WindowState,
)
from pybyd.models.vehicle import EnergyType

from parity.framework import Group, random_text
from parity.groups.models_vehicle import mutate_fields, register_enum
from parity.models_support import (
    SENTINELS,
    ModelCases,
    json_safe,
    jsonable,
    property_names,
    random_json,
    random_numeric_text,
    random_scalar,
)

group = Group("models_realtime")

for _cls in (OnlineState, ConnectState, VehicleState, ChargingState, SentryStatus, TirePressureUnit, DoorOpenState,
             LockState, WindowState, PowerGear, AirCirculationMode):
    register_enum(group, f"models.realtime.{_cls.__name__}", _cls)
register_enum(group, "models.realtime.StearingWheelHeat", StearingWheelHeat, ("to_command_level",))
register_enum(group, "models.realtime.SeatHeatVentState", SeatHeatVentState, ("to_command_level",))

# ---------------------------------------------------------------------------
# VehicleRealtimeData
# ---------------------------------------------------------------------------

CALL = "models.realtime.VehicleRealtimeData"


def energy(value: Any) -> dict[str, Any]:
    """The parity spelling of a context holding ``EnergyType(value)``."""
    return {"energy_type": {"EnergyType": value}}


def python_context(context: Any) -> Any:
    if isinstance(context, dict):
        member = context.get("energy_type")
        if isinstance(member, dict) and list(member) == ["EnergyType"]:
            return {**context, "energy_type": EnergyType(member["EnergyType"])}
    return context


ENERGY_CONTEXTS = [energy(0), energy(1), energy(2), energy(-1)]
CONTEXTS = [
    None, *ENERGY_CONTEXTS, {}, 0, [], "", {"energy_type": 2}, {"energy_type": None}, {"other": 1},
    energy("2"), energy(2.0), energy(True), energy(7), {"energy_type": {"EnergyType": 2, "x": 1}},
    {"energy_type": {"energytype": 2}},
]
_PROPERTIES = property_names(VehicleRealtimeData)


@group.oracle(CALL)
def realtime(raw: Any, context: Any = None) -> dict[str, Any]:
    model = VehicleRealtimeData.model_validate(raw, context=python_context(context))
    return {
        "dump": jsonable(model.model_dump(mode="json")),
        "properties": {name: jsonable(getattr(model, name)) for name in _PROPERTIES},
    }


@group.oracle("models.realtime.VehicleRealtimeData.is_ready_raw", errors=(ValueError, TypeError))
def is_ready_raw(vehicle_info: Any) -> bool:
    return VehicleRealtimeData.is_ready_raw(vehicle_info)


SHARK_BR = {  # Shark 6 PHEV in Brazil (hass-byd-vehicle#175)
    "totalConsumption": "(6.7度+7.2升)/百公里",
    "totalConsumptionEn": "(6.7kW·h+7.2L)/100km",
    "totalEnergy": "6.7kW·h/100km+7.2L/100km",
}
SEAL_UK = {  # Seal BEV in the UK with its display set to miles (hass-byd-vehicle#181)
    "energyConsumption": "19.4",
    "nearestEnergyConsumption": "31.2",
    "nearestEnergyConsumptionUnit": "kW·h/100miles",
    "recent50kmEnergy": "31.2kW·h/100miles",
    "totalConsumption": "19.9度/百公里",
    "totalConsumptionEn": "19.9kW·h/100km",
    "totalEnergy": "32.0kW·h/100miles",
}
ASLEEP = {
    "powerSystem": 0, "totalEnergy": "--", "elecPercent": 0, "enduranceMileageV2Unit": "--",
    "nearestEnergyConsumptionUnit": "--", "nearestEnergyConsumption": "--", "recent50kmEnergy": "--",
    "totalMileageV2Unit": "--", "onlineState": 0,
}
FULL = {
    "vin": "TESTVIN0000000001", "onlineState": 1, "connectState": 1, "vehicleState": 0, "requestSerial": "SERIAL0001",
    "elecPercent": 80, "powerBattery": 80.0, "enduranceMileage": 420, "evEndurance": "420", "enduranceMileageV2": 261,
    "enduranceMileageV2Unit": "mi", "totalMileage": 12345.6, "totalMileageV2": 7671, "totalMileageV2Unit": "mi",
    "speed": 0, "powerGear": 1, "tempInCar": 21.5, "mainSettingTemp": 7, "mainSettingTempNew": 21.0, "airRunState": 1,
    "mainSeatHeatState": 1, "mainSeatVentilationState": 1, "copilotSeatHeatState": 0, "copilotSeatVentilationState": 0,
    "stearingWheelHeatState": 1, "lrSeatHeatState": 0, "lrSeatVentilationState": 0, "rrSeatHeatState": 0,
    "rrSeatVentilationState": 0, "chargingState": 15, "chargeState": 15, "waitStatus": 0, "fullHour": -1,
    "fullMinute": -1, "remainingHours": -1, "remainingMinutes": -1, "bookingChargeState": 0, "bookingChargingHour": 22,
    "bookingChargingMinute": 30, "leftFrontDoor": 0, "rightFrontDoor": 0, "leftRearDoor": 0, "rightRearDoor": 0,
    "backCover": 0, "slidingDoor": 0, "forehold": 0, "leftFrontDoorLock": 2, "rightFrontDoorLock": 2,
    "leftRearDoorLock": 2, "rightRearDoorLock": 2, "slidingDoorLock": 0, "leftFrontWindow": 1, "rightFrontWindow": 1,
    "leftRearWindow": 1, "rightRearWindow": 1, "skylight": 1, "leftFrontWindowPct": 0, "rightFrontWindowPct": 0,
    "leftRearWindowPct": 0, "rightRearWindowPct": 0, "leftFrontTirepressure": 2.5, "rightFrontTirepressure": 2.5,
    "leftRearTirepressure": 2.6, "rightRearTirepressure": 2.6, "leftFrontTireStatus": 0, "rightFrontTireStatus": 0,
    "leftRearTireStatus": 0, "rightRearTireStatus": 0, "tirePressUnit": 1, "tirepressureSystem": 0, "rapidTireLeak": 0,
    "totalPower": 0.0, "gl": -1200, "totalEnergy": "16.6kW·h/100km", "nearestEnergyConsumption": "15.2",
    "nearestEnergyConsumptionUnit": "kW·h/100km", "recent50kmEnergy": "14.9kW·h/100km", "oilEndurance": -1,
    "oilPercent": -1, "totalOil": 0, "powerSystem": 0, "engineStatus": 0, "epb": 1, "eps": 0, "esp": 0, "abs": 0,
    "svs": 0, "srs": 0, "ect": -1, "ectValue": -129, "pwr": 0, "sentryStatus": 4, "batteryHeatState": 0,
    "chargeHeatState": 0, "upgradeStatus": 0, "lrThirdHeatState": 0, "lrThirdVentilationState": 0,
    "rrThirdHeatState": 0, "rrThirdVentilationState": 0, "rate": -999, "chargePower": "--", "chargePowerUnit": "kW",
    "lessOneMin": False, "energyConsumption": "15.2", "totalConsumption": "16.6度/百公里",
    "totalConsumptionEn": "16.6kW·h/100km", "oilPressureSystem": 0, "brakingSystem": 0, "chargingSystem": 0,
    "steeringSystem": 0, "okLight": 0, "repairModeSwitch": "0", "vehicleTimeZone": "Europe/Amsterdam",
    "powerBatteryConnection": -1, "ins": 0, "time": 1700000000,
}
CHARGING = {
    "onlineState": 1, "chargingState": 1, "chargeState": 1, "fullHour": 1, "fullMinute": 25, "rate": -11,
    "chargePower": "11.0", "chargePowerUnit": "kW", "lessOneMin": "false", "elecPercent": "55.5", "time": 1700000000123,
    "leftFrontDoorLock": 1, "rightFrontDoorLock": 2, "leftFrontDoor": 1, "skylight": 2, "powerGear": 3,
    "batteryHeatState": 1, "stearingWheelHeatState": -1,
}
HYBRID = {
    "onlineState": 2, "oilEndurance": 380, "oilPercent": 61, "energyConsumption": "6.1kW·h/100km+8.4L/100km",
    "nearestEnergyConsumption": "5.0", "nearestEnergyConsumptionUnit": "L/100km", **SHARK_BR,
    "recent50kmEnergy": "6.1+8.4", "time": "2024-01-01T10:00:00Z",
}
SAMPLES = [FULL, CHARGING, HYBRID, SEAL_UK, SHARK_BR, ASLEEP]

cases = ModelCases(VehicleRealtimeData, SAMPLES)
# The generic recorded cases vary one field at a time around a small payload,
# so the vector file stays a few megabytes (every case dumps ~170 fields).
# Every generated case still runs live in the fuzz source below.
COMPACT = {"onlineState": 1, "elecPercent": 80, "totalEnergy": "16.6kW·h/100km", "nearestEnergyConsumption": "15.2",
           "nearestEnergyConsumptionUnit": "kW·h/100km", "time": 1700000000}
RECORDED_EVERY = 12

SPLIT_KEYS = ["energyConsumption", "recent50kmEnergy", "totalEnergy", "totalConsumption", "totalConsumptionEn"]

# Combined strings that reach every branch of _split_combined_string,
# _classify_two_legs, _extract_unit and _first_num.
CONSUMPTION = [
    None, "", "--", " -- ", "nan", "N/A", "6.1", " 6.1 ", "-0", "-3.5", "6.", ".5", "١٢.٥", "6.1kW·h/100km",
    "6.1 kWh / 100 km", "3.5L/100km", "3.5升/百公里", "19.9度/百公里", "32.0kW·h/100miles", "7.2L/100mi",
    "kW·h/100km", "--kW·h/100km", "--L/100km", "6.1+8.4", " 6.1 + 8.4 ", "6.1kW·h/100km+8.4L/100km",
    "8.4L/100km+6.1kW·h/100km", "8.4L/100km+6.1", "6.1+8.4kW·h/100km", "6.1L+8.4L", "6.1kWh+8.4kWh",
    "6.1kWh+8.4L", "+8.4", "6.1+", "+", "--+--", "6.1+8.4+1.0", "(19.7kW·h+3.5L)/100km", "(6.7度+7.2升)/百公里",
    "(3.5L+19.7kW·h)/100km", "(6.1+8.4)/100km", "(6.1+8.4)", "(6.1+8.4)//100mi", "(--+--)/100km", "(6.1)/100km",
    "(6.1+8.4", "()", "(+)", "(a+b)/c", "6.1)+(8.4", "9" * 400 + "kWh", "-" + "9" * 400, "1e5", "1.5.5L/100km",
    "6.1mpg", "6.1%", 6.1, 0, -1, 1e16, 2**63, True, False, [], [6.1], {"a": "+5"}, {"a": 1},
]
NEAREST_UNITS = [
    None, "", "--", "kW·h/100km", "kWh/100miles", "L/100km", "升/百公里", "mpg", 0, 1, True, [], ["L"],
    "L", " L /100km ",
]


def _payload(**changes: Any) -> dict[str, Any]:
    return {key: value for key, value in changes.items() if value is not ...}


def _hybrid_overrides() -> Iterator[dict[str, Any]]:
    """Payloads for the HYBRID branch that reads the energyConsumption legs back."""
    for ev in [..., None, 6.1, "6.1", "--", 0, [], "x"]:
        for ev_unit in [..., None, "", "kWh/100km", 5, "--"]:
            yield _payload(nearestEnergyConsumption="5.0", nearestEnergyConsumptionUnit="L/100km",
                           energy_consumption_ev=ev, energy_consumption_ev_unit=ev_unit,
                           energy_consumption_fuel=ev, energy_consumption_fuel_unit=ev_unit)
    yield {"energyConsumption": "6.1+8.4", "nearestEnergyConsumption": "5.0"}
    yield {"energyConsumption": "6.1+8.4", "energyConsumptionEv": "1.0", "nearestEnergyConsumption": "--"}
    yield {"energyConsumption": "8.4L/100km", "nearestEnergyConsumption": 0, "nearestEnergyConsumptionUnit": "L"}


SENTINEL_FIELDS = {
    "tempInCar": [-129, -129.0, "-129", -128.9, -1, -130],
    "ectValue": [-129, -1, 0, 5, "-129"],
    "rate": [-999, -999.0, -1000, -998.5, -50, 0, "-999", 10],
    "fullHour": [-1, 0, 2], "fullMinute": [-1, 0, 59], "remainingHours": [-1, 0, 3], "remainingMinutes": [-1, 0, 30],
    "oilEndurance": [-1, -0.5, -0.0, 0, 380], "oilPercent": [-1, 0, 61], "enduranceMileage": [-1, 0, 420],
    "enduranceMileageV2": [-1, "-1", 261], "elecPercent": [-1, 0, 80, "-0.5"], "ect": [-1, 0, 1], "abs": [-1, 0, 1],
    "absWarning": [-1, 0], "svs": [-1, 0], "srs": [-1, 1], "eps": [-1, 0], "esp": [-1, 0], "pwr": [-1, 0],
    "powerSystem": [-1, 0], "tirepressureSystem": [-1, 0], "rapidTireLeak": [-1, 1], "leftFrontTireStatus": [-1, 0],
    "rightFrontTireStatus": [-1, 0], "leftRearTireStatus": [-1, 0], "rightRearTireStatus": [-1, 0],
    "upgradeStatus": [-1, 0, 2],
}
ALIAS_CASES = [
    {"backCover": 1}, {"backCover": 1, "trunkLid": 0}, {"abs": 1}, {"abs": 1, "absWarning": 0}, {"time": 1700000000},
    {"time": 1, "timestamp": 1700000000}, {"leftFrontTirepressure": 2.4, "leftFrontTirePressure": 2.5},
    {"rightRearTirepressure": "2.9"}, {"stearingWheelHeatState": 1}, {"stearingWheelHeatState": 1,
                                                                       "steeringWheelHeatState": -1},
    {"recent50kmEnergy": "6.1"}, {"recent50KmEnergy": "6.1"}, {"recent50KmEnergy": 6.1},
    {"recent50kmEnergy": "6.1", "recent50KmEnergy": "7.2"}, {"recent_50km_energy": "x"},
    {"totalEnergy": 6.1, "total_energy": "y"}, {"total_energy": 5}, {"totalEnergyEv": "1", "totalEnergy": "6.1"},
    {"total_energy_ev": "x", "totalEnergy": "6.1"},
]
STEERING = [2, "2", 2.0, " 2 ", -1, "-1", 0, 1, 3, 1.5, True, False, "x", 2**63]
PROPERTY_CASES = [
    {"chargeState": 1}, {"chargeState": 15}, {"chargeState": "1"}, {"chargingState": 1}, {"chargeState": 7},
    {"leftFrontDoorLock": 0, "rightFrontDoorLock": -1}, {"leftFrontDoorLock": 2, "rightFrontDoorLock": 1},
    {"leftRearDoorLock": 2, "rightRearDoorLock": 9}, {"slidingDoorLock": 1}, {"forehold": 1}, {"slidingDoor": 1},
    {"skylight": 2}, {"rightRearWindow": 3}, {"powerGear": 3}, {"powerGear": 1}, {"batteryHeatState": 0},
    {"batteryHeatState": -3}, {"onlineState": 1}, {"fullHour": 2**53, "fullMinute": 2**53},
    {"fullHour": 0, "fullMinute": "5"}, {"fullHour": "1"},
]
READY = [
    None, {}, {"onlineState": 2}, {"onlineState": 2.0, "time": 5}, {"onlineState": "2", "time": 5},
    {"onlineState": True, "time": 5}, {"leftFrontTirepressure": 2.5}, {"rightRearTirepressure": "2.5"},
    {"leftRearTirepressure": " 1_0 "}, {"leftFrontTirepressure": "abc"}, {"leftFrontTirepressure": [1]},
    {"leftFrontTirepressure": 2.5, "rightFrontTirepressure": "abc"}, {"leftFrontTirepressure": "nan"},
    {"leftFrontTirepressure": "-inf", "time": "inf"}, {"time": 1700000000}, {"time": "1700000000"},
    {"time": "١٢"}, {"time": "+5"}, {"time": "1.5"}, {"time": 1.5}, {"time": 0.5}, {"time": -3}, {"time": True},
    {"time": "9" * 50}, {"time": "-" + "9" * 50}, {"time": "1_" + "0" * 49}, {"time": "--5"}, {"time": {}},
    {"time": {"a": 1}}, {"time": 1e300}, {"time": 2**64 - 1}, {"enduranceMileage": 420},
    {"enduranceMileage": "0.1"}, {"enduranceMileage": -1}, {"enduranceMileage": ""}, {"enduranceMileage": "x"},
    {"enduranceMileage": [0]}, {"enduranceMileage": True}, {"leftFrontTirepressure": 0, "time": 0,
                                                            "enduranceMileage": 0},
    {"onlineState": 1, "leftFrontTirepressure": None, "time": None, "enduranceMileage": None},
]


@group.recorded
def recorded():
    generic = list(ModelCases(VehicleRealtimeData, [COMPACT, *SAMPLES]).recorded())
    for index, payload in enumerate(generic):
        # The first cases are the samples and the non-dict inputs: keep them all.
        if index < 14 or index % RECORDED_EVERY == 0:
            yield CALL, {"raw": payload, "context": CONTEXTS[index % len(CONTEXTS)]}
    for index, sample in enumerate(SAMPLES):
        for context in CONTEXTS[index % 3::3]:
            yield CALL, {"raw": copy.deepcopy(sample), "context": context}
    # Every split key and the nearest pair get the same value, under EV, ICE and HYBRID.
    for value in CONSUMPTION:
        for context in ENERGY_CONTEXTS[:3]:
            payload = {key: value for key in SPLIT_KEYS + ["nearestEnergyConsumption"]}
            yield CALL, {"raw": payload, "context": context}
    for index, value in enumerate(CONSUMPTION[:45]):
        for turn in range(3):
            unit = NEAREST_UNITS[(index + 5 * turn) % len(NEAREST_UNITS)]
            context = ENERGY_CONTEXTS[(index + turn) % 3]
            yield CALL, {"raw": {"nearestEnergyConsumption": value, "nearestEnergyConsumptionUnit": unit},
                         "context": context}
    for index, payload in enumerate(_hybrid_overrides()):
        yield CALL, {"raw": payload, "context": energy(2)}
        yield CALL, {"raw": payload, "context": ENERGY_CONTEXTS[index % 2]}
    for key, values in SENTINEL_FIELDS.items():
        for value in values + ["--", None]:
            yield CALL, {"raw": {key: value}, "context": None}
    for payload in ALIAS_CASES + PROPERTY_CASES:
        yield CALL, {"raw": payload, "context": energy(2)}
    for value in STEERING:
        yield CALL, {"raw": {"steeringWheelHeatState": value}, "context": None}
        yield CALL, {"raw": {"stearingWheelHeatState": value}, "context": None}
    # A caller-provided raw survives the split; a non-dict input never reads the context.
    yield CALL, {"raw": {"raw": {"a": 1}, "totalEnergy": "6.1+8.4"}, "context": energy(2)}
    yield CALL, {"raw": {"raw": "--", "totalEnergy": "6.1"}, "context": None}
    yield CALL, {"raw": [], "context": energy(2)}
    for info in READY:
        yield "models.realtime.VehicleRealtimeData.is_ready_raw", {"vehicle_info": info}


def random_consumption(rng: random.Random) -> Any:
    """A consumption value: combined strings built from parts, often broken."""
    if rng.random() < 0.15:
        return rng.choice([random_scalar(rng), random_json(rng), rng.choice(CONSUMPTION)])

    def number() -> str:
        return rng.choice(["6.1", "19.7", "-3.5", "0", "8.", ".4", "--", "", "١٢", "1e3", random_numeric_text(rng)])

    def unit() -> str:
        return rng.choice(["", "", "kW·h/100km", "kWh", "kW·h", "L/100km", "L", "升", "度", "/100km", "/100miles",
                           "/百公里", "kWh/100mi", "mpg", "%", random_text(rng, 4)])

    def leg() -> str:
        return number() + rng.choice(["", " "]) + unit()

    shape = rng.randrange(5)
    if shape == 0:
        text = leg()
    elif shape == 1:
        text = leg() + rng.choice(["+", " + ", "++"]) + leg()
    elif shape == 2:
        text = f"({leg()}+{leg()}){rng.choice(['', '/', ' / ', '//'])}{rng.choice(['100km', '100miles', '百公里', ''])}"
    elif shape == 3:
        text = rng.choice(["(", ")", "("]) + leg() + rng.choice(["", ")", "+", "+)"]) + leg()
    else:
        text = "+".join(leg() for _ in range(rng.randint(0, 4)))
    return rng.choice(["", " ", "\t"]) + text + rng.choice(["", " ", "　"])


@group.fuzz
def fuzz_generic(rng):
    generic = list(ModelCases(VehicleRealtimeData, [COMPACT, *SAMPLES]).recorded())
    while True:
        yield CALL, {"raw": cases.fuzz(rng), "context": rng.choice(CONTEXTS)}
        # The generic recorded cases left out of the vector file.
        yield CALL, {"raw": copy.deepcopy(rng.choice(generic)), "context": rng.choice(CONTEXTS)}


@group.fuzz
def fuzz_samples(rng):
    pools: dict[str, list[Any]] = {
        "steering_wheel_heat_state": STEERING,
        **{spec.name: SENTINEL_FIELDS.get(spec.keys[0], []) + SENTINELS
           for spec in cases.specs if spec.keys[0] in SENTINEL_FIELDS},
    }
    while True:
        payload = mutate_fields(rng, cases, pools)
        for key in rng.sample(SPLIT_KEYS + ["nearestEnergyConsumption", "nearestEnergyConsumptionUnit"],
                              k=rng.randint(0, 3)):
            if key == "nearestEnergyConsumptionUnit":
                payload[key] = rng.choice(NEAREST_UNITS + ["kW·h/100miles", random_text(rng, 6)])
            else:
                payload[key] = random_consumption(rng)
        if rng.random() < 0.15:
            payload.update(rng.choice(list(_hybrid_overrides())))
        if rng.random() < 0.1:
            payload.update(rng.choice(ALIAS_CASES + PROPERTY_CASES))
        if json_safe(payload):
            yield CALL, {"raw": payload, "context": rng.choice(CONTEXTS + ENERGY_CONTEXTS * 3)}


@group.fuzz
def fuzz_is_ready_raw(rng):
    keys = ["onlineState", "leftFrontTirepressure", "rightFrontTirepressure", "leftRearTirepressure",
            "rightRearTirepressure", "time", "enduranceMileage"]
    pool = [None, 0, 1, 2, 2.0, -1, 0.5, 1.5, "", "0", "2", "2.5", " 1_0 ", "abc", "nan", "inf", "-inf", "١٢",
            "9" * 50, "1.5", True, False, [], [1], {}, {"a": 1}, 1e300, 2**64 - 1]
    while True:
        if rng.random() < 0.1:
            info = rng.choice(READY)
        else:
            info = {key: rng.choice([lambda: rng.choice(pool), lambda: random_scalar(rng),
                                     lambda: random_numeric_text(rng)])()
                    for key in rng.sample(keys, k=rng.randint(0, len(keys)))}
        if json_safe(info):
            yield "models.realtime.VehicleRealtimeData.is_ready_raw", {"vehicle_info": info}

