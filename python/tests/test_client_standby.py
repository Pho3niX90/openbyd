"""The preconditioning plan methods of openbyd.Client, against pyBYD's over the wire.

Each case runs pyBYD's ``BydClient`` and ``openbyd.Client`` against one
``openbyd.testing.FakeServer`` with the same replies, and compares the
results (pyBYD's dumped with ``model_dump(mode="json")``) and the requests
each sent: everything in the decrypted inner payload but the per-request
``random`` and clock.

pyBYD is given its models; openbyd is given what its own methods return
(plain dicts), mappings of the models' fields, or pyBYD's models.
"""

from __future__ import annotations

import asyncio
import datetime as dt
import json
from collections.abc import Awaitable, Callable
from typing import Any
from zoneinfo import ZoneInfo

import pytest

import openbyd
import openbyd.client

pybyd = pytest.importorskip("pybyd")
testing = pytest.importorskip(
    "openbyd.testing", reason="extension built without the 'testing' feature", exc_type=ImportError
)
FakeServer, Reply = testing.FakeServer, testing.Reply

from pybyd.models.standby import StandbyPlanCommand, StandbySchedule  # noqa: E402
from pydantic import BaseModel  # noqa: E402

from parity.groups.models_standby import CAPTURED_COMMANDS, NEW_PLAN, PLAN  # noqa: E402

VIN = "TESTVIN0000000001"
USER = "user@example.com"
PASSWORD = "secret"
MADRID = ZoneInfo("Europe/Madrid")

LIST = "/control/standby-vehicles/list"
SAVE = "/control/standby-vehicles/save"
SET_DEFAULT = "/control/standby-vehicles/setDefault"
DELETE = "/control/standby-vehicles/delete"
SEND = "/control/standby-vehicles/send"
SEND_RESULT = "/control/standby-vehicles/send-result"
CREATED = {"id": "7", "planName": "Test plan", "planType": 2, "executeType": "0"}
#: What differs between two requests for the same thing.
PER_REQUEST = {"random", "timeStamp"}


def plain(value: Any) -> Any:
    return value.model_dump(mode="json") if isinstance(value, BaseModel) else value


async def outcome(call: Awaitable[Any]) -> dict[str, Any]:
    try:
        return {"result": plain(await call)}
    except Exception as error:  # noqa: BLE001 - the class is compared
        return {"error": [type(error).__name__, getattr(error, "code", None)]}


Call = Callable[[Any, str], Awaitable[Any]]


def compare(setup: Callable[[Any], None], call: Call, *, endpoint: str | None = None, ignore: set[str] = frozenset()) -> dict:
    """Runs ``call`` on both clients against one fake server and compares
    the outcomes and what each sent to ``endpoint``; returns openbyd's outcome."""

    async def main() -> tuple[list[dict], list[list[dict]]]:
        with FakeServer([(USER, PASSWORD)]) as server:
            config = {"username": USER, "password": PASSWORD, "base_url": server.url, "mqtt_enabled": False,
                      "time_zone": "Europe/Madrid"}
            results, sent = [], []
            for library, client in (
                ("pybyd", pybyd.BydClient(pybyd.BydConfig(**config))),
                ("openbyd", openbyd.Client(**config)),
            ):
                setup(server)
                async with client:
                    await client.login()
                    server.clear_requests()
                    results.append(await outcome(call(client, library)))
                    requests = server.requests(endpoint) if endpoint else []
                    sent.append([
                        {key: value for key, value in request["inner"].items() if key not in PER_REQUEST | ignore}
                        for request in requests
                    ])
            return results, sent

    results, sent = asyncio.run(main())
    assert json.dumps(results[1]) == json.dumps(results[0]), f"pyBYD: {results[0]}\nopenbyd: {results[1]}"
    assert json.dumps(sent[1]) == json.dumps(sent[0]), f"pyBYD sent: {sent[0]}\nopenbyd sent: {sent[1]}"
    if endpoint:
        assert sent[1], f"nothing was sent to {endpoint}"
    return results[1]


def plans(*listed: dict) -> Callable[[Any], None]:
    def setup(server: Any) -> None:
        server.reply(LIST, Reply.data(list(listed)))
        for endpoint in (SAVE, SET_DEFAULT, DELETE):
            server.reply(endpoint, Reply.data({"message": "ok"}))

    return setup


