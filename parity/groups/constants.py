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
        "TOKEN_REFRESH_CODES": sorted(_constants.TOKEN_REFRESH_CODES),
        "SERVER_TIME_DIFF_CODES": sorted(_constants.SERVER_TIME_DIFF_CODES),
        "SERVER_TIME_DRIFT_THRESHOLD_MS": _constants.SERVER_TIME_DRIFT_THRESHOLD_MS,
        "COMMAND_POLL_DELAYS_S": list(_constants.COMMAND_POLL_DELAYS_S),
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


@group.oracle("constants.time_span_to_minutes")
def time_span_to_minutes(code) -> int | None:
    return _constants.time_span_to_minutes(code)


@group.oracle("constants.resolve_poll_delays", errors=(TypeError, IndexError))
def resolve_poll_delays(poll_attempts, poll_interval, **ladder) -> list:
    if "default_ladder" in ladder:
        if not isinstance(ladder["default_ladder"], list):
            raise TypeError("the ladder must be a list")
        ladder["default_ladder"] = tuple(ladder["default_ladder"])
    return _constants.resolve_poll_delays(poll_attempts, poll_interval, **ladder)


#: Attempt counts worth trying: none, the ladder's edges, past its end and odd types.
POLL_ATTEMPTS = [0, 1, 2, 3, 9, 10, 11, 12, 25, -1, -10, True, False, 2.5, 3.0, -1.0, 0.0, "3", None, [3]]
POLL_INTERVALS = [None, 1.5, 2.0, 0, 0.0, 2, -1.0, 1e-9, "x", True]
LADDERS = [None, [], [1.0], [2, 4], [0.0, 0.5], [1.0, 0.5, 0.5, 2.0, 2.0, 2.0, 5.0, 5.0, 5.0, 10.0]]


def poll_delays_args(attempts, interval, ladder) -> dict:
    args = {"poll_attempts": attempts, "poll_interval": interval}
    if ladder is not None:
        args["default_ladder"] = ladder
    return args


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
    # Codes are compared with ==, so 1.0 and True find a duration and "1" does not.
    for code in [1, 2, 3, 4, 5, 0, 6, -1, 1.0, 5.0, 2.5, True, False, "1", "", None, [1], {}, 2**63, 2**64 - 1, 1e300]:
        yield "constants.time_span_to_minutes", {"code": code}
    for attempts in POLL_ATTEMPTS:
        for interval in POLL_INTERVALS:
            yield "constants.resolve_poll_delays", poll_delays_args(attempts, interval, None)
        for ladder in LADDERS[1:]:
            yield "constants.resolve_poll_delays", poll_delays_args(attempts, None, ladder)
    yield "constants.resolve_poll_delays", poll_delays_args(3, None, 5)


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
        yield "constants.time_span_to_minutes", {"code": rng.choice([1, 2, 3, 4, 5, random_number(rng)])}
        attempts = rng.choice([rng.randint(-5, 40), rng.choice(POLL_ATTEMPTS)])
        interval = rng.choice([None, None, round(rng.uniform(0, 10), rng.randint(0, 3)), rng.choice(POLL_INTERVALS)])
        steps = [round(rng.uniform(0, 10), 1) for _ in range(rng.randint(0, 12))]
        ladder = rng.choice([None, None, rng.choice(LADDERS), steps])
        yield "constants.resolve_poll_delays", poll_delays_args(attempts, interval, ladder)
