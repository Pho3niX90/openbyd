"""pydantic's lax-mode conversions, which every pyBYD model field goes through.

These are not pyBYD functions: the oracle is pydantic itself,
``TypeAdapter(<type>).validate_python(value)`` followed by
``dump_python(mode="json")``, with non-finite floats written as
``"NaN"``/``"Infinity"``/``"-Infinity"``. Integer results stay inside ``i64``
(see ``parity/models_support.py``).
"""

from __future__ import annotations

import enum
from functools import cache
from typing import Annotated, Any

from pydantic import Field, TypeAdapter
from pybyd.models._base import BydEnum

from parity.framework import Group, random_text
from parity.models_support import jsonable, json_safe, random_json, random_numeric_text, random_scalar

group = Group("coerce")

TYPES: dict[str, Any] = {
    "coerce.int": int,
    "coerce.float": float,
    "coerce.str": str,
    "coerce.bool": bool,
    "coerce.any": Any,
    "coerce.optional_int": int | None,
    "coerce.optional_float": float | None,
    "coerce.optional_str": str | None,
    "coerce.optional_bool": bool | None,
    "coerce.list_int": list[int],
    "coerce.list_float": list[float],
    "coerce.list_str": list[str],
    "coerce.dict_any": dict[str, Any],
    "coerce.dict_str_int": dict[str, int],
    "coerce.dict_str_bool": dict[str, bool],
    "coerce.int_or_str": int | str,
}
SCALARS = ["coerce.int", "coerce.float", "coerce.str", "coerce.bool", "coerce.int_or_str"]
CONTAINERS = ["coerce.list_int", "coerce.list_float", "coerce.list_str", "coerce.dict_any", "coerce.dict_str_int",
              "coerce.dict_str_bool"]
I64 = (-(2**63), 2**63 - 1)


def _validate(adapter: TypeAdapter, value: Any) -> Any:
    return jsonable(adapter.dump_python(adapter.validate_python(value), mode="json"))


def _register(call: str, annotation: Any) -> None:
    adapter = TypeAdapter(annotation)

    @group.oracle(call)
    def oracle(value: Any) -> Any:
        return _validate(adapter, value)


for _call, _annotation in TYPES.items():
    _register(_call, _annotation)


@group.oracle("coerce.int_bounded")
def int_bounded(value: Any, ge: int | None, le: int | None) -> Any:
    return _validate(TypeAdapter(Annotated[int, Field(ge=ge, le=le)]), value)


@cache
def _int_enum(members: tuple[int, ...], byd: bool) -> type[enum.IntEnum]:
    names = [("UNKNOWN" if value == -1 else f"M{index}", value) for index, value in enumerate(members)]
    return (BydEnum if byd else enum.IntEnum)("Probe", names)


@group.oracle("coerce.int_enum")
def int_enum(value: Any, members: list[int], byd: bool) -> Any:
    return _validate(TypeAdapter(_int_enum(tuple(members), byd)), value)


@group.oracle("coerce.str_enum")
def str_enum(value: Any, members: list[str]) -> Any:
    probe = enum.StrEnum("Probe", [(f"M{index}", member) for index, member in enumerate(members)])
    return _validate(TypeAdapter(probe), value)


# ---------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------