async def first_plan(client: Any) -> Any:
    """The preset plan as each library returns it: pyBYD's model, openbyd's dict."""
    listed = await client.get_standby_plans(VIN)
    return listed["plans"][0] if isinstance(listed, dict) else listed.plans[0]


def test_get_standby_plans() -> None:
    got = compare(plans(PLAN, NEW_PLAN), lambda client, _: client.get_standby_plans(VIN), endpoint=LIST)
    assert [plan["id"] for plan in got["result"]["plans"]] == ["1216038691305533440", "2"]
    assert got["result"]["plans"][0]["commands"][0]["control_params_map"]["mainSettingTemp"] == 5
    compare(lambda server: server.reply(LIST, Reply.data({"data": [PLAN]})), lambda client, _: client.get_standby_plans(VIN))
    for reply in [Reply.code("1001"), Reply.code("1008"), Reply.data("text"), Reply.status(502)]:
        got = compare(lambda server, reply=reply: server.reply(LIST, reply), lambda client, _: client.get_standby_plans(VIN))
        assert "error" in got


SCHEDULES: list[tuple[Any, Any]] = [
    # (what pyBYD is given, what openbyd is given)
    (None, None),
    (StandbySchedule.off(), {"repeat": "off"}),
    (StandbySchedule.weekly("12:55", [5, 4]), {"repeat": "weekly", "start_time": "12:55", "weekly_days": (4, 5)}),
    (StandbySchedule.weekly("12:55", [5, 4]), StandbySchedule.weekly("12:55", [5, 4])),
    (StandbySchedule.once(dt.datetime(2026, 11, 22, 19, 10, tzinfo=MADRID)),
     {"repeat": "once", "at": dt.datetime(2026, 11, 22, 19, 10, tzinfo=MADRID)}),
    (StandbySchedule.once(dt.datetime(2026, 7, 22, 19, 10, 0, 1000, tzinfo=MADRID)),
     StandbySchedule.once(dt.datetime(2026, 7, 22, 19, 10, 0, 1000, tzinfo=MADRID))),
    (StandbySchedule.once(dt.datetime(2030, 1, 2, 3, 4, 5, tzinfo=dt.timezone(dt.timedelta(hours=5, minutes=30)))),
     {"repeat": "once", "at": "2030-01-02T03:04:05+05:30"}),
    (StandbySchedule.once(dt.datetime(2030, 1, 2, 3, 4, 5, tzinfo=dt.UTC)), {"repeat": "once", "at": 1893553445}),
]


@pytest.mark.parametrize(("theirs", "ours"), SCHEDULES)
def test_save_standby_plan(theirs: Any, ours: Any) -> None:
    async def call(client: Any, library: str) -> Any:
        plan = await first_plan(client)
        return await client.save_standby_plan(VIN, plan, schedule=theirs if library == "pybyd" else ours)

    got = compare(plans(PLAN, NEW_PLAN), call, endpoint=SAVE)
    assert len(got["result"]["plans"]) == 2


def test_save_takes_pybyd_models_and_edits() -> None:
    async def call(client: Any, library: str) -> Any:
        # openbyd is handed pyBYD's own model here.
        plan = pybyd.models.standby.StandbyPlan.model_validate(PLAN)
        return await client.save_standby_plan(VIN, plan)

    compare(plans(PLAN), call, endpoint=SAVE)

    async def edited(client: Any, library: str) -> Any:
        plan = await first_plan(client)
        if library == "pybyd":
            command = plan.commands[0]
            changed = command.model_copy(update={"control_params_map": {**command.control_params_map, "mainSettingTemp": 9}})
            plan = plan.model_copy(update={"plan_name": "Warmer", "default_plan": 0, "commands": [changed]})
        else:
            plan = {**plan, "plan_name": "Warmer", "default_plan": 0}
            plan["commands"][0]["control_params_map"]["mainSettingTemp"] = 9
        return await client.save_standby_plan(VIN, plan, schedule=StandbySchedule.off() if library == "pybyd" else {"repeat": "off"})

    compare(plans(PLAN), edited, endpoint=SAVE)


