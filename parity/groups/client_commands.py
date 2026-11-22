"""pybyd/client.py: the remote commands of BydClient, compared by trace.

These send commands to real cars. Each command is driven through success
by MQTT acknowledgement and by polling, refusal, rate limiting and timeout,
with ``command_retries`` 1 and 3 (the option applications set by patching
``poll_remote_control``), the command gate blocking and bypassed, every PIN
error and commands not enabled. The charging commands go through
``_trigger_and_poll`` and their own result checks.
"""

from __future__ import annotations

from parity.api_support import (
    LIVE_SESSION,
    ALL_FUNCTIONS_CONFIG,
    COMMAND_PARAMS,
    DEDICATED_HEAT_CONFIG,
    OLD_SESSION,
    SIMPLE_COMMANDS,
    VIN,
    client_case,
    client_oracle,
    error_reply,
    logged_in,
    login_replies,
    mqtt_event,
    ok_reply,
    random_client_case,
    transport_error,
)
from parity.flow_cases import keep_representable
from parity.framework import Group
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION

group = Group("client_commands", crate="client")

CHARGING = ["save_charging_schedule", "toggle_smart_charging", "start_charging", "stop_charging"]
METHODS = ["verify_command_access", *SIMPLE_COMMANDS, *COMMAND_PARAMS, *CHARGING]
for name in METHODS:
    client_oracle(group, name)

VEHICLE = {"vin": VIN, "energyType": "0", "modelName": "SEAL"}
PIN = "123456"
HEX = "0123456789abcdef0123456789abcdef"
FAST = {"poll_interval": 0.5, "poll_attempts": 2}


def ready_state(**more) -> dict:
    """Logged in, command access verified, every function available."""
    return logged_in(vehicles=[VEHICLE], capabilities={VIN: ALL_FUNCTIONS_CONFIG}, commands_enabled=True, **more)


def reply(data) -> dict:
    return ok_reply(SAMPLE_SESSION, data)


def ack(at: float, data: dict, uuid: str | None = "S1") -> dict:
    return mqtt_event(at, "remoteControl", {"respondData": data}, uuid=uuid)


TRIGGER = reply({"requestSerial": "S1"})
POLL_PENDING = reply({"controlState": 0, "requestSerial": "S1"})
POLL_DONE = reply({"controlState": 1, "requestSerial": "S1"})
POLL_FAILED = reply({"controlState": 2, "requestSerial": "S1", "message": "refused"})


