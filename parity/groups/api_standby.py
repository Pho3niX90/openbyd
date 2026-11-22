"""pybyd/_api/standby.py: the preconditioning plan flows, compared by trace.

The flows take models; a case carries them as JSON: ``plan`` is the raw dict
``StandbyPlan.model_validate`` reads and ``schedule`` the keyword arguments
of ``StandbySchedule`` (a datetime as ISO text or a Unix timestamp), built
inside the flow, so a plan or schedule pyBYD refuses is part of the trace.
``make_send_fetch_fn`` returns a closure; its call builds it and calls it
once per entry of ``calls`` (``{"endpoint", "request_serial"}``), the way
the client's trigger-and-poll routine does.

``schedule_fields`` and ``build_save_inner`` are plain calls, answered with
the dict they return. They read the clock (``datetime.now`` for a daily
schedule without ``now``, ``time.time`` and ``secrets`` for the request's
base), so a case gives ``now_ms`` and ``seed`` and the oracle scripts them
as the flows' runner does. ``now`` is ISO text, read with
``datetime.fromisoformat``.

Plan ids stay within 64 bits: pyBYD would send a longer one as a longer
JSON number, which openbyd's JSON values cannot hold.
"""

from __future__ import annotations

import copy
import datetime as dt
import secrets
import time
from contextlib import ExitStack
from typing import Any
from unittest import mock
from zoneinfo import ZoneInfo

from pybyd._api import standby as sb
from pybyd.config import BydConfig
from pybyd.models.standby import StandbyPlan, StandbySchedule

from parity.api_support import (
    NOW_MS,
    VIN,
    _Clock,
    error_reply,
    flow_oracle,
    ok_reply,
    patch_datetime_now,
    random_reply,
    random_scenario_base,
    raw_reply,
    scenario,
    transport_error,
)
from parity.framework import Group, random_text
from parity.groups import models_standby as ms
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION
from parity.groups.pyjson import random_json, random_object
from parity.models_support import json_safe, skip_unrepresentable_integers

group = Group("api_standby", crate="client")

LIST = "/control/standby-vehicles/list"
SAVE = "/control/standby-vehicles/save"
SET_DEFAULT = "/control/standby-vehicles/setDefault"
DELETE = "/control/standby-vehicles/delete"
SEND = "/control/standby-vehicles/send"
SEND_RESULT = "/control/standby-vehicles/send-result"
#: Not supported, session expired (three ways), busy, unreachable, rate limited, not a guest, unmapped.
CODES = ["1001", "1002", "1011", "3001", "3003", "1005", "1010", "1008", "6002", "6024", "8003", "9999", 1001]
OWNER = {VIN: False}
GUEST = {VIN: True}
MADRID = {**SAMPLE_CONFIG, "time_zone": "Europe/Madrid"}
PLAN_ID = "1216038691305533440"

# ---------------------------------------------------------------------------
# Oracles
# ---------------------------------------------------------------------------


async def _fetch(config, session, transport, *, vin):
    plans = await sb.fetch_standby_plans(config, session, transport, vin)
    return plans.model_dump(mode="json")


async def _save(config, session, transport, *, vin, plan, schedule):
    return await sb.save_standby_plan(
        config, session, transport, vin, StandbyPlan.model_validate(plan), StandbySchedule(**schedule)
    )


def _plan_id_flow(function):
    async def run(config, session, transport, *, vin, plan_id):
        return await function(config, session, transport, vin, plan_id)

    return run


def _pair_flow(function):
    async def run(config, session, transport, *, vin, **kwargs):
        return list(await function(config, session, transport, vin, **kwargs))

    return run


async def _fetch_fn(config, session, transport, *, vin, plan_id, calls):
    fetch = sb.make_send_fetch_fn(plan_id)
    results = []
    for call in calls:
        results.append(list(await fetch(call["endpoint"], config, session, transport, vin, call.get("request_serial"))))
    return results


flow_oracle(group, "api.standby.fetch_standby_plans", _fetch)
flow_oracle(group, "api.standby.save_standby_plan", _save)
flow_oracle(group, "api.standby.set_default_standby_plan", _plan_id_flow(sb.set_default_standby_plan))
flow_oracle(group, "api.standby.delete_standby_plan", _plan_id_flow(sb.delete_standby_plan))
flow_oracle(group, "api.standby.trigger_send_standby_plan", _pair_flow(sb.trigger_send_standby_plan))
flow_oracle(group, "api.standby.poll_send_result", _pair_flow(sb.poll_send_result))
flow_oracle(group, "api.standby.make_send_fetch_fn", _fetch_fn)


def _scripted(now_ms: int, seed: str) -> ExitStack:
    """The clock and random values of a plain call, as ``run_flow`` scripts them."""
    clock = _Clock(now_ms, seed)
    stack = ExitStack()
    stack.enter_context(mock.patch.object(time, "time", clock.time))
    stack.enter_context(mock.patch.object(secrets, "token_hex", clock.token_hex))
    patch_datetime_now(stack, clock)
    return stack


