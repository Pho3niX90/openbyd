"""pybyd/models/gps.py"""

from __future__ import annotations

from pybyd.models.gps import GpsInfo

from parity.framework import Group
from parity.models_support import register_model

group = Group("models_gps")

register_model(
    group,
    "models.gps.GpsInfo",
    GpsInfo,
    samples=[
        {"data": {"latitude": 52.0, "longitude": 4.0, "speed": 0, "direction": 270.5, "gpsTimeStamp": 1700000000},
         "requestSerial": "SERIAL0001"},
        {"latitude": "52.0001", "longitude": "4.0002", "speed": "12.5", "direction": "--", "gpsTimeStamp": 1700000000123},
        {"latitude": 52.0, "longitude": 4.0, "gpsTimeStamp": 0, "requestSerial": ""},
    ],
)
