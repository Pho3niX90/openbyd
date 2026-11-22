"""pybyd/_api/ota.py: fetch_ota_version and sync_auto_upgrade compared by trace,
and normalize_ota_upgrade_time."""

from __future__ import annotations

from pybyd._api import ota

from parity.api_support import VIN, error_reply, flow_oracle, ok_reply, scenario
from parity.flow_cases import code_replies, dumped, shape_replies, token_fuzz, token_recorded
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION
from parity.groups.models_ota import CAPTURED_IDLE, INFO_SAMPLES, info_cases

group = Group("api_ota", crate="client")

FETCH = "api.ota.fetch_ota_version"
SYNC = "api.ota.sync_auto_upgrade"
flow_oracle(group, FETCH, dumped(ota.fetch_ota_version))


@group.oracle("api.ota.normalize_ota_upgrade_time", errors=(ValueError, AttributeError))
def normalize_ota_upgrade_time(value) -> str:
    return ota.normalize_ota_upgrade_time(value)


#: pyBYD's own test values, the edges of the pattern and the odd ones.
TIMES = [
    "2:00", "02:00", "00:05", "23:59", "0:00", "9:59", "", "24:00", "12:60", "2am", "1230", "1:2", "123:00", ":00",
    "1:000", " 3:07 ", "\t3:07\n", "3:07\n", "٣:٠٠", "²:00", "3 :00", "3: 00", "+3:00",
    "-1:00", "03:5", "1:2:3", "１２:００", "　 2:30　", None, 5, ["2:00"],
]
PIN = "0123456789ABCDEF0123456789ABCDEF"
SYNC_ARGS = [
    {"vin": VIN, "enabled": True, "auto_upgrade_time": "03:00", "command_pwd": PIN},
    {"vin": VIN, "enabled": False, "auto_upgrade_time": "2:00", "command_pwd": ""},
    {"vin": "", "enabled": True, "auto_upgrade_time": "23:59", "command_pwd": PIN},
    {"vin": VIN, "enabled": True, "auto_upgrade_time": "24:00", "command_pwd": PIN},
    {"vin": VIN, "enabled": True, "auto_upgrade_time": None, "command_pwd": PIN},
]


async def _sync(config, session, transport, *, vin, enabled, auto_upgrade_time=None, command_pwd):
    time = "2:00" if auto_upgrade_time is None else auto_upgrade_time
    ack = await ota.sync_auto_upgrade(
        config, session, transport, vin, enabled=enabled, auto_upgrade_time=time, command_pwd=command_pwd
    )
    return ack.model_dump(mode="json")


flow_oracle(group, SYNC, _sync)


@group.recorded
def recorded():
    for value in TIMES:
        yield "api.ota.normalize_ota_upgrade_time", {"value": value}
    payloads = [*INFO_SAMPLES, CAPTURED_IDLE, {"raw": "x", "vin": "OTHER"}, {"vin": 5}]
    replies = shape_replies(SAMPLE_SESSION, payloads) + code_replies()
    arg_sets = [{"vin": VIN}, {"vin": ""}, {"vin": "TESTVIN0000000002"}]
    yield from token_recorded(FETCH, arg_sets, replies, SAMPLE_SESSION)
    # The language goes with the request.
    for language in ["es", "", "zh-CN"]:
        yield FETCH, scenario(config={**SAMPLE_CONFIG, "language": language},
                              replies=[ok_reply(SAMPLE_SESSION, CAPTURED_IDLE)], vin=VIN)
    acks = shape_replies(SAMPLE_SESSION, [{}, {"result": "ok"}, {"result": 5, "requestSerial": "S"}]) + code_replies()
    for reply in acks:
        yield SYNC, scenario(replies=[reply], **SYNC_ARGS[0])
    for args in SYNC_ARGS[1:]:
        for reply in [ok_reply(SAMPLE_SESSION, ""), error_reply("1001"), error_reply("5005")]:
            yield SYNC, scenario(replies=[reply], **args)


@group.fuzz
def fuzz(rng):
    fetch = token_fuzz(rng, FETCH, lambda rng: {"vin": rng.choice([VIN, VIN, "", random_text(rng, 17)])},
                       lambda rng: info_cases.fuzz(rng))
    sync = token_fuzz(rng, SYNC, lambda rng: {
        "vin": rng.choice([VIN, "", random_text(rng, 17)]),
        "enabled": rng.random() < 0.5,
        "auto_upgrade_time": rng.choice([*TIMES[:20], f"{rng.randint(0, 30)}:{rng.randint(0, 70):02d}"]),
        "command_pwd": rng.choice([PIN, "", random_text(rng, 8)]),
    })
    while True:
        value = rng.choice([rng.choice(TIMES), f"{rng.randint(-1, 99)}:{rng.randint(0, 99):0{rng.randint(1, 3)}d}",
                            random_text(rng, 6)])
        yield "api.ota.normalize_ota_upgrade_time", {"value": value}
        yield next(fetch)
        yield next(sync)
