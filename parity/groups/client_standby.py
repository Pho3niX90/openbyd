"""pybyd/client.py: the preconditioning plan methods of BydClient, compared by trace.

These create, change, delete and run climate plans on real cars. A case
carries the models as JSON, built before the method is called as the pyBYD
caller builds them: ``plan`` and each of ``commands`` as the raw dict
``model_validate`` reads, ``schedule`` as ``StandbySchedule``'s keyword
arguments (or null for none).

``save``, ``setDefault`` and ``delete`` re-read the plans afterwards;
``create`` reads them before and after and wants exactly one new plan;
``run`` goes through ``_trigger_and_poll`` without waiting for MQTT and is
driven through every way out of its poll loop.
"""

from __future__ import annotations

import copy
from typing import Any

from pybyd.models.standby import StandbyPlan, StandbyPlanCommand, StandbySchedule

from parity import api_support
from parity.api_support import (
    NOW_MS,
    OLD_SESSION,
    VIN,
    client_case,
    client_oracle,
    error_reply,
    logged_in,
    login_replies,
    mqtt_event,
    ok_reply,
    raw_reply,
    transport_error,
)
from parity.flow_cases import keep_representable
from parity.framework import Group, random_text
from parity.groups import api_standby as flows
from parity.groups import models_standby as ms
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION
from parity.models_support import json_safe, jsonable

group = Group("client_standby", crate="client")

METHODS = [
    "get_standby_plans", "save_standby_plan", "create_standby_plan", "set_default_standby_plan", "delete_standby_plan",
    "run_standby_plan",
]


def _schedule(args: dict) -> StandbySchedule | None:
    return None if args.get("schedule") is None else StandbySchedule(**args["schedule"])


async def _get(client: Any, args: dict) -> Any:
    return jsonable(await client.get_standby_plans(args["vin"]))


async def _save(client: Any, args: dict) -> Any:
    plan = StandbyPlan.model_validate(args["plan"])
    return jsonable(await client.save_standby_plan(args["vin"], plan, schedule=_schedule(args)))


async def _create(client: Any, args: dict) -> Any:
    commands = [StandbyPlanCommand.model_validate(command) for command in args["commands"]]
    return jsonable(await client.create_standby_plan(args["vin"], args["name"], commands, schedule=_schedule(args)))


async def _set_default(client: Any, args: dict) -> Any:
    return jsonable(await client.set_default_standby_plan(args["vin"], args["plan_id"]))


async def _delete(client: Any, args: dict) -> Any:
    return jsonable(await client.delete_standby_plan(args["vin"], args["plan_id"]))


async def _run(client: Any, args: dict) -> Any:
    polls = {key: args[key] for key in ("poll_attempts", "poll_interval") if args.get(key) is not None}
    return jsonable(await client.run_standby_plan(args["vin"], args["plan_id"], **polls))


api_support.EXTRA_CLIENT_METHODS.update(
    get_standby_plans=_get,
    save_standby_plan=_save,
    create_standby_plan=_create,
    set_default_standby_plan=_set_default,
    delete_standby_plan=_delete,
    run_standby_plan=_run,
)
for name in METHODS:
    client_oracle(group, name)


def reply(data) -> dict:
    return ok_reply(SAMPLE_SESSION, data)


MADRID = {**SAMPLE_CONFIG, "time_zone": "Europe/Madrid"}
PLAN_ID = flows.PLAN_ID
PRESET = ms.PLAN
CREATED = {"id": "7", "planName": "Test plan", "planType": 2, "executeType": "0"}
LIST_PRESET = reply([PRESET])
LIST_BOTH = reply([PRESET, CREATED])
SAVED = reply({"message": "saved"})
COMMANDS = ms.CAPTURED_COMMANDS
#: Session expired, not supported, busy, unreachable, rate limited, unmapped; and the transport failing.
FAILURES = [error_reply("1001"), error_reply("1008"), error_reply("6002"), error_reply("6024"), error_reply("9999"),
            transport_error(500), raw_reply("text"), reply("text")]

SERIAL = reply({"requestSerial": "S1"})
PENDING = reply({"res": 1})
DONE = reply({"res": 2, "message": "ok"})
FAILED = reply({"res": 3, "message": "fail"})
FAST = {"poll_attempts": 3, "poll_interval": 0.5}


