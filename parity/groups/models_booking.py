"""pybyd/models/booking.py"""

from __future__ import annotations

from pybyd.models.booking import BookingList, ClimateBooking

from parity.framework import Group
from parity.models_support import register_model

group = Group("models_booking")

_BOOKING = {
    "bookingTime": 1780938216, "mainSettingTemp": 23, "mainSettingTempNew": 23.0, "timeSpan": 1, "acSwitch": 0,
    "cycleMode": 2, "windLevel": 0, "windMode": 0, "airConditionTempRange": 7, "bookingId": 1216038691305533440,
}

register_model(
    group,
    "models.booking.ClimateBooking",
    ClimateBooking,
    samples=[
        dict(_BOOKING),
        {"bookingId": "42", "bookingTime": "1780938216000", "mainSettingTemp": "22", "mainSettingTempNew": "22.5",
         "timeSpan": "--", "acSwitch": 1},
        {"bookingTime": 0, "bookingId": 2**53},
    ],
)

register_model(
    group,
    "models.booking.BookingList",
    BookingList,
    samples=[
        {"vin": "TESTVIN0000000001", "allowSetting": 0, "configData": 0, "listInfo": [dict(_BOOKING)]},
        {"vin": "TESTVIN0000000001", "bookings": [dict(_BOOKING), {"bookingId": 7}]},
        {"vin": "TESTVIN0000000001", "listInfo": [], "bookings": [dict(_BOOKING)]},
        {"allowSetting": "1", "configData": "--"},
    ],
)
