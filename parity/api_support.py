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
- the n-th ``secrets.token_hex(16)`` (from 0) is ``MD5("<seed>:<n>")``;
- ``asyncio.sleep(seconds)`` returns at once and adds to ``slept``;
- ``transport.post_secure`` records the step and answers with the next
  reply: an outer reply, or a ``BydTransportError``. With no reply left it
  raises ``ScriptExhausted``, which is not a pyBYD error, so it passes
  through every ``except`` in pyBYD and ends the flow.

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
import hashlib
import json
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
CODES = ["0", "1001", "1002", "1005", "1010", "1008", "6002", "6051", "8003", "8002", "9999", "", " 1001", "1001 "]
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
    **more_args: Any,
) -> dict:
    """The arguments of a flow's parity call. The function's own arguments are
    ``args`` and any keywords not named here (pass a pyBYD ``now_ms`` in
    ``args``: the keyword is the clock)."""
    return {
        "config": SAMPLE_CONFIG if config is None else config,
        "session": SAMPLE_SESSION if session is None else session,
        "guest_vins": guest_vins or {},
        "user_type_settled": [list(pair) for pair in user_type_settled],
        "now_ms": now_ms,
        "seed": seed,
        "replies": list(replies),
        "args": {**(args or {}), **more_args},
    }


def random_reply(rng, session: dict, data: Callable[[Any], Any] | None = None) -> dict:
    """A reply of any shape: success with assorted data, error codes, broken
    ``respondData``, non-object replies and transport errors."""
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
    for code in ["1002", "1005", "1010"]:
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
            replies.append(error_reply(rng.choice(["1001", "8003", "1001", "8003", "1005"])))
        else:
            replies.append(random_reply(rng, session, data))
    return {"vin": vin, "guest_vins": guest_vins, "user_type_settled": settled, "replies": replies}


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


class ScriptedTransport:
    """Stands in for ``SecureTransport``: records each request, answers from the script."""

    def __init__(self, replies: list[dict], steps: list[dict]) -> None:
        self.replies = list(replies)
        self.steps = steps

    async def post_secure(self, endpoint: str, outer_payload: dict) -> Any:
        self.steps.append({"post": endpoint, "outer": copy.deepcopy(dict(outer_payload))})
        if not self.replies:
            raise ScriptExhausted(f"no scripted reply left for {endpoint}")
        entry = self.replies.pop(0)
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
) -> dict:
    """Run a pyBYD flow against the script and return its trace."""
    clock = _Clock(now_ms, seed)
    steps: list[dict] = []
    transport = ScriptedTransport(replies, steps)

    async def sleep(delay: float, result: Any = None) -> Any:
        clock.slept += delay
        steps.append({"sleep": float(delay)})
        return result

    trace: dict[str, Any] = {"steps": steps}
    with ExitStack() as stack:
        stack.enter_context(mock.patch.object(time, "time", clock.time))
        stack.enter_context(mock.patch.object(time, "monotonic", clock.monotonic))
        stack.enter_context(mock.patch.object(secrets, "token_hex", clock.token_hex))
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
