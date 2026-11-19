"""The ``raw`` of ``openbyd.models.parse``: built from the caller's own objects when it is the whole reply.

The expected ``raw`` comes from the model's parity call, which reads the input as JSON text with
``serde_json`` and dumps it through ``BydModel::into_dump``: the path the bindings used before,
independent of the copy made from the caller's objects. Comparisons are type-strict.
"""

from __future__ import annotations

import collections
import enum
import gc
import json
import math
import random
from typing import Any

import pytest

import openbyd
from openbyd import _native, models
from parity.framework import load_groups
from parity.models_support import jsonable


def _model_calls() -> dict[str, str]:
    """The parity call of every model ``openbyd.models`` knows, by class name."""
    calls: dict[str, str] = {}
    for group in load_groups().values():
        for call, args in group.recorded_cases():
            name = call.rsplit(".", 1)[-1]
            if "raw" in args and name in models.NAMES:
                calls.setdefault(name, call)
    return calls


CALLS = _model_calls()

# A model whose ``raw`` is the reply itself, so its dump's ``raw`` is built from the caller's
# objects. ``BydBaseModel`` parses a cleaned copy and ``Vehicle`` parses by reference: their
# ``raw`` is converted from the parsed reply, as every model's was before.
KEEPS_REPLY = "VehicleRealtimeData"


def test_every_model_has_a_parity_call():
    assert set(CALLS) == set(models.NAMES)


def same(got: Any, want: Any, path: str = "raw") -> str | None:
    """Where ``got`` and ``want`` differ, types and key order included; ``None`` if nowhere."""
    if type(got) is not type(want):
        return f"{path}: {type(got).__name__} != {type(want).__name__}"
    if isinstance(got, dict):
        if [type(key) for key in got] != [str] * len(got) or list(got) != list(want):
            return f"{path}: keys {list(got)!r} != {list(want)!r}"
        for key in got:
            difference = same(got[key], want[key], f"{path}[{key!r}]")
            if difference:
                return difference
        return None
    if isinstance(got, list):
        if len(got) != len(want):
            return f"{path}: length {len(got)} != {len(want)}"
        for index, (left, right) in enumerate(zip(got, want, strict=True)):
            difference = same(left, right, f"{path}[{index}]")
            if difference:
                return difference
        return None
    if isinstance(got, float):
        if got != want or math.copysign(1, got) != math.copysign(1, want):
            return f"{path}: {got!r} != {want!r}"
        return None
    return None if got == want else f"{path}: {got!r} != {want!r}"


def old_path(name: str, raw: Any) -> dict[str, Any] | None:
    """The dump the parity call gives (JSON text in, ``into_dump`` out), or ``None`` if it fails."""
    answer = json.loads(_native.parity_call(CALLS[name], json.dumps({"raw": raw, "context": None})))
    return answer["ok"]["dump"] if "ok" in answer else None


def parsed(name: str, raw: Any) -> dict[str, Any] | None:
    try:
        return models.parse(name, raw)
    except (openbyd.ValidationError, ValueError, TypeError, AttributeError, OverflowError, OSError):
        return None


def check(name: str, raw: Any) -> None:
    got = parsed(name, raw)
    want = old_path(name, raw)
    assert (got is None) == (want is None), f"{name}: one side failed for {raw!r}"
    if got is not None and "raw" in want:
        difference = same(got["raw"], want["raw"])
        assert difference is None, f"{name}: {difference} for {raw!r}"
        assert jsonable(got) == jsonable(want), f"{name}: the dump differs for {raw!r}"


# ---------------------------------------------------------------------------
# Awkward inputs
# ---------------------------------------------------------------------------


class Text(str):
    pass


class Number(int):
    pass


class Real(float):
    pass


class Mapping(dict):
    pass


class Items(list):
    pass


class Pair(tuple):
    pass


class Level(enum.IntEnum):
    HIGH = 3


class Mode(enum.StrEnum):
    AUTO = "auto"


class Key(str):
    """A key with the text of another but its own identity in a dict."""

    def __hash__(self) -> int:
        return id(self)

    def __eq__(self, other: object) -> bool:
        return self is other


LEAVES = [
    0, 1, -1, 2**63 - 1, 2**63, 2**64 - 1, -(2**63), 0.0, -0.0, 1.5, 1e300, -2.5e-300,
    "", "x", "NaN", "Infinity", "-Infinity", "--", "é漢", True, False, None,
    Text("sub"), Number(7), Real(2.5), Real(-0.0), Level.HIGH, Mode.AUTO,
    (), (1, "x"), Pair((2.0,)), [], Items([1, Items([2])]), {}, Mapping(k=Text("v")),
    collections.OrderedDict(o=1),
]
KEYS = ["a", "b", Text("b"), "c", "vin", "soc", "time", "abs", "energyType", "requestSerial"]


