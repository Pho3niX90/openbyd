"""pybyd/_api/latest_config.py: fetch_latest_config compared by trace."""

from __future__ import annotations

from pybyd._api import latest_config
from pybyd.models.latest_config import VehicleLatestConfig

from parity.api_support import VIN, flow_oracle
from parity.flow_cases import (
    code_replies,
    corpus,
    dumped,
    model_cases,
    model_payloads,
    shape_replies,
    token_fuzz,
    token_recorded,
)
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_SESSION
from parity.groups.pyjson import random_json

group = Group("api_latest_config", crate="client")

CALL = "api.latest_config.fetch_latest_config"
flow_oracle(group, CALL, dumped(latest_config.fetch_latest_config))

CASES = model_cases(VehicleLatestConfig, "VehicleLatestConfig")
VIN2 = "TESTVIN0000000002"
VIN_LISTS = [
    [VIN],
    [],
    [VIN, VIN2],
    ["", VIN],
    ["Zoë 比亚迪", 'a"b\\c\n\t', " \x7f\x01"],
    [VIN] * 3,
]
ARG_SETS = [{"vin_list": vins} for vins in VIN_LISTS]
CONFIGS = corpus("VehicleLatestConfig")


def by_vin(*configs, vins=(VIN, VIN2, "OTHER00000000001")):
    return dict(zip(vins, configs, strict=False))


@group.recorded
def recorded():
    payloads = [
        by_vin(CONFIGS[0]),
        by_vin(*CONFIGS),
        by_vin(CONFIGS[0], {}),
        by_vin({}, {}, {}),
        {},
        # entries that are not objects are skipped
        {VIN: [], VIN2: 5, "X": None, "Y": "text", "Z": CONFIGS[0]},
        {"": CONFIGS[0]},
        {VIN: {"cfFixedList": "bad"}},
        {VIN: {"cfFixedList": [{"functionNo": 1001}], "extra": 1}},
        # a reply in the shape of a single config, not keyed by vin
        CONFIGS[0],
        *(by_vin(payload) for payload in model_payloads(CASES)),
    ]
    replies = shape_replies(SAMPLE_SESSION, payloads) + code_replies()
    yield from token_recorded(CALL, ARG_SETS, replies, SAMPLE_SESSION)


def configs(rng):
    roll = rng.random()
    if roll < 0.7:
        vins = [VIN, VIN2, "", random_text(rng, 17)]
        return {rng.choice(vins): CASES.fuzz(rng) for _ in range(rng.choice([0, 1, 1, 2, 3]))}
    if roll < 0.85:
        return {random_text(rng, 8): random_json(rng) for _ in range(rng.choice([1, 2]))}
    return random_json(rng)


def random_args(rng) -> dict:
    pool = [VIN, VIN2, "", random_text(rng, 17), "Zoë 比亚迪"]
    return {"vin_list": [rng.choice(pool) for _ in range(rng.choice([0, 1, 1, 2, 3]))]}


@group.fuzz
def fuzz(rng):
    yield from token_fuzz(rng, CALL, random_args, configs)
