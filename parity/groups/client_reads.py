"""pybyd/client.py: the reads of BydClient, compared by trace.

The realtime and GPS reads trigger, wait for the MQTT push and fall back to
HTTP polling (``_trigger_and_poll``); the cases walk each way out: the push
arriving, arriving late, never arriving, arriving without data, the "no GPS"
code, and every way the polling ends. The other reads are one request each
with the session handling around it.
"""

from __future__ import annotations

from parity.api_support import (
    OLD_SESSION,
    VIN,
    client_case,
    client_oracle,
    error_reply,

    logged_in,
    login_replies,
    mqtt_event,
    ok_reply,
    random_client_case,
    raw_reply,
    transport_error,
)
from parity.flow_cases import corpus, keep_representable
from parity.framework import Group
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION

group = Group("client_reads", crate="client")

POLLED = ["get_vehicle_realtime", "get_gps_info"]
SIMPLE = [
    "get_hvac_status",
    "get_charging_status",
    "get_charging_schedule",
    "get_charging_homepage",
    "get_energy_consumption",
    "get_push_state",
    "get_booking_list",
]
OTHERS = ["get_ac_current_limit", "set_push_state", "get_notifications", "get_all_notifications", "rename_vehicle"]
METHODS = POLLED + SIMPLE + OTHERS
for name in METHODS:
    client_oracle(group, name)

VEHICLE = {"vin": VIN, "energyType": "0", "modelName": "SEAL", "outModelType": "SEAL-EU"}
HYBRID = {"vin": VIN, "energyType": "2", "modelName": "SONG"}
UNKNOWN = {"vin": VIN, "energyType": "x"}
REALTIME = next(payload for payload in corpus("VehicleRealtimeData") if isinstance(payload, dict))
GPS = next(payload for payload in corpus("GpsInfo") if isinstance(payload, dict))
FAST = {"poll_interval": 0.5, "poll_attempts": 3}


def ready(data: dict, serial: str = "S1") -> dict:
    return ok_reply(SAMPLE_SESSION, {**data, "requestSerial": serial})


def pending(serial: str = "S1") -> dict:
    return ok_reply(SAMPLE_SESSION, {"requestSerial": serial})


def push(at: float, event: str, data: dict, uuid: str | None = "S1", vin: str = VIN) -> dict:
    return mqtt_event(at, event, {"respondData": data}, uuid=uuid, vin=vin)


