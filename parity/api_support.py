"""Trace parity for pyBYD's ``_api`` flows (the ``async def`` endpoint functions).

A flow is compared by running it against a script and recording what it
does. The parity call ``api.<file>.<function>`` takes::

    {
      "config": {...},              # BydConfig keyword arguments
      "session": {...},             # Session keyword arguments (give created_at)
      "guest_vins": {"VIN": true},  # the session's guest map before the call
      "user_type_settled": [["VIN", "/endpoint"]],
      "now_ms": 1700000000000,      # the wall clock at the start
      "seed": "seed",               # the random values derive from it
      "replies": [...],             # the script, see ok_reply() and friends
      "args": {...}                 # the function's own arguments
    }

and returns the trace::

    {
      "steps": [{"post": "/endpoint", "outer": {...}}, {"sleep": 1.0}, ...],
      "result": ...,                # or "error": {"class", "code", "endpoint", "status_code"}
      "guest_vins": {...},          # after the call
      "user_type_settled": [...],   # after the call, sorted
      "unused_replies": 0
    }

Both sides follow the same rules, so neither has to count clock reads:

- the wall clock is ``now_ms`` plus the total time slept so far
  (``now_ms + int(slept * 1000)``); ``time.time()`` returns that plus half a
  millisecond, so ``int(time.time() * 1000)`` gives it back exactly;
- ``time.monotonic()`` is ``now_ms / 1000 + slept``;
- ``datetime.now(tz)``, where a pyBYD module reads it (``_api/standby.py``), is the
  same wall clock, exact to the millisecond;
- the n-th ``secrets.token_hex(16)`` (from 0) is ``MD5("<seed>:<n>")``;
- ``asyncio.sleep(seconds)`` returns at once and adds to ``slept``;
- ``transport.post_secure`` records the step and answers with the next
  reply: an outer reply, or a ``BydTransportError``. With no reply left it
  raises ``ScriptExhausted``, which is not a pyBYD error, so it passes
  through every ``except`` in pyBYD and ends the flow. A reply may carry
  ``"date"``, the HTTP ``Date`` header it came with: the transport's
  ``server_time_offset_ms`` follows it through pyBYD's own
  ``SecureTransport._sync_clock_from_header``, on a host whose zone is UTC.

Porting a flow needs an oracle and cases::

    group = Group("api_gps", crate="client")
    flow_oracle(group, "api.gps.fetch_gps_endpoint", gps.fetch_gps_endpoint)

    @group.recorded
    def recorded():
        yield "api.gps.fetch_gps_endpoint", scenario(
            replies=[ok_reply(SAMPLE_SESSION, {"latitude": 52.0})],
            endpoint="/control/getGpsInfo", vin=VIN, request_serial=None,
        )
"""

from __future__ import annotations

import asyncio
import copy
import datetime
import hashlib
import json
import math
import secrets
import time
from collections.abc import Callable, Iterable
from contextlib import ExitStack
from typing import Any
from unittest import mock

from pybyd import exceptions
from pybyd._crypto.aes import aes_encrypt_hex
from pybyd._crypto.hashing import md5_hex
from pybyd.config import BydConfig
from pybyd.session import Session

from parity.framework import Group, random_hex, random_text
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION

VIN = "TESTVIN0000000001"
NOW_MS = 1_700_000_000_000

#: Codes worth trying on any endpoint: success, session expired, not
#: supported / guest not declared, busy, unreachable, GPS unavailable, not a
#: guest, unmapped ones and near misses.
CODES = [
    "0", "1001", "1002", "1011", "3001", "3003", "1005", "1010", "1008", "6002", "6051", "8003", "8002", "9999", "",
    " 1001", "1001 ", "1030", "1003",
]
#: Codes that are not strings: pyBYD compares ``str(code)``.
ODD_CODES = [0, 1001, 1001.0, 8003, None, True, [], {"a": 1}]


class ScriptExhausted(Exception):
    """The flow asked for more replies than the script holds."""


# ---------------------------------------------------------------------------
# Case builders
# ---------------------------------------------------------------------------


def content_key(session: dict) -> str:
    """The AES key a session's replies are sealed with."""
    return md5_hex(str(session["encry_token"]).strip())


def ok_reply(session: dict, data: Any, *, code: Any = "0", message: Any = "SUCCESS") -> dict:
    """A successful reply carrying ``data`` as ``respondData``, sealed with the session's key."""
    sealed = aes_encrypt_hex(json.dumps(data, ensure_ascii=False), content_key(session))
    return {"reply": {"code": code, "message": message, "respondData": sealed}}


def sealed_reply(session: dict, plaintext: str, *, code: Any = "0") -> dict:
    """A reply whose ``respondData`` decrypts to ``plaintext``, JSON or not."""
    return {"reply": {"code": code, "message": "", "respondData": aes_encrypt_hex(plaintext, content_key(session))}}


def error_reply(code: Any, message: Any = "error") -> dict:
    """A reply with a non-zero ``code`` and no data."""
    return {"reply": {"code": code, "message": message}}


def raw_reply(outer: Any) -> dict:
    """Any decoded outer reply, including ones that are not objects."""
    return {"reply": outer}


def transport_error(status_code: int | None = None) -> dict:
    """The transport fails with ``BydTransportError``."""
    return {"transport_error": {"status_code": status_code}}


def scenario(
    *,
    config: dict | None = None,
    session: dict | None = None,
    guest_vins: dict[str, bool] | None = None,
    user_type_settled: Iterable[tuple[str, str]] = (),
    now_ms: int = NOW_MS,
    seed: str = "seed",
    replies: Iterable[dict] = (),
    args: dict | None = None,
    server_time_offset_ms: int | None = None,
    **more_args: Any,
) -> dict:
    """The arguments of a flow's parity call. The function's own arguments are
    ``args`` and any keywords not named here (pass a pyBYD ``now_ms`` in
    ``args``: the keyword is the clock). ``server_time_offset_ms`` is the
    transport's clock offset before the flow, as earlier ``Date`` headers
    would have left it."""
    case = {
        "config": SAMPLE_CONFIG if config is None else config,
        "session": SAMPLE_SESSION if session is None else session,
        "guest_vins": guest_vins or {},
        "user_type_settled": [list(pair) for pair in user_type_settled],
        "now_ms": now_ms,
        "seed": seed,
        "replies": list(replies),
        "args": {**(args or {}), **more_args},
    }
    if server_time_offset_ms is not None:
        case["server_time_offset_ms"] = server_time_offset_ms
    return case


def random_date(rng) -> str:
    """A ``Date`` header near the usual scripted clock (within the drift
    threshold or beyond it), far from it, or one Python cannot read."""
    roll = rng.random()
    if roll < 0.8:
        return http_date(NOW_MS + rng.choice([0, 5_000, -29_000, 31_000, -45_000, 120_000, rng.randint(-10**7, 10**7)]))
    if roll < 0.9:
        return http_date(rng.randint(0, 2**41))
    return rng.choice(["", "nonsense", "Sat, 22 Nov 2026 10:00:00", "31 Feb 2026 10:00:00 GMT"])


def random_reply(rng, session: dict, data: Callable[[Any], Any] | None = None) -> dict:
    """A reply of any shape: success with assorted data, error codes, broken
    ``respondData``, non-object replies and transport errors; now and then
    with a ``Date`` header."""
    reply = _random_reply(rng, session, data)
    if rng.random() < 0.15:
        reply = date_reply(reply, random_date(rng))
    return reply


def _random_reply(rng, session: dict, data: Callable[[Any], Any] | None = None) -> dict:
    roll = rng.random()
    if roll < 0.35:
        payload = data(rng) if data else random_data(rng)
        return ok_reply(session, payload, code=rng.choice(["0", "0", 0]), message=random_text(rng, 8))
    if roll < 0.65:
        return error_reply(rng.choice(CODES + ODD_CODES), rng.choice(["", "msg", random_text(rng, 12), None, 5]))
    if roll < 0.75:
        return rng.choice(
            [
                raw_reply({}),
                raw_reply({"code": "0"}),
                raw_reply({"code": "0", "respondData": ""}),
                raw_reply({"code": "0", "respondData": None}),
                raw_reply({"code": "0", "respondData": "ZZ"}),
                raw_reply({"code": "0", "respondData": random_hex(rng, 16)}),
                sealed_reply(session, rng.choice(["", "  ", "{not json", "[1, 2", "null", "7", '"text"'])),
            ]
        )
    if roll < 0.85:
        return raw_reply(rng.choice([[], [1], "text", "", None, 0, 1.5, True]))
    return transport_error(rng.choice([None, 500, 502, 404]))