def _now(now: str | None) -> dt.datetime | None:
    return None if now is None else dt.datetime.fromisoformat(now)


PURE_ERRORS = (ValueError, TypeError, OverflowError)


def _in_zone(schedule: StandbySchedule, zone: str | None) -> StandbySchedule:
    """A daily schedule with its time moved into ``zone``, as a caller with a ``ZoneInfo`` datetime gives it.

    pyBYD then steps a stale time by that zone's calendar days; openbyd's
    side is ``StandbySchedule::daily_in_zone``.
    """
    if zone is None or schedule.repeat != "daily" or schedule.at is None:
        return schedule
    return schedule.model_copy(update={"at": schedule.at.astimezone(ZoneInfo(zone))})


@group.oracle("api.standby.schedule_fields", errors=PURE_ERRORS)
def schedule_fields(schedule: dict, now_ms: int, now: str | None = None, seed: str = "", zone: str | None = None) -> dict:
    built = _in_zone(StandbySchedule(**schedule), zone)
    with _scripted(now_ms, seed):
        return sb.schedule_fields(built, now=_now(now))


@group.oracle("api.standby.build_save_inner", errors=PURE_ERRORS)
def build_save_inner(
    config: dict, vin: str, plan: Any, schedule: dict, now_ms: int, now: str | None = None, seed: str = "",
    zone: str | None = None,
) -> dict:
    built_config = BydConfig(**config)
    built_plan = StandbyPlan.model_validate(plan)
    built_schedule = _in_zone(StandbySchedule(**schedule), zone)
    with _scripted(now_ms, seed):
        return sb.build_save_inner(built_config, vin, built_plan, built_schedule, now=_now(now))


@group.oracle("api.standby.is_send_ready", errors=(AttributeError,))
def is_send_ready(payload) -> bool:
    return sb.is_send_ready(payload)


@group.oracle("api.standby.constants")
def constants() -> dict:
    names = ["LIST_ENDPOINT", "SAVE_ENDPOINT", "SET_DEFAULT_ENDPOINT", "DELETE_ENDPOINT", "SEND_ENDPOINT", "SEND_RESULT_ENDPOINT"]
    return {name: getattr(sb, name) for name in names}


# ---------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------


def ok(data) -> dict:
    return ok_reply(SAMPLE_SESSION, data)


COMMON_REPLIES = [ok({}), ok([1]), ok("text"), ok(None), ok(0), ok(""), ok(7), ok(True), ok(1.5),
                  raw_reply({"code": "0"}), raw_reply([]), raw_reply("text"), raw_reply(None), transport_error(502),
                  transport_error(None), *(error_reply(code) for code in CODES)]
