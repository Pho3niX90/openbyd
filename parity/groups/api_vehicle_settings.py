"""pybyd/_api/vehicle_settings.py: rename_vehicle compared by trace."""

from __future__ import annotations

from pybyd._api import vehicle_settings

from parity.api_support import VIN, flow_oracle
from parity.flow_cases import (
    code_replies,
    dumped,
    shape_replies,
    token_fuzz,
    token_recorded,
)
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_SESSION
from parity.groups.pyjson import random_object

group = Group("api_vehicle_settings", crate="client")

CALL = "api.vehicle_settings.rename_vehicle"
flow_oracle(group, CALL, dumped(vehicle_settings.rename_vehicle))

NAMES = ["My Seal", "", "Zoë 比亚迪", 'quote " and \\ slash', "  padded  ", "--", "x" * 300]
ARG_SETS = [{"vin": VIN, "name": name} for name in NAMES] + [{"vin": "", "name": "My Seal"}]
ACKS = [
    {"result": "success"},
    {"result": None},
    {"result": 5, "requestSerial": "S-1"},
    {"vin": "OTHER00000000001"},
    {"raw": "x", "result": "y"},
    {},
    {"autoAlias": "My Seal"},
]


@group.recorded
def recorded():
    replies = shape_replies(SAMPLE_SESSION, ACKS) + code_replies(["8003"])
    yield from token_recorded(CALL, ARG_SETS, replies, SAMPLE_SESSION)


def acks(rng):
    return rng.choice(ACKS) if rng.random() < 0.6 else random_object(rng)


def random_args(rng) -> dict:
    return {
        "vin": rng.choice([VIN, VIN, "", random_text(rng, 17)]),
        "name": rng.choice(NAMES + [random_text(rng, 20)]),
    }


@group.fuzz
def fuzz(rng):
    yield from token_fuzz(rng, CALL, random_args, acks)