def vehicle_scripts(endpoint: str, ok: dict, *, vin: str = VIN) -> list[dict]:
    """``scenario`` keywords that walk every branch of ``post_vehicle_json``'s
    guest retry for one endpoint: right first time, each wrong guess corrected,
    refused both ways, the retry failing with another code, an expired
    session on either attempt, an already-settled pair, transport errors and
    running out of replies. ``ok`` is the success reply to use."""
    owner, guest = {vin: False}, {vin: True}
    other_codes = ["6002", "1008", "6051", "9999", "5005", "1001", "8003"]
    scripts = [
        {"guest_vins": {}, "replies": [ok]},
        {"guest_vins": owner, "replies": [ok]},
        {"guest_vins": guest, "replies": [ok]},
        {"guest_vins": owner, "replies": [error_reply("1001"), ok]},
        {"guest_vins": guest, "replies": [error_reply("8003"), ok]},
        {"guest_vins": {}, "replies": [error_reply("1001"), error_reply("8003")]},
        {"guest_vins": guest, "replies": [error_reply("8003"), error_reply("1001")]},
        {"guest_vins": owner, "replies": [error_reply("8003")]},
        {"guest_vins": guest, "replies": [error_reply("1001")]},
        {"guest_vins": owner, "user_type_settled": [(vin, endpoint)], "replies": [error_reply("1001"), ok]},
        {"guest_vins": guest, "user_type_settled": [(vin, endpoint)], "replies": [error_reply("8003"), ok]},
        {"guest_vins": owner, "user_type_settled": [(vin, "/other")], "replies": [error_reply("1001"), ok]},
        {"guest_vins": {"OTHERVIN000000001": True}, "replies": [error_reply("1001"), ok]},
        {"guest_vins": owner, "replies": [error_reply(1001), ok]},
        {"guest_vins": owner, "replies": [error_reply("1001"), transport_error(503)]},
        {"guest_vins": owner, "replies": [transport_error(None)]},
        {"guest_vins": owner, "replies": [error_reply("1001")]},
        {"guest_vins": guest, "replies": [error_reply("8003"), raw_reply([])]},
        {"guest_vins": owner, "replies": [raw_reply("text")]},
        {"guest_vins": owner, "replies": []},
        {"guest_vins": owner, "replies": [ok, ok]},
    ]
    # the codes that mean a new login is needed, and two that used to
    for code in ["1002", "1011", "3001", "3003", "1005", "1010"]:
        scripts.append({"guest_vins": owner, "replies": [error_reply(code), ok]})
        scripts.append({"guest_vins": owner, "replies": [error_reply("1001"), error_reply(code)]})
        scripts.append({"guest_vins": guest, "replies": [error_reply("8003"), error_reply(code)]})
    for code in other_codes:
        scripts.append({"guest_vins": owner, "replies": [error_reply("1001"), error_reply(code)]})
        scripts.append({"guest_vins": guest, "replies": [error_reply("8003"), error_reply(code)]})
    return scripts


def random_vehicle_script(rng, session: dict, endpoint: str, data: Callable[[Any], Any] | None = None) -> dict:
    """Random ``scenario`` keywords for a flow that uses ``post_vehicle_json``:
    guest or owner (believed and settled at random), one to four replies
    leaning on the codes that drive the retry."""
    vin = rng.choice([VIN, VIN, "", random_text(rng, 17)])
    guest_vins = {}
    if rng.random() < 0.8:
        guest_vins[vin] = rng.random() < 0.5
    if rng.random() < 0.2:
        guest_vins[random_text(rng, 17)] = rng.random() < 0.5
    settled = []
    if rng.random() < 0.2:
        settled.append((vin, endpoint))
    if rng.random() < 0.1:
        settled.append((random_text(rng, 17), random_text(rng, 10)))
    replies = []
    for _ in range(rng.choice([0, 1, 1, 2, 2, 2, 3, 4])):
        if rng.random() < 0.4:
            replies.append(error_reply(rng.choice(["1001", "8003", "1001", "8003", "1002", "1005"])))
        else:
            replies.append(random_reply(rng, session, data))
    return {"vin": vin, "guest_vins": guest_vins, "user_type_settled": settled, "replies": replies}


def random_offset(rng) -> int:
    """A clock offset earlier replies may have left: none, or a drift either way."""
    return rng.choice([0, 0, 0, 60_000, -45_000, rng.randint(-10**8, 10**8)])


def random_scenario_base(rng) -> dict:
    """Random config, session, clock and seed for a flow case."""
    from parity.groups.config import random_config_kwargs, random_session_kwargs

    return {
        "config": random_config_kwargs(rng, valid=True) if rng.random() < 0.5 else SAMPLE_CONFIG,
        "session": random_session_kwargs(rng) if rng.random() < 0.5 else SAMPLE_SESSION,
        "now_ms": rng.choice([NOW_MS, 0, rng.randint(0, 4_000_000_000_000)]),
        "seed": random_text(rng, 8),
    }


def random_data(rng) -> Any:
    """``respondData`` content: mostly objects, sometimes not."""
    from parity.groups.pyjson import random_object

    roll = rng.random()
    if roll < 0.5:
        return random_object(rng)
    if roll < 0.7:
        return rng.choice([{}, {"requestSerial": "S-" + random_text(rng, 6)}, {"requestSerial": 5}])
    return rng.choice([[], [1, 2], "text", 0, None, 1.5, True])


# ---------------------------------------------------------------------------
# The runner
# ---------------------------------------------------------------------------


class _Clock:
    def __init__(self, now_ms: int, seed: str) -> None:
        self.start_ms = now_ms
        self.seed = seed
        self.slept = 0.0
        self.draws = 0

    def now_ms(self) -> int:
        return self.start_ms + int(self.slept * 1000)

    def time(self) -> float:
        return (self.now_ms() + 0.5) / 1000

    def monotonic(self) -> float:
        return self.start_ms / 1000 + self.slept

    def token_hex(self, nbytes: int | None = None) -> str:
        assert nbytes == 16, f"the script only draws 16 random bytes, not {nbytes}"
        value = hashlib.md5(f"{self.seed}:{self.draws}".encode()).hexdigest()
        self.draws += 1
        return value


class _ScriptedDatetime:
    """Stands in for the ``datetime`` module in a pyBYD module that reads
    ``datetime.now``: ``now`` answers the scripted wall clock (``now_ms`` plus
    the time slept), everything else is the real module's."""

    def __init__(self, clock: _Clock) -> None:
        def now(_cls: type, tz: datetime.tzinfo | None = None) -> datetime.datetime:
            moment = datetime.datetime(1970, 1, 1, tzinfo=datetime.UTC) + datetime.timedelta(milliseconds=clock.now_ms())
            return moment.replace(tzinfo=None) if tz is None else moment.astimezone(tz)

        self.datetime = type("datetime", (datetime.datetime,), {"now": classmethod(now)})

    def __getattr__(self, name: str) -> Any:
        return getattr(datetime, name)


def patch_datetime_now(stack: ExitStack, clock: _Clock) -> None:
    """Make ``datetime.now`` in pyBYD's ``_api/standby.py`` (its only reader) follow ``clock``."""
    from pybyd._api import standby

    stack.enter_context(mock.patch.object(standby, "dt", _ScriptedDatetime(clock)))


def date_reply(entry: dict, date: str) -> dict:
    """``entry`` arriving with the HTTP ``Date`` header ``date``."""
    return {**entry, "date": date}


def http_date(ms: int) -> str:
    """The ``Date`` header for the instant ``ms`` (whole seconds, GMT)."""
    import email.utils

    return email.utils.formatdate(ms / 1000, usegmt=True)