#: ``list`` answers: the list itself, wrapped in ``data``, and what is neither.
LIST_DATA: list[Any] = [
    [ms.PLAN], [ms.PLAN, ms.NEW_PLAN], [], {"data": [ms.PLAN, ms.PLAN_SAMPLES[-1]]}, {"data": []}, {"data": None},
    {"data": "x"}, {"data": "--"}, {"data": {"a": 1}}, {"data": 5}, {"data": [5]}, {"data": 0}, {"message": "ok"},
    [5], [ms.PLAN, "x"], [[ms.PLAN]], [{}], {"data": [ms.PLAN], "vin": "OTHERVIN000000001"}, ms.PLAN_SAMPLES[3:6],
]
PLAN_IDS: list[str] = [
    PLAN_ID, "2", "7", "0", "007", "", "abc", "7a", "-7", " 7", "١٢", "²", "1²", "18446744073709551615", "9223372036854775808",
    "比亚迪", "1.5", "1_0",
    # int() reads at most 4300 digits, zeros included: the first is 7, the second a ValueError.
    "0" * 4299 + "7", "0" * 4300 + "7",
]
SERIALS: list[Any] = ["S1", "", None, 5, 0, True, False, 1.5, "比亚迪", [1], {"a": 1}]
SEND_DATA: list[Any] = [
    {"requestSerial": "S1"}, {"requestSerial": "S1", "res": 1}, {"requestSerial": 5}, {"requestSerial": None},
    {"requestSerial": ""}, {"requestSerial": [1]}, {"res": 2, "message": "ok"}, {}, [], "text", None, {"message": "x"},
]
RESULT_DATA: list[Any] = [
    {"res": 1}, {"res": 2, "message": "ok"}, {"res": 3, "message": "fail"}, {"res": 2, "requestSerial": "OTHER"},
    {"requestSerial": None, "res": 1}, {"requestSerial": ""}, {}, [], "text", None, 5, {"res": "2"}, {"res": True},
]
READY_SAMPLES: list[Any] = [
    {}, {"res": 1}, {"res": 2}, {"res": 3}, {"res": 0}, {"res": True}, {"res": False}, {"res": 2.0}, {"res": "2"},
    {"res": None}, {"res": -1}, {"res": 2**64 - 1}, {"requestSerial": "X"}, None, [], "x", 0,
]
#: The scripted clock of a flow starts at 2023-11-14T22:13:20Z.
NOW_ISO = "2023-11-14T22:13:20+00:00"
#: Daily run times around the clock: before, exactly at, just after, days and years off, with fractions.
DAILY_TIMES: list[Any] = [
    "2023-11-14T22:13:20+00:00", "2023-11-14T22:13:20.001+00:00", "2023-11-14T22:13:19.999+00:00",
    "2023-11-14T23:13:20+01:00", "2023-11-14T07:00:00+01:00", "2023-11-12T07:00:00+01:00", "2023-11-15T07:00:00+01:00",
    "2026-11-18T23:12:00+01:00", "2020-02-29T12:00:00-08:00", "2023-11-14T22:13:20.5+00:00", "1970-01-01T00:00:00Z",
    "2023-11-13T22:13:20.000001Z", "2023-11-14T22:13:21+23:59", "2023-11-14T22:13:19-23:59", 1700000000, 1700000000.001,
    1699999999999, 1700000000001, "1699913600.123456",
]
#: Run times where ``int(timestamp() * 1000)`` is not the exact millisecond, and the edges of the calendar.
ONCE_TIMES: list[Any] = [
    "2026-11-22T19:10:00+01:00", "1970-01-01T00:00:01.001Z", "1970-01-01T00:00:01.002Z", "2026-11-22T19:10:00.001+01:00",
    "2026-11-22T19:10:00.999999+01:00", "2026-11-22T19:10:00.0005Z", "1969-12-31T23:59:58.5Z", "1969-12-31T23:59:59.999Z",
    "0001-01-01T00:00:00Z", "0001-01-01T00:00:00+23:59", "9999-12-31T23:59:59.999999Z", "9999-12-31T23:59:59.999999-23:59",
    "2255-06-05T23:47:34.740993Z", "5000-01-01T12:34:56.789012+05:30", "1600-02-29T01:02:03.456789Z", 0, -1.5,
    1795371000, 1795371000000, "1795371000.0005", 253402300799999, -62135596800000,
]
SCHEDULES: list[dict[str, Any]] = [
    {"repeat": "off"},
    {"repeat": "weekly", "start_time": "12:55", "weekly_days": [4, 5]},
    {"repeat": "weekly", "start_time": "1٢:3٤", "weekly_days": [6, 0, 6]},
    {"repeat": "weekly", "start_time": "00:00", "weekly_days": [0, 1, 2, 3, 4, 5, 6]},
    {"repeat": "off", "at": "2026-11-22T19:10:00", "start_time": "zz", "weekly_days": [9]},
    {"repeat": "weekly", "at": "2026-11-22T19:10:00+01:00", "start_time": "23:59", "weekly_days": [3]},
    *({"repeat": "once", "at": at} for at in ONCE_TIMES),
    *({"repeat": "daily", "at": at} for at in DAILY_TIMES),
]
BAD_SCHEDULES: list[dict[str, Any]] = [
    {}, {"repeat": "once"}, {"repeat": "daily", "at": "2026-11-18T07:00:00"}, {"repeat": "weekly"},
    {"repeat": "weekly", "start_time": "25:00", "weekly_days": [1]}, {"repeat": "weekly", "start_time": "10:00", "weekly_days": [7]},
    {"repeat": "sometimes"},
]
NOWS: list[Any] = [
    None, NOW_ISO, "2026-11-18T15:42:00+01:00", "2026-11-18T15:42:00", "2023-11-14T22:13:20.001+00:00", "1970-01-01T00:00:00+00:00",
    "2030-01-01T00:00:00+05:30", "2023-11-14", "2023-11-14T22:13:20-00:30:15.5",
]
#: Plans as ``save`` gets them: with and without an id, the A/C first, last, missing or doubled.
OPENAIR, SEATS, WHEEL = ms.CAPTURED_COMMANDS
SAVE_PLANS: list[Any] = [
    ms.PLAN,
    ms.NEW_PLAN,
    {"planName": "Test plan", "planType": 2, "defaultPlan": 0, "controlParamsList": ms.CAPTURED_COMMANDS},
    {"planName": "比亚迪 🚗", "controlParamsList": [WHEEL, SEATS, OPENAIR]},
    {"planName": "x", "controlParamsList": [WHEEL, OPENAIR, SEATS, {**OPENAIR, "commandId": "9", "controlParamsMap": {"a": 1.0}}]},
    {"planName": "x", "controlParamsList": [WHEEL]},
    {"planName": "x", "controlParamsList": [SEATS, WHEEL]},
    {"planName": "x"},
    {"planName": "x", "controlParamsList": [{"commandType": "openair"}, {"commandType": ""}]},
    {"id": "7", "planName": "", "planType": None, "defaultPlan": None, "controlParamsList": [{"commandType": "OPENAIR"}]},
    {"id": 7.5, "planName": "nan", "planType": 1, "defaultPlan": 1, "controlParamsList": [{"commandType": "OPENAIR", "controlParamsMap": "x"}]},
    {"id": "abc", "controlParamsList": [{"commandType": "OPENAIR", "controlParamsMap": {"n": 1e300, "é": "比亚迪", "l": [1, None, True], "f": 1.0, "i": 2**64 - 1}}]},
    {"id": "²", "controlParamsList": [OPENAIR]},
    {"id": "²", "controlParamsList": [WHEEL]},
    {"id": "١٢", "controlParamsList": [OPENAIR]},
    {"id": "007", "controlParamsList": [OPENAIR]},
    {"id": 2**64 - 1, "controlParamsList": [OPENAIR]},
    {"id": True, "controlParamsList": [OPENAIR]},
    {"id": 0, "planType": 0, "defaultPlan": -1, "controlParamsList": [OPENAIR]},
    {key: value for key, value in ms.PLAN.items() if key != "controlParamsList"},
    {"planName": 5, "controlParamsList": [OPENAIR]},
    [],
    None,
]