def outcome_scripts() -> list[dict]:
    """Every way a command ends, as ``client_case`` keywords."""
    return [
        # success: by MQTT acknowledgement, by polling, at the trigger
        {"replies": [TRIGGER], "mqtt": [ack(1.0, {"res": 2})]},
        {"replies": [TRIGGER], "mqtt": [ack(1.0, {"controlState": 1})]},
        {"replies": [TRIGGER, POLL_PENDING, POLL_DONE]},
        {"replies": [TRIGGER, reply({"res": 1}), reply({"res": 2})]},
        {"replies": [reply({"controlState": 1, "requestSerial": "S1"})]},
        {"replies": [reply({"result": "ok"})]},
        # the acknowledgement is late, pending, for another serial, without a serial
        {"replies": [TRIGGER, POLL_DONE], "mqtt": [ack(12.0, {"res": 2})]},
        {"replies": [TRIGGER, POLL_DONE], "mqtt": [ack(1.0, {"res": 1})]},
        {"replies": [TRIGGER, POLL_DONE], "mqtt": [ack(1.0, {"res": 2}, uuid="S2")]},
        {"replies": [TRIGGER, POLL_DONE], "mqtt": [ack(1.0, {"res": 2}, uuid=None)]},
        {"replies": [TRIGGER, POLL_DONE], "mqtt": [ack(1.0, {"res": "x"})]},
        # refused: over MQTT (polled anyway), by polling, at the trigger
        {"replies": [TRIGGER, POLL_DONE], "mqtt": [ack(1.0, {"res": 3})]},
        {"replies": [TRIGGER, POLL_FAILED, TRIGGER, POLL_FAILED, TRIGGER, POLL_FAILED]},
        {"replies": [TRIGGER, POLL_FAILED, TRIGGER, POLL_DONE]},
        {"replies": [reply({"controlState": 2})] * 3},
        {"replies": [TRIGGER, POLL_FAILED], "options": {"command_retries": 1}},
        # rate limited, and the cloud's own refusals
        {"replies": [error_reply("6024")] * 3},
        {"replies": [error_reply("6024"), TRIGGER, POLL_DONE]},
        {"replies": [error_reply("1009")] * 3},
        {"replies": [error_reply("1001"), TRIGGER, POLL_DONE]},
        {"replies": [error_reply("1001")], "options": {"command_retries": 1}},
        {"replies": [error_reply("1008")]},
        {"replies": [transport_error(500)]},
        # timeout and no serial, with one attempt and three
        {"replies": [TRIGGER, *[POLL_PENDING] * 10] * 3},
        {"replies": [TRIGGER, *[POLL_PENDING] * 10], "options": {"command_retries": 1}},
        {"replies": [reply({})] * 3},
        {"replies": [reply({})], "options": {"command_retries": 1}},
        {"replies": [TRIGGER, error_reply("6051"), transport_error(500)]},
        # PIN errors from the cloud
        {"replies": [error_reply("5005")]},
        {"replies": [error_reply("5006")]},
        {"replies": [error_reply("5011")]},
        # the session expires at the trigger or while polling
        {"replies": [error_reply("1002"), *login_replies(), TRIGGER, POLL_DONE]},
        {"replies": [TRIGGER, error_reply("1002"), *login_replies(), TRIGGER, POLL_DONE]},
        {"replies": [error_reply("1011"), *login_replies(), TRIGGER, POLL_DONE]},
        {"replies": [TRIGGER, error_reply("3003"), *login_replies(), TRIGGER, POLL_DONE]},
        # 1005 and 1010 were such codes before pyBYD #96: now the command fails with them
        {"replies": [error_reply("1005"), *login_replies(), TRIGGER, POLL_DONE]},
        {"replies": [TRIGGER, error_reply("1010"), *login_replies(), TRIGGER, POLL_DONE]},
        {"replies": [*login_replies(), TRIGGER, POLL_DONE], "state": ready_state(session=OLD_SESSION)},
        # guests
        {"replies": [error_reply("1001"), TRIGGER, POLL_DONE], "state": ready_state(guest_vins={VIN: False})},
        {"replies": [TRIGGER, POLL_DONE], "state": ready_state(guest_vins={VIN: True})},
        # MQTT not running: straight to polling
        {"replies": [TRIGGER, POLL_DONE], "state": ready_state(mqtt=False)},
        # pyBYD #111: before the trigger is dispatched a session, token or
        # clock error is retried; after it, never (the command is not sent twice)
        {"replies": [error_reply("1030"), *login_replies(), TRIGGER, POLL_DONE]},
        {"replies": [error_reply("1003"), TRIGGER, POLL_DONE]},
        {"replies": [TRIGGER, error_reply("1030"), *login_replies(), TRIGGER, POLL_DONE]},
        {"replies": [TRIGGER, error_reply("1003"), TRIGGER, POLL_DONE]},
        {"replies": [TRIGGER, error_reply("1002"), TRIGGER, POLL_DONE], "options": {"command_retries": 3}},
        {"replies": [error_reply("1030"), ok_reply(SAMPLE_SESSION, REFRESHED), TRIGGER, POLL_DONE],
         "state": ready_state(session=REFRESHABLE)},
        # pyBYD #107: a refusal is never sent again, even with retries asked for
        {"replies": [TRIGGER, POLL_FAILED, TRIGGER, POLL_DONE], "options": {"command_retries": 3}},
        {"replies": [reply({"controlState": 2})] * 3, "options": {"command_retries": 3}},
        {"replies": [TRIGGER, POLL_DONE], "mqtt": [ack(1.0, {"res": 3, "message": "Door open"})],
         "options": {"command_retries": 3}},
        {"replies": [TRIGGER, *[POLL_PENDING] * 10, TRIGGER, POLL_DONE], "options": {"command_retries": 2}},
    ]


