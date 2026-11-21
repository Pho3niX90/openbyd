"""pybyd/_api/gps.py: fetch_gps_endpoint compared by trace, and is_gps_info_ready."""

from __future__ import annotations

from pybyd._api import gps

from parity.api_support import (
    VIN,
    error_reply,
    flow_oracle,
    ok_reply,
    random_scenario_base,
    random_vehicle_script,
    raw_reply,
    scenario,
    transport_error,
    vehicle_scripts,
)
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_SESSION
from parity.groups.pyjson import random_json, random_object

group = Group("api_gps", crate="client")

flow_oracle(group, "api.gps.fetch_gps_endpoint", gps.fetch_gps_endpoint)


@group.oracle("api.gps.is_gps_info_ready", errors=(AttributeError,))
def is_gps_info_ready(gps_info) -> bool:
    return gps.is_gps_info_ready(gps_info)


ENDPOINTS = ["/control/getGpsInfo", "/control/getGpsInfoResult"]
GPS = {"latitude": 52.0, "longitude": 4.0, "direction": 90, "speed": 0.0, "gpsTimeStamp": 1700000000}
SERIALS = [None, "", "SERIAL-1"]
READY_SAMPLES = [
    {},
    {"requestSerial": "S"},
    {"requestSerial": None},
    GPS,
    {**GPS, "requestSerial": "S"},
    {"a": 1},
    None,
    [],
    "",
    0,
    0.0,
    False,
    [1],
    "x",
    1,
    True,
]


@group.recorded
def recorded():
    for sample in READY_SAMPLES:
        yield "api.gps.is_gps_info_ready", {"gps_info": sample}
    data_replies = [
        ok_reply(SAMPLE_SESSION, GPS),
        ok_reply(SAMPLE_SESSION, {**GPS, "requestSerial": "SERIAL-2"}),
        ok_reply(SAMPLE_SESSION, {"requestSerial": "SERIAL-3"}),
        ok_reply(SAMPLE_SESSION, {"requestSerial": 7}),
        ok_reply(SAMPLE_SESSION, {"requestSerial": None}),
        ok_reply(SAMPLE_SESSION, {}),
        ok_reply(SAMPLE_SESSION, [GPS]),
        ok_reply(SAMPLE_SESSION, "text"),
        ok_reply(SAMPLE_SESSION, None),
        raw_reply({"code": "0"}),
        raw_reply([]),
        transport_error(502),
        *(error_reply(code) for code in ["1001", "1002", "1011", "3001", "3003", "1005", "1010", "1008", "6002", "6051", "8003", "9999"]),
    ]
    for endpoint in ENDPOINTS:
        for serial in SERIALS:
            for reply in data_replies:
                yield "api.gps.fetch_gps_endpoint", scenario(
                    replies=[reply], endpoint=endpoint, vin=VIN, request_serial=serial
                )
    for script in vehicle_scripts(ENDPOINTS[0], ok_reply(SAMPLE_SESSION, {**GPS, "requestSerial": "S-9"})):
        yield "api.gps.fetch_gps_endpoint", scenario(**script, endpoint=ENDPOINTS[0], vin=VIN, request_serial="SERIAL-1")
    yield "api.gps.fetch_gps_endpoint", scenario(
        guest_vins={"": False},
        replies=[error_reply("1001"), ok_reply(SAMPLE_SESSION, GPS)],
        endpoint=ENDPOINTS[1],
        vin="",
        request_serial=None,
    )


def gps_data(rng):
    roll = rng.random()
    if roll < 0.4:
        data = dict(GPS)
        if rng.random() < 0.5:
            data["requestSerial"] = rng.choice(["S-" + random_text(rng, 5), 5, None, ""])
        return data
    if roll < 0.7:
        return rng.choice([{}, {"requestSerial": random_text(rng, 6)}, random_object(rng)])
    return random_json(rng)


@group.fuzz
def fuzz(rng):
    while True:
        sample = rng.choice(READY_SAMPLES + [random_object(rng), random_json(rng)])
        yield "api.gps.is_gps_info_ready", {"gps_info": sample}
        base = random_scenario_base(rng)
        endpoint = rng.choice(ENDPOINTS + [random_text(rng, 12)])
        script = random_vehicle_script(rng, base["session"], endpoint, gps_data)
        yield "api.gps.fetch_gps_endpoint", {
            **base,
            "guest_vins": script["guest_vins"],
            "user_type_settled": [list(pair) for pair in script["user_type_settled"]],
            "replies": script["replies"],
            "args": {
                "endpoint": endpoint,
                "vin": script["vin"],
                "request_serial": rng.choice(SERIALS + [random_text(rng, 8)]),
            },
        }
