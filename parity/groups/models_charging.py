"""pybyd/models/charging.py"""

from __future__ import annotations

from pybyd.models.charging import ChargingStatus

from parity.framework import Group
from parity.models_support import register_model

group = Group("models_charging")

register_model(
    group,
    "models.charging.ChargingStatus",
    ChargingStatus,
    samples=[
        {"vin": "TESTVIN0000000001", "soc": 80, "chargingState": 1, "connectState": 1, "waitStatus": 0,
         "fullHour": 1, "fullMinute": 25, "chargePower": "7.2", "chargePowerUnit": "kW", "updateTime": 1700000000},
        {"vin": "TESTVIN0000000001", "elecPercent": "55", "chargingState": 0, "connectState": 0, "fullHour": -1,
         "fullMinute": -1, "chargePower": "--", "time": 1700000000123},
        {"chargePower": "5.1", "chargePowerUnit": "kW"},
    ],
)