REFRESHABLE = {**LIVE_SESSION, "refresh_token": "REFRESH-synthetic"}
REFRESHED = {"signToken": "SIGN-2", "encryToken": "ENCRY-2"}
#: Guest grants (pyBYD #103): basic control and charging, no location, no battery heat.
RANGE_TREE = [
    {"code": "2", "name": "Keys & Control", "childList": [{"code": "21", "name": "Basic control", "childList": []}]},
    {"code": "4", "name": "More control", "childList": [{"code": "41", "name": "Scheduled charging", "childList": []}]},
]
GUEST_CAPS = {"cfFixedList": [{"functionNo": no} for no in
                              ["1001", "1005", "1026", "10020004", "10020001", "10100001", "1012"]]}


def guest_cases():
    """A shared guest's grant decides which commands are sent."""
    grants = [RANGE_TREE, [{"code": "31"}], [{"code": "2", "childList": [{"code": "21"}, {"code": "1005"}]}],
              [{"code": "21"}, {"code": "2101"}], []]
    for grant in grants:
        vehicle = {**VEHICLE, "empowerType": -1, "empowerId": 114149, "rangeDetailList": grant}
        state = logged_in(vehicles=[vehicle], capabilities={VIN: GUEST_CAPS}, commands_enabled=True,
                          guest_vins={VIN: True})
        for method, extra in [("lock", {}), ("close_windows", {}), ("set_battery_heat", {"params": {"on": True}}),
                              ("start_charging", {}), ("start_climate", {"params": {}})]:
            pwd = {} if method == "start_charging" else {"command_pwd": PIN}
            yield f"client.{method}", client_case(state=state, replies=[TRIGGER, POLL_DONE, reply({"res": 2})], vin=VIN,
                                                  **extra, **pwd)
    # A permissive gate policy never sends what the grant excludes: with the
    # car's capabilities unknown (an empty list) or the seat unknown
    # (seat_target_unknown, decided before the grant), and an owner or a
    # guest whose grant allows the command is let through.
    bypass = {"gate_bypass": ["function_no_missing", "learn_info_missing", "seat_target_unknown"]}
    for grant in [[{"code": "2", "childList": [{"code": "21"}, {"code": "1005"}]}],
                  [{"code": "4", "childList": [{"code": "43"}]}], RANGE_TREE, None]:
        if grant is None:
            vehicle, guests = {**VEHICLE, "empowerType": 2}, {}
        else:
            vehicle, guests = {**VEHICLE, "empowerType": -1, "empowerId": 114149, "rangeDetailList": grant}, {VIN: True}
        state = logged_in(vehicles=[vehicle], capabilities={VIN: {"cfFixedList": []}}, commands_enabled=True,
                          guest_vins=guests)
        for method, extra in [("lock", {}), ("close_windows", {}), ("set_seat_climate", {"params": {}})]:
            yield f"client.{method}", client_case(state=state, replies=[TRIGGER, POLL_DONE], options=bypass, vin=VIN,
                                                  command_pwd=PIN, **extra)
    owner = {**VEHICLE, "empowerType": 2, "rangeDetailList": [{"code": "31"}]}
    yield "client.set_battery_heat", client_case(
        state=logged_in(vehicles=[owner], capabilities={VIN: GUEST_CAPS}, commands_enabled=True),
        replies=[TRIGGER, POLL_DONE], vin=VIN, params={"on": True}, command_pwd=PIN,
    )


