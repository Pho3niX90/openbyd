"""pybyd/models/message_center.py"""

from __future__ import annotations

import random

from pybyd.models.message_center import NotificationCategory, VehicleMsgItemInfo, VehicleMsgListResponse

from parity.framework import Group
from parity.models_support import (
    BOOL_AWKWARD,
    FLOAT_AWKWARD,
    INT_AWKWARD,
    SENTINELS,
    ModelCases,
    random_json,
    random_numeric_text,
    register_model,
)

group = Group("models_message_center")

_ITEMS = [
    {
        "content": "2026-10-02 20:40,ATTO 3 El coche comience a cargar.",
        "jumpUrl": "",
        "messageGroup": 1,
        "pushId": 211902416,
        "pushTime": 1790966443550,
        "title": "Empezar carga",
        "titleId": 1,
    },
    {
        "messageId": "102",
        "messageTitle": "Door open",
        "content": "--",
        "pushTime": "1790966443",
        "messageGroup": "",
    },
    {"pushId": "x1", "messageContent": "Zoë 比亚迪", "messageGroup": 2},
    {"pushId": "٠٠٧", "title": "Arabic-Indic digits"},
]

# --- NotificationCategory ----------------------------------------------------


@group.oracle("models.message_center.NotificationCategory")
def notification_category(value):
    member = NotificationCategory(value)
    return {"name": member.name, "value": int(member.value)}


_CATEGORY_VALUES = [
    1, 2, 0, 3, -1, 1.0, 2.0, 1.5, True, False, None, "1", "VEHICLE", [], {}, [1], 2**63, 2**64 - 1, -(2**63), 1e300,
    1.0000000000000001, 0.0, -0.0, "", " 1 ",
]


@group.recorded
def recorded_categories():
    for value in _CATEGORY_VALUES:
        yield "models.message_center.NotificationCategory", {"value": value}


@group.fuzz
def fuzz_categories(rng: random.Random):
    while True:
        value = rng.choice([
            lambda: rng.choice(_CATEGORY_VALUES),
            lambda: rng.randint(-5, 5),
            lambda: float(rng.randint(-5, 5)),
            lambda: random_numeric_text(rng),
            lambda: rng.choice(SENTINELS),
            lambda: random_json(rng),
            lambda: rng.choice(INT_AWKWARD + FLOAT_AWKWARD + BOOL_AWKWARD),
        ])()
        yield "models.message_center.NotificationCategory", {"value": value}


# --- Models ------------------------------------------------------------------

register_model(
    group,
    "models.message_center.VehicleMsgItemInfo",
    VehicleMsgItemInfo,
    samples=[dict(item) for item in _ITEMS],
)

register_model(
    group,
    "models.message_center.VehicleMsgListResponse",
    VehicleMsgListResponse,
    samples=[
        {"messages": [dict(item) for item in _ITEMS]},
        {"messages": [{"pushId": 1}], "raw": {"keep": True}},
        {"messages": []},
    ],
)

# The response accepts a bare list or a dict that wraps one under several keys.
_WRAPPED = [
    [dict(item) for item in _ITEMS],
    [],
    [{"pushId": 1}, {"title": "no id"}],
    [5, None, "x"],
    {"list": [dict(_ITEMS[0])]},
    {"messageList": [dict(_ITEMS[1])], "other": 1},
    {"rows": [dict(_ITEMS[2])]},
    {"records": [dict(_ITEMS[3])]},
    {"list": "notalist", "rows": [dict(_ITEMS[0])]},
    {"list": [], "rows": [dict(_ITEMS[0])]},
    {"list": [dict(_ITEMS[0])], "records": [dict(_ITEMS[1])]},
    {"messages": [dict(_ITEMS[0])], "list": [dict(_ITEMS[1])]},
    {"messages": None, "list": [dict(_ITEMS[1])]},
    {"messages": "--", "rows": [dict(_ITEMS[1])]},
    {"messages": "x"},
    {"messages": [dict(_ITEMS[0])], "raw": {"a": 1}},
    {"list": [dict(_ITEMS[0])], "raw": "x"},
    {"list": {"a": 1}},
    {"data": {"list": [dict(_ITEMS[0])]}},
]


@group.recorded
def recorded_responses():
    for raw in _WRAPPED:
        yield "models.message_center.VehicleMsgListResponse", {"raw": raw, "context": None}


@group.fuzz
def fuzz_responses(rng: random.Random):
    item_cases = ModelCases(VehicleMsgItemInfo, [dict(item) for item in _ITEMS])
    while True:
        items = [item_cases.fuzz(rng, 1) for _ in range(rng.randint(0, 3))]
        key = rng.choice(["list", "messageList", "rows", "records", "messages", "other"])
        if rng.random() < 0.3:
            raw = items
        else:
            raw = {key: items if rng.random() < 0.9 else random_json(rng), "extra": 1}
            if rng.random() < 0.1:
                raw["raw"] = rng.choice([{"a": 1}, "", None, 5])
        yield "models.message_center.VehicleMsgListResponse", {"raw": raw, "context": None}
