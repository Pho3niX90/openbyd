"""Python's json module, as pyBYD uses it to build signed text.

Not a pyBYD file: the oracle is the standard library. ``json.dumps`` output
is what BYD's signatures and checksums cover, so it is fuzzed hard.
"""

from __future__ import annotations

import json
import math
import struct

from parity.framework import Group, random_text

group = Group("pyjson")

SPECIAL_FLOATS = [
    0.0, -0.0, 1.0, -1.0, 0.1, 0.5, 1.5, 100.0, 1e15, 1e16, 1.5e16, 123456789012345678.0, 1e-4, 1e-5, 1.2345e-7,
    1e22, 1e23, 1e100, 1e-100, 5e-324, 2.2250738585072014e-308, 1.7976931348623157e308, 0.30000000000000004,
    2.0**53, 2.0**53 + 2, 9007199254740993.0, 1 / 3, 2 / 3, 43200.0, 3.14159, 999999999999999.9, 9999999999999998.0,
]
SPECIAL_INTS = [0, 1, -1, 2**31, -(2**31), 2**53 + 1, 2**63 - 1, -(2**63), 2**63, 2**64 - 1]
SPECIAL_STRINGS = [
    "", '"', "\\", "/", "\b\f\n\r\t", "".join(chr(code) for code in range(0x20)), "\x7f\x80\x9f\xa0\xff",
    "\u2028\u2029", "퟿￿", "\U00010000\U0010ffff", "🚗⚡😀", "Zoë’s 比亚迪", "a\x00b", "</script>",
]


def random_float(rng) -> float:
    roll = rng.random()
    if roll < 0.25:
        return rng.choice(SPECIAL_FLOATS) * rng.choice([1, -1])
    if roll < 0.55:
        while True:
            value = struct.unpack("<d", rng.randbytes(8))[0]
            if math.isfinite(value):
                return value
    if roll < 0.8:
        return round(rng.uniform(-1e6, 1e6), rng.randint(0, 8))
    return rng.random() * 10.0 ** rng.randint(-30, 30)


def random_int(rng) -> int:
    if rng.random() < 0.4:
        return rng.choice(SPECIAL_INTS)
    return rng.randint(-(2**63), 2**64 - 1) if rng.random() < 0.3 else rng.randint(-100000, 100000)


def random_string(rng) -> str:
    if rng.random() < 0.25:
        return rng.choice(SPECIAL_STRINGS)
    if rng.random() < 0.1:
        return "".join(chr(rng.choice([rng.randint(0, 0xD7FF), rng.randint(0xE000, 0x10FFFF)])) for _ in range(8))
    return random_text(rng)


def random_json(rng, depth: int = 0):
    """A JSON value serde_json can hold: no NaN, integers within i64/u64."""
    roll = rng.random()
    if depth > 3 or roll < 0.55:
        return rng.choice(
            [
                lambda: None,
                lambda: rng.random() < 0.5,
                lambda: random_int(rng),
                lambda: random_float(rng),
                lambda: random_string(rng),
                lambda: random_string(rng),
            ]
        )()
    if roll < 0.75:
        return [random_json(rng, depth + 1) for _ in range(rng.randint(0, 4))]
    return {random_string(rng): random_json(rng, depth + 1) for _ in range(rng.randint(0, 5))}


def random_object(rng) -> dict:
    return {random_string(rng): random_json(rng, 1) for _ in range(rng.randint(0, 8))}


def _representable(value) -> bool:
    """Whether serde_json holds ``value`` exactly: finite floats, ints in i64/u64, no lone surrogates."""
    if isinstance(value, float):
        return math.isfinite(value)
    if isinstance(value, str):
        # Lone surrogates, which json.loads accepts and serde_json does not.
        return not any(0xD800 <= ord(ch) <= 0xDFFF for ch in value)
    if isinstance(value, bool) or value is None:
        return True
    if isinstance(value, int):
        return -(2**63) <= value < 2**64
    if isinstance(value, list):
        return all(_representable(item) for item in value)
    return all(_representable(key) and _representable(item) for key, item in value.items())


