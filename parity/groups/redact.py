"""pybyd/_redact.py"""

from __future__ import annotations

from pybyd._redact import redact_for_log

from parity.framework import Group, random_text
from parity.groups.pyjson import random_json

group = Group("redact")


@group.oracle("redact.redact_for_log")
def redact(value, **limit):
    return redact_for_log(value, **limit)


KEYS = [
    "password", "Password", "PASSWORD", "signKey", "signToken", "encryToken", "refreshToken", "accessToken", "token",
    "Authorization", "cookie", "encryData", "respondData", "request", "response", "TOKEN", "tokens", "data",
]


def random_tree(rng, depth: int = 0):
    if depth > 3 or rng.random() < 0.4:
        return random_json(rng, 3) if rng.random() < 0.7 else "x" * rng.randint(0, 600)
    if rng.random() < 0.3:
        return [random_tree(rng, depth + 1) for _ in range(rng.randint(0, 3))]
    return {rng.choice(KEYS + [random_text(rng, 8)]): random_tree(rng, depth + 1) for _ in range(rng.randint(0, 5))}


def nested(levels: int):
    value = {"password": "x", "leaf": 1}
    for index in range(levels):
        value = {"level": value} if index % 2 else [value]
    return value


@group.recorded
def recorded():
    sample = {
        "password": "x",
        "encryData": "AB",
        "respondData": {"token": "y", "name": "Seal"},
        "request": ["a"],
        "list": [1, "abc", None, True, 1.5],
    }
    yield "redact.redact_for_log", {"value": sample}
    for limit in [0, 1, 3, -1, -5, 512]:
        yield "redact.redact_for_log", {"value": ["abcdef", "Zoë 🚗 比亚迪", ""], "max_string": limit}
    yield "redact.redact_for_log", {"value": "y" * 513}
    for levels in [18, 19, 20, 21, 22, 30]:
        yield "redact.redact_for_log", {"value": nested(levels)}


@group.fuzz
def fuzz(rng):
    while True:
        args = {"value": random_tree(rng)}
        if rng.random() < 0.5:
            args["max_string"] = rng.choice([0, 1, 5, -3, 20, 512, rng.randint(-50, 50)])
        yield "redact.redact_for_log", args
        if rng.random() < 0.1:
            yield "redact.redact_for_log", {"value": nested(rng.randint(15, 25))}