def value(rng: random.Random, depth: int = 0) -> Any:
    roll = rng.random()
    if depth < 3 and roll < 0.12:
        return {rng.choice(KEYS): value(rng, depth + 1) for _ in range(rng.randint(0, 3))}
    if depth < 3 and roll < 0.2:
        return [value(rng, depth + 1) for _ in range(rng.randint(0, 3))]
    if depth < 3 and roll < 0.24:
        return Mapping(x=value(rng, depth + 1))
    if depth < 3 and roll < 0.28:
        return tuple(value(rng, depth + 1) for _ in range(rng.randint(0, 2)))
    return rng.choice(LEAVES)


def generated(seed: int, count: int) -> list[dict[str, Any]]:
    rng = random.Random(seed)
    cases = []
    for _ in range(count):
        kind = rng.random()
        entries = {rng.choice(KEYS): value(rng) for _ in range(rng.randint(0, 8))}
        if kind < 0.1:
            entries["raw"] = value(rng)
        if kind > 0.9:
            entries = Mapping(entries)
        cases.append(entries)
    return cases


@pytest.mark.parametrize("seed", [1, 2])
def test_raw_matches_the_json_round_trip_for_every_model(seed):
    names = sorted(models.NAMES)
    for index, raw in enumerate(generated(seed, 1500)):
        # Every input through the two models whose raw is always the reply, and three others in turn.
        chosen = {"BydBaseModel", "VehicleRealtimeData"}
        chosen.update(names[(index * 3 + offset) % len(names)] for offset in range(3))
        for name in sorted(chosen):
            check(name, raw)


def test_raw_matches_the_held_value_round_trip():
    kept = 0
    for raw in generated(3, 300):
        if "raw" in raw:
            continue  # the model's own raw key: covered by the parity comparison
        expected = _native._Value(raw).to_python()
        assert same(models.parse("BydBaseModel", raw)["raw"], expected) is None
        # A model that keeps the reply itself, so its raw comes from the caller's objects.
        dump = parsed(KEEPS_REPLY, raw)
        if dump is not None:
            kept += 1
            assert same(dump["raw"], expected) is None
    assert kept > 100


def test_raw_is_made_of_the_callers_own_leaves_in_new_containers():
    # Objects made at run time, so none is a shared constant or a cached small value.
    text = "".join(["fresh ", "text"])
    number = int("1000000000000001")
    real = float("1.5")
    reply = {"speed": 1.0, "text": text, "number": number, "real": real, "list": [text, {"in": number}]}
    raw = models.parse(KEEPS_REPLY, reply)["raw"]
    assert same(raw, _native._Value(reply).to_python()) is None
    assert raw["text"] is text and raw["number"] is number and raw["real"] is real
    assert raw["list"][0] is text and raw["list"][1]["in"] is number
    assert raw is not reply and raw["list"] is not reply["list"] and raw["list"][1] is not reply["list"][1]


@pytest.mark.parametrize(
    ("given", "expected"),
    [
        ({"t": True, "f": False, "one": 1, "zero": 0}, {"t": True, "f": False, "one": 1, "zero": 0}),
        ({"level": Level.HIGH, "number": Number(5)}, {"level": 3, "number": 5}),
        ({"mode": Mode.AUTO, "text": Text("t")}, {"mode": "auto", "text": "t"}),
        ({"real": Real(1.25), "zero": Real(-0.0)}, {"real": 1.25, "zero": -0.0}),
        ({"neg": -0.0, "pos": 0.0}, {"neg": -0.0, "pos": 0.0}),
        ({"top": 2**64 - 1, "low": -(2**63), "mid": 2**63}, {"top": 2**64 - 1, "low": -(2**63), "mid": 2**63}),
        ({"items": (1, (2, 3)), "pair": Pair(("x",))}, {"items": [1, [2, 3]], "pair": ["x"]}),
        ({"list": Items([Items([1])])}, {"list": [[1]]}),
        ({"dict": Mapping(a=collections.OrderedDict(b=1))}, {"dict": {"a": {"b": 1}}}),
        ({Text("key"): 1, "other": 2}, {"key": 1, "other": 2}),
    ],
)
def test_each_kind_of_value_round_trips(given, expected):
    for name in ("BydBaseModel", "VehicleRealtimeData", "GpsInfo"):
        check(name, given)
    for name in ("BydBaseModel", KEEPS_REPLY):
        assert same(models.parse(name, given)["raw"], expected) is None


def test_a_dict_subclass_at_the_top_round_trips():
    given = collections.OrderedDict([("b", 1), ("a", Level.HIGH)])
    for name in ("BydBaseModel", KEEPS_REPLY):
        assert same(models.parse(name, given)["raw"], {"b": 1, "a": 3}) is None
    check("VehicleRealtimeData", given)


@pytest.mark.parametrize("first_is_subclass", [False, True])
def test_keys_with_the_same_text_keep_the_first_position_and_the_last_value(first_is_subclass):
    given: dict[Any, Any] = {"before": 0}
    if first_is_subclass:
        given[Key("a")] = 1
        given["middle"] = 2
        given["a"] = 3
    else:
        given["a"] = 1
        given["middle"] = 2
        given[Key("a")] = 3
    given[Key("a")] = 4  # a third key with the same text, another identity
    assert len(given) == 5
    expected = {"before": 0, "a": 4, "middle": 2}
    nested = {"outer": given}
    for name in ("BydBaseModel", KEEPS_REPLY):
        got = models.parse(name, given)["raw"]
        assert same(got, expected) is None
        assert same(got, _native._Value(given).to_python()) is None
        check(name, given)
        assert same(models.parse(name, nested)["raw"], {"outer": expected}) is None