def _id_fits(plan: Any) -> bool:
    """Whether the plan's id, if pyBYD sends it as a number, fits 64 bits."""
    try:
        plan_id = StandbyPlan.model_validate(plan).id
    except ValueError:
        return True
    return _text_id_fits(plan_id)


def _save_usable(plan: Any, schedule: Any) -> bool:
    return ms._usable(plan) and _id_fits(plan) and json_safe(schedule) and ms._ints_fit(schedule)


def _text_id_fits(plan_id: str) -> bool:
    if not plan_id.isdigit() or not plan_id.isdecimal():
        return True
    try:
        return int(plan_id) < 2**64
    except ValueError:
        return True  # past int()'s digit limit: both sides refuse it


@group.recorded
def recorded():
    yield "api.standby.constants", {}
    for payload in READY_SAMPLES:
        yield "api.standby.is_send_ready", {"payload": payload}

    # list
    for data in LIST_DATA:
        yield "api.standby.fetch_standby_plans", scenario(replies=[ok(data)], vin=VIN)
    for reply in COMMON_REPLIES:
        yield "api.standby.fetch_standby_plans", scenario(replies=[reply], vin=VIN, guest_vins=OWNER)
    yield "api.standby.fetch_standby_plans", scenario(replies=[ok([ms.PLAN])], vin=VIN, guest_vins=GUEST, config=MADRID)
    yield "api.standby.fetch_standby_plans", scenario(replies=[error_reply("1001")], vin=VIN, guest_vins=GUEST)
    yield "api.standby.fetch_standby_plans", scenario(replies=[error_reply("8003"), ok([])], vin=VIN, guest_vins=GUEST)
    yield "api.standby.fetch_standby_plans", scenario(replies=[ok([ms.PLAN])], vin="", session={**SAMPLE_SESSION, "user_id": "比亚迪 42"})
    yield "api.standby.fetch_standby_plans", scenario(replies=[], vin=VIN)
    yield "api.standby.fetch_standby_plans", scenario(replies=[ok([]), ok([])], vin=VIN)

    # save
    for plan in SAVE_PLANS:
        for schedule in SCHEDULES[:4]:
            yield "api.standby.save_standby_plan", scenario(replies=[ok({"message": "saved"})], vin=VIN, plan=plan, schedule=schedule, config=MADRID)
    for schedule in SCHEDULES + BAD_SCHEDULES:
        yield "api.standby.save_standby_plan", scenario(replies=[ok({"message": "saved"})], vin=VIN, plan=ms.PLAN, schedule=schedule)
        yield "api.standby.save_standby_plan", scenario(replies=[ok({})], vin=VIN, plan=SAVE_PLANS[2], schedule=schedule, now_ms=1795039920000)
    for reply in COMMON_REPLIES:
        yield "api.standby.save_standby_plan", scenario(replies=[reply], vin=VIN, plan=ms.PLAN, schedule=SCHEDULES[1], guest_vins=GUEST)
    yield "api.standby.save_standby_plan", scenario(replies=[], vin=VIN, plan=ms.PLAN, schedule={"repeat": "off"})
    # #91: a once run under 20 minutes ahead or over 30 days out is refused (6111).
    for code in ["6111", 6111, " 6111", "6112"]:
        yield "api.standby.save_standby_plan", scenario(
            replies=[error_reply(code, "Hora no válida")], vin=VIN, plan=ms.PLAN, schedule=SCHEDULES[1]
        )
    yield "api.standby.save_standby_plan", scenario(
        replies=[ok({"message": "La hora está a menos de 20 minutos; se omite hoy"})], vin=VIN, plan=ms.PLAN,
        schedule=SCHEDULES[1],
    )
    yield "api.standby.save_standby_plan", scenario(replies=[ok({})], vin="", plan=ms.NEW_PLAN, schedule={"repeat": "off"})

    # setDefault, delete, send
    for call in ["api.standby.set_default_standby_plan", "api.standby.delete_standby_plan"]:
        for plan_id in PLAN_IDS:
            yield call, scenario(replies=[ok({"message": "ok"})], vin=VIN, plan_id=plan_id, config=MADRID)
        for reply in COMMON_REPLIES:
            yield call, scenario(replies=[reply], vin=VIN, plan_id=PLAN_ID, guest_vins=OWNER)
        yield call, scenario(replies=[], vin=VIN, plan_id=PLAN_ID)
        yield call, scenario(replies=[ok({})], vin="", plan_id=PLAN_ID, guest_vins=GUEST)
    for plan_id in PLAN_IDS:
        yield "api.standby.trigger_send_standby_plan", scenario(replies=[ok({"requestSerial": "S1"})], vin=VIN, plan_id=plan_id, config=MADRID)
    for data in SEND_DATA:
        yield "api.standby.trigger_send_standby_plan", scenario(replies=[ok(data)], vin=VIN, plan_id=PLAN_ID)
    for reply in COMMON_REPLIES:
        yield "api.standby.trigger_send_standby_plan", scenario(replies=[reply], vin=VIN, plan_id=PLAN_ID, guest_vins=GUEST)
    yield "api.standby.trigger_send_standby_plan", scenario(replies=[], vin=VIN, plan_id=PLAN_ID, now_ms=0)

    # send-result
    for serial in SERIALS:
        for data in RESULT_DATA[:5]:
            yield "api.standby.poll_send_result", scenario(replies=[ok(data)], vin=VIN, request_serial=serial)
    for data in RESULT_DATA:
        yield "api.standby.poll_send_result", scenario(replies=[ok(data)], vin=VIN, request_serial="S1")
    for reply in COMMON_REPLIES:
        yield "api.standby.poll_send_result", scenario(replies=[reply], vin=VIN, request_serial="S1", guest_vins=GUEST)
    yield "api.standby.poll_send_result", scenario(replies=[], vin="", request_serial="S1")

    # the fetch_fn, as trigger-and-poll drives it
    runs = [
        ([{"endpoint": SEND}], [ok({"requestSerial": "S1"})]),
        ([{"endpoint": SEND}, {"endpoint": SEND_RESULT, "request_serial": "S1"}], [ok({"requestSerial": "S1"}), ok({"res": 1})]),
        ([{"endpoint": SEND}, {"endpoint": SEND_RESULT, "request_serial": "S1"}, {"endpoint": SEND_RESULT, "request_serial": "S1"}],
         [ok({"requestSerial": "S1"}), ok({"res": 1}), ok({"res": 2, "message": "ok"})]),
        ([{"endpoint": SEND_RESULT}], [ok({"res": 2})]),
        ([{"endpoint": SEND_RESULT, "request_serial": ""}], [ok({"res": 2})]),
        ([{"endpoint": SEND_RESULT, "request_serial": 0}], [ok({})]),
        ([{"endpoint": SEND_RESULT, "request_serial": 5}], [ok({"res": 3})]),
        ([{"endpoint": "/other", "request_serial": "S1"}], [ok({"requestSerial": 7})]),
        ([{"endpoint": SEND}, {"endpoint": SEND_RESULT, "request_serial": "S1"}], [error_reply("1001"), ok({})]),
        ([{"endpoint": SEND}, {"endpoint": SEND_RESULT, "request_serial": "S1"}], [ok({"requestSerial": "S1"}), error_reply("1001")]),
        ([{"endpoint": SEND}, {"endpoint": SEND_RESULT, "request_serial": "S1"}], [ok({"requestSerial": "S1"}), error_reply("1002")]),
        ([{"endpoint": SEND}], [transport_error(500)]),
        ([], []),
    ]
    for calls, replies in runs:
        yield "api.standby.make_send_fetch_fn", scenario(replies=replies, vin=VIN, plan_id=PLAN_ID, calls=calls)

    # the plain helpers
    for schedule in SCHEDULES + BAD_SCHEDULES:
        yield "api.standby.schedule_fields", {"schedule": schedule, "now_ms": NOW_MS}
    for schedule in ({"repeat": "daily", "at": at} for at in DAILY_TIMES):
        for now in NOWS:
            yield "api.standby.schedule_fields", {"schedule": schedule, "now_ms": NOW_MS, "now": now}
        yield "api.standby.schedule_fields", {"schedule": schedule, "now_ms": 0}
        yield "api.standby.schedule_fields", {"schedule": schedule, "now_ms": 1700000000001}
    for now in NOWS:
        for schedule in SCHEDULES[:3] + [{"repeat": "once", "at": ONCE_TIMES[0]}]:
            yield "api.standby.schedule_fields", {"schedule": schedule, "now_ms": NOW_MS, "now": now}
    # The upstream tests: 23:12 every day saved at 15:42, and a time two days stale.
    yield "api.standby.schedule_fields", {
        "schedule": {"repeat": "daily", "at": "2026-11-18T23:12:00+01:00"}, "now_ms": 0, "now": "2026-11-18T15:42:00+01:00",
    }
    yield "api.standby.schedule_fields", {
        "schedule": {"repeat": "daily", "at": "2026-11-18T07:00:00+01:00"}, "now_ms": 0, "now": "2026-11-20T08:00:00+01:00",
    }
    # Moving a stale time forward runs off the calendar.
    for at, now in [("9999-12-31T12:00:00Z", "9999-12-31T12:00:00+00:00"), ("9999-12-30T12:00:00Z", "9999-12-31T13:00:00+00:00"),
                    ("9999-12-30T12:00:00Z", "9999-12-31T11:00:00+00:00"), ("0001-01-01T00:00:00Z", NOW_ISO)]:
        yield "api.standby.schedule_fields", {"schedule": {"repeat": "daily", "at": at}, "now_ms": NOW_MS, "now": now}

    # A daily time in a time zone: a stale one moves on by that zone's calendar days.
    for zone, at, now in ZONED_DAILY:
        schedule = {"repeat": "daily", "at": at}
        yield "api.standby.schedule_fields", {"schedule": schedule, "now_ms": NOW_MS, "now": now, "zone": zone}
        yield "api.standby.schedule_fields", {"schedule": schedule, "now_ms": _epoch_ms(now), "zone": zone}
    for zone in ZONES:
        for schedule in SCHEDULES + BAD_SCHEDULES[:2]:  # only a daily schedule reads the zone
            yield "api.standby.schedule_fields", {"schedule": schedule, "now_ms": NOW_MS, "zone": zone}
    zone, at, now = ZONED_DAILY[0]
    yield "api.standby.build_save_inner", {
        "config": MADRID, "vin": VIN, "plan": ms.PLAN, "schedule": {"repeat": "daily", "at": at}, "now_ms": _epoch_ms(now),
        "seed": "z", "zone": zone,
    }

    for plan in SAVE_PLANS:
        for schedule in [SCHEDULES[0], SCHEDULES[1], {"repeat": "daily", "at": DAILY_TIMES[4]}]:
            for now in [None, "2026-11-18T15:42:00"]:
                yield "api.standby.build_save_inner", {
                    "config": MADRID, "vin": VIN, "plan": plan, "schedule": schedule, "now_ms": NOW_MS, "now": now, "seed": "s",
                }
    for schedule in SCHEDULES + BAD_SCHEDULES:
        yield "api.standby.build_save_inner", {
            "config": SAMPLE_CONFIG, "vin": "", "plan": ms.PLAN, "schedule": schedule, "now_ms": NOW_MS, "now": NOW_ISO, "seed": "t",
        }


