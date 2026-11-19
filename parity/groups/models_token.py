"""pybyd/models/token.py"""

from __future__ import annotations

from pybyd.models.token import AuthToken

from parity.framework import Group
from parity.groups.config import random_str_value
from parity.groups.pyjson import random_object

group = Group("models_token")


@group.oracle("models.token.AuthToken")
def auth_token(raw, context=None) -> dict:
    token = AuthToken.model_validate(raw, context=context)
    return {"dump": token.model_dump(mode="json"), "properties": {}}


GOOD = {"user_id": "1", "sign_token": " s ", "encry_token": "e", "raw": {"userId": 1, "extra": [1]}}


@group.recorded
def recorded():
    yield "models.token.AuthToken", {"raw": GOOD, "context": None}
    yield "models.token.AuthToken", {"raw": {**GOOD, "unknown": 1}, "context": None}
    yield "models.token.AuthToken", {"raw": {**GOOD, "user_id": 1}, "context": None}
    yield "models.token.AuthToken", {"raw": {**GOOD, "raw": []}, "context": None}
    yield "models.token.AuthToken", {"raw": {"user_id": "1"}, "context": None}
    yield "models.token.AuthToken", {"raw": [], "context": None}
    yield "models.token.AuthToken", {"raw": None, "context": None}


@group.fuzz
def fuzz(rng):
    while True:
        raw = dict(GOOD)
        for key in rng.sample(list(GOOD), k=rng.randint(0, 2)):
            if rng.random() < 0.3:
                del raw[key]
            else:
                raw[key] = random_object(rng) if key == "raw" and rng.random() < 0.7 else random_str_value(rng)
        yield "models.token.AuthToken", {"raw": raw, "context": None}