@pytest.mark.parametrize("month", [1, 7])
def test_a_stale_daily_time_keeps_its_time_of_day_across_daylight_saving(month: int) -> None:
    # Years stale, so the step to the next occurrence crosses changes of
    # daylight saving time; in one of the two months the offset then differs
    # from today's. The time of day is kept six hours from now, so the next
    # occurrence is the same for both clients.
    hour = (dt.datetime.now(MADRID).hour + 6) % 24
    at = dt.datetime(2020, month, 15, hour, 30, tzinfo=MADRID)

    async def call(client: Any, library: str) -> Any:
        plan = await first_plan(client)
        schedule = StandbySchedule.daily(at) if library == "pybyd" else {"repeat": "daily", "at": at}
        return await client.save_standby_plan(VIN, plan, schedule=schedule)

    compare(plans(PLAN), call, endpoint=SAVE)


def _day_before_the_last_clock_change(zone: ZoneInfo) -> dt.date:
    def noon(day: dt.date) -> dt.datetime:
        return dt.datetime(day.year, day.month, day.day, 12, tzinfo=zone)

    day = dt.datetime.now(zone).date()
    while noon(day - dt.timedelta(days=1)).utcoffset() == noon(day).utcoffset():
        day -= dt.timedelta(days=1)
    return day - dt.timedelta(days=1)


def test_a_daily_time_that_passes_while_the_request_is_built_keeps_its_time_of_day(monkeypatch) -> None:
    """The Python layer sees the run still ahead and leaves it alone; when the
    extension builds the request it has passed, and the clocks changed that
    night. pyBYD, which works in the time zone, sends the same time of day.
    The extension, which has only a UTC offset, would send it an hour off,
    were it not handed the run of the day after."""
    now = dt.datetime.now(MADRID)
    hour = (now.hour + 6) % 24  # hours from now, so the next run is the same for both clients
    hour = hour + 12 if hour < 4 else hour  # and after the hour the clocks change at
    at = dt.datetime.combine(_day_before_the_last_clock_change(MADRID), dt.time(hour, 30), tzinfo=MADRID)
    # The Python layer's clock stands a second before the run. The extension reads the real one.
    monkeypatch.setattr(openbyd.client, "_now", lambda: at.astimezone(dt.timezone.utc) - dt.timedelta(seconds=1))

    async def call(client: Any, library: str) -> Any:
        plan = await first_plan(client)
        schedule = StandbySchedule.daily(at) if library == "pybyd" else {"repeat": "daily", "at": at}
        return await client.save_standby_plan(VIN, plan, schedule=schedule)

    compare(plans(PLAN), call, endpoint=SAVE)


def test_a_daily_schedule_carries_the_run_of_the_day_after(monkeypatch) -> None:
    schedule = openbyd.client._schedule
    eight = dt.datetime(2026, 10, 24, 8, tzinfo=MADRID)  # the clocks go back that night
    for at, now, sent, day_after in [
        (eight, "2026-10-24T05:59:59+00:00", "2026-10-24T06:00:00+00:00", "2026-10-25T07:00:00+00:00"),  # 25 hours on
        (eight, "2026-10-24T06:00:00+00:00", "2026-10-25T07:00:00+00:00", "2026-10-26T07:00:00+00:00"),  # already passed
        # and forward: 23 hours on
        (dt.datetime(2026, 3, 28, 8, tzinfo=MADRID), "2026-03-28T06:59:59+00:00", "2026-03-28T07:00:00+00:00",
         "2026-03-29T06:00:00+00:00"),
    ]:
        monkeypatch.setattr(openbyd.client, "_now", lambda now=now: dt.datetime.fromisoformat(now))
        fields = schedule({"repeat": "daily", "at": at})
        assert (fields["at"], fields["_daily_after"]) == (sent, day_after)
        assert "_daily_after" not in schedule({"repeat": "once", "at": at})
        assert "_daily_after" not in schedule({"repeat": "daily", "at": at.isoformat()})  # text has no time zone
    # The last day a datetime holds has no day after.
    assert "_daily_after" not in schedule({"repeat": "daily", "at": dt.datetime(9999, 12, 31, 8, tzinfo=dt.timezone.utc)})


