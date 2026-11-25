"""pybyd/_api/maintenance.py: fetch_maintenance_history compared by trace."""

from __future__ import annotations

from pybyd._api import maintenance

from parity.api_support import VIN, error_reply, flow_oracle, ok_reply, scenario
from parity.flow_cases import code_replies, dumped, shape_replies, token_fuzz, token_recorded
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_SESSION
from parity.groups.models_maintenance import RECORD_SAMPLES, record_cases

group = Group("api_maintenance", crate="client")

FETCH = "api.maintenance.fetch_maintenance_history"
flow_oracle(group, FETCH, dumped(maintenance.fetch_maintenance_history))

CAPTURED_RECORD = {
    "ro_no": "RO-NEW",
    "total_mileage": 99678,
    "last_balance_date": 1791771512000,
    "shop_name": "BYD Test",
    "dealer_code": "D001",
    "repair_type_code": "JSXM",
    "customer_email": "owner@example.com",
    "shop_address": "123 Synthetic Rd",
    "shop_phone": "555-0100",
    "latitude": 52.0,
    "longitude": 4.0,
}

CAPTURED_HISTORY = {
    "ret": 200,
    "data": {
        "list": [
            {**CAPTURED_RECORD, "ro_no": "RO-OLD", "total_mileage": 50210, "last_balance_date": 1760000000000},
            CAPTURED_RECORD,
        ]
    },
    "msg": "操作成功",
}


@group.recorded
def recorded():
    payloads = [
        CAPTURED_HISTORY,
        {"ret": 200, "data": {"list": []}, "msg": "操作成功"},
        {"ret": 200.0, "data": {"list": [CAPTURED_RECORD, "not-a-dict", None, 42]}},
        {"ret": 200, "data": {"list": "not-a-list", "extra": 1}},
        {"ret": 200, "data": None, "list": [CAPTURED_RECORD]},
        {"ret": 500, "data": None, "msg": "Internal error"},
        {"ret": 500, "data": None, "msg": ""},
        {"ret": 500.0, "data": None, "msg": "float"},
        {"ret": True, "data": None, "msg": "bool"},
        {"ret": [1], "data": None},
        {"ret": {"k": 1}, "data": None},
        {"ret": "200", "data": {"list": []}},
        {"ret": None, "msg": 0},
        {"list": RECORD_SAMPLES[:2]},
        {"data": {"list": [{"ro_no": 5}]}},
        {},
    ]
    replies = shape_replies(SAMPLE_SESSION, payloads) + code_replies()
    arg_sets = [{"vin": VIN}, {"vin": ""}, {"vin": "--"}, {"vin": "TESTVIN0000000002"}]
    yield from token_recorded(FETCH, arg_sets, replies, SAMPLE_SESSION)
    for reply in [ok_reply(SAMPLE_SESSION, CAPTURED_HISTORY), error_reply("1001"), error_reply("1008")]:
        yield FETCH, scenario(replies=[reply], vin=VIN)


def _random_payload(rng):
    choice = rng.random()
    if choice < 0.25:
        return {
            "ret": rng.choice([200, 200.0, 500, 500.0, 404, "200", None, 0, True, [1], {"k": 1}]),
            "data": rng.choice([None, [], {"list": [CAPTURED_RECORD]}]),
            "msg": rng.choice(["", "ok", "fail", None, 123]),
        }
    items = [record_cases.fuzz(rng) for _ in range(rng.randint(0, 3))]
    if rng.random() < 0.5:
        items.append(dict(CAPTURED_RECORD))
    return {"ret": 200, "data": {"list": items}, "msg": "ok"}


@group.fuzz
def fuzz(rng):
    fetch = token_fuzz(
        rng,
        FETCH,
        lambda rng: {"vin": rng.choice([VIN, VIN, "", "--", random_text(rng, 17)])},
        _random_payload,
    )
    while True:
        yield next(fetch)
