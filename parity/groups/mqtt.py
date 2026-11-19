"""pybyd/_mqtt.py: the pure helpers (broker address, credentials, payloads),
the two models, and what ``BydMqttRuntime``'s ``on_message`` makes of a
message (``mqtt.runtime_messages``).

``fetch_mqtt_bootstrap`` is a flow, compared by trace in ``api_mqtt``.
"""

from __future__ import annotations

import json
import logging
from unittest import mock

import paho.mqtt.client as paho
from pybyd import _mqtt
from pybyd._crypto.aes import aes_encrypt_hex
from pybyd.config import BydConfig
from pybyd.session import Session

from parity.framework import Group, random_hex, random_text
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION, random_config_kwargs, random_session_kwargs
from parity.groups.pyjson import random_json, random_object
from parity.models_support import jsonable, register_model

group = Group("mqtt")

BOOTSTRAP = {
    "user_id": "1234567",
    "broker_host": "emq.example.com",
    "broker_port": 8883,
    "topic": "oversea/res/1234567",
    "client_id": "oversea_0123456789ABCDEF0123456789ABCDEF",
    "username": "1234567",
    "password": "1700000000" + "0" * 32,
}

register_model(
    group,
    "mqtt.MqttBootstrap",
    _mqtt.MqttBootstrap,
    samples=[BOOTSTRAP, {**BOOTSTRAP, "broker_port": "1883", "extra": 1}],
)
register_model(
    group,
    "mqtt.MqttEvent",
    _mqtt.MqttEvent,
    samples=[
        {"event": "vehicleInfo", "vin": "TESTVIN0000000001", "topic": "oversea/res/1",
         "payload": {"event": "vehicleInfo", "vin": "TESTVIN0000000001", "data": {"respondData": {"soc": 80}}}},
        {"event": "", "vin": None, "topic": "t", "payload": {}},
    ],
)


class _Loop:
    """Runs ``call_soon_threadsafe`` callbacks at once."""

    def call_soon_threadsafe(self, fn, *args):
        fn(*args)


class _Records(logging.Handler):
    def __init__(self) -> None:
        super().__init__(logging.DEBUG)
        self.messages: list[str] = []

    def emit(self, record: logging.LogRecord) -> None:
        self.messages.append(record.msg)


@group.oracle("mqtt.runtime_messages")
def runtime_messages(decrypt_key_hex: str, steps: list[dict]) -> dict:
    """Drives a real ``BydMqttRuntime`` (paho replaced by a mock, as in
    pyBYD's own tests) and reports what each message led to."""
    events: list = []
    decrypt_errors: list = []
    records = _Records()
    logger = logging.getLogger("parity.mqtt.runtime")
    logger.handlers[:] = [records]
    logger.setLevel(logging.DEBUG)
    logger.propagate = False
    runtime = _mqtt.BydMqttRuntime(
        loop=_Loop(),
        decrypt_key_hex=decrypt_key_hex,
        on_event=events.append,
        on_decrypt_error=lambda: decrypt_errors.append(True),
        logger=logger,
    )
    client = mock.MagicMock()
    with mock.patch.object(_mqtt.mqtt, "Client", return_value=client):
        runtime.start(_mqtt.MqttBootstrap(**BOOTSTRAP))
    outcomes = []
    for step in steps:
        if "update_key" in step:
            runtime.update_decrypt_key(step["update_key"])
            continue
        message = paho.MQTTMessage(topic=bytes.fromhex(step["topic_hex"]))
        message.payload = bytes.fromhex(step["payload_hex"])
        before = (len(events), len(decrypt_errors), len(records.messages))
        client.on_message(client, None, message)
        if len(events) > before[0]:
            outcomes.append({"event": jsonable(events[-1].model_dump(mode="json"))})
        elif len(decrypt_errors) > before[1]:
            outcomes.append("decrypt_failed")
        elif any("not for this session" in text for text in records.messages[before[2]:]):
            outcomes.append("not_for_session")
        else:
            assert any("parse failed" in text for text in records.messages[before[2]:]), records.messages
            outcomes.append("invalid")
    return {"outcomes": outcomes, "key_confirmed": runtime._key_confirmed}


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


