"""pybyd/_crypto/hashing.py"""

from __future__ import annotations

from pybyd._crypto import hashing

from parity.framework import Group, random_text

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


_SAMPLES = ["", "a", "password", "Tr0ub4dor&3", "name+byd@example.com", "Zoë’s Seal 比亚迪 🚗", "a=1&b=2&password=X"]


@group.recorded
def recorded():
    for text in _SAMPLES:
        yield "hashing.md5_hex", {"value": text}
        yield "hashing.pwd_login_key", {"password": text}
        yield "hashing.sha1_mixed", {"value": text}
    # sha1_mixed drops zeros on even positions, so cover many digests.
    for number in range(60):
        yield "hashing.sha1_mixed", {"value": f"sample-{number}"}


@group.fuzz
def fuzz(rng):
    while True:
        text = random_text(rng, 80)
        yield "hashing.md5_hex", {"value": text}
        yield "hashing.pwd_login_key", {"password": text}
        yield "hashing.sha1_mixed", {"value": text}
