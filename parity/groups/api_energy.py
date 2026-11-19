"""pybyd/_api/energy.py: fetch_energy_consumption compared by trace."""

from __future__ import annotations

from pybyd._api import energy
from pybyd.models.energy import EnergyConsumption
from pybyd.models.vehicle import EnergyType

from parity.api_support import VIN, flow_oracle, scenario
from parity.flow_cases import (
    code_replies,
    data_source,
    dumped,
    keep_representable,
    model_cases,
    model_payloads,
    shape_replies,
    token_fuzz,
    token_recorded,
)
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_SESSION

group = Group("api_energy", crate="client")

CALL = "api.energy.fetch_energy_consumption"


async def fetch_energy_consumption(*, power_type=None, **kwargs):
    """The pyBYD flow, with the JSON integer turned into an ``EnergyType``
    (left out, it keeps pyBYD's default)."""
    if power_type is not None:
        kwargs["power_type"] = EnergyType(power_type)
    return await energy.fetch_energy_consumption(**kwargs)


flow_oracle(group, CALL, dumped(fetch_energy_consumption))

CASES = model_cases(EnergyConsumption, "EnergyConsumption")
POWER_TYPES = [None, 0, 1, 2, -1]
MODEL_NAMES = [None, "", "BYD SHARK", "Zoë 比亚迪 \"7\"", "SEALION 7"]
ARG_SETS = [
    {"vin": VIN},
    {"vin": VIN, "power_type": 0},
    {"vin": VIN, "power_type": 1},
    {"vin": VIN, "power_type": 2},
    {"vin": VIN, "power_type": -1},
    {"vin": VIN, "auto_model_name": "BYD SHARK"},
    {"vin": VIN, "auto_model_name": ""},
    {"vin": VIN, "auto_model_name": None},
    {"vin": VIN, "power_type": 2, "auto_model_name": "Zoë 比亚迪 \"7\""},
    {"vin": "", "power_type": 1, "auto_model_name": "SEALION 7"},
]


@group.recorded
def recorded():
    replies = shape_replies(SAMPLE_SESSION, model_payloads(CASES)) + code_replies()
    yield from token_recorded(CALL, ARG_SETS, replies, SAMPLE_SESSION)


def random_args(rng) -> dict:
    args = {"vin": rng.choice([VIN, VIN, "", random_text(rng, 17)])}
    if rng.random() < 0.7:
        args["power_type"] = rng.choice(POWER_TYPES)
    if rng.random() < 0.7:
        args["auto_model_name"] = rng.choice(MODEL_NAMES + [random_text(rng, 12)])
    return args


@group.fuzz
def fuzz(rng):
    yield from token_fuzz(rng, CALL, random_args, data_source(CASES))


# Floats in the reply become Python ints of any size in the model's integer
# fields; leave out the cases whose answer openbyd's i64 cannot hold.
keep_representable(group)
