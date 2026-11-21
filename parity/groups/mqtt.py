"""pybyd/_mqtt.py: the pure helpers (broker address, credentials, payloads),
the two models, the event and section names, the flattening of a
``vehicleStatusPush`` and what ``BydMqttRuntime``'s ``on_message`` makes of a
message (``mqtt.runtime_messages``).

``mqtt.flatten_vehicle_status_push`` takes ``payload`` and ``dtos``: a list
of section keys, or ``null`` for pyBYD's default (the realtime sections).
``random_status_push`` builds the pushes the ``state``, ``car`` and
``client_mqtt`` groups feed their engines, cars and clients.

``fetch_mqtt_bootstrap`` is a flow, compared by trace in ``api_mqtt``.
"""

from __future__ import annotations

import copy
import json
import logging
import random
from typing import Any
from unittest import mock

import paho.mqtt.client as paho
from pybyd import _mqtt
from pybyd._crypto.aes import aes_encrypt_hex
from pybyd.config import BydConfig
from pybyd.session import Session

from parity.framework import Group, random_hex, random_text
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION, random_config_kwargs, random_session_kwargs
from parity.groups.pyjson import random_json, random_object
from parity.models_support import json_safe, jsonable, register_model
from pydantic import TypeAdapter

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


# ---------------------------------------------------------------------------
# Event and section names; flattening a vehicleStatusPush
# ---------------------------------------------------------------------------

ENUMS = {"mqtt.MqttEventType": _mqtt.MqttEventType, "mqtt.VehicleStatusPushDto": _mqtt.VehicleStatusPushDto}


def _register_enum(call: str, cls: Any) -> None:
    adapter = TypeAdapter(cls)

    @group.oracle(call)
    def oracle(value: Any) -> dict[str, Any]:
        member = adapter.validate_python(value)
        return {"name": member.name, "value": member.value}


for _call, _cls in ENUMS.items():
    _register_enum(_call, _cls)


def enum_values(cls: Any) -> list[Any]:
    """Every member's value, near misses and other types."""
    members = [member.value for member in cls]
    near = [members[0].lower(), members[0].upper(), f" {members[0]}", f"{members[0]} ", members[0][:-1],
            members[-1] + "s", cls(members[0]).name]
    return members + near + [None, [], {}, "", " ", True, False, 0, 1, 1.5, "x", [members[0]], {"value": members[0]}]


@group.oracle("mqtt.flatten_vehicle_status_push", errors=(ValueError, AttributeError))
def flatten_vehicle_status_push(payload: Any, dtos: list[Any] | None = None) -> dict[str, Any]:
    if dtos is None:
        return _mqtt.flatten_vehicle_status_push(payload)
    return _mqtt.flatten_vehicle_status_push(payload, tuple(_mqtt.VehicleStatusPushDto(dto) for dto in dtos))


DTO_KEYS = [dto.value for dto in _mqtt.VehicleStatusPushDto]
AIR, CHARGE = "airStatusDto", "chargeStatusDto"

#: Fields a section may carry, with the values worth sending: readings,
#: BYD's sentinels, numbers as text, wrong types. Next to a field's usual key
#: are the other spellings the models take (the field name, a key alias).
_STATE = [0, 1, 2, 3, -1, "1", "x", None, "--", "", 15, 1.0, True]
_NUMBER = [0, 0.0, -0.0, 1, 21.5, "22.5", 73, -1, -129, "--", "", None, "x", "NaN", 1e6]
_CONSUMPTION = ["--", "", "N/A", "6.7kW·h/100km+7.2L/100km", "(6.7度+7.2升)/百公里", "19.9kW·h/100km", "15.2", "6.1+8.4",
                "7.5L/100km", None]
