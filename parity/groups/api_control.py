"""pybyd/_api/control.py: remote commands compared by trace.

The flows run against scripted replies. The two hooks of
``poll_remote_control`` are scripted too, and record steps of their own:

- ``mqtt_result_waiter``: ``null`` for none, else ``{"sleep": seconds,
  "raises": bool, "result": raw}``. The waiter records ``{"mqtt_wait":
  serial}``, sleeps when told, raises when told, and otherwise returns
  ``RemoteControlResult.model_validate(result)`` (``None`` for null).
- ``on_trigger_dispatched``: false for none; ``true`` records
  ``{"dispatched": serial}``; ``"raises"`` records and then raises.

Results that are models are compared as ``model_dump(mode="json")``.
"""

from __future__ import annotations

import asyncio
import copy

from pybyd._api import control
from pybyd.models.control import BatteryHeatParams, RemoteCommand, RemoteControlResult

from parity.api_support import (
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
from parity.groups.pyjson import random_json, random_object

group = Group("api_control", crate="client")

TRIGGER = "/control/remoteControl"
RESULT = "/control/remoteControlResult"
HEAT_TRIGGER = "/control/remoteControlBatteryHeat"
HEAT_RESULT = "/control/remoteControlBatteryHeatResult"
VERIFY = "/vehicle/vehicleswitch/verifyControlPassword"
ENDPOINTS = [TRIGGER, RESULT, HEAT_TRIGGER, HEAT_RESULT, VERIFY, "/control/other"]
COMMANDS = [command.value for command in RemoteCommand]
PIN = "E10ADC3949BA59ABBE56E057F20F883E"
OWNER = {VIN: False}
GUEST = {VIN: True}


# ---------------------------------------------------------------------------
# Oracles
# ---------------------------------------------------------------------------


def _hooks(transport, mqtt_result_waiter, on_trigger_dispatched):
    steps = transport.steps
    waiter = None
    dispatched = None
    if mqtt_result_waiter is not None:
        script = mqtt_result_waiter

        async def waiter(serial):
            steps.append({"mqtt_wait": copy.deepcopy(serial)})
            if script.get("sleep") is not None:
                await asyncio.sleep(script["sleep"])
            if script.get("raises"):
                raise RuntimeError("scripted MQTT failure")
            if script.get("result") is None:
                return None
            return RemoteControlResult.model_validate(copy.deepcopy(script["result"]))

    if on_trigger_dispatched:

        def dispatched(serial):
            steps.append({"dispatched": copy.deepcopy(serial)})
            if on_trigger_dispatched == "raises":
                raise RuntimeError("scripted callback failure")

    return waiter, dispatched


def _command_flow(function):
    async def run(config, session, transport, *, vin, command, mqtt_result_waiter=None,
                  on_trigger_dispatched=False, **kwargs):
        waiter, dispatched = _hooks(transport, mqtt_result_waiter, on_trigger_dispatched)
        result = await function(
            config, session, transport, vin, RemoteCommand(command),
            mqtt_result_waiter=waiter, on_trigger_dispatched=dispatched, **kwargs,
        )
        return result.model_dump(mode="json")

    return run


async def _fetch_control_endpoint(config, session, transport, *, endpoint, vin, command, **kwargs):
    result = await control._fetch_control_endpoint(
        endpoint, config, session, transport, vin, RemoteCommand(command), **kwargs
    )
    return list(result)


async def _verify_control_password(config, session, transport, *, vin, command_pwd):
    result = await control.verify_control_password(config, session, transport, vin, command_pwd)
    return result.model_dump(mode="json")


flow_oracle(group, "api.control.poll_remote_control", _command_flow(control.poll_remote_control))
flow_oracle(group, "api.control._poll_remote_control_once", _command_flow(control._poll_remote_control_once))
flow_oracle(group, "api.control._fetch_control_endpoint", _fetch_control_endpoint)
flow_oracle(group, "api.control.verify_control_password", _verify_control_password)


@group.oracle("api.control._is_remote_control_ready", errors=(ValueError, TypeError, AttributeError))
def is_remote_control_ready(data) -> bool:
    return control._is_remote_control_ready(data)


@group.oracle("api.control.parse_remote_control_result_data", errors=(ValueError, TypeError))
def parse_remote_control_result_data(data) -> dict:
    return control.parse_remote_control_result_data(data).model_dump(mode="json")


@group.oracle("api.control.constants")
def constants() -> dict:
    def code_map(mapping):
        return [[sorted(codes), cls.__name__] for codes, cls in mapping.items()]

    return {
        "_CONTROL_PASSWORD_ERROR_CODES": sorted(control._CONTROL_PASSWORD_ERROR_CODES),
        "_REMOTE_CONTROL_SERVICE_ERROR_CODES": sorted(control._REMOTE_CONTROL_SERVICE_ERROR_CODES),
        "_REMOTE_CONTROL_GENERIC_ERROR_CODES": sorted(control._REMOTE_CONTROL_GENERIC_ERROR_CODES),
        "_REMOTE_CONTROL_ENDPOINTS": sorted(control._REMOTE_CONTROL_ENDPOINTS),
        "_VERIFY_CONTROL_PASSWORD_ENDPOINT": control._VERIFY_CONTROL_PASSWORD_ENDPOINT,
        "_CONTROL_EXTRA_CODES": code_map(control._CONTROL_EXTRA_CODES),
        "_REMOTE_CONTROL_EXTRA_CODES": code_map(control._REMOTE_CONTROL_EXTRA_CODES),
    }


# ---------------------------------------------------------------------------
# Replies
# ---------------------------------------------------------------------------


def ok(data) -> dict:
    return ok_reply(SAMPLE_SESSION, data)


SERIAL = ok({"requestSerial": "S1"})
PENDING = [ok({"controlState": 0}), ok({"res": 1}), ok({"requestSerial": "S1"}), ok({}), ok({"controlState": None})]
SUCCESS = [ok({"controlState": 1}), ok({"res": 2, "message": "ok"}), ok({"result": "done"}), ok({"controlState": "1"})]
FAILURE = [ok({"controlState": 2, "message": "refused"}), ok({"res": 3, "msg": "no"}), ok({"res": "7"}), ok({"controlState": 2})]
ODD = [
    ok({"controlState": "x"}),
    ok({"res": "two"}),
    ok({"res": [1]}),
    ok({"controlState": 5}),
    ok({"controlState": 1.9}),
    ok({"res": 99999999999999999999.0}),
    ok({"res": True}),
    ok({"controlState": 1, "success": "yes"}),
    ok({"controlState": 1, "requestSerial": 5}),
    ok([1, 2]),
    ok("text"),
    ok(None),
    raw_reply({"code": "0"}),
    raw_reply([]),
    sealed_reply(SAMPLE_SESSION, "{not json"),
]
# Codes a trigger or poll can fail with: the general ones, the control
# password, remote control service and generic refusals, busy, unreachable,
# rate limited, not a guest, unmapped.
CODES = [
    "1001", "1002", "1011", "3001", "3003", "1005", "1010", "1008", "1009", "5005", "5006", "5011", "6002", "6024", "8003",
    "9999",
]


def command_scripts() -> list[dict]:
    """``scenario`` keywords for the command flows: a script of replies plus options."""
    fast = {"poll_attempts": 3, "poll_interval": 1.5}
    scripts = []
    # Final at once on the trigger.
    for reply in SUCCESS + FAILURE:
        scripts.append({"replies": [reply]})
    # Serial, then each pending shape, then a final one.
    for pending in PENDING:
        for final in SUCCESS + FAILURE:
            scripts.append({"replies": [SERIAL, pending, final], **fast})
    # In progress until the attempts run out.
    for pending in PENDING[:3]:
        scripts.append({"replies": [SERIAL] + [pending] * 3, **fast})
    scripts.append({"replies": [SERIAL] + [ok({"res": 1})] * 10})
    # Odd shapes on the trigger and on a poll.
    for reply in ODD:
        scripts.append({"replies": [reply, SERIAL, ok({"controlState": 1})], **fast})
        scripts.append({"replies": [SERIAL, reply, ok({"controlState": 1})], **fast})
    # No serial to poll with.
    for reply in [ok({}), ok({"requestSerial": ""}), ok({"requestSerial": None}), ok({"requestSerial": 0}), ok([])]:
        scripts.append({"replies": [reply, ok({"controlState": 1})]})
    # Serials that are not strings, and a serial that changes.
    scripts.append({"replies": [ok({"requestSerial": 5}), ok({"res": 2})], **fast})
    scripts.append({"replies": [SERIAL, ok({"res": 1, "requestSerial": "S2"}), ok({"res": 2})], **fast})
    scripts.append({"replies": [SERIAL, ok({"res": 1, "requestSerial": ""}), ok({"res": 2})], **fast})
    # Each code on the trigger, then on a poll.
    for code in CODES:
        scripts.append({"replies": [error_reply(code), SERIAL, ok({"res": 2})], **fast})
        scripts.append({"replies": [SERIAL, error_reply(code), ok({"res": 2})], **fast})
    scripts.append({"replies": [error_reply(6024), SERIAL, ok({"res": 2})], **fast})
    scripts.append({"replies": [transport_error(502)]})
    scripts.append({"replies": [SERIAL, transport_error(None)], **fast})
    scripts.append({"replies": [SERIAL, error_reply("9999"), error_reply("1009"), error_reply("1008")], **fast})
    # 6024 retries: each count, then out of retries, and another error after it.
    for count in range(1, 4):
        scripts.append({"replies": [error_reply("6024")] * count + [SERIAL, ok({"res": 2})], **fast})
    scripts.append({"replies": [error_reply("6024"), error_reply("1009")], **fast})
    scripts.append({"replies": [error_reply("6024")] * 2 + [SERIAL, ok({"res": 2})], "rate_limit_retries": 2})
    scripts.append({"replies": [error_reply("6024"), SERIAL, ok({"res": 2})], "rate_limit_retries": 1})
    scripts.append({"replies": [SERIAL], "rate_limit_retries": 0})
    scripts.append({"replies": [error_reply("6024")] * 4, "rate_limit_retries": 4, "rate_limit_delay": 0.25})
    # Options at their edges.
    scripts.append({"replies": [SERIAL, ok({"res": 2})], "poll_interval": 0})
    scripts.append({"replies": [SERIAL, ok({"res": 2})], "poll_interval": -1.0})
    scripts.append({"replies": [SERIAL], "poll_attempts": 0})
    scripts.append({"replies": [SERIAL, ok({"res": 2})], "poll_attempts": 1, "poll_interval": 0.1})
    scripts.append({"replies": []})
    return scripts


WAITERS = [
    {"result": None},
    {"result": {"controlState": 1, "requestSerial": "S1"}},
    {"result": {"res": 2}},
    {"result": {"controlState": 0}},
    {"result": {"res": 1}},
    {"result": {"controlState": 2}},
    {"result": {"res": 3, "message": "no"}},
    {"result": {"res": "x"}},
    {"result": [1]},
    {"raises": True},
    {"sleep": 30.0, "result": None},
    {"sleep": 2.5, "result": {"controlState": 1}},
]

HEAT_DEDICATED = BatteryHeatParams(on=True).to_dedicated_control_params_map()
HEAT_LEGACY = BatteryHeatParams(on=True).to_control_params_map()
PARAMS = [None, {}, {"temperature": 21, "copilotTemperature": 21.5, "name": "é😀"}, HEAT_LEGACY, HEAT_DEDICATED]


def upstream_cases():
    """The assertions of pyBYD's own tests, as trace cases."""
    # test_command_outcomes: no serial -> code "no_serial"; never final -> code "timeout".
    yield "api.control._poll_remote_control_once", scenario(
        guest_vins=OWNER, replies=[ok({})], vin=VIN, command="FLASHLIGHTNOWHISTLE", mqtt_result_waiter=None
    )
    yield "api.control._poll_remote_control_once", scenario(
        guest_vins=OWNER,
        replies=[ok({"requestSerial": "S1"}), ok({"res": 1}), ok({"res": 1})],
        vin=VIN, command="FLASHLIGHTNOWHISTLE", poll_attempts=2, poll_interval=0, mqtt_result_waiter=None,
    )
    # test_battery_heat: the legacy route, the dedicated endpoints, a rejection.
    heat = {"vin": VIN, "command": "BATTERYHEAT", "command_pwd": "deadbeef", "poll_interval": 0.0, "command_retries": 1}
    yield "api.control.poll_remote_control", scenario(
        guest_vins=OWNER, replies=[ok({"requestSerial": "SERIAL_LEGACY"}), ok({"res": 1, "controlState": 1})],
        control_params=HEAT_LEGACY, **heat,
    )
    yield "api.control.poll_remote_control", scenario(
        guest_vins=OWNER,
        replies=[ok({"requestSerial": "SERIAL123"}), ok({"res": 1, "message": "succcess"}),
                 ok({"res": 2, "message": "succcess"})],
        control_params=HEAT_DEDICATED, **heat,
    )
    yield "api.control.poll_remote_control", scenario(
        guest_vins=OWNER,
        replies=[ok({"requestSerial": "SERIAL456"}),
                 ok({"res": 3, "message": "Las condiciones no se cumplen. No se puede activar el precalentamiento"})],
        control_params=HEAT_DEDICATED, **heat,
    )
    # test_control_password_errors and test_service_busy_error_mapping, on the real endpoints.
    for code in ["5005", "5006", "5011", "5999", "1008", "6002", "9999"]:
        yield "api.control.verify_control_password", scenario(
            replies=[error_reply(code, "test-message")], vin=VIN, command_pwd=PIN
        )
    for code in ["5005", "5006", "5011", "1009", "1008", "6002", "9999"]:
        yield "api.control._fetch_control_endpoint", scenario(
            guest_vins=OWNER, replies=[error_reply(code, "test-message")],
            endpoint=TRIGGER, vin=VIN, command="LOCKDOOR", command_pwd=PIN,
        )
    # test_poll_ladder (#101): the 33 s ladder by default, sleeping 1, 0.5, 0.5, 2 before a final poll.
    flash = {"vin": VIN, "command": "FLASHLIGHTNOWHISTLE", "command_pwd": PIN}
    yield "api.control.poll_remote_control", scenario(
        guest_vins=OWNER, replies=[ok({"controlState": 0, "requestSerial": "ser-1"})] * 4
        + [ok({"controlState": 1, "requestSerial": "ser-1"})], **flash,
    )
    yield "api.control.poll_remote_control", scenario(
        guest_vins=OWNER, replies=[ok({"requestSerial": "ser-1"})] + [ok({"res": 1})] * 12, poll_attempts=12, **flash,
    )
    # test_command_outcomes (#107): one attempt by default, a refusal never replayed.
    for replies in [[ok({"requestSerial": "S1"}), ok({"res": 3, "message": "Door is open"})],
                    [ok({"requestSerial": "S1"}), ok({"res": 1})],
                    [ok({"controlState": 2, "message": "Refused immediately"})],
                    [ok({"res": 3, "message": "Immediate res=3"})],
                    [ok({"requestSerial": "S1"}), ok({"controlState": 2, "message": "Poll controlState=2"})]]:
        for retries in [None, 3]:
            extra = {} if retries is None else {"command_retries": retries}
            yield "api.control.poll_remote_control", scenario(
                guest_vins=OWNER, replies=replies * 3, poll_interval=0.0, poll_attempts=1, **extra, **flash,
            )
    # A timeout and a 1009 are retried when asked for, a refusal stops the retries.
    yield "api.control.poll_remote_control", scenario(
        guest_vins=OWNER, poll_interval=0.0, poll_attempts=1, command_retries=3, command_retry_delay=0.0,
        replies=[SERIAL, ok({"res": 1}), error_reply("1009"), SERIAL, ok({"res": 2})], **flash,
    )
    yield "api.control.poll_remote_control", scenario(
        guest_vins=OWNER, poll_interval=0.0, poll_attempts=1, command_retries=3, command_retry_delay=0.0,
        replies=[SERIAL, ok({"res": 1}), SERIAL, ok({"res": 3, "message": "Rejected"}), SERIAL, ok({"res": 2})],
        **flash,
    )
    # An MQTT refusal ends the command at once; a failing waiter falls back to polling.
    for result in [{"res": 3, "message": "Door open", "requestSerial": "S1"}, {"controlState": 2},
                   {"controlState": 2, "msg": "no"}, {"controlState": 2, "message": "", "msg": 0}]:
        yield "api.control.poll_remote_control", scenario(
            guest_vins=OWNER, replies=[SERIAL, ok({"res": 2})], poll_interval=0.0, command_retries=3,
            mqtt_result_waiter={"result": result}, **flash,
        )
    yield "api.control.poll_remote_control", scenario(
        guest_vins=OWNER, replies=[SERIAL, ok({"res": 2, "requestSerial": "S1"})], poll_interval=0.0,
        mqtt_result_waiter={"raises": True}, **flash,
    )
    # 6024 on the trigger is still retried with one attempt.
    yield "api.control.poll_remote_control", scenario(
        guest_vins=OWNER, replies=[error_reply("6024"), SERIAL, ok({"res": 2})], poll_interval=0.0,
        rate_limit_delay=0.0, **flash,
    )


@group.recorded
def recorded():
    yield "api.control.constants", {}
    samples = [{}, {"controlState": 0}, {"controlState": None, "res": None}, {"controlState": "1"}, {"res": 1},
               {"res": 2}, {"res": "3"}, {"result": None}, {"controlState": "x"}, {"res": []}, {"a": 1},
               None, [], "", 0, [1], "x", 5, {"res": 1.9}, {"res": True}, {"controlState": -1}]
    for sample in samples:
        yield "api.control._is_remote_control_ready", {"data": sample}
        yield "api.control.parse_remote_control_result_data", {"data": sample}
    yield from upstream_cases()

    # verify_control_password: success shapes, every code, owner and guest
    # (no userType retry: it is a plain token request).
    for reply in [ok({"ok": True}), ok({"ok": "yes", "vin": "OTHER"}), ok({}), ok([1]), ok("text"),
                  raw_reply({"code": "0"}), transport_error(500), *(error_reply(code) for code in CODES)]:
        for guests in [OWNER, GUEST]:
            yield "api.control.verify_control_password", scenario(
                guest_vins=guests, replies=[reply], vin=VIN, command_pwd=PIN
            )

    # _fetch_control_endpoint: every endpoint, serials, parameters, codes.
    data_replies = [ok({"controlState": 1}), ok({"requestSerial": "S2"}), ok({"requestSerial": ""}),
                    ok({"requestSerial": 7}), ok([1]), ok("text"), ok({}), *(error_reply(code) for code in CODES)]
    for endpoint in ENDPOINTS:
        for serial in [None, "", "S1", 5]:
            for reply in data_replies:
                yield "api.control._fetch_control_endpoint", scenario(
                    guest_vins=OWNER, replies=[reply], endpoint=endpoint, vin=VIN, command="LOCKDOOR",
                    control_params=None, command_pwd=None, request_serial=serial,
                )
    for params in PARAMS:
        for pwd in [None, "", PIN]:
            yield "api.control._fetch_control_endpoint", scenario(
                guest_vins=OWNER, replies=[ok({"requestSerial": "S"})], endpoint=TRIGGER, vin=VIN,
                command="OPENAIR", control_params=params, command_pwd=pwd,
            )
    for script in vehicle_scripts(TRIGGER, ok({"requestSerial": "S9"})):
        yield "api.control._fetch_control_endpoint", scenario(**script, endpoint=TRIGGER, vin=VIN, command="LOCKDOOR")

    # The command flows: every script, once and with the retries.
    for script in command_scripts():
        base = {"guest_vins": OWNER, "vin": VIN, "command": "LOCKDOOR", "command_pwd": PIN, **script}
        yield "api.control._poll_remote_control_once", scenario(**base)
        yield "api.control.poll_remote_control", scenario(**base)
        yield "api.control.poll_remote_control", scenario(**{**base, "command_retries": 1})

    # The retry loop at each count.
    refused = ok({"controlState": 2, "message": "refused"})
    for retries in [-1, 0, 1, 2, 3, 4]:
        for replies in [[refused] * 4, [refused, ok({"controlState": 1})], [error_reply("1009")] * 4,
                        [refused, error_reply("5005")], [SERIAL, ok({"res": 1}), SERIAL, ok({"res": 2})]]:
            yield "api.control.poll_remote_control", scenario(
                guest_vins=OWNER, replies=replies, vin=VIN, command="OPENDOOR", command_pwd=PIN,
                command_retries=retries, command_retry_delay=0.5, poll_attempts=1,
            )
    yield "api.control.poll_remote_control", scenario(
        guest_vins=OWNER, replies=[refused] * 3, vin=VIN, command="LOCKDOOR", command_retry_delay=0,
    )

    # Hooks: the MQTT waiter and the trigger callback.
    for waiter in WAITERS:
        for replies in [[SERIAL, ok({"res": 2})], [SERIAL, ok({"res": 3})], [SERIAL, ok({"res": 1})],
                        [ok({"controlState": 1})], [ok({})]]:
            yield "api.control.poll_remote_control", scenario(
                guest_vins=OWNER, replies=replies, vin=VIN, command="LOCKDOOR", command_pwd=PIN,
                mqtt_result_waiter=waiter, on_trigger_dispatched=True, poll_attempts=1, command_retries=2,
            )
    for dispatched in [True, "raises"]:
        for replies in [[SERIAL, ok({"res": 2})], [ok({}), ok({"controlState": 1})], [error_reply("6024"), SERIAL]]:
            yield "api.control._poll_remote_control_once", scenario(
                guest_vins=OWNER, replies=replies, vin=VIN, command="LOCKDOOR",
                on_trigger_dispatched=dispatched, poll_attempts=1,
            )
    yield "api.control._poll_remote_control_once", scenario(
        guest_vins=OWNER, replies=[ok({"requestSerial": 5}), ok({"res": 2})], vin=VIN, command="LOCKDOOR",
        mqtt_result_waiter={"result": None}, on_trigger_dispatched=True,
    )

    # Commands, parameters and battery heat routing.
    for command in COMMANDS:
        for params in PARAMS:
            yield "api.control._poll_remote_control_once", scenario(
                guest_vins=OWNER, replies=[SERIAL, ok({"res": 1}), ok({"res": 2})], vin=VIN, command=command,
                control_params=params, command_pwd=PIN, poll_interval=0,
            )

    # Owner and guest: the userType retry on the trigger and on a poll.
    for guests, wrong in [(OWNER, "1001"), (GUEST, "8003"), ({}, "1001")]:
        for replies in [[error_reply(wrong), SERIAL, ok({"res": 2})],
                        [SERIAL, error_reply(wrong), ok({"res": 2})],
                        [error_reply("1001"), error_reply("8003")],
                        [error_reply(wrong), error_reply("1009")]]:
            yield "api.control.poll_remote_control", scenario(
                guest_vins=guests, replies=replies, vin=VIN, command="LOCKDOOR", command_pwd=PIN,
                poll_attempts=2, command_retries=1,
            )
    yield "api.control.poll_remote_control", scenario(
        guest_vins=OWNER, user_type_settled=[(VIN, TRIGGER)], replies=[error_reply("1001")] * 3,
        vin=VIN, command="LOCKDOOR",
    )


# ---------------------------------------------------------------------------
# Fuzz
# ---------------------------------------------------------------------------


def random_command_reply(rng, session: dict, kind: str | None = None) -> dict:
    """One reply of a random kind: success, in progress, refused, busy,
    expired, rate limited or malformed, sealed for ``session``."""

    def sealed(data):
        return ok_reply(session, data)

    serial = rng.choice(["S1", "S2", "", None, 5, random_text(rng, 6)])
    kind = kind or rng.choice(["success", "progress", "progress", "progress", "refused", "busy", "expired",
                               "rate", "malformed"])
    if kind == "success":
        return sealed(rng.choice([{"controlState": 1}, {"res": 2}, {"result": "ok"}, {"controlState": "1"},
                                  {"controlState": 1, "requestSerial": serial}]))
    if kind == "progress":
        return sealed(rng.choice([{"controlState": 0}, {"res": 1}, {"requestSerial": serial}, {},
                                  {"res": 1, "requestSerial": serial}]))
    if kind == "serial":
        return sealed({"requestSerial": rng.choice(["S1", "S1", 5, random_text(rng, 6) or "S"])})
    if kind == "refused":
        return rng.choice([sealed({"controlState": 2, "message": random_text(rng, 10)}),
                           sealed({"res": rng.randint(3, 9)}),
                           error_reply(rng.choice(["1009", "1001", "5005", "5006", "5011", "8003"]))])
    if kind == "busy":
        return error_reply(rng.choice(["1008", "6002", "9999"]))
    if kind == "expired":
        return error_reply(rng.choice(["1002", "1011", "3001", "3003", "1005", "1010"]))
    if kind == "rate":
        return error_reply(rng.choice(["6024", "6024", 6024]))
    return rng.choice([
        random_reply(rng, session),
        sealed(random_json(rng)),
        sealed({"controlState": random_json(rng)}),
        sealed({"res": random_json(rng)}),
        sealed(random_object(rng)),
    ])


def random_command_script(rng, session: dict) -> list[dict]:
    """A random sequence of replies of random length. Most start like a real
    command (perhaps rate limited, then a serial) so the poll loop is reached."""
    replies = []
    if rng.random() < 0.7:
        replies += [random_command_reply(rng, session, "rate") for _ in range(rng.choice([0, 0, 0, 1, 2, 3]))]
        replies.append(random_command_reply(rng, session, "serial"))
    replies += [random_command_reply(rng, session) for _ in range(rng.randint(0, 12))]
    return replies


def random_waiter(rng):
    roll = rng.random()
    if roll < 0.5:
        return None
    script = {}
    if rng.random() < 0.3:
        script["sleep"] = rng.choice([0.0, 1.0, 30.0])
    if rng.random() < 0.15:
        script["raises"] = True
    script["result"] = rng.choice([None, {"controlState": rng.randint(0, 3)}, {"res": rng.randint(0, 4)},
                                   {"controlState": 1, "requestSerial": "S1"}, random_object(rng)])
    return script


def random_command_args(rng) -> dict:
    args = {
        "vin": rng.choice([VIN, VIN, "", random_text(rng, 17)]),
        "command": rng.choice(COMMANDS),
        "control_params": rng.choice(PARAMS + [random_object(rng)]),
        "command_pwd": rng.choice([None, "", PIN, random_text(rng, 8)]),
    }
    if rng.random() < 0.3 and args["command"] == "BATTERYHEAT":
        args["control_params"] = HEAT_DEDICATED
    for key, values in [
        ("poll_attempts", [-1, 0, 1, 2, 2, 3, 3, 10]),
        ("poll_interval", [0, 0.0, 1.5, -1.0, 0.25, None]),
        ("rate_limit_retries", [0, 1, 2, 3, 3, 3, 5]),
        ("rate_limit_delay", [0.0, 5.0, 0.5]),
        ("command_retries", [-1, 0, 1, 1, 1, 2, 2, 3, 3, 4]),
        ("command_retry_delay", [0.0, 3.0, 1.25]),
    ]:
        if rng.random() < 0.6:
            args[key] = rng.choice(values)
    waiter = random_waiter(rng)
    if waiter is not None:
        args["mqtt_result_waiter"] = waiter
    if rng.random() < 0.4:
        args["on_trigger_dispatched"] = rng.choice([True, "raises"])
    return args


@group.fuzz
def fuzz(rng):
    while True:
        sample = rng.choice([random_object(rng), random_json(rng), {"controlState": random_json(rng)},
                             {"res": random_json(rng)}, {"res": rng.randint(-3, 5), "controlState": rng.randint(0, 3)}])
        yield "api.control._is_remote_control_ready", {"data": sample}
        yield "api.control.parse_remote_control_result_data", {"data": sample}

        base = random_scenario_base(rng)
        session = base["session"]
        guest_vins = rng.choice([{}, OWNER, GUEST])
        settled = [[VIN, TRIGGER]] if rng.random() < 0.1 else []
        replies = random_command_script(rng, session)
        args = random_command_args(rng)
        name = rng.choice(["api.control.poll_remote_control", "api.control._poll_remote_control_once"])
        if name.endswith("_once"):
            # One attempt has no retry arguments.
            args.pop("command_retries", None)
            args.pop("command_retry_delay", None)
        yield name, {**base, "guest_vins": guest_vins, "user_type_settled": settled, "replies": replies,
                     "args": args}

        endpoint = rng.choice(ENDPOINTS + [random_text(rng, 10)])
        script = random_vehicle_script(rng, session, endpoint)
        yield "api.control._fetch_control_endpoint", {
            **base,
            "guest_vins": script["guest_vins"],
            "user_type_settled": [list(pair) for pair in script["user_type_settled"]],
            "replies": script["replies"],
            "args": {
                "endpoint": endpoint, "vin": script["vin"], "command": rng.choice(COMMANDS),
                "control_params": rng.choice(PARAMS + [random_object(rng)]),
                "command_pwd": rng.choice([None, "", PIN]),
                "request_serial": rng.choice([None, "", "S1", 5, 0, random_text(rng, 8)]),
            },
        }
        script = random_vehicle_script(rng, session, VERIFY)
        yield "api.control.verify_control_password", {
            **base,
            "guest_vins": script["guest_vins"],
            "replies": script["replies"],
            "args": {"vin": script["vin"], "command_pwd": rng.choice(["", PIN, random_text(rng, 8)])},
        }
