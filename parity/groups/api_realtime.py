"""pybyd/_api/realtime.py: fetch_realtime_endpoint compared by trace."""

from __future__ import annotations

from pybyd._api import realtime
from pybyd.models.vehicle import EnergyType

from parity.api_support import VIN, flow_oracle, ok_reply, scenario
from parity.flow_cases import (
    code_replies,
    corpus,
    dumped,
    shape_replies,
    vehicle_fuzz,
    vehicle_recorded,
)
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_SESSION
from parity.groups.pyjson import random_json, random_object

group = Group("api_realtime", crate="client")

CALL = "api.realtime.fetch_realtime_endpoint"
ENDPOINTS = ["/vehicleInfo/vehicle/vehicleRealTimeRequest", "/vehicleInfo/vehicle/vehicleRealTimeResult"]


async def fetch_realtime_endpoint(*, energy_type=None, **kwargs):
    """The pyBYD flow, with the JSON integer turned into an ``EnergyType``
    (left out, it keeps pyBYD's default)."""
    if energy_type is not None:
        kwargs["energy_type"] = EnergyType(energy_type)
    return await realtime.fetch_realtime_endpoint(**kwargs)


flow_oracle(group, CALL, dumped(fetch_realtime_endpoint))

REALTIME = corpus("VehicleRealtimeData")
#: What `requestSerial` may be in a reply: pyBYD takes any truthy value as the next serial.
SERIALS_IN_REPLY = ["S-2", "", None, 0, 0.0, 7, 1.5, True, False, [], ["a"], {}, {"a": 1}, "0", " ", "Zoë"]
REQUEST_SERIALS = [None, "", "S-1", "Zoë 比亚迪"]
ARG_SETS = [
    {"endpoint": ENDPOINTS[0], "vin": VIN},
    {"endpoint": ENDPOINTS[0], "vin": VIN, "request_serial": "S-1"},
    {"endpoint": ENDPOINTS[1], "vin": VIN, "request_serial": ""},
    {"endpoint": ENDPOINTS[1], "vin": VIN, "request_serial": None, "energy_type": 0},
    {"endpoint": ENDPOINTS[1], "vin": VIN, "request_serial": "S-1", "energy_type": 1},
    {"endpoint": ENDPOINTS[0], "vin": VIN, "energy_type": 2},
    {"endpoint": ENDPOINTS[0], "vin": VIN, "energy_type": -1, "request_serial": "S-1"},
    {"endpoint": ENDPOINTS[0], "vin": "", "request_serial": "Zoë 比亚迪", "energy_type": 2},
]


@group.recorded
def recorded():
    payloads = [*REALTIME, {"onlineState": 1, "vehicleTimeZone": "Europe/Amsterdam"}]
    payloads += [{"requestSerial": serial, "onlineState": 1} for serial in SERIALS_IN_REPLY]
    replies = shape_replies(SAMPLE_SESSION, payloads) + code_replies(["8003", "8002", "6051"])
    yield from vehicle_recorded(CALL, ENDPOINTS[0], ARG_SETS, replies, ok_reply(SAMPLE_SESSION, REALTIME[0]), SAMPLE_SESSION)
    for serial in REQUEST_SERIALS:
        for reply_serial in SERIALS_IN_REPLY[:6]:
            yield CALL, scenario(
                replies=[ok_reply(SAMPLE_SESSION, {"requestSerial": reply_serial})],
                endpoint=ENDPOINTS[1],
                vin=VIN,
                request_serial=serial,
            )


def realtime_data(rng):
    roll = rng.random()
    if roll < 0.5:
        data = dict(rng.choice(REALTIME))
    elif roll < 0.7:
        data = random_object(rng)
    else:
        return random_json(rng)
    if rng.random() < 0.6:
        data["requestSerial"] = rng.choice(SERIALS_IN_REPLY + [random_text(rng, 8)])
    return data


def random_args(rng) -> dict:
    args = {"endpoint": rng.choice(ENDPOINTS + [random_text(rng, 12)])}
    if rng.random() < 0.7:
        args["request_serial"] = rng.choice(REQUEST_SERIALS + [random_text(rng, 8)])
    if rng.random() < 0.7:
        args["energy_type"] = rng.choice([None, 0, 1, 2, -1])
    return args


@group.fuzz
def fuzz(rng):
    yield from vehicle_fuzz(rng, CALL, ENDPOINTS[0], random_args, realtime_data)
