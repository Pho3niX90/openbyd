"""pybyd/_api/_common.py: post_token_json and post_vehicle_json, compared by trace.

The pure helpers of the same file are in the ``api_common`` group.
"""

from __future__ import annotations

from pybyd._api import _common

from parity.api_support import (
    CODES,
    ODD_CODES,
    VIN,
    error_reply,
    flow_oracle,
    ok_reply,
    random_reply,
    random_scenario_base,
    random_vehicle_script,
    raw_reply,
    scenario,
    sealed_reply,
    transport_error,
    vehicle_scripts,
)
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_SESSION

group = Group("api_common_flows", crate="client")

flow_oracle(group, "api.common.post_token_json", _common.post_token_json)
flow_oracle(group, "api.common.post_vehicle_json", _common.post_vehicle_json)

ENDPOINT = "/vehicleInfo/vehicle/vehicleRealTimeRequest"
INNER = {
    "deviceType": "0",
    "imeiMD5": "00000000000000000000000000000000",
    "networkType": "wifi",
    "random": "0123456789ABCDEF0123456789ABCDEF",
    "timeStamp": "1700000000000",
    "version": "1",
    "vin": VIN,
}
EXTRA_MAPS = [
    None,
    [],
    [[["6051"], "BydDataUnavailableError"]],
    [[["1001", "5005"], "BydControlPasswordError"], [["6024"], "BydRateLimitError"]],
    [[["8003"], "BydRemoteControlError"], [["8003"], "BydServiceBusyError"]],
]
NOT_SUPPORTED = [None, [], ["1001"], ["1001", "8003"]]
DATA = {"requestSerial": "S-1", "onlineState": 1, "speed": 0.0, "name": "Zoë 比亚迪"}


def token_args(**overrides) -> dict:
    args = {
        "endpoint": ENDPOINT,
        "inner": INNER,
        "now_ms": None,
        "vin": VIN,
        "not_supported_codes": None,
        "extra_code_map": None,
        "user_type": None,
    }
    return {**args, **overrides}


def vehicle_args(**overrides) -> dict:
    args = {
        "endpoint": ENDPOINT,
        "inner": INNER,
        "vin": VIN,
        "now_ms": None,
        "not_supported_codes": None,
        "extra_code_map": None,
    }
    return {**args, **overrides}


@group.recorded
def recorded():
    ok = ok_reply(SAMPLE_SESSION, DATA)
    # post_token_json: every reply shape and code mapping, with and without userType.
    replies = [
        ok,
        ok_reply(SAMPLE_SESSION, [1, "two"]),
        ok_reply(SAMPLE_SESSION, {}),
        ok_reply(SAMPLE_SESSION, DATA, code=0),
        raw_reply({"code": "0"}),
        raw_reply({"code": "0", "respondData": ""}),
        raw_reply({"code": "0", "respondData": "ZZ"}),
        sealed_reply(SAMPLE_SESSION, "{not json"),
        sealed_reply(SAMPLE_SESSION, "   "),
        raw_reply({}),
        raw_reply([]),
        raw_reply("text"),
        raw_reply(None),
        transport_error(500),
        transport_error(None),
        *(error_reply(code, "msg ü") for code in CODES + ODD_CODES),
    ]
    for reply in replies:
        yield "api.common.post_token_json", scenario(replies=[reply], args=token_args())
    yield "api.common.post_token_json", scenario(replies=[], args=token_args())
    yield "api.common.post_token_json", scenario(replies=[ok, ok], args=token_args(now_ms=1234, user_type="1"))
    yield "api.common.post_token_json", scenario(replies=[ok], args=token_args(vin=None, user_type="2", inner={}))
    for not_supported in NOT_SUPPORTED:
        for extra in EXTRA_MAPS:
            for code in ["1001", "1005", "5005", "6051", "8003", "6024", "1008", "6002"]:
                yield "api.common.post_token_json", scenario(
                    replies=[error_reply(code)],
                    args=token_args(not_supported_codes=not_supported, extra_code_map=extra, vin=None),
                )
    # post_vehicle_json: every branch of the guest retry.
    for script in vehicle_scripts(ENDPOINT, ok):
        yield "api.common.post_vehicle_json", scenario(**script, args=vehicle_args())
    for not_supported in NOT_SUPPORTED:
        for extra in EXTRA_MAPS:
            for first, second in [("1001", "8003"), ("8003", "1001"), ("1001", "5005"), ("1001", "6051")]:
                yield "api.common.post_vehicle_json", scenario(
                    guest_vins={VIN: first == "8003"},
                    replies=[error_reply(first), error_reply(second)],
                    args=vehicle_args(not_supported_codes=not_supported, extra_code_map=extra),
                )
    yield "api.common.post_vehicle_json", scenario(
        guest_vins={VIN: False}, replies=[error_reply("1001"), ok], args=vehicle_args(now_ms=42)
    )
    yield "api.common.post_vehicle_json", scenario(
        guest_vins={"": True}, replies=[error_reply("8003"), ok], args=vehicle_args(vin="")
    )


def random_inner(rng) -> dict:
    inner = dict(INNER)
    for key in rng.sample(list(INNER), k=rng.randint(0, 3)):
        del inner[key]
    if rng.random() < 0.3:
        inner[random_text(rng, 8)] = random_text(rng, 12)
    return inner


@group.fuzz
def fuzz(rng):
    while True:
        base = random_scenario_base(rng)
        session = base["session"]
        common = {
            "endpoint": rng.choice([ENDPOINT, "/control/getGpsInfo", random_text(rng, 15)]),
            "inner": random_inner(rng),
            "now_ms": rng.choice([None, None, rng.randint(0, 4_000_000_000_000)]),
            "not_supported_codes": rng.choice(NOT_SUPPORTED),
            "extra_code_map": rng.choice(EXTRA_MAPS),
        }
        replies = [random_reply(rng, session) for _ in range(rng.choice([0, 1, 1, 1, 2]))]
        yield "api.common.post_token_json", {
            **base,
            "guest_vins": {},
            "user_type_settled": [],
            "replies": replies,
            "args": {
                **common,
                "vin": rng.choice([None, VIN, ""]),
                "user_type": rng.choice([None, None, "1", "", random_text(rng, 3)]),
            },
        }
        script = random_vehicle_script(rng, session, common["endpoint"])
        yield "api.common.post_vehicle_json", {
            **base,
            "guest_vins": script["guest_vins"],
            "user_type_settled": [list(pair) for pair in script["user_type_settled"]],
            "replies": script["replies"],
            "args": {**common, "vin": script["vin"]},
        }
