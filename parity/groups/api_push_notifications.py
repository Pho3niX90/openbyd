"""pybyd/_api/push_notifications.py: fetch_push_state and set_push_state compared by trace."""

from __future__ import annotations

from pybyd._api import push_notifications

from parity.api_support import VIN, flow_oracle
from parity.flow_cases import (
    code_replies,
    corpus,
    dumped,
    shape_replies,
    token_fuzz,
    token_recorded,
)
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_SESSION
from parity.groups.pyjson import random_json, random_object

group = Group("api_push_notifications", crate="client")

FETCH = "api.push_notifications.fetch_push_state"
SET = "api.push_notifications.set_push_state"
flow_oracle(group, FETCH, dumped(push_notifications.fetch_push_state))
flow_oracle(group, SET, dumped(push_notifications.set_push_state))

STATE = corpus("PushNotificationState")[0]
SWITCH_LISTS = [
    [{"type": 701, "state": 1}, {"type": 1, "state": 0}],
    [{"type": 701, "state": 0}],
    [{"type": "701", "state": "1"}],
    [{"type": 5, "state": "--"}, {"type": "", "state": None}, {"state": 1}, {"type": 2}],
    [{"type": 701, "state": 1, "extra": "x"}],
    [{}],
    [],
    [1, "x", None, [], {"type": 701, "state": 1}],
    [{"type": "abc", "state": 1}],
    [{"type": 1.5, "state": 1}],
    [{"type": True, "state": False}],
    [{"type": [1], "state": {}}],
    STATE.get("switches", []),
    # a reply that is an object, not a list: no switches
    {"type": 701, "state": 1},
    STATE,
]
FETCH_ARGS = [{"vin": VIN}, {"vin": ""}, {"vin": "Zoë 比亚迪"}, {"vin": "--"}, {"vin": " nan "}]
ACKS = [
    {"result": "ok"},
    {"result": 5},
    {"result": None, "requestSerial": "S-1"},
    {"vin": "OTHER00000000001", "result": "x"},
    {"raw": {"a": 1}, "result": "y"},
    {"requestSerial": 7},
    {},
    {"code": 1},
]
SET_ARGS = [
    {"vin": VIN, "enable": True},
    {"vin": VIN, "enable": False},
    {"vin": VIN, "enable": True, "push_type": 701},
    {"vin": VIN, "enable": False, "push_type": 1},
    {"vin": VIN, "enable": True, "push_type": 0},
    {"vin": VIN, "enable": True, "push_type": -5},
    {"vin": "", "enable": False, "push_type": 99999},
]


@group.recorded
def recorded():
    fetch_replies = shape_replies(SAMPLE_SESSION, SWITCH_LISTS) + code_replies()
    yield from token_recorded(FETCH, FETCH_ARGS, fetch_replies, SAMPLE_SESSION)
    set_replies = shape_replies(SAMPLE_SESSION, ACKS) + code_replies(["8003", "6024"])
    yield from token_recorded(SET, SET_ARGS, set_replies, SAMPLE_SESSION)


def switches(rng):
    roll = rng.random()
    if roll < 0.6:
        pool = [{"type": rng.choice([701, 1, "701", None, 5]), "state": rng.choice([0, 1, "1", None, "--", 2])}]
        return [rng.choice(pool + [random_object(rng), random_json(rng)]) for _ in range(rng.choice([0, 1, 2, 3]))]
    if roll < 0.8:
        return rng.choice(SWITCH_LISTS)
    return random_json(rng)


def acks(rng):
    return rng.choice(ACKS) if rng.random() < 0.6 else random_object(rng)


def random_vin(rng) -> str:
    return rng.choice([VIN, VIN, "", random_text(rng, 17)])


def set_args(rng) -> dict:
    args = {"vin": random_vin(rng), "enable": rng.random() < 0.5}
    if rng.random() < 0.6:
        args["push_type"] = rng.choice([701, 1, 0, -1, rng.randint(-1000, 100000)])
    return args


@group.fuzz
def fuzz(rng):
    fetches = token_fuzz(rng, FETCH, lambda rng: {"vin": random_vin(rng)}, switches)
    sets = token_fuzz(rng, SET, set_args, acks)
    while True:
        yield next(fetches)
        yield next(sets)
