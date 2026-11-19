"""pybyd/_crypto/signing.py"""

from __future__ import annotations

from pybyd._crypto import signing

from parity.framework import Group, random_text
from parity.groups.pyjson import random_float, random_int

group = Group("signing")


@group.oracle("signing.build_sign_string")
def build_sign_string(fields: dict, password: str) -> str:
    return signing.build_sign_string(fields, password)


def random_fields(rng) -> dict:
    fields = {}
    for _ in range(rng.randint(0, 12)):
        value = rng.choice(
            [
                lambda: random_text(rng),
                lambda: random_text(rng),
                lambda: None,
                lambda: rng.random() < 0.5,
                lambda: random_int(rng),
                lambda: random_float(rng),
            ]
        )()
        fields[random_text(rng, 12)] = value
    return fields


@group.recorded
def recorded():
    yield "signing.build_sign_string", {"fields": {}, "password": "P"}
    yield "signing.build_sign_string", {"fields": {"b": "2", "a": "1", "B": "3", "é": "4", "_": "5"}, "password": ""}
    yield "signing.build_sign_string", {
        "fields": {"x": None, "y": True, "z": 1, "w": 1.0, "v": 1e16, "u": ""},
        "password": "pa$$&=",
    }
    yield "signing.build_sign_string", {"fields": {"name": 'My "Seal" 🚗', "k": "a&b=c"}, "password": "Zoë 🚗"}


@group.fuzz
def fuzz(rng):
    while True:
        yield "signing.build_sign_string", {"fields": random_fields(rng), "password": random_text(rng)}