#: Zones whose rules have long been settled, so that two copies of the time
#: zone database of different dates agree: daylight saving north and south,
#: a half-hour shift, none at all, and a day skipped at the date line.
ZONES = ["Europe/Madrid", "America/New_York", "Australia/Sydney", "Australia/Lord_Howe", "Asia/Kolkata",
         "Africa/Johannesburg", "Pacific/Apia", "Asia/Tokyo", "UTC"]
#: (zone, the daily time, now).
ZONED_DAILY: list[tuple[str, str, str]] = [
    # the eve of the clocks going back: the next 08:00 is 25 hours on; and still ahead
    ("Europe/Madrid", "2026-10-24T08:00:00+02:00", "2026-10-24T06:00:01+00:00"),
    ("Europe/Madrid", "2026-10-24T08:00:00+02:00", "2026-10-24T05:59:59+00:00"),
    # and of them going forward: 23 hours on
    ("Europe/Madrid", "2026-03-28T08:00:00+01:00", "2026-03-28T07:00:01+00:00"),
    # tomorrow's 02:30 is skipped, or happens twice
    ("Europe/Madrid", "2026-03-28T02:30:00+01:00", "2026-03-28T12:00:00+00:00"),
    ("Europe/Madrid", "2026-10-24T02:30:00+02:00", "2026-10-24T12:00:00+00:00"),
    ("America/New_York", "2026-03-07T02:30:00-05:00", "2026-03-07T08:00:00+00:00"),
    # years stale, from winter into summer; given in another offset than the zone's
    ("Europe/Madrid", "2020-01-15T07:30:00+01:00", "2026-07-20T12:00:00+00:00"),
    ("Australia/Sydney", "2021-06-30T22:15:00.250000Z", "2026-01-05T03:00:00+11:00"),
    # a half-hour shift, no daylight saving, the day Samoa skipped, and UTC
    ("Australia/Lord_Howe", "2026-04-04T01:45:00+11:00", "2026-04-04T00:00:00+00:00"),
    ("Asia/Kolkata", "2026-11-18T07:00:00+05:30", "2026-11-20T08:00:00+01:00"),
    ("Pacific/Apia", "2011-12-29T10:00:00-10:00", "2011-12-29T21:00:00+00:00"),
    ("UTC", "2026-11-18T07:00:00+01:00", "2026-11-20T08:00:00+01:00"),
    # off either end of the calendar
    ("Europe/Madrid", "9999-12-30T12:00:00Z", "9999-12-31T13:00:00+00:00"),
    ("Asia/Tokyo", "9999-12-31T20:00:00Z", "2026-11-20T08:00:00+00:00"),
    ("America/New_York", "0001-01-01T00:00:00Z", "2026-11-20T08:00:00+00:00"),
]


