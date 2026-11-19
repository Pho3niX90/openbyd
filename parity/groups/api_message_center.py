"""pybyd/_api/message_center.py: fetch_notifications compared by trace."""

from __future__ import annotations

from pybyd._api import message_center
from pybyd.models.message_center import VehicleMsgListResponse

from parity.api_support import flow_oracle, ok_reply, scenario
from parity.flow_cases import (
    code_replies,
    corpus,
    data_source,
    dumped,
    model_payloads,
    shape_replies,
    token_fuzz,
    token_recorded,
)
from parity.framework import Group
from parity.groups.config import SAMPLE_SESSION
from parity.models_support import ModelCases

group = Group("api_message_center", crate="client")

CALL = "api.message_center.fetch_notifications"
flow_oracle(group, CALL, dumped(message_center.fetch_notifications))

ITEMS = corpus("VehicleMsgItemInfo")
CASES = ModelCases(
    VehicleMsgListResponse,
    [{"messages": ITEMS}, {"messages": [{"pushId": 1}], "raw": {"keep": True}}, {"messages": []}],
)
#: Values for `category`: members, equal numbers, and values that are no member
#: (pyBYD raises ValueError before sending anything).
CATEGORIES = [1, 2, 1.0, 2.0, True, 0, 3, -1, False, "1", "VEHICLE", None, [], {}, 1.5, 99, 2**40]
ARG_SETS = [{}, {"category": 1}, {"category": 2}, {"category": 1.0}, {"category": True}]
LISTS = [
    corpus("VehicleMsgListResponse")[0],
    ITEMS,
    {"list": ITEMS},
    {"list": []},
    {"messageList": [{"messageId": 1}]},
    {"rows": [{"messageId": "7", "title": "t"}]},
    {"records": [{}]},
    [{"messageId": 1}],
    [],
    {"list": [5]},
    {"list": "bad"},
]


@group.recorded
def recorded():
    payloads = [*model_payloads(CASES), *LISTS]
    replies = shape_replies(SAMPLE_SESSION, payloads) + code_replies()
    yield from token_recorded(CALL, ARG_SETS, replies, SAMPLE_SESSION)
    for category in CATEGORIES:
        yield CALL, scenario(replies=[ok_reply(SAMPLE_SESSION, payloads[0])], category=category)
        yield CALL, scenario(replies=[], category=category)


def random_args(rng) -> dict:
    return {} if rng.random() < 0.3 else {"category": rng.choice(CATEGORIES)}


@group.fuzz
def fuzz(rng):
    yield from token_fuzz(rng, CALL, random_args, data_source(CASES))