def polled_cases(method: str, event: str, data: dict) -> list[dict]:
    """Every way out of ``_trigger_and_poll`` for one read."""
    state = logged_in(vehicles=[VEHICLE])
    gps = method == "get_gps_info"
    other_event = "vehicleInfo" if gps else "remoteControl"
    cases = [
        # the push arrives in time
        {"replies": [pending()], "mqtt": [push(2.0, event, data)]},
        {"replies": [pending()], "mqtt": [push(0.0, event, data)]},
        # a push of another event type: GPS takes any, realtime only vehicleInfo
        {"replies": [pending(), ready(data)], "mqtt": [push(1.0, other_event, data)], **FAST},
        # the push arrives late: polling, and the push lands during a sleep
        {"replies": [pending(), pending(), ready(data)], "mqtt": [push(10.5, event, data)], **FAST},
        {"replies": [pending(), pending(), pending(), pending()], "mqtt": [push(30.0, event, data)], **FAST},
        # it never arrives: the polls decide
        {"replies": [pending(), ready(data)]},
        {"replies": [pending(), pending(), pending(), ready(data)], **FAST},
        {"replies": [pending(), pending(), pending(), pending()], **FAST},
        {"replies": [pending(), *[pending()] * 10]},
        # a push without data, for another serial, for another car, without a serial
        {"replies": [pending(), ready(data)], "mqtt": [push(1.0, event, {})], **FAST},
        {"replies": [pending(), ready(data)], "mqtt": [push(1.0, event, data, uuid="S2")], **FAST},
        {"replies": [pending(), ready(data)], "mqtt": [push(1.0, event, data, vin="OTHERVIN000000001")], **FAST},
        {"replies": [pending(), ready(data)], "mqtt": [push(1.0, event, data, uuid=None)], **FAST},
        {"replies": [pending(), ready(data)], "mqtt": [push(1.0, event, {**data, "requestSerial": "S1"}, uuid=None)]},
        # "no GPS" over MQTT ends the wait without polling
        {"replies": [pending()], "mqtt": [mqtt_event(1.0, event, {"code": "6051"}, uuid="S1")]},
        {"replies": [pending(), ready(data)], "mqtt": [mqtt_event(1.0, event, {"code": 6051}, uuid="S1")], **FAST},
        {"replies": [pending(), ready(data)], "mqtt": [mqtt_event(1.0, event, {"code": "1001"}, uuid="S1")], **FAST},
        # the trigger answers at once, or gives nothing to poll with
        {"replies": [ready(data)]},
        {"replies": [ok_reply(SAMPLE_SESSION, {})]},
        {"replies": [ok_reply(SAMPLE_SESSION, {"requestSerial": ""})]},
        {"replies": [ok_reply(SAMPLE_SESSION, [])]},
        {"replies": [ok_reply(SAMPLE_SESSION, {"requestSerial": 5}), ready(data)], **FAST},
        {"replies": [ok_reply(SAMPLE_SESSION, {"requestSerial": {"a": 1}})]},
        {"replies": [ok_reply(SAMPLE_SESSION, {"requestSerial": ["S"]})]},
        # the trigger fails
        {"replies": [error_reply("1001")]},
        {"replies": [error_reply("6051")]},
        {"replies": [error_reply("1008")]},
        {"replies": [transport_error(502)]},
        {"replies": []},
        # polling: no signal, other errors, the session expiring
        {"replies": [pending(), error_reply("6051"), error_reply("6051"), ready(data)], **FAST},
        {"replies": [pending(), error_reply("6051"), pending(), error_reply("6051"), ready(data)], **FAST},
        {"replies": [pending(), error_reply("6051"), ready(data)], **FAST, "signal_retries": 1},
        {"replies": [pending(), error_reply("6002"), transport_error(500), ready(data)], **FAST},
        {"replies": [pending(), error_reply("1001"), raw_reply("x"), ok_reply(SAMPLE_SESSION, []), ready(data)], **FAST},
        {"replies": [pending(), error_reply("1005"), *login_replies(), pending(), ready(data)], **FAST},
        {"replies": [pending(), error_reply("1005"), *login_replies(), pending(), error_reply("1005")], **FAST},
        {"replies": [pending(), pending()], **FAST},
        # no wait: MQTT not running, timeout 0, or not given at all
        {"replies": [pending(), ready(data)], "state": logged_in(mqtt=False, vehicles=[VEHICLE])},
        {"replies": [pending(), ready(data)], "mqtt_timeout": 0.0},
        {"replies": [pending(), ready(data)], "mqtt_timeout": 2.5, "mqtt": [push(2.0, event, data)]},
        {"replies": [pending(), ready(data)], "poll_interval": 0.0, "poll_attempts": 1},
        {"replies": [pending()], "poll_attempts": 0},
        # the session: expired before, expired at the trigger
        {"replies": [*login_replies(), pending()], "mqtt": [push(1.0, event, data)], "state": logged_in(session=OLD_SESSION, vehicles=[VEHICLE])},
        {"replies": [error_reply("1005"), *login_replies(), pending()], "mqtt": [push(1.0, event, data)]},
        {"replies": [error_reply("1005"), *login_replies(), error_reply("1005")]},
        # guests declare themselves
        {"replies": [pending(), ready(data)], "state": logged_in(vehicles=[VEHICLE], guest_vins={VIN: True}), **FAST},
        {"replies": [error_reply("1001"), pending(), ready(data)], **FAST},
    ]
    built = []
    for case in cases:
        case = dict(case)
        case.setdefault("state", state)
        built.append(client_case(vin=VIN, **case))
    return built


def energy_cases(method: str) -> list[dict]:
    """The vehicle metadata a read needs: cached, unknown, missing (a vehicle list first)."""
    data = REALTIME if method == "get_vehicle_realtime" else {"a": 1}
    return [
        client_case(state=logged_in(vehicles=[HYBRID]), replies=[ready(data)], vin=VIN),
        client_case(state=logged_in(vehicles=[UNKNOWN]), replies=[ready(data)], vin=VIN),
        client_case(state=logged_in(), replies=[ok_reply(SAMPLE_SESSION, [HYBRID]), ready(data)], vin=VIN),
        client_case(state=logged_in(), replies=[ok_reply(SAMPLE_SESSION, []), ready(data)], vin=VIN),
        client_case(state=logged_in(), replies=[error_reply("1002")], vin=VIN),
        client_case(state=logged_in(), replies=[error_reply("1005"), *login_replies(), ok_reply(SAMPLE_SESSION, [VEHICLE]), ready(data)], vin=VIN),
    ]


def simple_cases(method: str, data) -> list[dict]:
    state = logged_in(vehicles=[VEHICLE])
    scripts = [
        [ok_reply(SAMPLE_SESSION, data)],
        [ok_reply(SAMPLE_SESSION, {})],
        [ok_reply(SAMPLE_SESSION, [])],
        [error_reply("1001")],
        [error_reply("1008")],
        [error_reply("6002")],
        [transport_error(500)],
        [error_reply("1005"), *login_replies(), ok_reply(SAMPLE_SESSION, data)],
        [error_reply("1005"), *login_replies(), error_reply("1005")],
        [error_reply("1005"), error_reply("1002")],
        [],
    ]
    cases = [client_case(state=state, replies=replies, vin=VIN) for replies in scripts]
    cases.append(client_case(state=logged_in(session=OLD_SESSION, vehicles=[VEHICLE]), replies=[*login_replies(), ok_reply(SAMPLE_SESSION, data)], vin=VIN))
    cases.append(client_case(replies=[*login_replies(), ok_reply(SAMPLE_SESSION, data)], vin=VIN))
    cases.append(client_case(state=state, setup=[{"method": "async_close"}], vin=VIN))
    return cases