class ScriptedTransport:
    """Stands in for ``SecureTransport``: records each request, answers from the script."""

    def __init__(self, replies: list[dict], steps: list[dict]) -> None:
        import logging

        self.replies = list(replies)
        self.steps = steps
        self.server_time_offset_ms = 0
        self._logger = logging.getLogger("parity.transport")

    async def post_secure(self, endpoint: str, outer_payload: dict) -> Any:
        from pybyd._transport import SecureTransport

        from parity.groups.envelope import utc_host

        self.steps.append({"post": endpoint, "outer": copy.deepcopy(dict(outer_payload))})
        if not self.replies:
            raise ScriptExhausted(f"no scripted reply left for {endpoint}")
        entry = self.replies.pop(0)
        if entry.get("date") is not None:
            with utc_host():
                SecureTransport._sync_clock_from_header(self, entry["date"])
        if "transport_error" in entry:
            raise exceptions.BydTransportError(
                f"scripted transport error from {endpoint}",
                status_code=entry["transport_error"].get("status_code"),
                endpoint=endpoint,
            )
        return copy.deepcopy(entry["reply"])


def describe(error: BaseException) -> dict:
    """The trace form of an exception: its class and the attributes pyBYD's exceptions carry."""
    api = isinstance(error, exceptions.BydApiError)
    transport = isinstance(error, exceptions.BydTransportError)
    return {
        "class": type(error).__name__,
        "code": error.code if api else None,
        "endpoint": error.endpoint if api or transport else None,
        "status_code": error.status_code if transport else None,
    }


def _code_arguments(args: dict) -> dict:
    """JSON code arguments as pyBYD takes them: frozensets and exception classes."""
    args = dict(args)
    if args.get("not_supported_codes") is not None:
        args["not_supported_codes"] = frozenset(args["not_supported_codes"])
    if args.get("extra_code_map") is not None:
        args["extra_code_map"] = {
            frozenset(codes): getattr(exceptions, name) for codes, name in args["extra_code_map"]
        }
    return args


class Stalled(Exception):
    """The flow waited on something the script cannot give (a real future)."""


def drive(coroutine: Any) -> Any:
    """Run a coroutine that never really waits, without an event loop (whose
    own clock reads would see the patched ``time``). Mirrors the Rust
    ``block_on``."""
    try:
        coroutine.send(None)
    except StopIteration as done:
        return done.value
    coroutine.close()
    raise Stalled("the flow waited on something the script cannot give")


def run_flow(
    function: Callable[..., Any],
    *,
    config: dict,
    session: dict,
    now_ms: int,
    seed: str,
    replies: list[dict],
    args: dict,
    guest_vins: dict[str, bool] | None = None,
    user_type_settled: list[list[str]] | None = None,
    server_time_offset_ms: int = 0,
) -> dict:
    """Run a pyBYD flow against the script and return its trace."""
    clock = _Clock(now_ms, seed)
    steps: list[dict] = []
    transport = ScriptedTransport(replies, steps)
    transport.server_time_offset_ms = server_time_offset_ms

    async def sleep(delay: float, result: Any = None) -> Any:
        clock.slept += delay
        steps.append({"sleep": float(delay)})
        return result

    trace: dict[str, Any] = {"steps": steps}
    with ExitStack() as stack:
        stack.enter_context(mock.patch.object(time, "time", clock.time))
        stack.enter_context(mock.patch.object(time, "monotonic", clock.monotonic))
        stack.enter_context(mock.patch.object(secrets, "token_hex", clock.token_hex))
        patch_datetime_now(stack, clock)
        built_config = BydConfig(**config)
        built_session = Session(**session)
        built_session.guest_vins.update(guest_vins or {})
        built_session.user_type_settled.update((vin, endpoint) for vin, endpoint in user_type_settled or [])
        stack.enter_context(mock.patch.object(asyncio, "sleep", sleep))
        try:
            trace["result"] = drive(
                function(config=built_config, session=built_session, transport=transport, **_code_arguments(args))
            )
        except Stalled:
            raise
        except Exception as error:  # noqa: BLE001 - every failure is part of the trace
            trace["error"] = describe(error)
    trace["guest_vins"] = dict(sorted(built_session.guest_vins.items()))
    trace["user_type_settled"] = [list(pair) for pair in sorted(built_session.user_type_settled)]
    trace["unused_replies"] = len(transport.replies)
    return trace


def flow_oracle(group: Group, call: str, function: Callable[..., Any]) -> None:
    """Register ``function`` (a pyBYD flow) as the oracle of ``call``, compared by trace."""

    def oracle(**scenario_args: Any) -> dict:
        return run_flow(function, **scenario_args)

    oracle.__name__ = call.replace(".", "_")
    group.oracle(call)(oracle)


# ---------------------------------------------------------------------------
# The client runner: pyBYD's real BydClient against a script
# ---------------------------------------------------------------------------
#
# A parity call ``client.<method>`` runs one ``BydClient`` method against
# the same kind of script as the flows, plus an MQTT timeline. Its arguments
# and trace are described in ``crates/openbyd-client/src/client/parity.rs``;
# the rules of the timeline in ``crates/openbyd-client/src/scripted.rs``.
#
# pyBYD's client runs without an event loop here, as the flows do: the
# patched ``asyncio.wait_for`` delivers the MQTT timeline while it "waits",
# ``asyncio.ensure_future`` queues background tasks that run to their end
# right after the delivery that started them, ``random.uniform`` draws from
# the seed, and the MQTT runtime is a stand-in that records starts, stops
# and key changes. ``Session`` and ``_PendingCommand`` read the patched
# monotonic clock when created: their ``default_factory`` holds the real
# ``time.monotonic``, which would make every session look expired.


def _num(value: float) -> Any:
    """A float as the trace writes it: non-finite values as text."""
    if math.isfinite(value):
        return value
    if math.isnan(value):
        return "nan"
    return "inf" if value > 0 else "-inf"


class _FakeLoop:
    """The client's ``_loop``: never running, so callbacks run inline, and
    ``run_in_executor`` runs the function at once."""

    _real = asyncio.new_event_loop()

    def create_future(self) -> asyncio.Future:
        return self._real.create_future()

    def is_running(self) -> bool:
        return False

    def run_in_executor(self, _executor: Any, function: Callable[..., Any], *args: Any) -> asyncio.Future:
        future = self._real.create_future()
        try:
            future.set_result(function(*args))
        except Exception as error:  # noqa: BLE001 - the awaiting code sees it
            future.set_exception(error)
        return future


class _CarSpy:
    """Stands in for a ``BydCar``: records what the client hands it."""

    def __init__(self, vin: str, steps: list[dict]) -> None:
        self.vin = vin
        self.steps = steps

    def _record(self, method: str, *models: Any) -> None:
        from parity.models_support import jsonable

        self.steps.append({"car": [method, self.vin, *(None if model is None else jsonable(model) for model in models)]})

    def handle_mqtt_realtime(self, data: Any) -> None:
        self._record("handle_mqtt_realtime", data)

    def handle_mqtt_charging(self, data: Any) -> None:
        self._record("handle_mqtt_charging", data)

    def handle_mqtt_energy(self, data: Any) -> None:
        self._record("handle_mqtt_energy", data)

    def handle_mqtt_status_push(self, data: Any, *, hvac: Any = None, charging: Any = None) -> None:
        self._record("handle_mqtt_status_push", data, hvac, charging)


class _CarSpies(dict):
    """The client's ``_cars`` under the ``car_pushes`` option: a recording
    car for every VIN, so the trace shows each push the client routes to the
    car layer (openbyd records the same through its ``CarRouter``). Not for
    cases that call ``get_car``, which would be given the spy."""

    def __init__(self, steps: list[dict]) -> None:
        super().__init__()
        self.steps = steps

    def get(self, vin: Any, default: Any = None) -> Any:
        return _CarSpy(vin, self.steps)


