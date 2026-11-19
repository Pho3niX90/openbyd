"""Case builders shared by the ``api_*`` groups of the read flows.

Each of these flows posts one request and turns the reply into a model, so
the cases are the same everywhere: realistic replies from the corpus, replies
the model builders must cope with, every error code, replies of the wrong
shape and random mixtures of those.
"""

from __future__ import annotations

import json
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any

from pybyd.models._base import BydBaseModel

from parity.api_support import (
    CODES,
    ODD_CODES,
    error_reply,
    ok_reply,
    random_data,
    random_reply,
    random_scenario_base,
    random_vehicle_script,
    raw_reply,
    scenario,
    sealed_reply,
    transport_error,
    vehicle_scripts,
)
from parity.framework import Group
from parity.groups.pyjson import random_json
from parity.models_support import ModelCases, jsonable

CORPUS = Path(__file__).parent / "corpus"

#: Codes every endpoint maps (expired session, busy, unreachable) and one it does not.
GENERAL_CODES = ["1002", "1005", "1010", "1008", "6002", "9999"]


def dumped(function: Callable[..., Any]) -> Callable[..., Any]:
    """``function`` (a pyBYD flow) with its result as JSON data: models as
    their ``model_dump(mode="json")``, tuples as lists."""

    async def flow(**kwargs: Any) -> Any:
        return jsonable(await function(**kwargs))

    flow.__name__ = function.__name__
    return flow


def _ints(value: Any):
    if isinstance(value, bool):
        return
    if isinstance(value, int):
        yield value
    elif isinstance(value, dict):
        for key, item in value.items():
            if key != "raw":  # the input echoed back; openbyd keeps it as JSON
                yield from _ints(item)
    elif isinstance(value, list):
        for item in value:
            yield from _ints(item)


def keep_representable(group: Group) -> None:
    """Leave out of ``group`` the cases whose pyBYD answer holds an integer
    beyond ``i64``. A model reads a float such as ``1e300`` into a Python int
    of any size; openbyd's integers are ``i64`` (docs/porting-models.md,
    "Representation limits"). Every other case, errors included, is kept."""

    def keep(source: Callable[..., Iterable[Any]]) -> Callable[..., Iterable[Any]]:
        def wrapped(*arguments: Any) -> Iterable[Any]:
            for call, args in source(*arguments):
                answer = group.answer(call, args)
                if all(-(2**63) < number < 2**63 for number in _ints(answer.get("ok"))):
                    yield call, args

        return wrapped

    group._recorded = [keep(source) for source in group._recorded]
    group._fuzz = [keep(source) for source in group._fuzz]


def corpus(name: str) -> list[Any]:
    """The payloads of ``parity/corpus/<name>.json``."""
    entries = json.loads((CORPUS / f"{name}.json").read_text(encoding="utf-8"))
    return [entry["payload"] for entry in entries]


def model_cases(cls: type[BydBaseModel], name: str) -> ModelCases:
    """Payload generator for a model, seeded with its corpus."""
    return ModelCases(cls, [payload for payload in corpus(name) if isinstance(payload, dict)])


def model_payloads(cases: ModelCases, *, every: int = 6) -> list[Any]:
    """The corpus and a thinned selection of the model's own recorded cases."""
    thinned = [payload for index, payload in enumerate(cases.recorded()) if index % every == 0]
    return [*cases.samples, *thinned]


def data_source(cases: ModelCases, wrap: Callable[[Any], Any] | None = None) -> Callable[[Any], Any]:
    """A ``respondData`` generator: mostly model payloads (optionally wrapped,
    for list replies), sometimes anything at all."""

    def data(rng) -> Any:
        roll = rng.random()
        if roll < 0.75:
            payload = cases.fuzz(rng)
            return wrap(payload) if wrap else payload
        if roll < 0.9:
            return random_data(rng)
        return random_json(rng)

    return data


#: ``respondData`` of the wrong shape: a list for an object, text, numbers, nothing.
WRONG_SHAPES = [[], [{"a": 1}], [1, "x"], "text", "", 0, 7, 1.5, True, False, None, {}]


