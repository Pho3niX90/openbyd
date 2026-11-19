"""pybyd/_mqtt.py: fetch_mqtt_bootstrap, compared by trace (the broker lookup and the credentials)."""

from __future__ import annotations

from pybyd import _mqtt

from parity.api_support import (
    CODES,
    ODD_CODES,
    error_reply,
    flow_oracle,
    ok_reply,
    random_reply,
    random_scenario_base,
    raw_reply,
    scenario,
    sealed_reply,
    transport_error,
)
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION
from parity.groups.mqtt import BROKERS
from parity.groups.pyjson import random_json

group = Group("api_mqtt", crate="client")


async def fetch_mqtt_bootstrap(config, session, transport) -> dict:
    """pyBYD's flow, with the model answered as its JSON dump."""
    bootstrap = await _mqtt.fetch_mqtt_bootstrap(config, session, transport)
    return bootstrap.model_dump(mode="json")


flow_oracle(group, "mqtt.fetch_mqtt_bootstrap", fetch_mqtt_bootstrap)

#: Values of ``emqBorker``/``emqBroker``: addresses, falsy values that fall
#: through to the other key, and non-strings.
BROKER_VALUES = [
    "emq.example.com:8883", " ssl://emq.example.com:1883/mqtt \n", "", "   ", None, 0, False, [], {},
    5, True, ["h"], {"h": 1}, "h:\u00b2", "h:99999", "\u3000h\u3000",
]


def broker_replies(session: dict) -> list[dict]:
    replies = []
    for value in BROKER_VALUES:
        replies.append(ok_reply(session, {"emqBorker": value}))
        replies.append(ok_reply(session, {"emqBroker": value}))
        replies.append(ok_reply(session, {"emqBorker": value, "emqBroker": "fallback.example.com"}))
        replies.append(ok_reply(session, {"emqBorker": "first.example.com:1", "emqBroker": value}))
    return replies


@group.recorded
def recorded():
    replies = broker_replies(SAMPLE_SESSION) + [
        ok_reply(SAMPLE_SESSION, {}),
        ok_reply(SAMPLE_SESSION, {"other": "x"}),
        ok_reply(SAMPLE_SESSION, ["emq.example.com"]),
        ok_reply(SAMPLE_SESSION, "emq.example.com"),
        ok_reply(SAMPLE_SESSION, None),
        sealed_reply(SAMPLE_SESSION, "not json"),
        raw_reply({"code": "0"}),
        raw_reply([]),
        raw_reply("text"),
        transport_error(502),
        transport_error(None),
        *(error_reply(code) for code in CODES[1:] + ODD_CODES),
    ]
    for reply in replies:
        yield "mqtt.fetch_mqtt_bootstrap", scenario(replies=[reply])
    for broker in BROKERS:
        yield "mqtt.fetch_mqtt_bootstrap", scenario(replies=[ok_reply(SAMPLE_SESSION, {"emqBorker": broker})])
    yield "mqtt.fetch_mqtt_bootstrap", scenario(replies=[])
    good = ok_reply(SAMPLE_SESSION, {"emqBorker": "emq.example.com"})
    yield "mqtt.fetch_mqtt_bootstrap", scenario(replies=[good, good])
    for now_ms in [0, 999, 1000, 1_700_000_000_999, 4_000_000_000_000]:
        yield "mqtt.fetch_mqtt_bootstrap", scenario(now_ms=now_ms, replies=[good])
    for imei_md5 in ["0" * 32, "abcdef0123456789abcdef0123456789"]:
        config = {**SAMPLE_CONFIG, "device": {"imei_md5": imei_md5}}
        yield "mqtt.fetch_mqtt_bootstrap", scenario(config=config, replies=[good])
    session = {**SAMPLE_SESSION, "user_id": "\u00e9 7", "sign_token": ""}
    yield "mqtt.fetch_mqtt_bootstrap", scenario(
        session=session, replies=[ok_reply(session, {"emqBroker": "emq.example.com:1"})]
    )


def broker_data(rng):
    roll = rng.random()
    if roll < 0.6:
        key = rng.choice(["emqBorker", "emqBroker"])
        data = {key: rng.choice(BROKER_VALUES + BROKERS + [random_text(rng, 20)])}
        if rng.random() < 0.3:
            other = "emqBroker" if key == "emqBorker" else "emqBorker"
            data[other] = rng.choice(BROKER_VALUES + [random_json(rng)])
        if rng.random() < 0.2:
            data[random_text(rng, 6)] = random_json(rng)
        return data
    return random_json(rng)


@group.fuzz
def fuzz(rng):
    while True:
        base = random_scenario_base(rng)
        replies = [random_reply(rng, base["session"], broker_data) for _ in range(rng.choice([0, 1, 1, 1, 2]))]
        yield "mqtt.fetch_mqtt_bootstrap", {**base, "replies": replies, "args": {}}