class _ClientHarness:
    """The MQTT timeline, the start script and the background tasks of one run."""

    def __init__(self, clock: _Clock, steps: list[dict], mqtt: list[dict], starts: list[bool]) -> None:
        self.clock = clock
        self.steps = steps
        self.mqtt = list(enumerate(mqtt))
        self.starts = list(starts)
        self.current: Any = None
        self.spawned: list[Any] = []
        self.uniform_draws = 0

    def runtime_class(self) -> type:
        harness = self

        class FakeRuntime:
            def __init__(
                self,
                *,
                loop: Any,
                decrypt_key_hex: str,
                on_event: Callable[..., None],
                on_decrypt_error: Callable[[], None] | None = None,
                on_connack_refused: Callable[[int], None] | None = None,
                on_connected: Callable[[], None] | None = None,
                keepalive: int = 120,
                logger: Any = None,
            ) -> None:
                self.key = decrypt_key_hex
                self.on_event = on_event
                self.on_decrypt_error = on_decrypt_error
                self.on_connack_refused = on_connack_refused
                self.on_connected = on_connected
                self.keepalive = keepalive
                self._running = False

            @property
            def is_running(self) -> bool:
                return self._running

            def update_decrypt_key(self, key_hex: str) -> None:
                harness.steps.append({"mqtt_key": key_hex})

            def start(self, bootstrap: Any) -> None:
                harness.steps.append(
                    {
                        "mqtt_start": {
                            "bootstrap": bootstrap.model_dump(mode="json"),
                            "key": self.key,
                            "keepalive": self.keepalive,
                        }
                    }
                )
                if not (harness.starts.pop(0) if harness.starts else True):
                    raise exceptions.BydError("scripted MQTT start failure")
                self._running = True
                harness.current = self

            def stop(self) -> None:
                harness.steps.append({"mqtt_stop": None})
                self._running = False
                if harness.current is self:
                    harness.current = None

        return FakeRuntime

    def deliver_one(self, limit: float) -> bool:
        from pybyd._mqtt import MqttEvent

        if not self.mqtt or self.mqtt[0][1]["at"] > limit:
            return False
        index, entry = self.mqtt.pop(0)
        if entry["at"] > self.clock.slept:
            self.clock.slept = entry["at"]
        runtime = self.current if self.current is not None and self.current.is_running else None
        self.steps.append({"mqtt" if runtime is not None else "mqtt_lost": index})
        if runtime is not None:
            if "event" in entry:
                runtime.on_event(MqttEvent.model_validate(entry["event"]))
            elif "decrypt_error" in entry:
                runtime.on_decrypt_error()
            elif "connack_refused" in entry:
                runtime.on_connack_refused(entry["connack_refused"])
            elif "connected" in entry:
                runtime.on_connected()
            while self.spawned:
                drive(self.spawned.pop(0))
        return True

    def drain(self) -> None:
        while self.deliver_one(math.inf):
            pass


def _is_hex32(text: str) -> bool:
    return len(text) == 32 and all(ch in "0123456789abcdefABCDEF" for ch in text)


_PLAIN_READS = {
    "get_latest_config",
    "get_hvac_status",
    "get_charging_status",
    "get_charging_schedule",
    "get_energy_consumption",
    "get_push_state",
    "get_booking_list",
}
_SIMPLE_COMMANDS = {
    "lock",
    "unlock",
    "stop_climate",
    "flash_lights",
    "close_windows",
    "open_windows",
    "find_car",
    "open_trunk",
    "close_trunk",
}


#: Client methods a group module calls itself: ``{method: async (client, args) -> JSON data}``.
#: ``_client_method`` falls back to these, so a group can add a method without a branch here.
EXTRA_CLIENT_METHODS: dict[str, Callable[[Any, dict], Any]] = {}


async def _client_method(client: Any, method: str, args: dict) -> Any:  # noqa: C901, PLR0911, PLR0912
    """Calls ``method`` on pyBYD's client and returns its result as JSON data."""
    from pybyd.models import control
    from pybyd.models.message_center import NotificationCategory

    from parity.models_support import jsonable

    vin = args.get("vin")
    pwd = args.get("command_pwd")
    polls = {key: args[key] for key in ("poll_attempts", "poll_interval", "mqtt_timeout", "signal_retries") if key in args}
    charge = {key: args[key] for key in ("mqtt_timeout", "poll_attempts", "poll_interval") if key in args}
    params = {
        "start_climate": control.ClimateStartParams,
        "schedule_climate": control.ClimateScheduleParams,
        "set_seat_climate": control.SeatClimateParams,
        "set_battery_heat": control.BatteryHeatParams,
    }
    if method == "login":
        return await client.login()
    if method == "ensure_session":
        session = await client.ensure_session()
        return [session.user_id, _num(session.created_at)]
    if method == "invalidate_session":
        return client.invalidate_session()
    if method == "async_start":
        client._transport = client._parity_transport
        client._loop = _FakeLoop()
        return None
    if method == "async_close":
        return await client.async_close()
    if method == "commands_enabled":
        return client.commands_enabled
    if method == "get_command_ack_diagnostics":
        return jsonable(client.get_command_ack_diagnostics())
    if method == "_on_mqtt_event":
        return None
    if method == "get_car":
        from pybyd.models.vehicle import Vehicle

        cached = vin in client._cars
        vehicle = None if args.get("vehicle") is None else Vehicle.model_validate(args["vehicle"])
        car = await client.get_car(vin, vehicle=vehicle, projection_ttl=args.get("projection_ttl", 30.0))
        return {
            "cached": cached,
            "vin": car.vin,
            "capabilities": jsonable(car.capabilities),
            "state": {
                name: None if getattr(car.state, name) is None else jsonable(getattr(car.state, name))
                for name in ("vehicle", "realtime", "hvac", "gps", "charging", "energy", "charging_schedule")
            },
        }
    if method == "get_vehicles":
        return jsonable(await client.get_vehicles())
    if method == "get_latest_configs":
        return jsonable(await client.get_latest_configs(list(args.get("vins") or [])))
    if method in _PLAIN_READS:
        return jsonable(await getattr(client, method)(vin))
    if method == "get_vehicle_capabilities":
        force = bool(args.get("force_refresh", False))
        return jsonable(await client.get_vehicle_capabilities(vin, force_refresh=force))
    if method in ("get_vehicle_realtime", "get_gps_info"):
        return jsonable(await getattr(client, method)(vin, **polls))
    if method == "get_charging_homepage":
        return jsonable(list(await client.get_charging_homepage(vin)))
    if method == "get_ac_current_limit":
        return jsonable(await client.get_ac_current_limit(vin, energy_type=args.get("energy_type")))
    if method == "set_push_state":
        enable = bool(args.get("enable", False))
        return jsonable(await client.set_push_state(vin, enable=enable, push_type=args.get("push_type", 701)))
    if method == "get_notifications":
        category = args.get("category")
        chosen = NotificationCategory.VEHICLE if category is None else NotificationCategory(category)
        return jsonable(await client.get_notifications(chosen))
    if method == "get_all_notifications":
        found = await client.get_all_notifications()
        return {str(int(category)): jsonable(messages) for category, messages in found.items()}
    if method == "rename_vehicle":
        return jsonable(await client.rename_vehicle(vin, name=args["name"]))
    if method == "verify_command_access":
        return jsonable(await client.verify_command_access(vin, command_pwd=pwd))
    if method in _SIMPLE_COMMANDS:
        return jsonable(await getattr(client, method)(vin, command_pwd=pwd))
    if method in params:
        built = params[method](**args["params"])
        return jsonable(await getattr(client, method)(vin, params=built, command_pwd=pwd))
    if method == "save_charging_schedule":
        result = await client.save_charging_schedule(
            vin,
            start_charge_time=args["start_charge_time"],
            end_charge_time=args["end_charge_time"],
            charge_way=args["charge_way"],
            enabled=bool(args.get("enabled", True)),
            **charge,
        )
        return jsonable(result)
    if method == "toggle_smart_charging":
        return jsonable(await client.toggle_smart_charging(vin, enable=bool(args.get("enable", False))))
    if method in ("start_charging", "stop_charging"):
        return jsonable(await getattr(client, method)(vin, **charge))
    if method == "get_ota_version":
        return jsonable(await client.get_ota_version(vin))
    if method == "set_ota_auto_upgrade":
        time_arg = args.get("auto_upgrade_time")
        return jsonable(
            await client.set_ota_auto_upgrade(
                vin,
                enabled=bool(args.get("enabled", False)),
                auto_upgrade_time="2:00" if time_arg is None else time_arg,
                command_pwd=pwd,
            )
        )
    if method == "get_empower_detail":
        return jsonable(await client.get_empower_detail(vin, empower_id=args.get("empower_id")))
    if method in EXTRA_CLIENT_METHODS:
        return await EXTRA_CLIENT_METHODS[method](client, args)
    raise KeyError(f"no client method {method!r}")