def run_scripts() -> list[dict]:
    """Every way a plan run ends, as ``client_case`` keywords."""
    return [
        # done after a poll or two, at the first poll, at the trigger
        {"replies": [SERIAL, PENDING, DONE]},
        {"replies": [SERIAL, PENDING, PENDING, PENDING, DONE]},
        {"replies": [SERIAL, DONE]},
        {"replies": [reply({"requestSerial": "S1", "res": 2})]},
        {"replies": [reply({"res": 2})]},
        # still running when polling gives up: returned as it is
        {"replies": [SERIAL, *[PENDING] * 10]},
        {"replies": [SERIAL, PENDING, PENDING, PENDING], **FAST},
        {"replies": [SERIAL, PENDING], "poll_attempts": 1, "poll_interval": 0.0},
        {"replies": [SERIAL], "poll_attempts": 0},
        {"replies": [reply({"requestSerial": "S1", "res": 1}), PENDING, DONE]},
        # no serial, or one that is not text
        {"replies": [reply({})]},
        {"replies": [reply({"message": "queued"})]},
        {"replies": [reply({"requestSerial": ""}), DONE]},
        {"replies": [reply({"requestSerial": None}), DONE]},
        {"replies": [reply({"requestSerial": 5}), DONE]},
        {"replies": [reply({"requestSerial": True}), PENDING, DONE]},
        {"replies": [reply({"requestSerial": [1]}), DONE]},
        {"replies": [reply({"requestSerial": {"a": 1}}), DONE]},
        {"replies": [reply([]), DONE]},
        {"replies": [reply("text"), DONE]},
        # rejected: at a poll, at the trigger, with odd codes
        {"replies": [SERIAL, FAILED]},
        {"replies": [SERIAL, PENDING, FAILED]},
        {"replies": [reply({"requestSerial": "S1", "res": 3, "message": "比亚迪"})]},
        {"replies": [SERIAL, reply({"res": 0})]},
        {"replies": [SERIAL, reply({"res": -1, "message": None})]},
        {"replies": [SERIAL, reply({"res": 4})]},
        {"replies": [SERIAL, reply({"res": False}), DONE]},
        {"replies": [SERIAL, reply({"res": True}), DONE]},
        {"replies": [SERIAL, reply({"res": 2.0}), DONE]},
        {"replies": [SERIAL, reply({"res": "2"}), DONE], **FAST},
        {"replies": [SERIAL, reply({"res": "x"})], **FAST},
        {"replies": [SERIAL, reply({"res": 2, "requestSerial": "OTHER", "vin": "OTHERVIN000000001"})]},
        # polls that fail: skipped, twice unreachable, the session expiring, the transport
        {"replies": [SERIAL, error_reply("1001"), DONE]},
        {"replies": [SERIAL, error_reply("9999"), error_reply("1008"), DONE]},
        {"replies": [SERIAL, error_reply("6002"), DONE]},
        {"replies": [SERIAL, error_reply("6002"), error_reply("6002"), DONE]},
        {"replies": [SERIAL, PENDING, error_reply("6002"), error_reply("6002"), DONE]},
        {"replies": [SERIAL, error_reply("6002"), PENDING, error_reply("6002"), DONE], "poll_attempts": 4},
        {"replies": [SERIAL, transport_error(500), DONE]},
        {"replies": [SERIAL, raw_reply("text"), reply("text"), reply([]), DONE]},
        {"replies": [SERIAL, PENDING, error_reply("1002"), *login_replies(), SERIAL, DONE]},
        {"replies": [SERIAL, error_reply("1002"), *login_replies(), SERIAL, error_reply("1002")]},
        {"replies": [SERIAL, PENDING]},
        # the trigger fails
        {"replies": [error_reply("1001")]},
        {"replies": [error_reply("6002")]},
        {"replies": [error_reply("1008")]},
        {"replies": [transport_error(None)]},
        {"replies": [error_reply("1002"), *login_replies(), SERIAL, DONE]},
        {"replies": [error_reply("1002"), error_reply("1002")]},
        {"replies": []},
        # logged out, an old session, MQTT running or not (it is never waited for)
        {"replies": [*login_replies(), SERIAL, DONE], "state": {}},
        {"replies": [*login_replies(), SERIAL, DONE], "state": logged_in(session=OLD_SESSION)},
        {"replies": [SERIAL, PENDING, DONE], "state": logged_in(mqtt=False)},
        {"replies": [SERIAL, PENDING, DONE], "mqtt": [mqtt_event(0.5, "remoteControl", {"respondData": {"res": 2}}, uuid="S1")]},
        {"replies": [SERIAL, PENDING, DONE], "mqtt": [mqtt_event(0.0, "vehicleInfo", {"respondData": {"res": 2}}, uuid="S1")],
         "config": {**SAMPLE_CONFIG, "mqtt_timeout": 30.0}},
        {"replies": [SERIAL, DONE], "state": logged_in(guest_vins={VIN: True})},
    ]


