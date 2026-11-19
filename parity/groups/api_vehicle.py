"""pybyd/_api/vehicle.py: fetch_vehicle_list compared by trace."""

from __future__ import annotations

from pybyd._api import vehicle
from pybyd.models.vehicle import Vehicle

from parity.api_support import flow_oracle, ok_reply, scenario
from parity.flow_cases import (
    code_replies,
    corpus,
    data_source,
    dumped,
    model_cases,
    model_payloads,
    shape_replies,
    token_fuzz,
)
from parity.framework import Group
from parity.groups.config import SAMPLE_SESSION

group = Group("api_vehicle", crate="client")

flow_oracle(group, "api.vehicle.fetch_vehicle_list", dumped(vehicle.fetch_vehicle_list))

CALL = "api.vehicle.fetch_vehicle_list"
CASES = model_cases(Vehicle, "Vehicle")
LISTS = [
    corpus("Vehicle"),
    [],
    [{}],
    [{"vin": "TESTVIN0000000001", "modelName": "SEALION 7", "energyType": "2", "totalMileage": "1234.5"}],
    [{"vin": "TESTVIN0000000001"}, {"vin": "TESTVIN0000000002", "empowerType": 3}],
    [*corpus("Vehicle"), 5],
    [None],
    ["x"],
    [[]],
    [{"vin": "TESTVIN0000000001", "totalMileage": "abc"}],
    [{"vin": "TESTVIN0000000001"}, {"vin": None}, {"vin": ""}, {"vin": "--"}],
]


@group.recorded
def recorded():
    replies = shape_replies(SAMPLE_SESSION, LISTS) + code_replies()
    replies += [ok_reply(SAMPLE_SESSION, [payload]) for payload in model_payloads(CASES)]
    for reply in replies:
        yield CALL, scenario(replies=[reply])
    yield CALL, scenario(replies=[ok_reply(SAMPLE_SESSION, LISTS[0]), ok_reply(SAMPLE_SESSION, [])])
    yield CALL, scenario(replies=[])
    yield CALL, scenario(guest_vins={"TESTVIN0000000001": True}, replies=[ok_reply(SAMPLE_SESSION, LISTS[0])])


def vehicles(rng):
    return [data_source(CASES)(rng) for _ in range(rng.choice([0, 1, 1, 2, 3]))]


@group.fuzz
def fuzz(rng):
    data = data_source(CASES, wrap=lambda payload: [payload, *CASES.samples[: rng.randint(0, 2)]])
    yield from token_fuzz(rng, CALL, lambda rng: {}, lambda rng: vehicles(rng) if rng.random() < 0.5 else data(rng))