@group.recorded
def recorded():
    for case in polled_cases("get_vehicle_realtime", "vehicleInfo", REALTIME):
        yield "client.get_vehicle_realtime", case
    for case in polled_cases("get_gps_info", "remoteControl", GPS):
        yield "client.get_gps_info", case
    for case in energy_cases("get_vehicle_realtime"):
        yield "client.get_vehicle_realtime", case
    payloads = {
        "get_hvac_status": next(p for p in corpus("HvacStatus") if isinstance(p, dict)),
        "get_charging_status": next(p for p in corpus("ChargingStatus") if isinstance(p, dict)),
        "get_charging_schedule": {"smartChargeDto": {"startChargeTime": "22:00"}},
        "get_charging_homepage": next(p for p in corpus("ChargingStatus") if isinstance(p, dict)),
        "get_energy_consumption": next(p for p in corpus("EnergyConsumption") if isinstance(p, dict)),
        "get_push_state": next(p for p in corpus("PushNotificationState") if isinstance(p, dict)),
        "get_booking_list": next(p for p in corpus("BookingList") if isinstance(p, (dict, list))),
    }
    for method, data in payloads.items():
        for case in simple_cases(method, data):
            yield f"client.{method}", case
    for case in energy_cases("get_energy_consumption"):
        yield "client.get_energy_consumption", case
    yield "client.get_energy_consumption", client_case(
        state=logged_in(vehicles=[{"vin": VIN, "energyType": "2"}]), replies=[ok_reply(SAMPLE_SESSION, {})], vin=VIN
    )
    limit = next(p for p in corpus("AcCurrentLimit") if isinstance(p, dict))
    for energy_type in [None, 0, 2, "1", "x"]:
        yield "client.get_ac_current_limit", client_case(
            state=logged_in(vehicles=[HYBRID]), replies=[ok_reply(SAMPLE_SESSION, limit)], vin=VIN, energy_type=energy_type
        )
    for case in energy_cases("get_ac_current_limit"):
        yield "client.get_ac_current_limit", case
    for enable, push_type, reply in [
        (True, 701, ok_reply(SAMPLE_SESSION, {"result": "ok"})),
        (False, 702, ok_reply(SAMPLE_SESSION, {})),
        # the Sealion 7 EU answers 1001 (upstream test_push_switch_eu)
        (True, 701, error_reply("1001")),
        (True, 701, error_reply("1005")),
    ]:
        yield "client.set_push_state", client_case(
            state=logged_in(), replies=[reply, *login_replies(), reply], vin=VIN, enable=enable, push_type=push_type
        )
    messages = next(p for p in corpus("VehicleMsgListResponse") if isinstance(p, (dict, list)))
    for category in [None, 1, 2]:
        yield "client.get_notifications", client_case(state=logged_in(), replies=[ok_reply(SAMPLE_SESSION, messages)], category=category)
    yield "client.get_notifications", client_case(state=logged_in(), replies=[error_reply("1001")])
    yield "client.get_all_notifications", client_case(
        state=logged_in(), replies=[ok_reply(SAMPLE_SESSION, messages), ok_reply(SAMPLE_SESSION, [])]
    )
    yield "client.get_all_notifications", client_case(state=logged_in(), replies=[ok_reply(SAMPLE_SESSION, messages), error_reply("1008")])
    yield "client.get_all_notifications", client_case(
        state=logged_in(), replies=[error_reply("1005"), *login_replies(), ok_reply(SAMPLE_SESSION, []), ok_reply(SAMPLE_SESSION, [])]
    )
    for name, reply in [("Seal 🚗", ok_reply(SAMPLE_SESSION, {"result": "ok"})), ("", error_reply("1001"))]:
        yield "client.rename_vehicle", client_case(state=logged_in(), replies=[reply], vin=VIN, name=name)
    yield "client.get_hvac_status", client_case(config={**SAMPLE_CONFIG, "mqtt_enabled": False}, replies=[login_replies()[0], ok_reply(SAMPLE_SESSION, {})], vin=VIN)


WEIGHTED = POLLED * 6 + SIMPLE + OTHERS


@group.fuzz
def fuzz(rng):
    while True:
        method = rng.choice(WEIGHTED)
        yield f"client.{method}", random_client_case(rng, method)


keep_representable(group)