def shape_replies(session: dict, payloads: Iterable[Any]) -> list[dict]:
    """Successful replies carrying each payload, then the oddities of the
    wire: wrong shapes, broken ``respondData``, non-object replies and a
    transport failure."""
    payloads = list(payloads)
    replies = [ok_reply(session, payload) for payload in payloads]
    replies += [ok_reply(session, shape) for shape in WRONG_SHAPES if shape not in payloads]
    replies += [
        raw_reply({"code": "0"}),
        raw_reply({"code": "0", "respondData": ""}),
        raw_reply({"code": "0", "respondData": None}),
        raw_reply({"code": "0", "respondData": "ZZ"}),
        raw_reply({}),
        raw_reply([]),
        raw_reply("text"),
        raw_reply(None),
        sealed_reply(session, "{not json"),
        sealed_reply(session, "  "),
        ok_reply(session, {"a": 1}, code=0),
        ok_reply(session, {"a": 1}, code="0 "),
        transport_error(502),
        transport_error(None),
    ]
    return replies


def code_replies(codes: Iterable[Any] = ()) -> list[dict]:
    """Error replies for the general codes, the given ones and a few odd ones."""
    wanted = list(dict.fromkeys([*GENERAL_CODES, "1001", *codes]))
    return [error_reply(code) for code in wanted] + [error_reply(code) for code in ODD_CODES[1:5]]


def token_fuzz(rng, call: str, args: Callable[[Any], dict], data: Callable[[Any], Any] | None = None):
    """Endless random cases for a flow that uses ``post_token_json``."""
    while True:
        base = random_scenario_base(rng)
        count = rng.choice([0, 1, 1, 1, 1, 2])
        yield call, {
            **base,
            "guest_vins": {},
            "user_type_settled": [],
            "replies": [random_reply(rng, base["session"], data) for _ in range(count)],
            "args": args(rng),
        }


def token_recorded(call: str, arg_sets: list[dict], replies: list[dict], session: dict) -> Iterable[tuple[str, dict]]:
    """Recorded cases for a flow that uses ``post_token_json``: every reply
    with the first argument set, a short selection with the others, and the
    guest state, a second unused reply and an empty script."""
    main, *others = arg_sets
    for reply in replies:
        yield call, scenario(replies=[reply], **main)
    short = [replies[0], error_reply("1001"), error_reply("1002"), error_reply("9999"), transport_error(500)]
    for args in others:
        for reply in short:
            yield call, scenario(replies=[reply], **args)
    # A guest account sends no userType on these endpoints, and nothing
    # about guest state changes, even when the cloud answers 1001 or 8003.
    vin = main.get("vin", "TESTVIN0000000001")
    for code in ["1001", "8003", "8002"]:
        yield call, scenario(guest_vins={vin: True}, replies=[error_reply(code)], **main)
        yield call, scenario(guest_vins={vin: False}, replies=[error_reply(code)], **main)
    yield call, scenario(guest_vins={vin: True}, replies=[replies[0]], **main)
    yield call, scenario(replies=[replies[0], replies[0]], **main)
    yield call, scenario(replies=[], **main)


def vehicle_recorded(
    call: str, endpoint: str, arg_sets: list[dict], replies: list[dict], ok: dict, session: dict
) -> Iterable[tuple[str, dict]]:
    """Recorded cases for a flow that uses ``post_vehicle_json``: every reply
    for an owner, and every branch of the guest retry with each argument set."""
    main, *others = arg_sets
    for reply in replies:
        yield call, scenario(replies=[reply], **main)
    for index, args in enumerate([main, *others]):
        scripts = vehicle_scripts(endpoint, ok, vin=args.get("vin", "TESTVIN0000000001"))
        for script in scripts if index == 0 else scripts[:12]:
            yield call, scenario(**script, **{key: value for key, value in args.items() if key != "vin"}, vin=args.get("vin", "TESTVIN0000000001"))


def vehicle_fuzz(rng, call: str, endpoint: str, args: Callable[[Any], dict], data: Callable[[Any], Any] | None = None):
    """Endless random cases for a flow that uses ``post_vehicle_json``."""
    while True:
        base = random_scenario_base(rng)
        script = random_vehicle_script(rng, base["session"], endpoint, data)
        yield call, {
            **base,
            "guest_vins": script["guest_vins"],
            "user_type_settled": [list(pair) for pair in script["user_type_settled"]],
            "replies": script["replies"],
            "args": {**args(rng), "vin": script["vin"]},
        }


__all__ = [
    "token_recorded",
    "vehicle_recorded",
    "vehicle_fuzz",
    "CODES",
    "GENERAL_CODES",
    "WRONG_SHAPES",
    "code_replies",
    "corpus",
    "data_source",
    "keep_representable",
    "dumped",
    "model_cases",
    "model_payloads",
    "shape_replies",
    "token_fuzz",
]
