"""pyBYD's own tests that concern the client, as client parity cases.

Each block names the upstream test file it ports (``tests/`` at the pinned
commit) and replays its scenario through the whole client, so the
assertion upstream makes is part of the compared trace: the error class
and code, the endpoints posted, the counters and the guest map. Tests of
pure helpers and models are covered by the model and flow groups; the
``BydCar`` parts of ``test_issue_74`` and ``test_command_outcomes`` belong
to the car layer.
"""

from __future__ import annotations

from parity.api_support import (
    ALL_FUNCTIONS_CONFIG,
    DEDICATED_HEAT_CONFIG,
    NOW_MS,
    VIN,
    client_case,
    client_oracle,
    error_reply,
    logged_in,
    login_replies,
    mqtt_event,
    ok_reply,
)
from parity.framework import Group
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION

group = Group("client_upstream", crate="client")

METHODS = [
    "flash_lights",
    "lock",
    "verify_command_access",
    "get_hvac_status",
    "get_gps_info",
    "get_vehicle_realtime",
    "get_charging_status",
    "set_battery_heat",
    "set_seat_climate",
    "open_windows",
    "close_windows",
    "set_push_state",
    "get_push_state",
    "get_notifications",
    "get_all_notifications",
    "get_booking_list",
    "stop_charging",
    "login",
    "_on_mqtt_event",
]
for name in METHODS:
    client_oracle(group, name)

PIN = "123456"
VEHICLE = {"vin": VIN, "energyType": "0"}


def reply(data) -> dict:
    return ok_reply(SAMPLE_SESSION, data)


def ready(config=ALL_FUNCTIONS_CONFIG, vehicle=VEHICLE, **more) -> dict:
    return logged_in(vehicles=[vehicle], capabilities={VIN: config}, commands_enabled=True, **more)