def _epoch_ms(iso: str) -> int:
    return int(dt.datetime.fromisoformat(iso).timestamp() * 1000)


def _random_zoned(rng) -> dict[str, Any]:
    """A daily time near a random moment of 2000 to 2037, in a random zone."""
    now_ms = rng.randint(946684800000, 2145916800000)
    moment = dt.datetime(1970, 1, 1, tzinfo=dt.UTC) + dt.timedelta(milliseconds=now_ms)
    behind = rng.choice([rng.randint(-3 * 86400, 3 * 86400), rng.randint(-5, 5), rng.randint(0, 1500 * 86400)])
    offset = dt.timezone(dt.timedelta(minutes=rng.choice([0, 0, 60, 120, -300, 330, 765, -570])))
    at = (moment - dt.timedelta(seconds=behind, microseconds=rng.choice([0, 0, rng.randint(0, 999999)]))).astimezone(offset)
    now = rng.choice([None, None, (moment + dt.timedelta(seconds=rng.randint(-86400 * 30, 86400 * 30))).isoformat()])
    return {"schedule": {"repeat": "daily", "at": at.isoformat()}, "now_ms": now_ms, "now": now, "zone": rng.choice(ZONES)}


def _random_plan(rng) -> Any:
    roll = rng.random()
    if roll < 0.35:
        return rng.choice(SAVE_PLANS)
    if roll < 0.5:
        return ms._random_plan(rng)
    plan = copy.deepcopy(rng.choice([ms.PLAN, ms.NEW_PLAN, SAVE_PLANS[2]]))
    commands = [ms._random_command(rng) for _ in range(rng.choice([0, 1, 1, 2, 3]))]
    if rng.random() < 0.8:
        commands.insert(rng.randint(0, len(commands)), {"commandType": "OPENAIR", "controlParamsMap": ms._random_map(rng)})
    plan["controlParamsList"] = commands
    if rng.random() < 0.5:
        plan["id"] = rng.choice(ms.IDS + PLAN_IDS)
    if rng.random() < 0.3:
        plan["planName"] = rng.choice(["Test plan", "比亚迪", "", "nan", random_text(rng, 12)])
    for key in ("planType", "defaultPlan"):
        if rng.random() < 0.3:
            plan[key] = rng.choice([0, 1, 2, None, "1", -1, 2**53])
    return plan