@pytest.mark.parametrize("extra", ["garbage", "2099-01-01T00:00:00Z"])
@pytest.mark.parametrize(
    "fields",
    [
        {"repeat": "off"},
        {"repeat": "weekly", "start_time": "12:55", "weekly_days": [4, 5]},
        {"repeat": "once", "at": dt.datetime(2026, 11, 22, 19, 10, tzinfo=MADRID)},
        {"repeat": "daily", "at": "2026-10-24T08:00:00+02:00"},
        {"repeat": "daily", "at": dt.datetime(2026, 10, 24, 8, tzinfo=MADRID)},
        {"repeat": "daily", "at": dt.datetime(9999, 12, 31, 8, tzinfo=dt.timezone.utc)},
    ],
)
def test_caller_daily_after_key_is_ignored(fields: dict[str, Any], extra: str) -> None:
    """A caller's own ``_daily_after`` key is an unknown field, ignored as pyBYD ignores it."""
    given = {**fields, "_daily_after": extra}

    async def call(client: Any, library: str) -> Any:
        plan = await first_plan(client)
        schedule = StandbySchedule(**given) if library == "pybyd" else given
        return await client.save_standby_plan(VIN, plan, schedule=schedule)

    compare(plans(PLAN), call, endpoint=SAVE)


def test_native_schedule_ignores_daily_after_when_not_daily() -> None:
    """The extension itself only parses ``_daily_after`` on a daily schedule."""

    async def call(client: Any, library: str) -> Any:
        plan = await first_plan(client)
        if library == "pybyd":
            return await client.save_standby_plan(VIN, plan, schedule=StandbySchedule.off())
        return await client._native.save_standby_plan(VIN, plan, schedule={"repeat": "off", "_daily_after": "garbage"})

    compare(plans(PLAN), call, endpoint=SAVE)


@pytest.mark.parametrize(
    ("repeat", "at"),
    [
        ("once", dt.datetime(1, 1, 1, 0, 0, tzinfo=dt.timezone(dt.timedelta(hours=1)))),
        ("daily", dt.datetime(9999, 12, 31, 23, 30, tzinfo=dt.timezone(dt.timedelta(hours=-1)))),
        ("off", dt.datetime(1, 1, 1, 0, 0, tzinfo=dt.timezone(dt.timedelta(hours=1)))),
    ],
)
def test_schedule_at_the_ends_of_the_calendar_does_not_overflow(repeat: str, at: dt.datetime) -> None:
    """Aware datetimes whose UTC reading leaves years 1 to 9999 keep their offset and match pyBYD."""

    async def call(client: Any, library: str) -> Any:
        plan = await first_plan(client)
        schedule = StandbySchedule(repeat=repeat, at=at) if library == "pybyd" else {"repeat": repeat, "at": at}
        return await client.save_standby_plan(VIN, plan, schedule=schedule)

    compare(plans(PLAN), call, endpoint=SAVE)


def test_what_the_models_refuse_is_refused_here() -> None:
    async def run(call: Callable[[Any], Awaitable[Any]]) -> BaseException:
        async with openbyd.Client(username=USER, password=PASSWORD, base_url="http://127.0.0.1:9", mqtt_enabled=False) as client:
            with pytest.raises(Exception) as raised:  # noqa: PT011 - the class is checked by the caller
                await call(client)
            return raised.value

    plan = {"id": "7", "commands": [{"command_type": "OPENAIR"}]}
    naive = dt.datetime(2026, 11, 22, 19, 10)
    for schedule in [{"repeat": "once"}, {"repeat": "daily", "at": naive}, {"repeat": "weekly", "start_time": "25:00", "weekly_days": [1]},
                     {"repeat": "sometimes"}, {"repeat": "weekly", "start_time": "10:00", "weekly_days": {7}}]:
        error = asyncio.run(run(lambda client, schedule=schedule: client.save_standby_plan(VIN, plan, schedule=schedule)))
        assert isinstance(error, openbyd.ValidationError), schedule
    assert isinstance(asyncio.run(run(lambda client: client.save_standby_plan(VIN, {"plan_name": 5}))), openbyd.ValidationError)
    for bad in [5, "plan", None]:
        assert isinstance(asyncio.run(run(lambda client, bad=bad: client.save_standby_plan(VIN, bad))), TypeError)
        assert isinstance(asyncio.run(run(lambda client, bad=bad: client.save_standby_plan(VIN, plan, schedule=bad or 5))), TypeError)
        assert isinstance(asyncio.run(run(lambda client, bad=bad: client.create_standby_plan(VIN, "x", [bad]))), TypeError)
    # No A/C: refused before anything is sent (nothing listens on that port).
    error = asyncio.run(run(lambda client: client.create_standby_plan(VIN, "x", [CAPTURED_COMMANDS[2]])))
    assert type(error) is ValueError