VALUES: list[Any] = [
    # ints and floats at the edges
    0, 1, -1, 2, 7, 12, 1000, 2**31, 2**53 + 1, 2**63 - 1, -(2**63), 2**63, 2**64 - 1,
    0.0, -0.0, 1.0, 1.5, -1.5, 0.5, 12.0, 1e19, 9.2e18, 9.3e18, -9.223372036854776e18, 9.223372036854775e18,
    1e300, -1e300, 5e-324, 1.7976931348623157e308, 2.0**53,
    # booleans and null
    True, False, None,
    # numeric strings
    "1", "0", "-1", " 1 ", "\t12\n", "+5", "-0", "00012", "1_000", "1__0", "_1", "1_", "+_1", "-_1", "1e3", "1E-3",
    "1.0", "1.00", "1.", ".5", ".0", "1.5", "1.0_0", "1_0.0", "1_.5", "1e_5", " 1_0 ", "1_0 ", "0x10", "0b1",
    "9223372036854775807", "-9223372036854775808", "9223372036854775808", "99999999999999999999.0",
    "0000000000000000000000001", "+000000000000000000001", "0__1", "-0__1", "0-1", "0__-1_0", "-0-1", "0+1", "0__",
    "0__0", "00_", "0__1.00", "0__.0", "0_9223372036854775808", "0_-0532", "0-0", "0-00", "0-_1", "0__-0_1", "inf", "-Infinity", "INFINITY", "nan", "NAN", "+nan",
    "nan(1)", "1e400", "-1e400", "1e-400", "1.7976931348623159e308", "٣", "1٣", "１", "²",
    " 1　", "\x1c1", "​1", "\x851\x85",
    # words
    "", " ", "--", "NaN ", "true", "True", "TRUE", "false", "yes", "no", "on", "off", "t", "f", "y", "n", " true",
    "true ", "2", "abc", "比亚迪", "\U0001f697",
    # containers
    [], [1], [1, "2", 3.0], ["a", "b"], [None], [True, 1.5], ["1", "x"], {}, {"a": 1}, {"a": "1", "b": 2.0},
    {"a": "x"}, {"a": None}, {"a": True, "b": "off"}, {"a": [1]}, [[1]], {"": 0},
]
ENUM_MEMBERS: list[tuple[list[int], bool]] = [
    ([-1, 0, 1], True),
    ([-1, 0, 12, 255, 1000], True),
    ([0, 1, 2], True),
    ([0, 1, 2], False),
    ([1, 1, 3], False),
]
STR_MEMBERS = [["LOCKDOOR", "OPENDOOR"], ["", "a", "1"]]
BOUNDS = [(0, 3), (1, None), (None, 5), (None, None)]


def _int_result_fits(call: str, args: dict[str, Any]) -> bool:
    """False when pydantic's answer holds an int ``i64`` cannot carry (openbyd answers "Unsupported")."""
    try:
        answer = group.answer(call, args)
    except Exception:
        return True

    def fits(value: Any) -> bool:
        if isinstance(value, bool):
            return True
        if isinstance(value, int):
            return I64[0] <= value <= I64[1]
        if isinstance(value, list):
            return all(fits(item) for item in value)
        if isinstance(value, dict):
            return all(fits(item) for item in value.values())
        return True

    return fits(answer.get("ok"))


def _cases(values: list[Any]):
    for value in values:
        for call in [*TYPES, "coerce.int_bounded", "coerce.int_enum", "coerce.str_enum"]:
            if call in CONTAINERS and not isinstance(value, (list, dict)) and value not in ("x", None, 1):
                continue
            if call == "coerce.int_bounded":
                for ge, le in BOUNDS:
                    yield call, {"value": value, "ge": ge, "le": le}
            elif call == "coerce.int_enum":
                for members, byd in ENUM_MEMBERS:
                    yield call, {"value": value, "members": members, "byd": byd}
            elif call == "coerce.str_enum":
                for members in STR_MEMBERS:
                    yield call, {"value": value, "members": members}
            else:
                yield call, {"value": value}


@group.recorded
def recorded():
    for call, args in _cases(VALUES):
        if _int_result_fits(call, args):
            yield call, args


@group.fuzz
def fuzz(rng):
    while True:
        value = rng.choice([
            lambda: random_scalar(rng),
            lambda: random_numeric_text(rng),
            lambda: random_json(rng),
            lambda: [random_scalar(rng) for _ in range(rng.randint(0, 3))],
            lambda: {random_text(rng, 5): random_scalar(rng) for _ in range(rng.randint(0, 3))},
            lambda: rng.choice(VALUES),
        ])()
        if not json_safe(value):
            continue
        call = rng.choice([*TYPES, "coerce.int_bounded", "coerce.int_enum", "coerce.str_enum"])
        if call == "coerce.int_bounded":
            ge, le = rng.choice(BOUNDS)
            args = {"value": value, "ge": ge, "le": le}
        elif call == "coerce.int_enum":
            members, byd = rng.choice(ENUM_MEMBERS)
            args = {"value": value, "members": members, "byd": byd}
        elif call == "coerce.str_enum":
            args = {"value": value, "members": rng.choice(STR_MEMBERS)}
        else:
            args = {"value": value}
        if _int_result_fits(call, args):
            yield call, args