def run_client(  # noqa: C901, PLR0913, PLR0915
    method: str,
    *,
    config: dict,
    now_ms: int,
    seed: str,
    replies: list[dict],
    args: dict | None = None,
    mqtt: list[dict] | None = None,
    mqtt_starts: list[bool] | None = None,
    callbacks: bool = False,
    options: dict | None = None,
    state: dict | None = None,
    setup: list[dict] | None = None,
) -> dict:
    """Run pyBYD's ``BydClient.<method>`` against the script and return its trace."""
    import pybyd.client as client_module
    from pybyd.models.latest_config import VehicleCapabilities, VehicleLatestConfig
    from pybyd.models.vehicle import Vehicle

    from parity.models_support import jsonable

    clock = _Clock(now_ms, seed)
    steps: list[dict] = []
    transport = ScriptedTransport(replies, steps)
    harness = _ClientHarness(clock, steps, mqtt or [], mqtt_starts or [])
    options = options or {}

    async def sleep(delay: float, result: Any = None) -> Any:
        clock.slept += delay
        steps.append({"sleep": float(delay)})
        limit = clock.slept
        while harness.deliver_one(limit):
            pass
        return result

    async def wait_for(future: Any, timeout: float) -> Any:
        steps.append({"mqtt_wait": float(timeout)})
        deadline = clock.slept + timeout
        while True:
            if future.done():
                return future.result()
            if not harness.deliver_one(deadline):
                break
        if deadline > clock.slept:
            clock.slept = deadline
        steps.append({"mqtt_timeout": float(timeout)})
        raise TimeoutError

    def ensure_future(coroutine: Any, **_: Any) -> None:
        harness.spawned.append(coroutine)

    def uniform(low: float, high: float) -> float:
        draw = harness.uniform_draws
        harness.uniform_draws += 1
        unit = int(hashlib.md5(f"{seed}:uniform:{draw}".encode()).hexdigest()[:13], 16) / 16**13
        return low + (high - low) * unit

    original_session = client_module.Session
    original_pending = client_module._PendingCommand
    original_refreshed = original_session.with_refreshed_tokens

    def make_session(**kwargs: Any) -> Any:
        kwargs.setdefault("created_at", time.monotonic())
        return original_session(**kwargs)

    def with_refreshed_tokens(self: Any, **tokens: Any) -> Any:
        # Its Session() takes the real time.monotonic bound at import; pyBYD
        # means "now", so the refreshed session is made at the patched now.
        refreshed = original_refreshed(self, **tokens)
        return refreshed.model_copy(update={"created_at": time.monotonic()}, deep=True)

    def make_pending(*, command: str) -> Any:
        return original_pending(command=command, created_at=time.monotonic())

    def record(name: str) -> Callable[..., None]:
        def callback(*values: Any) -> None:
            if name == "on_mqtt_event":
                steps.append({name: [values[0], values[1], jsonable(copy.deepcopy(values[2]))]})
            elif name == "on_vehicle_info":
                steps.append({name: [values[0], jsonable(values[1])]})
            elif name == "on_mqtt_connect":
                steps.append({name: None})
            else:
                steps.append({name: jsonable(values[0])})

        return callback

    trace: dict[str, Any] = {"steps": steps}
    with ExitStack() as stack:
        stack.enter_context(mock.patch.object(time, "time", clock.time))
        stack.enter_context(mock.patch.object(time, "monotonic", clock.monotonic))
        stack.enter_context(mock.patch.object(secrets, "token_hex", clock.token_hex))
        patch_datetime_now(stack, clock)
        stack.enter_context(mock.patch.object(asyncio, "sleep", sleep))
        stack.enter_context(mock.patch.object(asyncio, "wait_for", wait_for))
        stack.enter_context(mock.patch.object(asyncio, "ensure_future", ensure_future))
        stack.enter_context(mock.patch.object(client_module.random, "uniform", uniform))
        stack.enter_context(mock.patch.object(client_module, "BydMqttRuntime", harness.runtime_class()))
        stack.enter_context(mock.patch.object(client_module, "Session", make_session))
        stack.enter_context(mock.patch.object(client_module, "_PendingCommand", make_pending))
        stack.enter_context(mock.patch.object(original_session, "with_refreshed_tokens", with_refreshed_tokens))
        # pyBYD logs failed background re-logins as warnings; they are part of the trace already.
        stack.enter_context(mock.patch.object(client_module._logger, "disabled", True))
        if "command_retries" in options:
            # What applications patch today: poll_remote_control with a fixed retry count.
            original_poll = client_module._control_api.poll_remote_control

            async def poll_remote_control(*positional: Any, **keywords: Any) -> Any:
                keywords["command_retries"] = options["command_retries"]
                return await original_poll(*positional, **keywords)

            stack.enter_context(
                mock.patch.object(client_module._control_api, "poll_remote_control", poll_remote_control)
            )
        if options.get("gate_bypass"):
            # What applications patch today: the gate, letting some refusals through.
            original_gate = client_module.evaluate_command_gate
            bypass = set(options["gate_bypass"])

            def evaluate_command_gate(*positional: Any, **keywords: Any) -> Any:
                verdict = original_gate(*positional, **keywords)
                if not verdict.supported and verdict.reason in bypass:
                    return verdict.model_copy(update={"supported": True})
                return verdict

            stack.enter_context(mock.patch.object(client_module, "evaluate_command_gate", evaluate_command_gate))

        hooks = {}
        if callbacks:
            names = ("on_vehicle_info", "on_mqtt_event", "on_command_ack", "on_command_lifecycle", "on_mqtt_connect")
            hooks = {name: record(name) for name in names}
        client = client_module.BydClient(
            BydConfig(**config),
            command_ack_ttl_seconds=options.get("command_ack_ttl_seconds", 300.0),
            **hooks,
        )
        client._transport = transport
        client._parity_transport = transport
        client._loop = _FakeLoop()
        if options.get("car_pushes"):
            client._cars = _CarSpies(steps)
        if options.get("pin_passthrough"):
            # What applications patch today: a digest goes out as given.
            original_resolve = client._resolve_command_pwd

            def resolve(command_pwd: str | None) -> str:
                if command_pwd is not None and _is_hex32(command_pwd.strip()):
                    return command_pwd.strip()
                return original_resolve(command_pwd)

            client._resolve_command_pwd = resolve

        start = state or {}
        session = None
        if start.get("session") is not None:
            session = make_session(**start["session"])
            client._session = session
            client._guest_vins = session.guest_vins
            session.guest_vins.update(start.get("guest_vins") or {})
            session.user_type_settled.update((vin, endpoint) for vin, endpoint in start.get("user_type_settled") or [])
        else:
            client._guest_vins.update(start.get("guest_vins") or {})
        client._commands_enabled = bool(start.get("commands_enabled", False))
        for raw in start.get("vehicles") or []:
            vehicle = Vehicle.model_validate(raw)
            client._vehicle_metadata_by_vin[vehicle.vin] = vehicle
        for vin, raw in (start.get("capabilities") or {}).items():
            client._vehicle_capabilities[vin] = VehicleCapabilities.from_latest_config(
                vin, VehicleLatestConfig.model_validate(raw)
            )
        if start.get("mqtt"):
            runtime = harness.runtime_class()(
                loop=client._loop,
                decrypt_key_hex=session.content_key() if session is not None else "",
                on_event=client._on_mqtt_event,
                on_decrypt_error=client._schedule_mqtt_reauth,
                on_connack_refused=client._on_mqtt_connack_refused,
                on_connected=client._on_mqtt_connected,
            )
            runtime._running = True
            harness.current = runtime
            client._mqtt_runtime = runtime

        for step in setup or []:
            if "advance" in step:
                clock.slept += step["advance"]
                steps.append({"advance": step["advance"]})
                limit = clock.slept
                while harness.deliver_one(limit):
                    pass
                continue
            try:
                drive(_client_method(client, step["method"], step.get("args") or {}))
            except Stalled:
                raise
            except BaseException as error:  # noqa: BLE001 - recorded in the trace
                steps.append({"setup_error": type(error).__name__})

        try:
            trace["result"] = drive(_client_method(client, method, args or {}))
        except Stalled:
            raise
        except BaseException as error:  # noqa: BLE001 - every failure is part of the trace
            trace["error"] = describe(error)
        harness.drain()

        current = client._session
        trace["state"] = {
            "session": None
            if current is None
            else [
                current.user_id,
                current.sign_token,
                current.encry_token,
                _num(current.created_at),
                _num(current.ttl),
                current.refresh_token,
            ],
            "guest_vins": dict(sorted(client._guest_vins.items())),
            "user_type_settled": [list(pair) for pair in sorted(current.user_type_settled)] if current else [],
            "commands_enabled": client._commands_enabled,
            "vehicles": list(client._vehicle_metadata_by_vin),
            "capabilities": sorted([vin, caps.source] for vin, caps in client._vehicle_capabilities.items()),
            "mqtt_running": client._mqtt_runtime is not None and client._mqtt_runtime.is_running,
            "waiters": len(client._mqtt_waiters),
            "pending": [
                [vin, serial, pending.command, _num(pending.created_at)]
                for (vin, serial), pending in client._pending_commands.items()
            ],
            "counters": [
                client._pending_matched_count,
                client._pending_expired_count,
                client._pending_uncorrelated_count,
            ],
            "data_poll_serials": len(client._data_poll_serials),
            "reauth": [_num(client._mqtt_reauth_at), client._mqtt_decrypt_reauths],
            "recovery": [client._mqtt_recovery_attempts, _num(client._mqtt_recovery_last_at)],
            "busy": [
                None if client._busy_probe_last_at is None else _num(client._busy_probe_last_at),
                None if client._busy_reauth_last_at is None else _num(client._busy_reauth_last_at),
            ],
            "closed": client._transport is None,
        }
    trace["unused_replies"] = len(transport.replies)
    trace["unused_mqtt"] = len(harness.mqtt)
    return trace


