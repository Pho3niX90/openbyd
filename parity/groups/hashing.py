"""pybyd/_crypto/hashing.py"""

from __future__ import annotations

from pybyd._crypto import hashing

from parity.framework import Group, random_text
from parity.groups.pyjson import random_object

group = Group("hashing")


@group.oracle("hashing.md5_hex")
def md5_hex(value: str) -> str:
    return hashing.md5_hex(value)


@group.oracle("hashing.pwd_login_key")
def pwd_login_key(password: str) -> str:
    return hashing.pwd_login_key(password)


@group.oracle("hashing.sha1_mixed")
def sha1_mixed(value: str) -> str:
    return hashing.sha1_mixed(value)


@group.oracle("hashing.compute_checkcode")
def compute_checkcode(payload) -> str:
    return hashing.compute_checkcode(payload)


_SAMPLES = ["", "a", "password", "Tr0ub4dor&3", "name+byd@example.com", "Zoë’s Seal 比亚迪 🚗", "a=1&b=2&password=X"]

_PAYLOADS = [
    {},
    {"a": "1", "b": 2, "c": 1.0, "d": None, "e": True},
    {"name": 'My "Seal" 🚗', "path": "a/b" + chr(92) + "c", "ctl": chr(0) + chr(31) + chr(127), "big": 2**64 - 1},
    {"f": 1e16, "g": 1e-5, "h": -0.0, "i": [1, {"j": "k"}]},
    {"z": "1", "a": "2"},
    [1, "x"],
]


@group.recorded
def recorded():
    for text in _SAMPLES:
        yield "hashing.md5_hex", {"value": text}
        yield "hashing.pwd_login_key", {"password": text}
        yield "hashing.sha1_mixed", {"value": text}
    # sha1_mixed drops zeros on even positions, so cover many digests.
    for number in range(60):
        yield "hashing.sha1_mixed", {"value": f"sample-{number}"}
    for payload in _PAYLOADS:
        yield "hashing.compute_checkcode", {"payload": payload}


@group.fuzz
def fuzz(rng):
    while True:
        text = random_text(rng, 80)
        yield "hashing.md5_hex", {"value": text}
        yield "hashing.pwd_login_key", {"password": text}
        yield "hashing.sha1_mixed", {"value": text}
        yield "hashing.compute_checkcode", {"payload": random_object(rng)}