SECTION_FIELDS: dict[str, dict[str, list[Any]]] = {
    AIR: {
        "cycleModel": _STATE, "cycleChoice": _STATE, "airRunState": _STATE, "status": _STATE, "acSwitch": _STATE,
        "mainSettingTempNew": _NUMBER, "mainSettingTemp": [7, "7", 17, 0, 7.5, "--", None], "tempInCar": _NUMBER,
        "tempOutCar": _NUMBER, "pm": _NUMBER, "windPosition": [0, 1, 4, 7, 9, -1], "windMode": _STATE,
        "airConditioningMode": _STATE, "mainSeatHeatState": _STATE, "stearingWheelHeatState": _STATE,
        "steeringWheelHeatState": _STATE, "statusNow": [{"status": 1, "tempInCar": 20}, {}, "x", None],
        "air_run_state": _STATE,
    },
    CHARGE: {
        "chargingState": _STATE, "chargeState": _STATE, "connectState": _STATE, "chargePower": _NUMBER,
        "chargePowerUnit": ["kW", "", "--", None, 5], "soc": [70, "55", -1, 0, None, "x", 100.0],
        "elecPercent": _NUMBER, "fullHour": [1, -1, 0, "2", None], "fullMinute": [25, -1, 0, "--"],
        "waitStatus": _STATE, "updateTime": [1700000000, 1700000000123, "x", 0, None, -5],
        "vin": ["TESTVIN0000000001", "", None, 5], "charging_state": _STATE, "charge_power": _NUMBER,
    },
    "doorStatusDto": {
        "leftFrontDoor": _STATE, "rightFrontDoor": _STATE, "leftRearDoor": _STATE, "rightRearDoor": _STATE,
        "trunkLid": _STATE, "backCover": _STATE, "slidingDoor": _STATE, "forehold": _STATE,
        "leftFrontDoorLock": _STATE, "rightFrontDoorLock": _STATE, "leftRearDoorLock": _STATE,
        "rightRearDoorLock": _STATE, "slidingDoorLock": _STATE, "left_front_door_lock": _STATE,
    },
    "windowStatusDto": {
        "leftFrontWindow": _STATE, "rightFrontWindow": _STATE, "leftRearWindow": _STATE, "rightRearWindow": _STATE,
        "skylight": _STATE, "leftFrontWindowPct": [0, 10, 100, "50", -1, None],
    },
    "tireStatusDto": {
        "leftFrontTirepressure": [*_NUMBER, 2.4, 38.4, 250.0], "rightFrontTirepressure": [*_NUMBER, 2.5, 37.2],
        "leftRearTirepressure": [*_NUMBER, 2.6], "rightRearTirepressure": [*_NUMBER, 2.7],
        "leftFrontTirePressure": _NUMBER, "tirePressUnit": [1, 2, 3, 0, -1, "2", "--", None],
        "leftFrontTireStatus": _STATE, "tirepressureSystem": _STATE, "rapidTireLeak": _STATE,
    },
    "drivingStatusDto": {
        "speed": _NUMBER, "powerGear": _STATE, "totalMileage": [*_NUMBER, 18234, 18240.5],
        "totalMileageV2": _NUMBER, "enduranceMileage": [*_NUMBER, 320, 400], "evEndurance": _NUMBER,
        "oilEndurance": _NUMBER, "oilPercent": _NUMBER, "epb": _STATE, "abs": _STATE, "absWarning": _STATE,
    },
    "commonDto": {
        "onlineState": _STATE, "connectState": _STATE, "vehicleState": _STATE, "sentryStatus": [2, 4, 0, -1, "2"],
        "requestSerial": ["S1", "", None, 5], "vehicleTimeZone": ["Europe/Amsterdam", "", None],
        "repairModeSwitch": [0, "1", "x", None, 1.5, True], "lessOneMin": [True, False, "false", 0, 2, None],
        "time": [1700000000, 1700000000123, "1700000000", "x", 0, None, 1.5],
        "timestamp": [1700000001, "2026-05-07T05:56:45.000+00:00", None, "x"],
        "raw": [{"own": 1}, {}, "x", None, [1]],
    },
    "can4A505Dto": {"elecPercent": [*_NUMBER, 82, 85, 91], "powerBattery": _NUMBER, "elec_percent": _NUMBER},
    "can21511Dto": {"batteryHeatState": _STATE, "chargeHeatState": _STATE, "ectValue": [-129, -1, 90, "x"],
                    "rate": [-999, -50, 0, 7, "--"], "gl": _NUMBER, "totalPower": _NUMBER},
    "can3D903Dto": {"totalEnergy": _CONSUMPTION, "recent50kmEnergy": _CONSUMPTION, "energyConsumption": _CONSUMPTION,
                    "nearestEnergyConsumption": _CONSUMPTION,
                    "nearestEnergyConsumptionUnit": ["kW·h/100km", "L/100km", "--", "", None],
                    "totalConsumption": _CONSUMPTION, "totalConsumptionEn": _CONSUMPTION},
}
_ALL_FIELDS = {key: values for fields in SECTION_FIELDS.values() for key, values in fields.items()}
_BAD_SECTIONS = [None, [], "x", 5, {}, [{"elecPercent": 1}], True, 1.5, ""]
_META = [1700000000, 1700000000123, "1700000000", "x", 0, None, 1.5, [1], {"a": 1}, "", "--"]
#: Pushes from the upstream tests (tests/test_mqtt_status_push.py).
STATUS_PUSHES = [
    {"doorStatusDto": {"leftFrontDoor": 1, "leftFrontDoorLock": 1}, "can4A505Dto": {"elecPercent": 82},
     "unknownDto": {"foo": "bar"}, "time": 1795006707},
    {"time": 1795006707},
    {"airStatusDto": {"cycleModel": 2, "airRunState": 1, "mainSettingTempNew": 23.0},
     "chargeStatusDto": {"chargingState": 15, "connectState": 0, "chargeState": 15},
     "can4A505Dto": {"elecPercent": 85}},
    {"doorStatusDto": {"leftFrontDoor": 1}, "can4A505Dto": {"elecPercent": 91}, "airStatusDto": {"cycleModel": 2}},
    {"time": 1700000000, "doorStatusDto": {"leftFrontDoor": 1}},
]