@group.recorded
def recorded():  # noqa: C901, PLR0915
    # test_command_outcomes: no serial is "no_serial", a poll that never ends is "timeout"
    yield "client.flash_lights", client_case(
        state=ready(), options={"command_retries": 1}, replies=[reply({})], vin=VIN, command_pwd=PIN
    )
    yield "client.flash_lights", client_case(
        state=ready(mqtt=False),
        options={"command_retries": 1},
        replies=[reply({"requestSerial": "S1"}), *[reply({"res": 1})] * 10],
        vin=VIN,
        command_pwd=PIN,
    )
    yield "client.flash_lights", client_case(
        state=ready(mqtt=False), replies=[reply({"requestSerial": "S1"}), *[reply({"res": 1})] * 10] * 3, vin=VIN, command_pwd=PIN
    )

    # test_control_password_errors: 5005/5006/5011 are BydControlPasswordError
    # on verifyControlPassword and on remoteControl; 1009 stays a
    # BydRemoteControlError; an unmapped code is a plain BydApiError
    for code in ["5005", "5006", "5011", "5999"]:
        yield "client.verify_command_access", client_case(state=logged_in(), replies=[error_reply(code)], vin=VIN, command_pwd=PIN)
        yield "client.lock", client_case(state=ready(), replies=[error_reply(code)], vin=VIN, command_pwd=PIN)
    yield "client.lock", client_case(state=ready(), options={"command_retries": 1}, replies=[error_reply("1009")], vin=VIN, command_pwd=PIN)

    # test_guest_user_type: the vehicle list seeds the guest guess and it survives a login
    for raw in [{"vin": VIN, "empowerType": -1, "empowerId": 123}, {"vin": VIN, "empowerType": 3}, {"vin": VIN}]:
        yield "client.login", client_case(
            state=logged_in(), setup=[{"method": "get_vehicles"}], replies=[reply([raw]), *login_replies()]
        )
    # a known guest declares itself at once; an owner never sends userType;
    # wrong guesses are corrected by the answer, and an endpoint whose 1001
    # survives both guesses is asked once only afterwards
    yield "client.get_hvac_status", client_case(state=logged_in(guest_vins={VIN: True}), replies=[reply({})], vin=VIN)
    yield "client.get_hvac_status", client_case(state=logged_in(guest_vins={VIN: False}), replies=[reply({})], vin=VIN)
    yield "client.get_hvac_status", client_case(state=logged_in(), replies=[error_reply("1001"), reply({})], vin=VIN)
    yield "client.get_hvac_status", client_case(state=logged_in(guest_vins={VIN: True}), replies=[error_reply("8003"), reply({})], vin=VIN)
    yield "client.get_hvac_status", client_case(
        state=logged_in(),
        setup=[{"method": "get_hvac_status", "args": {"vin": VIN}}],
        replies=[error_reply("1001"), error_reply("8003"), error_reply("1001")],
        vin=VIN,
    )
    yield "client.get_hvac_status", client_case(state=logged_in(), replies=[error_reply("1001"), error_reply("6002")], vin=VIN)
    yield "client.get_hvac_status", client_case(state=logged_in(), replies=[error_reply("1008")], vin=VIN)
    yield "client.get_gps_info", client_case(
        state=logged_in(guest_vins={VIN: True}), replies=[reply({"latitude": 52.0, "longitude": 4.0})], vin=VIN
    )

    # test_service_busy_error_mapping: 1008 is BydServiceBusyError and 6002
    # BydDataUnavailableError on every endpoint
    for code in ["1008", "6002", "9999"]:
        yield "client.lock", client_case(state=ready(), replies=[error_reply(code)], vin=VIN, command_pwd=PIN)
        yield "client.get_vehicle_realtime", client_case(state=logged_in(vehicles=[VEHICLE]), replies=[error_reply(code)], vin=VIN)
        yield "client.get_charging_status", client_case(state=logged_in(), replies=[error_reply(code)], vin=VIN)

    # test_battery_heat: the legacy route keeps the generic endpoints, the
    # dedicated pair sends its parameters on every poll, and a refusal there
    # names the result endpoint
    heat_both = {"cfFixedList": [{"functionNo": "10300002"}]}
    yield "client.set_battery_heat", client_case(
        state=ready(config=heat_both), options={"command_retries": 1}, replies=[reply({"requestSerial": "S1"}), reply({"controlState": 1})],
        vin=VIN, params={"on": True}, command_pwd=PIN,
    )
    yield "client.set_battery_heat", client_case(
        state=ready(config=DEDICATED_HEAT_CONFIG, mqtt=False),
        options={"command_retries": 1},
        replies=[reply({"requestSerial": "SERIAL123"}), reply({"res": 1, "message": "succcess"}), reply({"res": 2, "message": "succcess"})],
        vin=VIN, params={"on": True}, command_pwd=PIN,
    )
    yield "client.set_battery_heat", client_case(
        state=ready(config=DEDICATED_HEAT_CONFIG, mqtt=False),
        options={"command_retries": 1},
        replies=[reply({"requestSerial": "SERIAL456"}), reply({"res": 3, "message": "Las condiciones no se cumplen."})],
        vin=VIN, params={"on": True}, command_pwd=PIN,
    )
    yield "client.set_battery_heat", client_case(
        state=ready(config=DEDICATED_HEAT_CONFIG), vin=VIN, params={"on": False, "heat_type": 1}, command_pwd=PIN,
        replies=[reply({"requestSerial": "S1"})], mqtt=[mqtt_event(0.5, "remoteControl", {"respondData": {"res": 2}}, uuid="S1")],
    )

    # test_rear_seat_climate: rear chair types pass the gate with rear-seat
    # function numbers and are refused without them
    rear = {"cfFixedList": [{"functionNo": no} for no in ["10030006", "10030007", "10030008", "10030009"]]}
    front = {"cfFixedList": [{"functionNo": no} for no in ["10030001", "10030002"]]}
    for config in (rear, front):
        for chair, key in [("3", "lr_seat_heat_state"), ("3", "lr_seat_ventilation_state"), ("4", "rr_seat_heat_state"), ("4", "rr_seat_ventilation_state")]:
            yield "client.set_seat_climate", client_case(
                state=ready(config=config), replies=[reply({"controlState": 1})], vin=VIN, params={"chair_type": chair, key: 1}, command_pwd=PIN
            )

    # test_windows_open_available: opening follows the learn-info gate,
    # closing does not; without the function number neither is available
    windows = {"cfFixedList": [{"functionNo": "1026"}]}
    for learn in [{"openWindowLearnInfo": 0, "openWindow499LearnInfo": 1}, {"openWindowLearnInfo": 1, "openWindow499LearnInfo": 1}, {}]:
        vehicle = {"vin": VIN, "vehicleFunLearnInfo": learn}
        for method in ("open_windows", "close_windows"):
            yield f"client.{method}", client_case(state=ready(config=windows, vehicle=vehicle), replies=[reply({"controlState": 1})], vin=VIN, command_pwd=PIN)
    yield "client.open_windows", client_case(state=ready(config={"cfFixedList": []}), vin=VIN, command_pwd=PIN)

    # test_push_switch_eu: the EU list shape, a non-list reply, and the
    # switch answering 1001 on the Sealion 7 EU
    yield "client.get_push_state", client_case(state=logged_in(), replies=[reply([{"type": 701, "state": 1}, {"type": 1, "state": 0}])], vin=VIN)
    yield "client.get_push_state", client_case(state=logged_in(), replies=[reply({"unexpected": "shape"})], vin=VIN)
    yield "client.set_push_state", client_case(state=logged_in(), replies=[error_reply("1001")], vin=VIN, enable=True, push_type=701)

    # test_message_center: both categories, the EU bare list and wrapped lists
    items = [
        {"content": "2026-10-02 20:40,ATTO 3 El coche comience a cargar.", "jumpUrl": "", "messageGroup": 1,
         "pushId": 211902416, "pushTime": 1790966443550, "title": "Empezar carga", "titleId": 1},
        {"messageId": "102", "messageTitle": "Door open", "content": "--", "pushTime": "1790966443", "messageGroup": ""},
    ]
    for payload in (items, {"list": items}, {"messageList": items}, {}, [], "text"):
        yield "client.get_notifications", client_case(state=logged_in(), replies=[reply(payload)], category=1)
    yield "client.get_notifications", client_case(state=logged_in(), replies=[reply(items)], category=2)
    yield "client.get_notifications", client_case(state=logged_in(), replies=[error_reply("1001")], category=1)
    yield "client.get_all_notifications", client_case(state=logged_in(), replies=[reply(items), reply(items[:1])])

    # test_booking_list: the captured Sealion 7 payload, and an empty one
    captured = {
        "allowSetting": 0, "configData": 0,
        "listInfo": [{"bookingTime": 1780938216, "mainSettingTemp": 23, "airConditionTempRange": 7, "cycleMode": 2,
                      "windLevel": 0, "timeSpan": 1, "mainSettingTempNew": 23.0, "acSwitch": 0,
                      "bookingId": 1216038691305533440, "windMode": 0}],
    }
    for payload in (captured, {}, {"listInfo": []}):
        yield "client.get_booking_list", client_case(state=logged_in(), replies=[reply(payload)], vin=VIN)

    # test_stop_charging: always refused, nothing sent
    yield "client.stop_charging", client_case(state=ready(), replies=[reply({"res": 2})], vin=VIN)

    # test_issue_fixes: the login's identifier type and app name
    for config in (SAMPLE_CONFIG, {**SAMPLE_CONFIG, "identifier_type": "1"}, {**SAMPLE_CONFIG, "username": "+31600000000"}):
        yield "client.login", client_case(config=config, replies=login_replies())

    # test_mqtt_decrypt_reauth: one re-login, none while failures survive it,
    # and a decrypted payload re-arms it
    yield "client._on_mqtt_event", client_case(
        state=logged_in(),
        mqtt=[{"at": 120.0 * index, "decrypt_error": True} for index in range(4)],
        replies=[*login_replies()] * 4,
    )
    yield "client._on_mqtt_event", client_case(
        state=logged_in(),
        mqtt=[
            {"at": 0.0, "decrypt_error": True},
            {"at": 120.0, "decrypt_error": True},
            {"at": 121.0, "event": {"event": "unknown", "vin": None, "topic": "t", "payload": {}}},
            {"at": 240.0, "decrypt_error": True},
        ],
        replies=[*login_replies()] * 3,
    )

    # test_mqtt_connack_recovery: below the threshold nothing happens; a
    # successful CONNACK resets the counters and calls back; the threshold
    # schedules one re-login; the backoff blocks an immediate retry; the
    # ceiling stops the channel
    yield "client._on_mqtt_event", client_case(
        state=logged_in(), mqtt=[{"at": 0.0, "connack_refused": 1}, {"at": 1.0, "connack_refused": 2}]
    )
    yield "client._on_mqtt_event", client_case(
        state=logged_in(),
        mqtt=[{"at": 0.0, "connack_refused": 3}, {"at": 1.0, "connected": True}],
        replies=login_replies(),
    )
    yield "client._on_mqtt_event", client_case(state=logged_in(), mqtt=[{"at": 0.0, "connack_refused": 3}], replies=login_replies())
    yield "client._on_mqtt_event", client_case(
        state=logged_in(), mqtt=[{"at": 0.0, "connack_refused": 3}, {"at": 1.0, "connack_refused": 4}], replies=login_replies() * 2
    )
    yield "client._on_mqtt_event", client_case(
        state=logged_in(),
        mqtt=[{"at": 3000.0 * index, "connack_refused": 99} for index in range(8)],
        replies=login_replies() * 8,
        now_ms=NOW_MS,
    )



