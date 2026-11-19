"""pybyd/_api/hvac.py: fetch_hvac_status compared by trace."""

from __future__ import annotations

from pybyd._api import hvac
from pybyd.models.hvac import HvacStatus

from parity.api_support import VIN, flow_oracle, ok_reply
from parity.flow_cases import (
    code_replies,
    data_source,
    dumped,
    model_cases,
    model_payloads,
    shape_replies,
    vehicle_fuzz,
    vehicle_recorded,
)
from parity.framework import Group
from parity.groups.config import SAMPLE_SESSION

group = Group("api_hvac", crate="client")

CALL = "api.hvac.fetch_hvac_status"
ENDPOINT = "/control/getStatusNow"
flow_oracle(group, CALL, dumped(hvac.fetch_hvac_status))

CASES = model_cases(HvacStatus, "HvacStatus")
ARG_SETS = [{"vin": VIN}, {"vin": ""}, {"vin": "TESTVIN0000000002"}]


@group.recorded
def recorded():
    payloads = model_payloads(CASES)
    replies = shape_replies(SAMPLE_SESSION, payloads) + code_replies(["8003", "8002", "6051"])
    yield from vehicle_recorded(CALL, ENDPOINT, ARG_SETS, replies, ok_reply(SAMPLE_SESSION, payloads[0]), SAMPLE_SESSION)


@group.fuzz
def fuzz(rng):
    yield from vehicle_fuzz(rng, CALL, ENDPOINT, lambda rng: {}, data_source(CASES))