OTHER_KEY = "FFEEDDCCBBAA99887766554433221100"
TOPIC = "oversea/res/1234567".encode().hex()
EVENT_VALUES = [
    "vehicleInfo", "", None, 0, 1, 1.0, 0.0, True, False, [], [1, "a"], {}, {"a": "b"}, "é\x00", -5, 2**63 - 1,
]
VIN_VALUES = ["TESTVIN0000000001", "", None, 5, ["V"], {"v": 1}, True, " "]


def sealed(body, key: str = KEY) -> str:
    """A message payload: ``body`` (JSON-encoded unless it is text already) sealed with ``key``."""
    text = body if isinstance(body, str) else json.dumps(body)
    return aes_encrypt_hex(text, key).encode().hex()


def message(body, key: str = KEY, topic: str = TOPIC) -> dict:
    return {"topic_hex": topic, "payload_hex": sealed(body, key)}


def runtime_cases():
    """The branches of ``on_message``, alone and in sequences."""
    ours = message({"event": "vehicleInfo", "vin": "TESTVIN0000000001", "data": {}})
    theirs = message({"event": "vehicleInfo", "vin": "TESTVIN0000000001", "data": {}}, OTHER_KEY)
    garbage = {"topic_hex": TOPIC, "payload_hex": b"zz not hex".hex()}
    sequences = [
        [ours],
        [theirs],
        [ours, theirs],
        [theirs, ours, theirs],
        [ours, {"update_key": OTHER_KEY}, ours],
        [ours, {"update_key": OTHER_KEY}, theirs, ours],
        [{"update_key": OTHER_KEY}, theirs, {"update_key": KEY}, theirs],
        [garbage],
        [ours, garbage],
        [{"topic_hex": TOPIC, "payload_hex": ""}],
        [ours, {"topic_hex": TOPIC, "payload_hex": ""}],
        [message("[]"), theirs],
        [message("not json"), theirs],
        [message(""), theirs],
        [message("null")],
        [message({"event": "x"}, topic="ff fe".replace(" ", "")), theirs],
        [message({"event": "x"}, topic=""), message({"event": "y"}, topic="c3a9")],
        [{"topic_hex": TOPIC, "payload_hex": (" " + aes_encrypt_hex("{}", KEY) + "\n").encode().hex()}],
    ]
    for value in EVENT_VALUES:
        sequences.append([message({"event": value, "vin": "TESTVIN0000000001"})])
    for value in VIN_VALUES:
        sequences.append([message({"event": "remoteControl", "vin": value, "data": {"uuid": "S"}})])
    sequences.append([message({"vin": "TESTVIN0000000001"})])
    for steps in sequences:
        yield "mqtt.runtime_messages", {"decrypt_key_hex": KEY, "steps": steps}


def random_runtime_case(rng) -> dict:
    key = random_hex(rng, 16)
    other = random_hex(rng, rng.choice([16, 16, 24, 32]))
    steps = []
    for _ in range(rng.randint(0, 6)):
        roll = rng.random()
        topic = rng.choice([TOPIC, TOPIC, random_text(rng, 12).encode("utf-8", "surrogatepass").hex(),
                            rng.randbytes(rng.randint(0, 6)).hex()])
        if roll < 0.1:
            steps.append({"update_key": rng.choice([key, other])})
            continue
        body = random_object(rng)
        if rng.random() < 0.6:
            body["event"] = rng.choice(EVENT_VALUES + [random_json(rng), random_text(rng, 10)])
        if rng.random() < 0.6:
            body["vin"] = rng.choice(VIN_VALUES + [random_json(rng), random_text(rng, 17)])
        if roll < 0.5:
            steps.append(message(body, key, topic))
        elif roll < 0.75:
            steps.append(message(body, other, topic))
        elif roll < 0.85:
            steps.append(message(rng.choice(["[]", "1", "nul", "", json.dumps(random_json(rng))]), key, topic))
        else:
            steps.append({"topic_hex": topic, "payload_hex": rng.choice(payload_cases(rng, key))})
    return {"decrypt_key_hex": key, "steps": steps}


@group.recorded
def recorded():
    yield from runtime_cases()
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
        yield "mqtt.runtime_messages", random_runtime_case(rng)