def save_scripts() -> list[dict]:
    """``save`` then ``list``, and each way either fails."""
    return [
        {"replies": [SAVED, LIST_PRESET]},
        {"replies": [reply({}), reply({"data": [PRESET, CREATED]})]},
        {"replies": [reply([]), reply([])]},
        {"replies": [SAVED]},
        {"replies": [SAVED, reply({"data": "x"})]},
        {"replies": [SAVED, reply("text")]},
        {"replies": []},
        *({"replies": [failure, LIST_PRESET]} for failure in FAILURES),
        *({"replies": [SAVED, failure]} for failure in FAILURES[:6]),
        {"replies": [error_reply("1002"), *login_replies(), SAVED, LIST_PRESET]},
        {"replies": [SAVED, error_reply("1002"), *login_replies(), LIST_PRESET]},
        {"replies": [error_reply("1002"), *login_replies(), error_reply("1002")]},
        {"replies": [*login_replies(), SAVED, LIST_PRESET], "state": {}},
        {"replies": [*login_replies(), SAVED, LIST_PRESET], "state": logged_in(session=OLD_SESSION)},
        {"replies": [SAVED, LIST_PRESET], "state": logged_in(mqtt=False, guest_vins={VIN: True})},
    ]


def create_scripts() -> list[dict]:
    """``list``, ``save``, ``list``: one new plan, none, several, and each step failing."""
    other = {"id": "8", "planName": "Other", "planType": 2}
    return [
        {"replies": [LIST_PRESET, SAVED, LIST_BOTH]},
        {"replies": [reply([]), SAVED, reply({"data": [CREATED]})]},
        {"replies": [reply({"data": [PRESET]}), reply({}), reply([CREATED, PRESET])]},
        # no new plan, two of them, the same id twice, one without an id
        {"replies": [LIST_PRESET, SAVED, LIST_PRESET]},
        {"replies": [LIST_PRESET, SAVED, reply([PRESET, CREATED, other])]},
        {"replies": [LIST_PRESET, SAVED, reply([PRESET, CREATED, CREATED])]},
        {"replies": [LIST_PRESET, SAVED, reply([])]},
        {"replies": [LIST_BOTH, SAVED, reply([CREATED, other])]},
        {"replies": [LIST_PRESET, SAVED, reply([PRESET, {"planName": "no id"}])]},
        {"replies": [reply([{"planName": "no id"}]), SAVED, reply([{"planName": "no id"}, {"planName": "another"}])]},
        {"replies": [reply([{"id": 7}]), SAVED, reply([{"id": "7"}, {"id": 7.0}])]},
        # a step fails or is missing
        {"replies": []},
        {"replies": [LIST_PRESET]},
        {"replies": [LIST_PRESET, SAVED]},
        *({"replies": [failure, SAVED, LIST_BOTH]} for failure in FAILURES[:6]),
        *({"replies": [LIST_PRESET, failure, LIST_BOTH]} for failure in FAILURES),
        *({"replies": [LIST_PRESET, SAVED, failure]} for failure in FAILURES[:6]),
        {"replies": [reply({"data": "x"}), SAVED, LIST_BOTH]},
        {"replies": [LIST_PRESET, SAVED, reply({"data": [5]})]},
        # the session expires at each step
        {"replies": [error_reply("1002"), *login_replies(), LIST_PRESET, SAVED, LIST_BOTH]},
        {"replies": [LIST_PRESET, error_reply("1002"), *login_replies(), SAVED, LIST_BOTH]},
        {"replies": [LIST_PRESET, SAVED, error_reply("1002"), *login_replies(), LIST_BOTH]},
        {"replies": [*login_replies(), LIST_PRESET, SAVED, LIST_BOTH], "state": {}},
        {"replies": [LIST_PRESET, SAVED, LIST_BOTH], "state": logged_in(guest_vins={VIN: True})},
    ]