def gate_and_pin_cases(method: str, base: dict) -> list[dict]:
    blocked = logged_in(vehicles=[VEHICLE], capabilities={VIN: {"cfFixedList": []}}, commands_enabled=True)
    return [
        # commands not enabled, no PIN anywhere, a PIN from the config, digests
        client_case(state=logged_in(vehicles=[VEHICLE], capabilities={VIN: ALL_FUNCTIONS_CONFIG}), **base),
        client_case(state=ready_state(), **{**base, "command_pwd": None}),
        client_case(state=ready_state(), config={**SAMPLE_CONFIG, "control_pin": "9999"}, replies=[TRIGGER, POLL_DONE], **{**base, "command_pwd": None}),
        client_case(state=ready_state(), replies=[TRIGGER, POLL_DONE], **{**base, "command_pwd": HEX}),
        client_case(state=ready_state(), replies=[TRIGGER, POLL_DONE], options={"pin_passthrough": True}, **{**base, "command_pwd": f" {HEX} "}),
        client_case(state=ready_state(), replies=[TRIGGER, POLL_DONE], **{**base, "command_pwd": " 1234 "}),
        # the gate: blocked, bypassed, capabilities fetched first
        client_case(state=blocked, **base),
        client_case(state=blocked, replies=[TRIGGER, POLL_DONE], options={"gate_bypass": ["function_no_missing", "learn_info_missing"]}, **base),
        client_case(
            state=logged_in(vehicles=[VEHICLE], commands_enabled=True),
            replies=[reply({VIN: ALL_FUNCTIONS_CONFIG}), TRIGGER, POLL_DONE],
            **base,
        ),
        client_case(state=logged_in(vehicles=[VEHICLE], commands_enabled=True), replies=[error_reply("1001")], **base),
        client_case(
            state=logged_in(capabilities={VIN: ALL_FUNCTIONS_CONFIG}, commands_enabled=True),
            replies=[reply([VEHICLE]), TRIGGER, POLL_DONE],
            **base,
        ),
        client_case(
            state=logged_in(capabilities={VIN: ALL_FUNCTIONS_CONFIG}, commands_enabled=True),
            replies=[error_reply("9999"), TRIGGER, POLL_DONE],
            **base,
        ),
    ]