def _random_schedule(rng, now_ms: int) -> dict:
    roll = rng.random()
    if roll < 0.2:
        return rng.choice(SCHEDULES + BAD_SCHEDULES)
    if roll < 0.35:
        return ms._schedule_kwargs(rng)
    if roll < 0.75:
        # A run time near the clock, so a daily one is moved or not.
        offset_ms = rng.choice([0, 1, -1, 999, -999, rng.randint(-5 * 86_400_000, 5 * 86_400_000), rng.randint(-10**11, 10**11)])
        moment = dt.datetime(1970, 1, 1, tzinfo=dt.UTC) + dt.timedelta(milliseconds=now_ms + offset_ms, microseconds=rng.choice([0, 0, 1, 500, 999]))
        zone = dt.timezone(dt.timedelta(minutes=rng.choice([0, 0, 60, -480, 330, 1439, -1439])))
        at: Any = moment.astimezone(zone).isoformat()
        if rng.random() < 0.15:
            at = (now_ms + offset_ms) / 1000 if rng.random() < 0.5 else now_ms + offset_ms
        return {"repeat": rng.choice(["daily", "daily", "once"]), "at": at}
    return {"repeat": rng.choice(["once", "daily"]), "at": ms.random_datetime(rng)}


def _daily_is_near(schedule: dict, now_ms: int, now: str | None = None) -> bool:
    """Whether a daily time is within a few thousand days of the clock: pyBYD steps to it a day at a time."""
    try:
        built = StandbySchedule(**schedule)
    except ValueError:
        return True
    if built.repeat != "daily" or built.at is None:
        return True
    reference = dt.datetime(1970, 1, 1, tzinfo=dt.UTC) + dt.timedelta(milliseconds=now_ms)
    if now is not None:
        given = dt.datetime.fromisoformat(now)
        if given.tzinfo is None:
            return True
        reference = given
    return reference - built.at < dt.timedelta(days=20000)