# ---------------------------------------------------------------------------
# Client case builders
# ---------------------------------------------------------------------------

#: The token a scripted login returns: the tokens of ``SAMPLE_SESSION``, so
#: replies sealed with ``ok_reply(SAMPLE_SESSION, ...)`` read before and
#: after a login.
LOGIN_TOKEN = {
    "userId": SAMPLE_SESSION["user_id"],
    "signToken": SAMPLE_SESSION["sign_token"],
    "encryToken": SAMPLE_SESSION["encry_token"],
}
#: A session created now (``NOW_MS``), so it has not expired.
LIVE_SESSION = {**SAMPLE_SESSION, "created_at": NOW_MS / 1000}
#: A session that expired long ago.
OLD_SESSION = {**SAMPLE_SESSION, "created_at": 100.0}
#: The broker lookup a login with MQTT makes.
BROKER_REPLY_DATA = {"emqBorker": "emq.example.com:8883"}


def login_reply(token: Any = None, *, password: str | None = None, code: Any = "0", **extra: Any) -> dict:
    """A login reply carrying ``token`` (``LOGIN_TOKEN`` by default) and any
    ``extra`` keys beside it (``newTokens``), sealed with the key of
    ``password`` (the sample config's by default)."""
    from pybyd._crypto.hashing import pwd_login_key

    key = pwd_login_key(SAMPLE_CONFIG["password"] if password is None else password)
    inner = json.dumps({"token": LOGIN_TOKEN if token is None else token, **extra}, ensure_ascii=False)
    return {"reply": {"code": code, "message": "ok", "respondData": aes_encrypt_hex(inner, key)}}


def broker_reply(session: dict = SAMPLE_SESSION) -> dict:
    """The answer to the MQTT broker lookup."""
    return ok_reply(session, BROKER_REPLY_DATA)


def login_replies() -> list[dict]:
    """A successful login and its MQTT broker lookup."""
    return [login_reply(), broker_reply()]


def logged_in(*, mqtt: bool = True, session: dict | None = None, **more: Any) -> dict:
    """The starting state of a logged-in client (MQTT running by default)."""
    return {"session": LIVE_SESSION if session is None else session, "mqtt": mqtt, **more}


def mqtt_event(at: float, event: str, data: Any, *, vin: str = VIN, uuid: Any = None, topic: str = "oversea/res/1234567") -> dict:
    """An MQTT event arriving at ``at``: ``data`` is the ``data`` object
    (put ``respondData`` in it), with ``uuid`` when given."""
    payload_data = dict(data) if isinstance(data, dict) else data
    if uuid is not None and isinstance(payload_data, dict):
        payload_data["uuid"] = uuid
    return {"at": at, "event": {"event": event, "vin": vin, "topic": topic, "payload": {"event": event, "vin": vin, "data": payload_data}}}


def client_case(
    *,
    replies: Iterable[dict] = (),
    mqtt: Iterable[dict] = (),
    mqtt_starts: Iterable[bool] = (),
    state: dict | None = None,
    setup: Iterable[dict] = (),
    options: dict | None = None,
    callbacks: bool = True,
    config: dict | None = None,
    now_ms: int = NOW_MS,
    seed: str = "seed",
    **args: Any,
) -> dict:
    """The arguments of a ``client.<method>`` parity call; keywords not named
    here are the method's own arguments."""
    return {
        "config": SAMPLE_CONFIG if config is None else config,
        "now_ms": now_ms,
        "seed": seed,
        "replies": list(replies),
        "mqtt": list(mqtt),
        "mqtt_starts": list(mqtt_starts),
        "callbacks": callbacks,
        "options": options or {},
        "state": state or {},
        "setup": list(setup),
        "args": args,
    }


# ---------------------------------------------------------------------------
# Client fuzzing
# ---------------------------------------------------------------------------

#: Serials the random triggers and MQTT events share, so they sometimes match.
FUZZ_SERIALS = ["S1", "S1", "S2", "", None, 5, "S3"]
#: Codes that drive the client: session expired, busy, unreachable, no GPS,
#: rate limited, refused command, PIN errors, guest retries.
CLIENT_CODES = [
    "1002", "1011", "3001", "3003", "1002", "1005", "1010", "1008", "6002", "6051", "6024", "1009", "1001", "8003", "5005",
    "5006", "5011", "9999", "1030", "1003", "1008",
]
#: Commands with their parameter models.
COMMAND_PARAMS = {
    "start_climate": [{}, {"temperature": 21.0}, {"temperature": 25.5, "time_span": 2}],
    "schedule_climate": [{"booking_id": 1, "booking_time": 1700000000}, {}],
    "set_seat_climate": [{"chair_type": "1", "main_heat": 3}, {"chair_type": "3", "lr_seat_heat_state": 2}, {}],
    "set_battery_heat": [{"on": True}, {"on": False}],
}
SIMPLE_COMMANDS = sorted(_SIMPLE_COMMANDS)
#: A latest config whose function numbers allow every command.
ALL_FUNCTIONS_CONFIG = {
    "cfFixedList": [
        {"functionNo": no}
        for no in [
            "1001", "1005", "1006", "1007", "1008", "1012", "1015", "1020", "1021", "1026", "10030001",
            "10030002", "10030004", "10030005", "10030006", "10030007", "10030008", "10030009", "10030010",
            "10300001", "10300002", "10300003", "10300004",
        ]
    ]
}
#: The same, with the dedicated battery heat endpoint and without the generic one.
DEDICATED_HEAT_CONFIG = {"cfFixedList": [{"functionNo": no} for no in ["1005", "10100001"]]}


def _fuzz_serial(rng) -> Any:
    return rng.choice(FUZZ_SERIALS)


def _command_data(rng) -> Any:
    """``respondData`` of a command trigger or poll."""
    serial = _fuzz_serial(rng)
    choices = [
        {"requestSerial": serial},
        {"requestSerial": serial, "controlState": rng.choice([0, 1, 2, "1", None])},
        {"requestSerial": serial, "res": rng.choice([1, 2, 3, "2", 7])},
        {"controlState": rng.choice([0, 1, 2])},
        {"res": rng.choice([1, 2, 3])},
        {"result": rng.choice(["ok", None, 1]), "requestSerial": serial},
        {"message": "fail", "controlState": 2},
        {},
    ]
    return rng.choice(choices)


