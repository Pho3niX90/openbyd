"""pybyd/_api/charging.py: fetch_charging_homepage and fetch_charging_status compared by trace."""

from __future__ import annotations

from pybyd._api import charging
from pybyd.models.charging import ChargingStatus

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

group = Group("api_charging", crate="client")

HOMEPAGE = "api.charging.fetch_charging_homepage"
STATUS = "api.charging.fetch_charging_status"
flow_oracle(group, HOMEPAGE, dumped(charging.fetch_charging_homepage))
flow_oracle(group, STATUS, dumped(charging.fetch_charging_status))

CASES = model_cases(ChargingStatus, "ChargingStatus")
HOME = {
    "soc": 77,
    "chargingState": 15,
    "fullHour": 1,
    "fullMinute": 30,
    "chargeWay": 0,
    "startTime": "22:00",
    "endTime": "06:00",
    "requestSerial": "S-1",
}
PAYLOADS = [*model_payloads(CASES), HOME]
ARG_SETS = [{"vin": VIN}, {"vin": ""}, {"vin": "TESTVIN0000000002"}, {"vin": "Zoë 比亚迪"}]


@group.recorded
def recorded():
    replies = shape_replies(SAMPLE_SESSION, PAYLOADS) + code_replies()
    for call in (HOMEPAGE, STATUS):
        yield from token_recorded(call, ARG_SETS, replies, SAMPLE_SESSION)


@group.fuzz
def fuzz(rng):
    data = data_source(CASES)
    homepage = token_fuzz(rng, HOMEPAGE, lambda rng: {"vin": rng.choice([VIN, "", random_text(rng, 17)])}, data)
    status = token_fuzz(rng, STATUS, lambda rng: {"vin": rng.choice([VIN, "", random_text(rng, 17)])}, data)
    while True:
        yield next(homepage)
        yield next(status)
