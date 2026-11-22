"""pybyd/_api/login.py: refresh_session_token compared by trace.

The login request and reply are pure and in the ``envelope`` group.
"""

from __future__ import annotations

from pybyd._api import login

from parity.api_support import error_reply, flow_oracle, ok_reply, random_reply, random_scenario_base, scenario
from parity.flow_cases import code_replies, shape_replies
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_SESSION
from parity.groups.pyjson import random_json

group = Group("api_login", crate="client")

CALL = "envelope.refresh_session_token"


async def _refresh(config, session, transport):
    token = await login.refresh_session_token(config, session, transport)
    return token.model_dump(mode="json")


flow_oracle(group, CALL, _refresh)

REFRESHABLE = {**SAMPLE_SESSION, "refresh_token": "REFRESH-synthetic"}
TOKENS = {"signToken": "SIGN-2", "encryToken": "ENCRY-2", "refreshToken": "REFRESH-2", "userId": "7654321"}
REPLIES = [
    TOKENS,
    {"newTokens": TOKENS},
    {"newTokens": {"signToken": "S", "encryToken": "E"}},
    {"signToken": "S", "encryToken": "E", "userId": 0, "refreshToken": ""},
    {"signToken": "S", "encryToken": "E", "userId": 5, "refreshToken": 6},
    {"newTokens": "x", "signToken": "S", "encryToken": "E"},
    {"newTokens": {}, "signToken": "S", "encryToken": "E"},
    {"newTokens": {"signToken": "S"}},
    {"signToken": "", "encryToken": "E"},
    {"signToken": "S"},
    {"signToken": 1.5, "encryToken": True},
    {"signToken": [1], "encryToken": {"a": None}},
    {},
]


@group.recorded
def recorded():
    for session in [REFRESHABLE, SAMPLE_SESSION]:
        for reply in shape_replies(session, REPLIES) + code_replies(["1030", "1003"]):
            yield CALL, scenario(session=session, replies=[reply])
    yield CALL, scenario(session=REFRESHABLE, replies=[])
    yield CALL, scenario(session=REFRESHABLE, guest_vins={"TESTVIN0000000001": True},
                         replies=[ok_reply(REFRESHABLE, TOKENS), error_reply("1030")])


def random_tokens(rng):
    tokens = {key: rng.choice([value, "", None, random_text(rng, 8), random_json(rng)]) for key, value in TOKENS.items()
              if rng.random() < 0.85}
    return {"newTokens": tokens} if rng.random() < 0.4 else tokens


@group.fuzz
def fuzz(rng):
    while True:
        base = random_scenario_base(rng)
        if rng.random() < 0.7:
            base["session"] = {**base["session"], "refresh_token": rng.choice(["R", "", random_text(rng, 10)])}
        count = rng.choice([0, 1, 1, 1, 2])
        yield CALL, {
            **base,
            "guest_vins": {},
            "user_type_settled": [],
            "replies": [random_reply(rng, base["session"], random_tokens) for _ in range(count)],
            "args": {},
        }
