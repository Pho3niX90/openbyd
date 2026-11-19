"""pybyd/_mqtt.py: the pure helpers (broker address, credentials, payloads)."""

from __future__ import annotations

import json

from pybyd import _mqtt
from pybyd._crypto.aes import aes_encrypt_hex
from pybyd.config import BydConfig
from pybyd.session import Session

from parity.framework import Group, random_hex, random_text
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION, random_config_kwargs, random_session_kwargs
from parity.groups.pyjson import random_json, random_object

group = Group("mqtt")


@group.oracle("mqtt.parse_broker")
def parse_broker(raw_broker: str):
    return list(_mqtt._parse_broker(raw_broker))


@group.oracle("mqtt.build_client_id")
def build_client_id(config: dict) -> str:
    return _mqtt._build_client_id(BydConfig(**config))


@group.oracle("mqtt.build_mqtt_password")
def build_mqtt_password(session: dict, now: float, client_id: str, ts_seconds: int) -> str:
    return _mqtt._build_mqtt_password(Session(**session), client_id, ts_seconds)


@group.oracle("mqtt.decode_mqtt_payload")
def decode_mqtt_payload(payload_hex: str, decrypt_key_hex: str):
    return list(_mqtt.decode_mqtt_payload(bytes.fromhex(payload_hex), decrypt_key_hex))


BROKERS = [
    "", "   ", "emq.example.com", "emq.example.com:1883", "ssl://emq.example.com:8884/mqtt", "tcp://h", "h:",
    ":1883", "h:0080", "h:12a", "[::1]:1883", "a:b:c", "a://b://c:5", "h/x:5", " \x1ch:1\x1f ", "h:\u0661",
    "h:\u00b2", "h:1\u2460", "h:\uff11\u0662", "h:00000000000000000000080", "wss://user:pw@h:443/path", "h:-1",
]
KEY = "000102030405060708090A0B0C0D0E0F"


def payload_cases(rng, key: str) -> list[str]:
    obj = json.dumps(random_object(rng), ensure_ascii=rng.random() < 0.5)
    sealed = aes_encrypt_hex(obj, key)
    spaced = " ".join(sealed[i : i + 7] for i in range(0, len(sealed), 7))
    return [
        sealed.encode().hex(),
        ("\t" + spaced + "\r\n").encode().hex(),
        sealed.lower().encode().hex(),
        aes_encrypt_hex(json.dumps(random_json(rng)), key).encode().hex(),
        aes_encrypt_hex("not json", key).encode().hex(),
        aes_encrypt_hex("", key).encode().hex(),
        (sealed[:10] + "\u00e9" + sealed[10:]).encode("utf-8").hex(),
        sealed[:-2].encode().hex(),
        rng.randbytes(rng.randint(0, 40)).hex(),
        b"\x1c".hex() + sealed.encode().hex(),
    ]


@group.recorded
def recorded():
    for broker in BROKERS:
        yield "mqtt.parse_broker", {"raw_broker": broker}
    yield "mqtt.build_client_id", {"config": SAMPLE_CONFIG}
    for imei_md5 in ["0" * 32, "000", "", " abc ", "\x1c0\x1c", "ß", "0" * 31 + "1"]:
        yield "mqtt.build_client_id", {"config": {**SAMPLE_CONFIG, "device": {"imei_md5": imei_md5}}}
    for ts in [0, 1700000000, -5]:
        yield "mqtt.build_mqtt_password", {"session": SAMPLE_SESSION, "now": 0.0, "client_id": "oversea_X", "ts_seconds": ts}
    import random

    rng = random.Random("mqtt-recorded")
    for payload in payload_cases(rng, KEY):
        yield "mqtt.decode_mqtt_payload", {"payload_hex": payload, "decrypt_key_hex": KEY}
    yield "mqtt.decode_mqtt_payload", {"payload_hex": payload_cases(rng, KEY)[0], "decrypt_key_hex": "00"}


@group.fuzz
def fuzz(rng):
    while True:
        broker = rng.choice(BROKERS) if rng.random() < 0.3 else random_text(rng, 30)
        if rng.random() < 0.5:
            broker += ":" + str(rng.randint(0, 99999))
        yield "mqtt.parse_broker", {"raw_broker": broker}
        yield "mqtt.build_client_id", {"config": random_config_kwargs(rng, valid=True)}
        yield "mqtt.build_mqtt_password", {
            "session": random_session_kwargs(rng),
            "now": 0.0,
            "client_id": random_text(rng),
            "ts_seconds": rng.randint(-(2**62), 2**62),
        }
        key = random_hex(rng, rng.choice([16, 24, 32]))
        yield "mqtt.decode_mqtt_payload", {"payload_hex": rng.choice(payload_cases(rng, key)), "decrypt_key_hex": key}