def _random_plans_data(rng) -> Any:
    roll = rng.random()
    if roll < 0.25:
        return rng.choice(LIST_DATA)
    plans = [plan for plan in (ms._random_plan(rng) for _ in range(rng.randint(0, 3))) if ms._usable(plan)]
    if roll < 0.6:
        return plans
    if roll < 0.9:
        return {"data": plans, **({"vin": "x"} if rng.random() < 0.1 else {})}
    return rng.choice([random_object(rng), random_json(rng)])


def _replies(rng, session: dict, data) -> list[dict]:
    replies = []
    for _ in range(rng.choice([0, 1, 1, 1, 1, 2])):
        if rng.random() < 0.6:
            payload = data(rng)
            replies.append(ok_reply(session, payload) if json_safe(payload) else ok_reply(session, {}))
        else:
            replies.append(random_reply(rng, session, data))
    return replies


@group.fuzz
def fuzz(rng):
    while True:
        base = random_scenario_base(rng)
        session = base["session"]
        vin = rng.choice([VIN, VIN, VIN, "", random_text(rng, 17)])
        guests = rng.choice([{}, OWNER, GUEST])
        roll = rng.random()
        if roll < 0.2:
            yield "api.standby.fetch_standby_plans", scenario(**base, replies=_replies(rng, session, _random_plans_data), vin=vin, guest_vins=guests)
        elif roll < 0.5:
            plan = _random_plan(rng)
            schedule = _random_schedule(rng, base["now_ms"])
            if _save_usable(plan, schedule) and _daily_is_near(schedule, base["now_ms"]):
                replies = _replies(rng, session, lambda r: r.choice([{"message": "saved"}, {}, [], None, random_object(r)]))
                yield "api.standby.save_standby_plan", scenario(**base, replies=replies, vin=vin, plan=plan, schedule=schedule, guest_vins=guests)
        elif roll < 0.62:
            call = rng.choice(["api.standby.set_default_standby_plan", "api.standby.delete_standby_plan"])
            plan_id = rng.choice(PLAN_IDS + [str(rng.randint(0, 2**64 - 1)), random_text(rng, 8)])
            if _text_id_fits(plan_id):
                replies = _replies(rng, session, lambda r: r.choice([{"message": "ok"}, {}, [], random_object(r)]))
                yield call, scenario(**base, replies=replies, vin=vin, plan_id=plan_id, guest_vins=guests)
        elif roll < 0.72:
            plan_id = rng.choice(PLAN_IDS + [str(rng.randint(0, 2**64 - 1)), random_text(rng, 8)])
            replies = _replies(rng, session, lambda r: r.choice(SEND_DATA + [random_object(r)]))
            yield "api.standby.trigger_send_standby_plan", scenario(**base, replies=replies, vin=vin, plan_id=plan_id, guest_vins=guests)
        elif roll < 0.82:
            serial = rng.choice(SERIALS + [random_text(rng, 8)])
            replies = _replies(rng, session, lambda r: r.choice(RESULT_DATA + [random_object(r)]))
            yield "api.standby.poll_send_result", scenario(**base, replies=replies, vin=vin, request_serial=serial, guest_vins=guests)
        elif roll < 0.9:
            calls = [
                {"endpoint": rng.choice([SEND, SEND_RESULT, SEND_RESULT, "/other"]), "request_serial": rng.choice(SERIALS[:8])}
                for _ in range(rng.randint(0, 4))
            ]
            replies = [
                random_reply(rng, session, lambda r: r.choice(SEND_DATA + RESULT_DATA)) for _ in range(rng.randint(0, len(calls) + 1))
            ]
            yield "api.standby.make_send_fetch_fn", scenario(**base, replies=replies, vin=vin, plan_id=rng.choice(PLAN_IDS), calls=calls)
        else:
            now_ms = base["now_ms"]
            schedule = _random_schedule(rng, now_ms)
            now = rng.choice([None, None, NOW_ISO, "2026-11-18T15:42:00", (dt.datetime(1970, 1, 1, tzinfo=dt.UTC) + dt.timedelta(milliseconds=now_ms + rng.randint(-10**9, 10**9))).isoformat()])
            if json_safe(schedule) and ms._ints_fit(schedule) and _daily_is_near(schedule, now_ms, now):
                yield "api.standby.schedule_fields", {"schedule": schedule, "now_ms": now_ms, "now": now}
                plan = _random_plan(rng)
                if _save_usable(plan, schedule):
                    yield "api.standby.build_save_inner", {
                        "config": base["config"], "vin": vin, "plan": plan, "schedule": schedule, "now_ms": now_ms, "now": now,
                        "seed": base["seed"],
                    }
        yield "api.standby.is_send_ready", {"payload": rng.choice(READY_SAMPLES + [{"res": rng.choice([1, 2, 1.0, -5, "1", None])}])}
        if rng.random() < 0.3:
            yield "api.standby.schedule_fields", _random_zoned(rng)


skip_unrepresentable_integers(group)