@group.recorded
def recorded():  # noqa: C901
    # verify_command_access: success, every PIN error, no PIN
    for replies, kwargs in [
        ([reply({"ok": True})], {"command_pwd": PIN}),
        ([reply({"ok": "yes"})], {"command_pwd": HEX}),
        ([reply({})], {"command_pwd": " 1234 "}),
        ([error_reply("5005")], {"command_pwd": PIN}),
        ([error_reply("5006")], {"command_pwd": PIN}),
        ([error_reply("5011")], {"command_pwd": PIN}),
        ([error_reply("1001")], {"command_pwd": PIN}),
        ([error_reply("1002"), *login_replies(), reply({"ok": True})], {"command_pwd": PIN}),
        ([], {"command_pwd": None}),
        ([], {"command_pwd": ""}),
    ]:
        yield "client.verify_command_access", client_case(state=logged_in(), replies=replies, vin=VIN, **kwargs)
    yield "client.verify_command_access", client_case(
        state=logged_in(), config={**SAMPLE_CONFIG, "control_pin": "4321"}, replies=[reply({"ok": True})], vin=VIN
    )
    yield "client.verify_command_access", client_case(
        state=logged_in(), options={"pin_passthrough": True}, replies=[reply({"ok": True})], vin=VIN, command_pwd=HEX
    )
    # once verified, commands work; a failed check leaves them disabled
    yield "client.lock", client_case(
        state=logged_in(vehicles=[VEHICLE], capabilities={VIN: ALL_FUNCTIONS_CONFIG}),
        setup=[{"method": "verify_command_access", "args": {"vin": VIN, "command_pwd": PIN}}],
        replies=[reply({"ok": True}), TRIGGER, POLL_DONE],
        vin=VIN,
        command_pwd=PIN,
    )
    yield "client.lock", client_case(
        state=logged_in(vehicles=[VEHICLE], capabilities={VIN: ALL_FUNCTIONS_CONFIG}),
        setup=[{"method": "verify_command_access", "args": {"vin": VIN, "command_pwd": PIN}}],
        replies=[error_reply("5005")],
        vin=VIN,
        command_pwd=PIN,
    )

    # every outcome with lock; a success and the gate/PIN cases with every command
    for script in outcome_scripts():
        script = dict(script)
        state = script.pop("state", ready_state())
        yield "client.lock", client_case(state=state, vin=VIN, command_pwd=PIN, **script)
    for method in SIMPLE_COMMANDS:
        yield f"client.{method}", client_case(state=ready_state(), replies=[TRIGGER], mqtt=[ack(1.0, {"res": 2})], vin=VIN, command_pwd=PIN)
        yield f"client.{method}", client_case(state=ready_state(), replies=[TRIGGER, POLL_DONE], vin=VIN, command_pwd=PIN, options={"command_retries": 1})
        for case in gate_and_pin_cases(method, {"vin": VIN, "command_pwd": PIN}):
            yield f"client.{method}", case
    for method, param_sets in COMMAND_PARAMS.items():
        for params in param_sets:
            yield f"client.{method}", client_case(state=ready_state(), replies=[TRIGGER, POLL_DONE], vin=VIN, params=params, command_pwd=PIN)
        for case in gate_and_pin_cases(method, {"vin": VIN, "params": param_sets[0], "command_pwd": PIN}):
            yield f"client.{method}", case
        yield f"client.{method}", client_case(state=ready_state(), replies=[TRIGGER, POLL_FAILED] * 3, vin=VIN, params=param_sets[0], command_pwd=PIN)
    # parameters the model refuses
    yield "client.start_climate", client_case(state=ready_state(), vin=VIN, params={"temperature": "hot"}, command_pwd=PIN)
    yield "client.set_seat_climate", client_case(state=ready_state(), vin=VIN, params={"main_heat": 9}, command_pwd=PIN)
    # seats: a rear seat or a missing chair type (upstream test_rear_seat_climate)
    rear_only = {"cfFixedList": [{"functionNo": no} for no in ["10030006", "10030007"]]}
    for chair, config in [("3", rear_only), ("4", rear_only), ("1", rear_only), ("9", ALL_FUNCTIONS_CONFIG)]:
        yield "client.set_seat_climate", client_case(
            state=logged_in(vehicles=[VEHICLE], capabilities={VIN: config}, commands_enabled=True),
            replies=[TRIGGER, POLL_DONE],
            vin=VIN,
            params={"chair_type": chair, "lr_seat_heat_state": 2},
            command_pwd=PIN,
        )
    # battery heat: the dedicated endpoint pair when only 10100001 is listed (upstream test_battery_heat)
    for config in [DEDICATED_HEAT_CONFIG, {"cfFixedList": [{"functionNo": no} for no in ["10100001", "10300002"]]}, ALL_FUNCTIONS_CONFIG]:
        for on in (True, False):
            yield "client.set_battery_heat", client_case(
                state=logged_in(vehicles=[VEHICLE], capabilities={VIN: config}, commands_enabled=True),
                replies=[TRIGGER, POLL_PENDING, POLL_DONE],
                vin=VIN,
                params={"on": on},
                command_pwd=PIN,
            )
    # battery heat listed only as 10100002: the gate stops it, and past the gate it takes the dedicated pair too
    for functions in (["10100002"], ["10100002", "10300002"]):
        config = {"cfFixedList": [{"functionNo": no} for no in functions]}
        for options in ({}, {"gate_bypass": ["function_no_missing"]}):
            yield "client.set_battery_heat", client_case(
                state=logged_in(vehicles=[VEHICLE], capabilities={VIN: config}, commands_enabled=True),
                replies=[TRIGGER, POLL_PENDING, POLL_DONE],
                options=options,
                vin=VIN,
                params={"on": True},
                command_pwd=PIN,
            )
    # open windows: the learn info gate (upstream test_windows_open_available)
    windows = {"cfFixedList": [{"functionNo": "1026"}]}
    for learn in [None, 1, 0, -1]:
        vehicle = dict(VEHICLE)
        if learn is not None:
            vehicle["vehicleFunLearnInfo"] = {"openWindowLearnInfo": learn}
        for options in ({}, {"gate_bypass": ["learn_info_missing"]}):
            yield "client.open_windows", client_case(
                state=logged_in(vehicles=[vehicle], capabilities={VIN: windows}, commands_enabled=True),
                replies=[TRIGGER, POLL_DONE],
                options=options,
                vin=VIN,
                command_pwd=PIN,
            )

    # charging
    charge_trigger = reply({"requestSerial": "S1"})
    charge_pending = reply({"res": 1, "requestSerial": "S1"})
    for method, extra in [
        ("start_charging", {}),
        ("save_charging_schedule", {"start_charge_time": "22:00", "end_charge_time": "full", "charge_way": "e", "enabled": True}),
    ]:
        scripts = [
            {"replies": [charge_trigger], "mqtt": [mqtt_event(1.0, "smartCharge", {"respondData": {"res": 2}}, uuid="S1")]},
            {"replies": [charge_trigger], "mqtt": [mqtt_event(1.0, "smartCharge", {"respondData": {"res": 3, "message": "x"}}, uuid="S1")]},
            {"replies": [charge_trigger, charge_pending, reply({"res": 2})]},
            {"replies": [charge_trigger, *[charge_pending] * 6]},
            {"replies": [charge_trigger, reply({"res": 3})]},
            {"replies": [charge_trigger, reply({"res": 9, "message": "bad"})]},
            {"replies": [charge_trigger, reply({"res": True})]},
            {"replies": [reply({"res": 2})]},
            {"replies": [reply({})]},
            {"replies": [error_reply("6002")]},
            {"replies": [error_reply("1001")]},
            {"replies": [charge_trigger, error_reply("6051"), error_reply("6051")]},
            {"replies": [error_reply("1002"), *login_replies(), charge_trigger, reply({"res": 2})]},
            # dispatched: an expiry while polling is not retried (pyBYD #111)
            {"replies": [charge_trigger, error_reply("1002"), *login_replies(), charge_trigger, reply({"res": 2})],
             "mqtt_timeout": 0.0},
            {"replies": [charge_trigger, error_reply("1030"), charge_trigger], "mqtt_timeout": 0.0},
            {"replies": [error_reply("1003"), charge_trigger, reply({"res": 2})], "mqtt_timeout": 0.0},
            # the official ladder by default, a fixed interval when asked
            {"replies": [charge_trigger, *[charge_pending] * 11, reply({"res": 2})], "mqtt_timeout": 0.0,
             "poll_attempts": 12},
            {"replies": [charge_trigger, charge_pending, reply({"res": 2})], "mqtt_timeout": 0.0, "poll_interval": 2.0},
            {"replies": [charge_trigger, reply({"res": 2})], "mqtt_timeout": 0.0, "poll_interval": 1.0, "poll_attempts": 1},
            {"replies": [charge_trigger, reply({"res": 2})], "state": ready_state(mqtt=False)},
            {"replies": [], "state": logged_in(vehicles=[VEHICLE], capabilities={VIN: {"cfFixedList": []}})},
            {"replies": [charge_trigger, reply({"res": 2})], "state": logged_in(vehicles=[VEHICLE], capabilities={VIN: {"cfFixedList": []}}), "options": {"gate_bypass": ["function_no_missing"]}},
            {"replies": [reply({VIN: ALL_FUNCTIONS_CONFIG}), charge_trigger, reply({"res": 2})], "state": logged_in(vehicles=[VEHICLE])},
        ]
        for script in scripts:
            script = dict(script)
            state = script.pop("state", ready_state())
            yield f"client.{method}", client_case(state=state, vin=VIN, **extra, **script)
    yield "client.save_charging_schedule", client_case(
        state=ready_state(), replies=[charge_trigger, reply({"res": 2})], vin=VIN,
        start_charge_time="00:30", end_charge_time="06:00", charge_way="0,1,2", enabled=False, **FAST,
    )
    # stop_charging never sends anything (upstream test_stop_charging)
    yield "client.stop_charging", client_case(state=ready_state(), vin=VIN)
    yield "client.stop_charging", client_case(vin=VIN)
    yield from guest_cases()
    for enable, replies in [
        (True, [reply({"result": "ok"})]),
        (False, [reply({})]),
        (True, [error_reply("1001")]),
        (True, [error_reply("1002"), *login_replies(), reply({})]),
    ]:
        yield "client.toggle_smart_charging", client_case(state=logged_in(), replies=replies, vin=VIN, enable=enable)


WEIGHTED = ["lock", "unlock", "verify_command_access", "start_charging", "save_charging_schedule"] * 3 + METHODS


@group.fuzz
def fuzz(rng):
    while True:
        method = rng.choice(WEIGHTED)
        yield f"client.{method}", random_client_case(rng, method)


keep_representable(group)