def _read_data(rng) -> Any:
    """``respondData`` of a read: corpus payloads, serials, odd shapes."""
    from parity.flow_cases import corpus

    roll = rng.random()
    if roll < 0.35:
        name = rng.choice(["VehicleRealtimeData", "GpsInfo", "ChargingStatus", "HvacStatus", "Vehicle"])
        payloads = [payload for payload in corpus(name) if isinstance(payload, dict)]
        payload = dict(rng.choice(payloads)) if payloads else {}
        if rng.random() < 0.5:
            payload["requestSerial"] = _fuzz_serial(rng)
        return payload
    if roll < 0.75:
        return _command_data(rng)
    return rng.choice([[], "text", None, 0, {"onlineState": 1}, {"latitude": 52.0, "longitude": 4.0}])


def _random_client_reply(rng, session: dict) -> dict:
    roll = rng.random()
    if roll < 0.12:
        return login_reply(code=rng.choice(["0", "0", "0", "1002"]))
    if roll < 0.18:
        return broker_reply(session)
    if roll < 0.62:
        return ok_reply(session, _read_data(rng))
    if roll < 0.9:
        return error_reply(rng.choice(CLIENT_CODES))
    if roll < 0.95:
        return transport_error(rng.choice([None, 500]))
    return raw_reply(rng.choice([[], "text", {"code": "0"}]))


def _random_event(rng, at: float) -> dict:
    roll = rng.random()
    if roll < 0.1:
        return {"at": at, "decrypt_error": True}
    if roll < 0.2:
        return {"at": at, "connack_refused": rng.choice([1, 2, 3, 4, 7])}
    if roll < 0.27:
        return {"at": at, "connected": True}
    name = rng.choice(["vehicleInfo", "remoteControl", "remoteControl", "smartCharge", "energyConsumption", "other"])
    vin = rng.choice([VIN, VIN, VIN, "OTHERVIN000000001", ""])
    shape = rng.random()
    if shape < 0.6:
        data = {"respondData": _read_data(rng) if name != "remoteControl" else _command_data(rng)}
        if rng.random() < 0.3 and isinstance(data["respondData"], dict):
            data["respondData"]["time"] = rng.choice([1700000000000, "1700000000000", "x", None, 1.5])
        uuid = rng.choice(["S1", "S1", "S2", "", None, 5])
        if uuid is not None:
            data["uuid"] = uuid
    elif shape < 0.75:
        data = {"code": rng.choice(["6051", "1001", 6051]), "uuid": rng.choice(["S1", "S2"])}
    elif shape < 0.85:
        data = rng.choice([None, "text", [], {"respondData": "text"}])
    else:
        payload = {"event": name, "vin": vin, "res": rng.choice([1, 2, 3]), "requestSerial": rng.choice(["S1", "S2"])}
        return {"at": at, "event": {"event": name, "vin": vin or None, "topic": "t", "payload": payload}}
    return {
        "at": at,
        "event": {"event": name, "vin": vin or None, "topic": "t", "payload": {"event": name, "vin": vin, "data": data}},
    }


def _method_args(rng, method: str) -> dict:  # noqa: PLR0911
    vin = rng.choice([VIN, VIN, VIN, "TESTVIN0000000002"])
    pwd = rng.choice([None, "123456", " 1234 ", "", "0123456789abcdef0123456789ABCDEF", "x" * 32])
    polls = {}
    if rng.random() < 0.7:
        polls = {
            "poll_attempts": rng.choice([0, 1, 2, 3]),
            "poll_interval": rng.choice([0.0, 0.5, 1.5]),
            "mqtt_timeout": rng.choice([None, 0.0, 2.0, 10.0]),
        }
    if method in ("get_vehicle_realtime", "get_gps_info"):
        if rng.random() < 0.5:
            polls["signal_retries"] = rng.choice([1, 2, 3])
        return {"vin": vin, **polls}
    if method in ("start_charging", "stop_charging"):
        polls.pop("signal_retries", None)
        return {"vin": vin, **polls}
    if method == "save_charging_schedule":
        return {
            "vin": vin,
            "start_charge_time": rng.choice(["22:00", "00:30"]),
            "end_charge_time": rng.choice(["06:00", "full"]),
            "charge_way": rng.choice(["s", "e", "0,1,2"]),
            "enabled": rng.random() < 0.8,
            **polls,
        }
    if method in COMMAND_PARAMS:
        return {"vin": vin, "params": rng.choice(COMMAND_PARAMS[method]), "command_pwd": pwd}
    if method in _SIMPLE_COMMANDS or method == "verify_command_access":
        return {"vin": vin, "command_pwd": pwd}
    if method == "get_latest_configs":
        return {"vins": rng.choice([[VIN], [VIN, "TESTVIN0000000002"], []])}
    if method == "get_vehicle_capabilities":
        return {"vin": vin, "force_refresh": rng.random() < 0.3}
    if method == "get_ac_current_limit":
        return {"vin": vin, "energy_type": rng.choice([None, None, 0, 2, "1"])}
    if method == "set_push_state":
        return {"vin": vin, "enable": rng.random() < 0.5, "push_type": rng.choice([701, 702])}
    if method == "get_notifications":
        return {"category": rng.choice([None, 1, 2])}
    if method == "rename_vehicle":
        return {"vin": vin, "name": rng.choice(["Seal", "比亚迪", ""])}
    if method == "toggle_smart_charging":
        return {"vin": vin, "enable": rng.random() < 0.5}
    if method == "set_ota_auto_upgrade":
        return {"vin": vin, "enabled": rng.random() < 0.5, "command_pwd": pwd,
                "auto_upgrade_time": rng.choice([None, "2:00", "03:30", "24:00", "x"])}
    if method == "get_empower_detail":
        return {"vin": vin, "empower_id": rng.choice([None, None, 114149, 0])}
    return {"vin": vin}


#: Methods that trigger and then wait for MQTT or poll.
POLLING_METHODS = {
    *_SIMPLE_COMMANDS,
    *COMMAND_PARAMS,
    "start_charging",
    "save_charging_schedule",
    "get_vehicle_realtime",
    "get_gps_info",
}


def _focused_reply(rng, method: str) -> dict:
    """A reply a trigger or a poll of ``method`` could get."""
    roll = rng.random()
    if roll < 0.2:
        return error_reply(rng.choice(["6024", "1009", "1001", "1002", "3003", "1005", "6051", "1008", "5005", "6002",
                                       "1030", "1003"]))
    if roll < 0.25:
        return transport_error(rng.choice([None, 500]))
    serial = rng.choice(["S1", "S1", "S1", "S2", "", 5])
    if method in ("get_vehicle_realtime", "get_gps_info"):
        data = rng.choice(
            [
                {"requestSerial": serial},
                {"requestSerial": serial, "time": 1700000000, "latitude": 52.0, "elecPercent": 50},
                {"requestSerial": serial, "onlineState": 2},
                {},
            ]
        )
    elif method in ("start_charging", "save_charging_schedule"):
        data = rng.choice([{"requestSerial": serial}, {"res": rng.choice([1, 1, 2, 3, 9, True])}, {}])
    else:
        data = rng.choice(
            [
                {"requestSerial": serial},
                {"requestSerial": serial, "controlState": rng.choice([0, 0, 1, 2])},
                {"requestSerial": serial, "res": rng.choice([1, 1, 2, 3])},
                {"controlState": rng.choice([0, 1, 2])},
                {},
            ]
        )
    return ok_reply(SAMPLE_SESSION, data)