@pytest.mark.parametrize("name", ["BydBaseModel", "VehicleRealtimeData"])
def test_raw_shares_no_container_with_the_input(name):
    flat = {"a": 1, "b": "x", "c": 2.5}
    raw = models.parse(name, flat)["raw"]
    assert raw == flat and raw is not flat
    flat["a"] = 99
    flat["new"] = 1
    assert raw == {"a": 1, "b": "x", "c": 2.5}
    raw["b"] = "changed"
    assert flat["b"] == "x"

    nested = {"list": [1, {"in": [2]}], "dict": {"d": 3}, "tuple": (4,)}
    raw = models.parse(name, nested)["raw"]
    assert raw["list"] is not nested["list"] and raw["list"][1] is not nested["list"][1]
    assert raw["list"][1]["in"] is not nested["list"][1]["in"] and raw["dict"] is not nested["dict"]
    nested["list"].append(5)
    nested["list"][1]["in"].append(6)
    nested["dict"]["d"] = 7
    assert raw == {"list": [1, {"in": [2]}], "dict": {"d": 3}, "tuple": [4]}
    raw["dict"]["d"] = 8
    raw["list"][1]["in"].clear()
    assert nested["dict"]["d"] == 7 and nested["list"][1]["in"] == [2, 6]


def test_a_collection_during_the_copy_cannot_change_the_reply():
    # Python 3.11 runs the collector inside an allocation, and its callbacks are arbitrary code.
    # The copy allocates a list per item; a callback that edits the reply must not run until the
    # reply has been read both for the parse and for raw.
    reply: dict[str, Any] = {"items": [[index] for index in range(2000)], "speed": 2.0}

    def edit(phase: str, info: dict[str, Any]) -> None:
        reply["speed"] = 9.0
        reply["added"] = 1

    thresholds = gc.get_threshold()
    gc.collect()
    gc.callbacks.append(edit)
    gc.set_threshold(1, 1, 1)
    try:
        dump = models.parse(KEEPS_REPLY, reply)
    finally:
        gc.set_threshold(*thresholds)
        gc.callbacks.remove(edit)
    assert dump["raw"]["speed"] == dump["speed"]
    assert ("added" in dump["raw"]) == (dump["speed"] == 9.0)
    assert len(dump["raw"]["items"]) == 2000


def test_the_collector_is_left_as_it_was():
    was_enabled = gc.isenabled()
    try:
        gc.enable()
        models.parse(KEEPS_REPLY, {"speed": 1.0})
        assert gc.isenabled()
        with pytest.raises(TypeError):
            models.parse(KEEPS_REPLY, {"speed": object()})
        assert gc.isenabled()
        with pytest.raises(TypeError):
            models.parse(KEEPS_REPLY, {"nested": [{"bad": {1, 2}}]})
        assert gc.isenabled()
        gc.disable()
        models.parse(KEEPS_REPLY, {"speed": 1.0})
        assert not gc.isenabled()
    finally:
        gc.enable() if was_enabled else gc.disable()


def test_vehicle_fills_its_pictures_from_raw():
    reply = {"vin": "TESTVIN0000000001", "energyType": 1, "cfPic": {"pic_main_url": "m.png", "pic_set_url": "s.png"}}
    dump = models.parse("Vehicle", reply)
    assert dump["pic_main_url"] == "m.png" and dump["pic_set_url"] == "s.png"
    assert same(dump["raw"], old_path("Vehicle", reply)["raw"]) is None
    check("Vehicle", reply)


def test_a_reply_with_its_own_raw_key():
    reply = {"raw": {"own": [1, Level.HIGH]}, "speed": 2}
    dump = models.parse("VehicleRealtimeData", reply)
    assert same(dump["raw"], {"own": [1, 3]}) is None
    reply["raw"]["own"].append(4)
    assert dump["raw"] == {"own": [1, 3]}
    check("VehicleRealtimeData", reply)
    check("BydBaseModel", {"raw": "--", "a": 1})


@pytest.mark.parametrize("raw", [[1], (1,), "text", 1, None])
def test_a_reply_that_is_not_a_dict_fails_as_before(raw):
    with pytest.raises(openbyd.ValidationError):
        models.parse("BydBaseModel", raw)
    assert old_path("BydBaseModel", raw) is None


def test_the_dump_keeps_its_keys_and_their_order():
    reply = {"speed": 3.0, "vin": "TESTVIN0000000001"}
    dump = models.parse("VehicleRealtimeData", reply)
    assert list(dump) == list(old_path("VehicleRealtimeData", reply))
    assert models.properties("VehicleRealtimeData", reply) == models.properties("VehicleRealtimeData", dict(reply))