#: Plans whose own schedule ``save`` keeps (``schedule`` null): weekly, once, daily, none, and not a schedule.
KEPT_PLANS: list[Any] = [
    PRESET,
    ms.NEW_PLAN,
    {**PRESET, "executeType": "0", "startTime": None, "weeklyDays": "", "executeTime": 1795371000000},
    {**PRESET, "executeType": "1", "startTime": None, "weeklyDays": "", "executeTime": 1795371000000},
    {**PRESET, "executeType": "1", "startTime": None, "weeklyDays": "", "executeTime": NOW_MS - 3 * 86_400_000 - 1},
    {**PRESET, "startTime": "", "executeTime": 0},
    {**PRESET, "startTime": "25:00"},
    {**PRESET, "weeklyDays": [8]},
    {**PRESET, "controlParamsList": [COMMANDS[2]], "controlParams": None},
    {**PRESET, "startTime": "25:00", "controlParamsList": [COMMANDS[2]], "controlParams": None},
    {**PRESET, "controlParamsList": [{**COMMANDS[1], "controlParamsMap": {"mainHeat": [1]}}, COMMANDS[0]]},
    {"planName": "new", "controlParamsList": COMMANDS},
    {"id": "²", "controlParamsList": COMMANDS},
    [],
]
SCHEDULES: list[Any] = [
    None, {"repeat": "off"}, {"repeat": "weekly", "start_time": "12:55", "weekly_days": [5, 4]},
    {"repeat": "once", "at": "2026-11-22T19:10:00+01:00"}, {"repeat": "daily", "at": "2023-11-12T07:00:00.001+01:00"},
    {"repeat": "daily", "at": 1700000000}, {"repeat": "once"}, {"repeat": "weekly", "start_time": "9:30", "weekly_days": [1]}, {},
]
NAMES: list[str] = ["Test plan", "比亚迪 🚗", "", "nan", "--", " -- ", " padded ", "NaN ", "x" * 40]
COMMAND_SETS: list[Any] = [
    COMMANDS, [COMMANDS[0]], [COMMANDS[2], COMMANDS[0]], [ms.COMMAND], [], [COMMANDS[1], COMMANDS[2]],
    [{"commandType": "openair"}], [{"commandType": "OPENAIR", "commandId": 9, "controlParamsMap": "x"}],
    [COMMANDS[0], {"commandType": 5}], [COMMANDS[0], 5], [COMMANDS[0], {**COMMANDS[0], "controlParamsMap": {"n": 1.5, "é": "比亚迪"}}],
]


