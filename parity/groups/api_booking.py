"""pybyd/_api/booking.py: fetch_booking_list compared by trace."""

from __future__ import annotations

from pybyd._api import booking
from pybyd.models.booking import BookingList

from parity.api_support import VIN, flow_oracle
from parity.flow_cases import (
    code_replies,
    data_source,
    dumped,
    model_cases,
    model_payloads,
    shape_replies,
    token_fuzz,
    token_recorded,
)
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_SESSION

group = Group("api_booking", crate="client")

CALL = "api.booking.fetch_booking_list"
flow_oracle(group, CALL, dumped(booking.fetch_booking_list))

CASES = model_cases(BookingList, "BookingList")
ARG_SETS = [{"vin": VIN}, {"vin": ""}, {"vin": "TESTVIN0000000002"}, {"vin": "Zoë"}]
# The reply's own vin replaces the requested one, whatever it is.
VIN_REPLIES = [
    {"vin": "OTHER00000000001", "bookings": []},
    {"vin": None},
    {"vin": ""},
    {"vin": 5},
    {"vin": "--", "bookingList": []},
]


@group.recorded
def recorded():
    payloads = [*model_payloads(CASES), *VIN_REPLIES]
    replies = shape_replies(SAMPLE_SESSION, payloads) + code_replies()
    yield from token_recorded(CALL, ARG_SETS, replies, SAMPLE_SESSION)


@group.fuzz
def fuzz(rng):
    yield from token_fuzz(
        rng, CALL, lambda rng: {"vin": rng.choice([VIN, VIN, "", random_text(rng, 17)])}, data_source(CASES)
    )
