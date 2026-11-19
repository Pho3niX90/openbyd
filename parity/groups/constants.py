"""pybyd/_constants.py"""

from __future__ import annotations

from pybyd import _constants

from parity.framework import Group
from parity.groups.config import FLOAT_STRINGS
from parity.groups.pyjson import random_float

group = Group("constants")

_ERRORS = (ValueError, TypeError)


@group.oracle("constants.values")
def values() -> dict:
    return {
        "USER_AGENT": _constants.USER_AGENT,
        "SESSION_EXPIRED_CODES": sorted(_constants.SESSION_EXPIRED_CODES),
        "DEFAULT_TEMP_OFFSET_C": _constants.DEFAULT_TEMP_OFFSET_C,
        "VALID_CLIMATE_DURATIONS": list(_constants.VALID_CLIMATE_DURATIONS),
    }


@group.oracle("constants.temp_offset_from_range", errors=_ERRORS)
def temp_offset_from_range(range_min, range_max) -> float:
    return _constants.temp_offset_from_range(range_min, range_max)


@group.oracle("constants.celsius_to_scale", errors=_ERRORS)
def celsius_to_scale(temp_c, **offset) -> int:
    return _constants.celsius_to_scale(temp_c, **offset)


@group.oracle("constants.minutes_to_time_span", errors=_ERRORS)
def minutes_to_time_span(minutes) -> int:
    return _constants.minutes_to_time_span(minutes)


NUMBERS = [0, 1, 14, 15, 15.0, 16.5, 17, 17.0, 21, 21.5, 22.5, 31, 31.0, 31.5, 33, 33.0, 14.999, -1, 1e300, -0.0, True]


@group.recorded
def recorded():
    yield "constants.values", {}
    for low in [None, 15, 15.0, 17, 17.5, 16, "15", True]:
        for high in [None, 31, 33, 33.0, 32, 17, 15, 34, 33.5, "33", [1]]:
            yield "constants.temp_offset_from_range", {"range_min": low, "range_max": high}
    for value in [*NUMBERS, "21", " 21.5 ", "21,5", "nan", "inf", "", None, [21]]:
        yield "constants.celsius_to_scale", {"temp_c": value}
        for offset in [14.0, 16, 16.0, 2**62, -1.5, True, "14", None]:
            yield "constants.celsius_to_scale", {"temp_c": value, "offset": offset}
    for minutes in [10, 15, 20, 25, 30, 10.0, 30.0, 10.5, 0, 5, 35, -10, True, False, "10", None, [10], {}, 1e20]:
        yield "constants.minutes_to_time_span", {"minutes": minutes}


def random_number(rng):
    roll = rng.random()
    if roll < 0.2:
        return rng.choice(NUMBERS)
    if roll < 0.3:
        return rng.choice(FLOAT_STRINGS)
    if roll < 0.35:
        return rng.choice([None, [], "x"])
    if roll < 0.5:
        return rng.randint(-(2**62), 2**62) if rng.random() < 0.2 else rng.randint(0, 40)
    if roll < 0.6:
        return random_float(rng)
    return round(rng.uniform(10, 36), rng.randint(0, 3))


@group.fuzz
def fuzz(rng):
    while True:
        low = random_number(rng)
        high = low + rng.choice([0, 1, 15, 16, 16.5, 17, -1]) if isinstance(low, (int, float)) and rng.random() < 0.6 else random_number(rng)
        yield "constants.temp_offset_from_range", {"range_min": low, "range_max": high}
        args = {"temp_c": random_number(rng)}
        if rng.random() < 0.6:
            args["offset"] = rng.choice([14.0, 16.0, 15.5, 16, random_number(rng)])
        yield "constants.celsius_to_scale", args
        yield "constants.minutes_to_time_span", {"minutes": rng.choice([10, 15, 20, 25, 30, random_number(rng)])}
