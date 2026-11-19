"""Converting between Python objects and Rust JSON values, without JSON text."""

from __future__ import annotations

import collections
import enum
import json
import math
import os
import time
from pathlib import Path

import pytest

from openbyd._native import _Value

CORPUS = Path(__file__).resolve().parents[2] / "parity" / "corpus"


def roundtrip(value):
    return _Value(value).to_python()


@pytest.mark.parametrize(
    "value",
    [
        None, True, False, 0, -1, 2**63 - 1, -(2**63), 2**64 - 1, 1.5, -0.0, 5e-324, 1.7976931348623157e308,
        "", "比亚迪 🚗", "\x00", [], {}, [1, [2, [3]]], {"b": 1, "a": {"z": None, "y": [True]}},
    ],
)
def test_roundtrip_keeps_values_and_types(value):
    back = roundtrip(value)
    assert back == value
    assert json.dumps(back) == json.dumps(value)  # 1 vs 1.0 and True vs 1 told apart


def test_dict_order_is_kept():
    keys = [f"k{i}" for i in range(50)][::-1]
    assert list(roundtrip({key: 1 for key in keys})) == keys


def test_same_as_through_json_text():
    for path in sorted(CORPUS.glob("*.json")):
        payloads = [entry["payload"] for entry in json.loads(path.read_text("utf-8"))]
        assert roundtrip(payloads) == payloads
        assert json.loads(_Value(payloads).to_json()) == payloads
        assert _Value.from_json(json.dumps(payloads)).to_python() == payloads


def test_integers_beyond_64_bits_raise_overflow():
    for number in (2**64, -(2**63) - 1, 10**30):
        with pytest.raises(OverflowError):
            _Value(number)
        with pytest.raises(OverflowError):
            _Value({"nested": [number]})


def test_non_finite_floats_raise():
    for number in (math.nan, math.inf, -math.inf):
        with pytest.raises(ValueError, match="JSON"):
            _Value([number])


def test_subclasses_and_tuples():
    class Color(enum.IntEnum):
        RED = 3

    class Name(str, enum.Enum):
        SEAL = "seal"

    class MyFloat(float):
        pass

    value = collections.OrderedDict([("a", (1, 2)), ("b", Color.RED), ("c", Name.SEAL), ("d", MyFloat(2.5))])
    back = roundtrip(value)
    assert back == {"a": [1, 2], "b": 3, "c": "seal", "d": 2.5}
    assert type(back) is dict and type(back["b"]) is int and type(back["c"]) is str


def test_bool_stays_bool():
    assert roundtrip([True, 1, False, 0]) == [True, 1, False, 0]
    assert [type(item) for item in roundtrip([True, 1])] == [bool, int]


@pytest.mark.parametrize("value", [{1: "x"}, {None: 1}, {("a",): 1}])
def test_dict_keys_must_be_str(value):
    with pytest.raises(TypeError, match="keys must be str"):
        _Value(value)


@pytest.mark.parametrize("value", [object(), b"bytes", {1, 2}, 1j, [Path(".")]])
def test_other_types_raise_type_error(value):
    with pytest.raises(TypeError, match="not a JSON value"):
        _Value(value)


def test_deep_and_cyclic_values_raise():
    deep: list = []
    node = deep
    for _ in range(300):
        node.append([])
        node = node[0]
    with pytest.raises(ValueError, match="deeper"):
        _Value(deep)
    cyclic: list = []
    cyclic.append(cyclic)
    with pytest.raises(ValueError, match="deeper"):
        _Value(cyclic)
    ok: list = []
    node = ok
    for _ in range(200):
        node.append([])
        node = node[0]
    assert roundtrip(ok) == ok


def test_from_json_rejects_bad_text():
    with pytest.raises(ValueError):
        _Value.from_json("{")


def _per_call(function, rounds: int) -> float:
    function()
    started = time.perf_counter()
    for _ in range(rounds):
        function()
    return (time.perf_counter() - started) / rounds


def test_direct_conversion_beats_json_text():
    """Benchmark: direct conversion against json.dumps + serde_json and back. Prints the numbers."""
    payload = json.loads((CORPUS / "VehicleRealtimeData.json").read_text("utf-8"))[-1]["payload"]
    held = _Value(payload)
    text = json.dumps(payload)
    rounds = int(os.environ.get("OPENBYD_BENCH_ROUNDS", "2000"))
    to_rust_direct = _per_call(lambda: _Value(payload), rounds)
    to_rust_text = _per_call(lambda: _Value.from_json(json.dumps(payload)), rounds)
    to_python_direct = _per_call(held.to_python, rounds)
    to_python_text = _per_call(lambda: json.loads(held.to_json()), rounds)
    print(
        f"\nrealtime payload ({len(payload)} keys, {len(text)} bytes of JSON):"
        f"\n  Python -> Rust: direct {to_rust_direct * 1e6:.1f} us, via JSON text {to_rust_text * 1e6:.1f} us"
        f"\n  Rust -> Python: direct {to_python_direct * 1e6:.1f} us, via JSON text {to_python_text * 1e6:.1f} us"
    )
    # Generous: only catches a conversion that has become much slower than the text route.
    assert to_rust_direct < to_rust_text * 1.5
    assert to_python_direct < to_python_text * 1.5