def _focused_event(rng, at: float, method: str) -> dict:
    """A push ``method`` might be waiting for, or one that only looks like it."""
    if rng.random() < 0.15:
        return _random_event(rng, at)
    name = {"get_vehicle_realtime": "vehicleInfo", "start_charging": "smartCharge", "save_charging_schedule": "smartCharge"}.get(
        method, "remoteControl"
    )
    if rng.random() < 0.2:
        name = rng.choice(["vehicleInfo", "remoteControl", "smartCharge"])
    respond = rng.choice(
        [
            {"res": rng.choice([1, 2, 2, 3])},
            {"controlState": rng.choice([0, 1, 2])},
            {"time": 1700000000, "latitude": 52.0, "elecPercent": 40},
            {"code": "6051"},
            {},
        ]
    )
    data: dict = {"respondData": respond} if rng.random() < 0.9 else {"code": "6051"}
    uuid = rng.choice(["S1", "S1", "S1", "S2", None])
    return mqtt_event(at, name, data, uuid=uuid, vin=rng.choice([VIN, VIN, VIN, "OTHERVIN000000001"]))


def _focused_client_case(rng, method: str) -> dict:
    """A case for a command or polled read that gets past the checks in
    front (session, PIN, gate) and into the trigger, wait and polls."""
    vehicle = {"vin": VIN, "energyType": rng.choice(["0", "2"]), "modelName": "SEAL"}
    if rng.random() < 0.15:
        vehicle.update(empowerType=-1, empowerId=114149,
                       rangeDetailList=rng.choice([[{"code": "21"}, {"code": "41"}], [{"code": "31"}], []]))
    session = LIVE_SESSION if rng.random() < 0.8 else {**LIVE_SESSION, "refresh_token": "REFRESH-synthetic"}
    state = logged_in(
        session=session,
        mqtt=rng.random() < 0.85,
        vehicles=[vehicle],
        capabilities={VIN: rng.choice([ALL_FUNCTIONS_CONFIG] * 5 + [DEDICATED_HEAT_CONFIG])},
        commands_enabled=True,
        guest_vins={VIN: rng.random() < 0.2},
    )
    config = dict(SAMPLE_CONFIG)
    if rng.random() < 0.3:
        config["mqtt_timeout"] = rng.choice([0.0, 1.0, 3.0])
    replies = [_focused_reply(rng, method) for _ in range(rng.choice([1, 2, 3, 4, 6, 8, 12]))]
    if rng.random() < 0.1:
        replies = [date_reply(reply, random_date(rng)) if rng.random() < 0.3 else reply for reply in replies]
    if rng.random() < 0.15:
        replies.insert(rng.randint(0, len(replies)), login_reply())
        replies.insert(rng.randint(0, len(replies)), broker_reply())
    times = sorted(rng.choice([0.0, 0.5, 1.0, 2.0, 4.0, 9.0, 11.0, 15.0, 40.0]) for _ in range(rng.choice([0, 1, 1, 2, 3])))
    options: dict = {}
    if rng.random() < 0.5:
        options["command_retries"] = rng.choice([1, 1, 2, 3])
    if rng.random() < 0.2:
        options["command_ack_ttl_seconds"] = rng.choice([1.0, 5.0])
    args = _method_args(rng, method)
    if "command_pwd" in args and rng.random() < 0.8:
        args["command_pwd"] = rng.choice(["123456", "0123456789abcdef0123456789ABCDEF"])
    if "vin" in args and rng.random() < 0.9:
        args["vin"] = VIN
    if "poll_interval" in args and rng.random() < 0.5:
        args["poll_interval"] = rng.choice([0.5, 1.5])
    return client_case(
        config=config,
        state=state,
        replies=replies,
        mqtt=[_focused_event(rng, at, method) for at in times],
        mqtt_starts=[rng.random() < 0.9],
        options=options,
        callbacks=rng.random() < 0.9,
        now_ms=rng.choice([NOW_MS, rng.randint(0, 4_000_000_000_000)]),
        seed=random_text(rng, 6),
        **args,
    )


def random_client_case(rng, method: str) -> dict:
    """Random ``client.<method>`` arguments: config, starting state, setup
    calls, replies and an MQTT timeline, leaning on what drives the client
    (expired sessions, serials that match, pushes that come or do not).
    Commands and polled reads usually get past the checks in front of them."""
    if method in POLLING_METHODS and rng.random() < 0.7:
        return _focused_client_case(rng, method)
    config = dict(SAMPLE_CONFIG)
    if rng.random() < 0.25:
        config["mqtt_timeout"] = rng.choice([0.0, 2.0, 30.0])
    if rng.random() < 0.15:
        config["mqtt_enabled"] = False
    if rng.random() < 0.15:
        config["session_ttl"] = rng.choice([0, 30, 3600])
    if rng.random() < 0.15:
        config["control_pin"] = rng.choice(["123456", " 9999 ", ""])
    state: dict = {}
    roll = rng.random()
    if roll < 0.65:
        state = logged_in(mqtt=rng.random() < 0.8, session=rng.choice([LIVE_SESSION, LIVE_SESSION, OLD_SESSION]))
    elif roll < 0.75:
        state = {"guest_vins": {VIN: rng.random() < 0.5}}
    if state and rng.random() < 0.6:
        energy = rng.choice(["0", "1", "2", "-1", "x"])
        vehicle = {"vin": VIN, "energyType": energy, "modelName": "SEAL"}
        if rng.random() < 0.25:
            vehicle.update(empowerType=-1, empowerId=rng.choice([114149, None]),
                           rangeDetailList=rng.choice([[], [{"code": "21"}], [{"code": "31"}, {"code": "41"}],
                                                       [{"code": "2", "childList": [{"code": "21"}, {"code": "1005"}]}]]))
        if rng.random() < 0.3:
            vehicle["vehicleFunLearnInfo"] = {"openWindowLearnInfo": rng.choice([0, 1, -1])}
        if rng.random() < 0.2:
            vehicle.update({"empowerType": -1, "empowerId": 7})
        state["vehicles"] = [vehicle]
    if state and rng.random() < 0.5:
        state["capabilities"] = {VIN: rng.choice([ALL_FUNCTIONS_CONFIG, DEDICATED_HEAT_CONFIG, {"cfFixedList": []}])}
    if state and rng.random() < 0.6:
        state["commands_enabled"] = True
    if state and rng.random() < 0.2:
        state["guest_vins"] = {VIN: rng.random() < 0.5}
    setup: list[dict] = []
    if rng.random() < 0.2:
        setup.append({"advance": rng.choice([1.0, 60.0, 50000.0])})
    if rng.random() < 0.1:
        setup.append({"method": rng.choice(["login", "invalidate_session", "get_vehicles"])})
    replies = [_random_client_reply(rng, SAMPLE_SESSION) for _ in range(rng.choice([0, 1, 2, 3, 4, 5, 6, 8]))]
    if rng.random() < 0.3:
        replies.insert(rng.randint(0, len(replies)), login_reply())
    times = sorted(rng.choice([0.0, 0.5, 1.0, 2.0, 3.0, 5.0, 9.0, 12.0, 40.0]) for _ in range(rng.choice([0, 0, 1, 2, 3, 4])))
    mqtt = [_random_event(rng, at) for at in times]
    options: dict = {}
    if rng.random() < 0.3:
        options["command_retries"] = rng.choice([1, 2, 3])
    if rng.random() < 0.2:
        options["gate_bypass"] = rng.choice([["function_no_missing", "learn_info_missing"], ["function_no_missing"]])
    if rng.random() < 0.2:
        options["pin_passthrough"] = True
    if rng.random() < 0.2:
        options["command_ack_ttl_seconds"] = rng.choice([0, 1.0, 5.0])
    return client_case(
        config=config,
        state=state,
        setup=setup,
        replies=replies,
        mqtt=mqtt,
        mqtt_starts=[rng.random() < 0.85 for _ in range(rng.choice([0, 1, 2]))],
        options=options,
        callbacks=rng.random() < 0.8,
        now_ms=rng.choice([NOW_MS, NOW_MS, 0, rng.randint(0, 4_000_000_000_000)]),
        seed=random_text(rng, 6),
        **_method_args(rng, method),
    )


def client_oracle(group: Group, method: str) -> None:
    """Register pyBYD's ``BydClient.<method>`` as the oracle of ``client.<method>``."""

    def oracle(**call_args: Any) -> dict:
        return run_client(method, **call_args)

    oracle.__name__ = f"client_{method}"
    group.oracle(f"client.{method}")(oracle)