@group.recorded
def recorded():
    ready = logged_in()

    def case(method: str, script: dict, **args: Any):
        script = dict(script)
        state = script.pop("state", ready)
        return f"client.{method}", client_case(state=state, **{"vin": VIN, **args, **script})

    # get_standby_plans
    for data in flows.LIST_DATA:
        yield case("get_standby_plans", {"replies": [reply(data)]})
    for failure in FAILURES:
        yield case("get_standby_plans", {"replies": [failure]})
    yield case("get_standby_plans", {"replies": [error_reply("1002"), *login_replies(), LIST_PRESET]})
    yield case("get_standby_plans", {"replies": [error_reply("1002"), *login_replies(), error_reply("1002")]})
    yield case("get_standby_plans", {"replies": [*login_replies(), LIST_PRESET], "state": {}})
    yield case("get_standby_plans", {"replies": [LIST_PRESET], "state": logged_in(guest_vins={VIN: True}), "config": MADRID})
    yield case("get_standby_plans", {"replies": []})
    yield "client.get_standby_plans", client_case(state=ready, replies=[LIST_PRESET], vin="")

    # save_standby_plan
    for script in save_scripts():
        yield case("save_standby_plan", script, plan=PRESET, schedule=None)
    for plan in KEPT_PLANS:
        yield case("save_standby_plan", {"replies": [SAVED, LIST_PRESET]}, plan=plan, schedule=None)
        yield case("save_standby_plan", {"replies": [SAVED, LIST_PRESET], "config": MADRID}, plan=plan, schedule={"repeat": "off"})
        # What fails before anything is sent fails before a login too; what fails in the flow, after it.
        yield case("save_standby_plan", {"replies": [*login_replies(), SAVED, LIST_PRESET], "state": {}}, plan=plan, schedule=None)
    for schedule in SCHEDULES:
        yield case("save_standby_plan", {"replies": [SAVED, LIST_PRESET]}, plan=PRESET, schedule=schedule)
        yield case("save_standby_plan", {"replies": [SAVED, LIST_PRESET]}, plan=ms.NEW_PLAN, schedule=schedule)
    # #91: the save's message comes back with the list; a refused once run is 6111.
    for saved in [reply({"message": "La hora está a menos de 20 minutos; se omite hoy"}), reply({"message": ""}),
                  reply({}), reply({"message": None}), reply([])]:
        yield case("save_standby_plan", {"replies": [saved, LIST_PRESET]}, plan=PRESET, schedule=SCHEDULES[0])
    yield case("save_standby_plan", {"replies": [error_reply("6111", "Hora no válida")]}, plan=PRESET, schedule=SCHEDULES[0])
    yield case("create_standby_plan", {"replies": [LIST_PRESET, error_reply("6111")]}, name="Test plan", commands=COMMANDS,
               schedule=SCHEDULES[0])

    # create_standby_plan
    for script in create_scripts():
        yield case("create_standby_plan", script, name="Test plan", commands=COMMANDS, schedule=None)
    for name in NAMES:
        yield case("create_standby_plan", {"replies": [LIST_PRESET, SAVED, LIST_BOTH], "config": MADRID}, name=name, commands=[ms.COMMAND], schedule=None)
    for commands in COMMAND_SETS:
        yield case("create_standby_plan", {"replies": [LIST_PRESET, SAVED, LIST_BOTH]}, name="Test plan", commands=commands, schedule=None)
        yield case("create_standby_plan", {"replies": [*login_replies(), LIST_PRESET, SAVED, LIST_BOTH], "state": {}}, name="x", commands=commands, schedule={"repeat": "off"})
    for schedule in SCHEDULES:
        yield case("create_standby_plan", {"replies": [LIST_PRESET, SAVED, LIST_BOTH]}, name="Test plan", commands=COMMANDS, schedule=schedule)

    # setDefault and delete
    for method in ["set_default_standby_plan", "delete_standby_plan"]:
        for script in save_scripts():
            yield case(method, script, plan_id=PLAN_ID)
        for plan_id in flows.PLAN_IDS:
            yield case(method, {"replies": [reply({"message": "ok"}), LIST_PRESET]}, plan_id=plan_id)

    # run_standby_plan
    for script in run_scripts():
        yield case("run_standby_plan", script, plan_id=PLAN_ID)
    for plan_id in flows.PLAN_IDS[:8]:
        yield case("run_standby_plan", {"replies": [SERIAL, DONE], "config": MADRID}, plan_id=plan_id, poll_interval=0.0)
    yield "client.run_standby_plan", client_case(state=ready, replies=[SERIAL, PENDING, DONE], vin="", plan_id=PLAN_ID)
    # pyBYD #111: an expiry while the result is polled does not run the plan again.
    for code in ["1002", "1030", "1003"]:
        yield case("run_standby_plan", {"replies": [SERIAL, error_reply(code), *login_replies(), SERIAL, DONE]},
                   plan_id=PLAN_ID, poll_interval=0.0)
        yield case("run_standby_plan", {"replies": [error_reply(code), *login_replies(), SERIAL, DONE]},
                   plan_id=PLAN_ID, poll_interval=0.0)


def _plans_reply(rng, known: list[dict]) -> dict:
    roll = rng.random()
    if roll < 0.6:
        plans = [copy.deepcopy(plan) for plan in known]
        for _ in range(rng.choice([0, 0, 1, 1, 1, 2])):
            plans.insert(rng.randint(0, len(plans)), {"id": rng.choice(["7", "8", 9, "", PLAN_ID]), "planName": random_text(rng, 8), "planType": 2})
        if rng.random() < 0.15 and plans:
            plans.pop(rng.randrange(len(plans)))
        return reply(plans if rng.random() < 0.7 else {"data": plans})
    if roll < 0.75:
        data = flows._random_plans_data(rng)
        return reply(data if json_safe(data) else [])
    return _failure(rng)


def _failure(rng) -> dict:
    return rng.choice([
        error_reply(rng.choice(["1001", "1002", "1005", "1008", "6002", "6024", "8003", "9999"])),
        transport_error(rng.choice([None, 500])), raw_reply(rng.choice([[], "text", {"code": "0"}])), reply(rng.choice(["text", 5, None, []])),
    ])


def _run_reply(rng) -> dict:
    roll = rng.random()
    if roll < 0.2:
        return _failure(rng)
    serial = rng.choice(["S1", "S1", "S1", "", None, 5, [1], True])
    return reply(rng.choice([
        {"requestSerial": serial}, {"requestSerial": serial, "res": rng.choice([1, 1, 2, 3])}, {"res": 1}, {"res": 1}, {"res": 2},
        {"res": rng.choice([0, 3, 4, -1, True, False, 2.0, "2", "x", None])}, {"res": 2, "message": random_text(rng, 8)}, {}, [],
    ]))