def random_section(rng: random.Random, dto: str) -> Any:
    """What a push carries under one section key: usually fields, sometimes not a dict at all."""
    if rng.random() < 0.1:
        return copy.deepcopy(rng.choice(_BAD_SECTIONS))
    own = SECTION_FIELDS.get(dto) or _ALL_FIELDS
    fields: dict[str, Any] = {}
    for _ in range(rng.randint(0, 5)):
        pool = own if rng.random() < 0.8 else _ALL_FIELDS
        key = rng.choice(list(pool))
        fields[key] = copy.deepcopy(rng.choice(pool[key]))
    if rng.random() < 0.1:
        fields[rng.choice(["foo", "unknownKey", "", "比亚迪"])] = rng.choice(["bar", 1, None, [1], {"a": 1}])
    return fields


def random_status_push(rng: random.Random) -> dict[str, Any]:
    """A ``vehicleStatusPush`` payload: a few sections, known and unknown,
    with and without the envelope's own ``time``."""
    if rng.random() < 0.1:
        return copy.deepcopy(rng.choice(STATUS_PUSHES))
    payload: dict[str, Any] = {}
    favourites = [AIR, CHARGE, "doorStatusDto", "can4A505Dto", "tireStatusDto", "drivingStatusDto", "commonDto"]
    for _ in range(rng.choice([0, 1, 1, 2, 2, 3, 4, 6])):
        dto = rng.choice(favourites) if rng.random() < 0.7 else rng.choice(DTO_KEYS)
        payload[dto] = random_section(rng, dto)
    if rng.random() < 0.15:
        payload[rng.choice(["unknownDto", "airStatusDTO", "smartChargeDto", "dto", ""])] = random_section(rng, "")
    for meta in ("time", "timestamp"):
        if rng.random() < 0.3:
            payload[meta] = copy.deepcopy(rng.choice(_META))
    if rng.random() < 0.1:
        payload[rng.choice(["res", "requestSerial", "elecPercent", "code"])] = rng.choice([1, "S1", 50, "6051"])
    return payload


