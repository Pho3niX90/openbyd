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
    keep_representable,
    model_cases,
    model_payloads,
    shape_replies,
    token_fuzz,
    token_recorded,
)
from parity.framework import Group, random_text
from parity.groups.pyjson import random_json
from parity.models_support import TIMESTAMP_VALUES, timestamp_safe
from parity.groups.config import SAMPLE_SESSION

group = Group("api_vehicle", crate="client")

flow_oracle(group, "api.vehicle.fetch_vehicle_list", dumped(vehicle.fetch_vehicle_list))
flow_oracle(group, "api.vehicle.fetch_empower_detail_by_vin", dumped(vehicle.fetch_empower_detail_by_vin))

EMPOWER = "api.vehicle.fetch_empower_detail_by_vin"
#: The reply of pyBYD's empower detail test, with nothing identifying.
RANGE_TREE = [
    {"code": "2", "name": "Keys & Control", "childList": [{"code": "21", "name": "Basic control", "childList": []}]},
    {"code": "4", "name": "More control", "childList": [{"code": "41", "name": "Scheduled charging", "childList": []}]},
]
EMPOWER_REPLIES = [
    {
        "empowerControlPwdStatus": False,
        "data": {"empowerId": 114149, "empowerStatus": 0, "ownerEmail": "owner@example.com",
                 "email": "user@example.com", "authPeriod": 4, "expireDate": 1799999999000, "detailList": RANGE_TREE},
    },
    {"empowerControlPwdStatus": True, "data": {"detailList": []}},
    {"empowerControlPwdStatus": "yes", "data": "x"},
    {"empowerControlPwdStatus": 0, "data": None},
    {"data": {"vin": "OTHER", "empowerControlPwdStatus": True, "raw": 5}},
    {"data": {"detailList": [None]}},
    {"data": {"expireDate": "--", "authPeriod": "4"}},
    {"empowerControlPwdStatus": [1]},
    {},
]
EMPOWER_ARGS = [{"vin": "TESTVIN0000000001", "empower_id": 114149}, {"vin": "", "empower_id": 0},
                {"vin": "TESTVIN0000000002", "empower_id": -5}, {"vin": "TESTVIN0000000001", "empower_id": 2**40}]

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
    empower = shape_replies(SAMPLE_SESSION, EMPOWER_REPLIES) + code_replies()
    yield from token_recorded(EMPOWER, EMPOWER_ARGS, empower, SAMPLE_SESSION)


def vehicles(rng):
    return [data_source(CASES)(rng) for _ in range(rng.choice([0, 1, 1, 2, 3]))]


def empower_reply(rng):
    reply = dict(rng.choice(EMPOWER_REPLIES))
    if rng.random() < 0.5:
        data = dict(reply.get("data") if isinstance(reply.get("data"), dict) else {})
        key = rng.choice(["empowerId", "authPeriod", "email", "expireDate"])
        # A date Windows cannot convert fails there and parses on Linux; keep to the portable ones.
        value = random_json(rng) if key != "expireDate" else rng.choice([*TIMESTAMP_VALUES, random_json(rng)])
        if key != "expireDate" or timestamp_safe(value):
            data[key] = value
        reply["data"] = data
    return reply


@group.fuzz
def fuzz(rng):
    data = data_source(CASES, wrap=lambda payload: [payload, *CASES.samples[: rng.randint(0, 2)]])
    lists = token_fuzz(rng, CALL, lambda rng: {}, lambda rng: vehicles(rng) if rng.random() < 0.5 else data(rng))
    details = token_fuzz(rng, EMPOWER, lambda rng: {"vin": rng.choice(["TESTVIN0000000001", "", random_text(rng, 17)]),
                                                    "empower_id": rng.choice([114149, 0, -1, rng.randint(0, 2**40)])},
                         empower_reply)
    while True:
        yield next(lists)
        yield next(details)


# A grant id beyond i64 (docs/porting-models.md, "Representation limits").
keep_representable(group)