@group.fuzz
def fuzz(rng):
    while True:
        method = rng.choice(METHODS + ["run_standby_plan", "create_standby_plan", "save_standby_plan"])
        config = dict(rng.choice([SAMPLE_CONFIG, MADRID]))
        if rng.random() < 0.15:
            config["mqtt_enabled"] = False
        state: dict = {}
        roll = rng.random()
        if roll < 0.8:
            state = logged_in(mqtt=rng.random() < 0.7, session=rng.choice([api_support.LIVE_SESSION] * 5 + [OLD_SESSION]))
            if rng.random() < 0.3:
                state["guest_vins"] = {VIN: rng.random() < 0.5}
        known = rng.choice([[PRESET], [PRESET, CREATED], [], [ms.NEW_PLAN]])
        ok_step = lambda: rng.choice([SAVED, reply({}), reply([]), reply({"message": "ok"})])  # noqa: E731
        step = lambda good: good() if rng.random() < 0.85 else _failure(rng)  # noqa: E731
        args: dict = {"vin": rng.choice([VIN, VIN, VIN, "", "TESTVIN0000000002"])}
        if method == "get_standby_plans":
            replies = [_plans_reply(rng, known)]
        elif method == "save_standby_plan":
            plan = rng.choice(KEPT_PLANS) if rng.random() < 0.4 else flows._random_plan(rng)
            schedule = rng.choice(SCHEDULES) if rng.random() < 0.5 else flows._random_schedule(rng, NOW_MS)
            if not (ms._usable(plan) and flows._id_fits(plan)):
                continue
            if schedule is not None and not (json_safe(schedule) and ms._ints_fit(schedule) and flows._daily_is_near(schedule, NOW_MS)):
                continue
            args.update(plan=plan, schedule=schedule)
            replies = [step(ok_step), _plans_reply(rng, known)]
        elif method == "create_standby_plan":
            commands = rng.choice(COMMAND_SETS) if rng.random() < 0.5 else [ms._random_command(rng) for _ in range(rng.randint(0, 3))] + [COMMANDS[0]]
            if not (json_safe(commands) and ms.in_domain(commands)):
                continue
            schedule = rng.choice(SCHEDULES)
            args.update(name=rng.choice(NAMES + [random_text(rng, 10)]), commands=commands, schedule=schedule)
            replies = [_plans_reply(rng, known), step(ok_step), _plans_reply(rng, known + ([CREATED] if rng.random() < 0.7 else []))]
        elif method in ("set_default_standby_plan", "delete_standby_plan"):
            plan_id = rng.choice(flows.PLAN_IDS + [str(rng.randint(0, 2**64 - 1)), random_text(rng, 6)])
            if not flows._text_id_fits(plan_id):
                continue
            args["plan_id"] = plan_id
            replies = [step(ok_step), _plans_reply(rng, known)]
        else:
            args["plan_id"] = rng.choice(flows.PLAN_IDS + [random_text(rng, 6)])
            if rng.random() < 0.7:
                args["poll_attempts"] = rng.choice([0, 1, 2, 3, 5])
                args["poll_interval"] = rng.choice([0.0, 0.5, 1.5])
            replies = [_run_reply(rng) for _ in range(rng.choice([0, 1, 2, 3, 4, 6, 12]))]
        if rng.random() < 0.2:
            position = rng.randint(0, len(replies))
            replies[position:position] = [error_reply("1002"), *login_replies()] if state else login_replies()
        elif not state:
            replies = [*login_replies(), *replies]
        if rng.random() < 0.1 and replies:
            replies.pop()
        times = sorted(rng.choice([0.0, 0.5, 1.0, 2.0, 4.0]) for _ in range(rng.choice([0, 0, 0, 1, 2])))
        mqtt = [mqtt_event(at, rng.choice(["remoteControl", "vehicleInfo"]), {"respondData": {"res": rng.choice([1, 2, 3])}}, uuid=rng.choice(["S1", "S2", None])) for at in times]
        yield f"client.{method}", client_case(
            config=config, state=state, replies=replies, mqtt=mqtt, callbacks=rng.random() < 0.8,
            now_ms=rng.choice([NOW_MS, NOW_MS, 1795039920000]), seed=random_text(rng, 6), **args,
        )


keep_representable(group)