@group.oracle("pyjson.dumps")
def dumps(value, ensure_ascii: bool, separators: str, sort_keys: bool) -> str:
    chosen = (",", ":") if separators == "compact" else None
    return json.dumps(value, ensure_ascii=ensure_ascii, separators=chosen, sort_keys=sort_keys)


@group.oracle("pyjson.loads")
def loads(text: str):
    return json.loads(text)


@group.oracle("pyjson.py_str")
def py_str(value) -> str:
    return str(value)


DUMP_MODES = [
    {"ensure_ascii": True, "separators": "compact", "sort_keys": False},
    {"ensure_ascii": False, "separators": "compact", "sort_keys": False},
    {"ensure_ascii": True, "separators": "default", "sort_keys": False},
    {"ensure_ascii": False, "separators": "default", "sort_keys": False},
    {"ensure_ascii": True, "separators": "compact", "sort_keys": True},
    {"ensure_ascii": False, "separators": "default", "sort_keys": True},
]

LOAD_SAMPLES = [
    "{}", "[]", " {\"a\" : 1 } ", "1", "-0", "-0.0", "1.0", "1e5", "1E+2", "0.1", "1e-7", '"\\u00e9\\ud83d\\ude97"',
    '{"a":1,"a":2,"b":3}', "[1,]", "{'a':1}", "", " ", "\ufeff{}", "nul", "[1 2]", '"\x01"', "01", "1.", ".5",
    "18446744073709551615", "-9223372036854775808", '{"k": [true, false, null]}', "tru", "[\"a\\x\"]", "\t\n[1]\r ",
]


@group.recorded
def recorded():
    for mode in DUMP_MODES:
        for value in [*SPECIAL_FLOATS, *SPECIAL_INTS, *SPECIAL_STRINGS, None, True, False, [], {}, [1, [2, {}]]]:
            yield "pyjson.dumps", {"value": value, **mode}
        yield "pyjson.dumps", {"value": {"b": 1, "a": [1.0, "x"], "é": None, "": {"z": 0}}, **mode}
    for text in LOAD_SAMPLES:
        yield "pyjson.loads", {"text": text}
    for value in [None, True, False, 0, -1, 1.0, 1e16, 1e-5, "x", [], {}, ["a'", 'b"', "it's \"q\""], {"k": None}]:
        yield "pyjson.py_str", {"value": value}


def _mutate(rng, text: str) -> str:
    if not text:
        return text
    at = rng.randrange(len(text))
    roll = rng.random()
    if roll < 0.3:
        return text[:at]
    if roll < 0.6:
        return text[:at] + rng.choice(list(',:[]{}"\\ e-.0x\ufeff')) + text[at:]
    return text[:at] + text[at + 1 :]


@group.fuzz
def fuzz(rng):
    while True:
        value = random_json(rng)
        yield "pyjson.dumps", {"value": value, **rng.choice(DUMP_MODES)}
        yield "pyjson.dumps", {"value": random_object(rng), **rng.choice(DUMP_MODES)}
        text = json.dumps(random_json(rng), ensure_ascii=rng.random() < 0.5, separators=rng.choice([None, (",", ":")]))
        if rng.random() < 0.4:
            text = _mutate(rng, text)
        try:
            # Leave out what serde_json cannot hold (see pyjson.rs): non-finite
            # numbers and integers beyond 64 bits.
            if not _representable(json.loads(text)) or "NaN" in text or "Infinity" in text:
                continue
        except json.JSONDecodeError:
            pass
        except RecursionError:
            continue
        yield "pyjson.loads", {"text": text}
        scalar = rng.choice([None, True, False, random_int(rng), random_float(rng), random_text(rng)])
        yield "pyjson.py_str", {"value": scalar}
        yield "pyjson.py_str", {"value": [scalar, random_text(rng), {random_text(rng): scalar}]}