def test_create_standby_plan() -> None:
    def setup(server: Any) -> None:
        plans()(server)
        server.script(LIST, [Reply.data([PLAN]), Reply.data([PLAN, CREATED])])

    async def call(client: Any, library: str) -> Any:
        if library == "pybyd":
            commands: list[Any] = [StandbyPlanCommand.model_validate(command) for command in CAPTURED_COMMANDS]
            schedule: Any = StandbySchedule.weekly("07:00", [0, 1])
        else:
            commands = CAPTURED_COMMANDS
            schedule = {"repeat": "weekly", "start_time": "07:00", "weekly_days": [0, 1]}
        return await client.create_standby_plan(VIN, "Test plan", commands, schedule=schedule)

    got = compare(setup, call, endpoint=SAVE)
    assert got["result"]["id"] == "7"

    async def copied(client: Any, library: str) -> Any:
        # The commands of the preset plan, as each library returned them.
        plan = await first_plan(client)
        commands = plan["commands"] if isinstance(plan, dict) else plan.commands
        return await client.create_standby_plan(VIN, "nan", commands)

    def twice(server: Any) -> None:
        plans()(server)
        server.script(LIST, [Reply.data([PLAN]), Reply.data([PLAN]), Reply.data([PLAN, CREATED])])

    assert compare(twice, copied, endpoint=SAVE)["result"]["plan_name"] == "Test plan"

    def nothing_new(server: Any) -> None:
        plans(PLAN)(server)

    assert compare(nothing_new, call)["error"][0] == "BydError"

    async def no_climate(client: Any, library: str) -> Any:
        command = CAPTURED_COMMANDS[2]
        return await client.create_standby_plan(VIN, "x", [StandbyPlanCommand.model_validate(command) if library == "pybyd" else command])

    assert compare(nothing_new, no_climate)["error"][0] == "ValueError"


@pytest.mark.parametrize(("method", "endpoint"), [("set_default_standby_plan", SET_DEFAULT), ("delete_standby_plan", DELETE)])
def test_plan_id_methods(method: str, endpoint: str) -> None:
    for plan_id in ["1216038691305533440", "7", "abc"]:
        got = compare(plans(PLAN), lambda client, _, plan_id=plan_id: getattr(client, method)(VIN, plan_id), endpoint=endpoint)
        assert got["result"]["plans"][0]["id"] == "1216038691305533440"

    def failing(server: Any) -> None:
        plans(PLAN)(server)
        server.reply(endpoint, Reply.code("1001"))

    assert compare(failing, lambda client, _: getattr(client, method)(VIN, "7"))["error"] == ["BydEndpointNotSupportedError", "1001"]


def test_run_standby_plan() -> None:
    def setup(results: list[Any]) -> Callable[[Any], None]:
        def install(server: Any) -> None:
            server.reply(SEND, Reply.data({"requestSerial": "S1"}))
            server.script(SEND_RESULT, results)

        return install

    def run(client: Any, _: str) -> Awaitable[Any]:
        return client.run_standby_plan(VIN, "1216038691305533440", poll_attempts=3, poll_interval=0.0)

    # `executeTime` is the clock, like the request's own `timeStamp`.
    done = compare(setup([Reply.data({"res": 1}), Reply.data({"res": 2, "message": "ok"})]), run, endpoint=SEND, ignore={"executeTime"})
    assert done["result"]["res"] == 2 and done["result"]["vin"] == VIN
    polls = compare(setup([Reply.data({"res": 1}), Reply.data({"res": 2})]), run, endpoint=SEND_RESULT)
    assert polls["result"]["request_serial"] == "S1"
    still = compare(setup([Reply.data({"res": 1})] * 3), run)
    assert still["result"]["res"] == 1
    failed = compare(setup([Reply.data({"res": 3, "message": "no"})]), run)
    assert failed["error"] == ["BydRemoteControlError", "3"]
    refused = compare(lambda server: server.reply(SEND, Reply.code("1001")), run)
    assert refused["error"] == ["BydEndpointNotSupportedError", "1001"]
