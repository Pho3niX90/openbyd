"""pybyd/models/energy.py"""

from __future__ import annotations

from typing import Any

from pybyd.exceptions import BydError
from pybyd.models.energy import (
    CumulativeEnergyConsumption,
    EnergyConsumption,
    EnergyConsumptionGraph,
    NearestEnergyConsumption,
)

from parity.framework import Group
from parity.models_support import register_model, skip_unrepresentable_integers

group = Group("models_energy")

_GRAPH = {"energyConsumption": [14.2, "15.1", "--", 13], "energyConsumptionUnit": "kW·h/100km"}
_CUMULATIVE = {
    "totalMileage": "12345.6", "mileageUnit": "km", "avgEVConsumption": 15.4, "evUnit": "kW·h/100km",
    "avgOilConsumption": "--", "oilUnit": "L/100km",
}
_NEAREST = {
    "avgEvConsumption": "14.8", "evConsumption": 7.4, "evUnit": "kWh/100km", "evValueUnit": "kW·h",
    "avgOilConsumption": 6.1, "oilConsumption": "3.05", "oilUnit": "升/百公里", "oilValueUnit": "升",
    "avgEqOilConsumption": 1.7, "driveDistribution": 60, "electDistribution": "30",
    "airDistribution": "10.0", "otherDistribution": "--",
}

register_model(
    group,
    "models.energy.EnergyConsumptionGraph",
    EnergyConsumptionGraph,
    samples=[
        dict(_GRAPH),
        {"energyConsumption": ["1.5", " 2 ", "NAN", "inf", "١٢", "1_0", True, None, [1], {"a": 1}, "abc"],
         "energyConsumptionUnit": "度/百公里"},
        {"energyConsumption": "notalist", "energyConsumptionUnit": 5},
        {"energyConsumption": [], "energyConsumptionUnit": "KWH/100 KM"},
    ],
)

register_model(
    group,
    "models.energy.CumulativeEnergyConsumption",
    CumulativeEnergyConsumption,
    samples=[
        dict(_CUMULATIVE),
        {"totalMileage": " 1_000.5 ", "mileageUnit": "mi", "avgEvConsumption": "1e3", "evUnit": "kWh/100miles",
         "avgOilConsumption": "NAN", "oilUnit": "L"},
        {"totalMileage": [1], "avgEvConsumption": {"a": 1}, "avgOilConsumption": True},
        {"totalMileage": "١٢٣", "avgEvConsumption": "1.e5", "avgOilConsumption": ".5"},
    ],
)

nearest_cases = register_model(
    group,
    "models.energy.NearestEnergyConsumption",
    NearestEnergyConsumption,
    samples=[
        dict(_NEAREST),
        {"drivenDistribution": 1, "driveDistribution": "12.9", "electDistribution": -3.7,
         "airDistribution": " 7 ", "otherDistribution": True},
        {"driveDistribution": "nan", "electDistribution": "abc", "airDistribution": [1],
         "otherDistribution": {"a": 1}},
        {"evUnit": "kW.h/100km", "evValueUnit": "KWH", "oilUnit": "unknown/100 km", "oilValueUnit": "l",
         "driveDistribution": "١٢"},
        # -2^63 as a float is exact i64::MIN; python int(float) accepts it (nightly issue #16).
        {"driveDistribution": -(2.0**63), "electDistribution": -9223372036854775808},
    ],
)

register_model(
    group,
    "models.energy.EnergyConsumption",
    EnergyConsumption,
    samples=[
        {"selfGraph": dict(_GRAPH), "cumulativeEnergyConsumption": dict(_CUMULATIVE),
         "nearestEnergyConsumption": dict(_NEAREST), "autoModelGraph": dict(_GRAPH), "timestamp": 1700000000},
        {"selfGraph": dict(_GRAPH), "time": 1700000000123},
        {"time": "2024-05-07T05:56:45.000+00:00", "timestamp": "--"},
        {"nearestEnergyConsumption": {"driveDistribution": "inf"}},
    ],
)

# Python's ``int(float("inf"))`` raises OverflowError, which pydantic does not wrap,
# so it escapes ``model_validate`` as itself. openbyd reports the same exception.
for _call in (
    "models.energy.NearestEnergyConsumption",
    "models.energy.EnergyConsumption",
):
    group.expected_errors[_call] = (BydError, ValueError, OverflowError)


# ``_to_int`` turns a float such as ``1e300`` into an integer of any size.
skip_unrepresentable_integers(group)