def random_dtos(rng: random.Random) -> list[Any] | None:
    roll = rng.random()
    if roll < 0.4:
        return None
    if roll < 0.6:
        return [rng.choice([AIR, CHARGE])]
    if roll < 0.9:
        return rng.sample(DTO_KEYS, k=rng.randint(0, 6))
    return [rng.choice(DTO_KEYS), rng.choice(["airStatus", "AIR_STATUS", "", 5, None, AIR.upper()])]


def flatten_cases():
    call = "mqtt.flatten_vehicle_status_push"
    for payload in STATUS_PUSHES:
        yield call, {"payload": payload, "dtos": None}
        yield call, {"payload": payload, "dtos": [AIR]}
        yield call, {"payload": payload, "dtos": [CHARGE]}
    both = {"time": 5, "timestamp": 6, AIR: {"cycleModel": 2, "time": 7}, CHARGE: {"cycleModel": 1, "timestamp": None},
            "doorStatusDto": "x", "smartChargeDto": {"soc": 80}, "smartJourneyDto": {"status": 1}}
    # Sections are read in the order asked for; the later one wins a shared key, in the first one's place.
    for dtos in (None, [AIR, CHARGE], [CHARGE, AIR], [AIR, AIR], ["smartChargeDto"], ["smartJourneyDto", "smartChargeDto"],
                 [], ["doorStatusDto"], DTO_KEYS, list(reversed(DTO_KEYS))):
        yield call, {"payload": both, "dtos": dtos}
    # A section that is not a dict, an empty one, one of only nulls: nothing is taken, so no time either.
    for section in [*_BAD_SECTIONS, {"a": None}, {"a": None, "b": 0}, {"": ""}, {"time": None}]:
        yield call, {"payload": {AIR: section, "time": 1, "timestamp": 2}, "dtos": None}
    # The envelope's time: null, missing, a section's own, both.
    for payload in ({AIR: {"a": 1}}, {AIR: {"a": 1}, "time": None, "timestamp": None}, {AIR: {"a": 1}, "timestamp": "x"},
                    {AIR: {"timestamp": 0}, "time": [1], "timestamp": 9}, {AIR: {"a": 1}, "time": 0, "timestamp": False},
                    {}, {"airStatusDTO": {"a": 1}, "time": 1}):
        yield call, {"payload": payload, "dtos": None}
    # Not a payload, not a section key.
    for payload in (None, [], "text", 5, [{AIR: {"a": 1}}]):
        yield call, {"payload": payload, "dtos": None}
        yield call, {"payload": payload, "dtos": []}
        yield call, {"payload": payload, "dtos": ["nope"]}
    for dtos in (["nope"], [AIR, "AIR_STATUS"], [5], [None], [AIR.upper()], [""]):
        yield call, {"payload": both, "dtos": dtos}


@group.recorded
def recorded_status_push():
    for call, cls in ENUMS.items():
        for value in enum_values(cls):
            yield call, {"value": value}
    yield from flatten_cases()
    rng = random.Random("mqtt-status-push")
    for _ in range(40):
        yield "mqtt.flatten_vehicle_status_push", {"payload": random_status_push(rng), "dtos": random_dtos(rng)}


@group.fuzz
def fuzz_status_push(rng):
    values = {call: enum_values(cls) for call, cls in ENUMS.items()}
    while True:
        call = rng.choice(list(ENUMS))
        value = rng.choice(values[call]) if rng.random() < 0.8 else random_json(rng)
        yield call, {"value": value}
        for _ in range(4):
            payload = random_status_push(rng) if rng.random() < 0.95 else random_json(rng)
            case = {"payload": payload, "dtos": random_dtos(rng)}
            if json_safe(case):
                yield "mqtt.flatten_vehicle_status_push", case


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
