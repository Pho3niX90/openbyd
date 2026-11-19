"""Generic parity cases for pyBYD's pydantic reply models.

Registering a model takes one call in its group module::

    from parity.models_support import register_model
    from pybyd.models.gps import GpsInfo

    register_model(group, "models.gps.GpsInfo", GpsInfo, samples=[{"latitude": 52.0, ...}])

That adds:

- an oracle: ``Model.model_validate(raw, context=context)``, answered as
  ``{"dump": model_dump(mode="json"), "properties": {name: value}}`` with
  every ``@property`` of the class (and its pyBYD bases);
- recorded cases (a few hundred, deterministic) and an endless fuzz source,
  both generated from the class itself: each field's validation keys
  (alias, ``AliasChoices``, the snake_case name, ``_KEY_ALIASES`` old keys),
  its type, and the sentinels ``_clean_dict`` and ``_SENTINEL_RULES`` react to.

JSON cannot carry NaN or infinity, and ``serde_json::Value`` cannot carry an
integer outside ``i64``/``u64``. So, everywhere in openbyd's parity tests:

- a non-finite float *result* is written as the string ``"NaN"``,
  ``"Infinity"`` or ``"-Infinity"`` (see :func:`jsonable`);
- inputs are finite JSON; NaN and infinity reach a model only through
  strings such as ``"NAN"`` or ``"-inf"``, which pydantic parses;
- integers stay inside ``[-2**63, 2**64 - 1]``, and a value that becomes a
  model ``int`` stays within ``±2**53`` so properties that do arithmetic on
  it stay inside ``i64``.

Two platform limits keep timestamps in range: on Windows
``datetime.fromtimestamp`` raises ``OSError`` past 3001-01-19, on Linux only
past 9999. Epoch values that would land beyond 3001 are never generated, so
recorded vectors are the same on every platform.
"""

from __future__ import annotations

import copy
import enum
import math
import random
import types
import typing
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

import pydantic_core
from pydantic import AliasChoices, BaseModel, TypeAdapter, ValidationError

from parity.framework import Group, random_text

# ---------------------------------------------------------------------------
# JSON form of answers
# ---------------------------------------------------------------------------

NONFINITE = {"nan": "NaN", "inf": "Infinity", "-inf": "-Infinity"}


def jsonable(value: Any) -> Any:
    """``value`` as JSON-ready data, with non-finite floats as ``"NaN"``/``"Infinity"``/``"-Infinity"``."""
    if value is None or isinstance(value, (bool, str)):
        return value
    if isinstance(value, enum.Enum):
        return jsonable(value.value)
    if isinstance(value, int):
        return int(value)
    if isinstance(value, float):
        return value if math.isfinite(value) else NONFINITE[repr(value)]
    if isinstance(value, dict):
        return {str(key): jsonable(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [jsonable(item) for item in value]
    if isinstance(value, BaseModel):
        return jsonable(value.model_dump(mode="json"))
    return jsonable(pydantic_core.to_jsonable_python(value))


# ---------------------------------------------------------------------------
# Domain limits
# ---------------------------------------------------------------------------

MODEL_INT_LIMIT = 2**53
WINDOWS_MAX_EPOCH_SECONDS = 32_536_850_399
_INT = TypeAdapter(int)


def epoch_seconds(value: Any) -> int | None:
    """The seconds ``parse_byd_timestamp`` would pass to ``fromtimestamp``, or ``None``."""
    if isinstance(value, bool):
        stamp = int(value)
    elif isinstance(value, int):
        stamp = value
    elif isinstance(value, float):
        if not math.isfinite(value):
            return None
        stamp = int(value)
    elif isinstance(value, str):
        text = value.strip()
        if not text.lstrip("-").isdigit():
            return None
        try:
            stamp = int(text)
        except ValueError:
            return None
    else:
        return None
    if stamp <= 0:
        return None
    return stamp // 1000 if stamp >= 1_000_000_000_000 else stamp


def timestamp_safe(value: Any) -> bool:
    """Whether ``parse_byd_timestamp(value)`` gives the same answer on Windows and Linux."""
    seconds = epoch_seconds(value)
    return seconds is None or seconds <= WINDOWS_MAX_EPOCH_SECONDS


def json_safe(value: Any) -> bool:
    """Finite floats and integers ``serde_json`` holds exactly."""
    if isinstance(value, bool) or value is None or isinstance(value, str):
        return True
    if isinstance(value, int):
        return -(2**63) <= value <= 2**64 - 1
    if isinstance(value, float):
        return math.isfinite(value)
    if isinstance(value, dict):
        return all(isinstance(key, str) and json_safe(item) for key, item in value.items())
    if isinstance(value, list):
        return all(json_safe(item) for item in value)
    return False


def model_int_safe(value: Any) -> bool:
    """Whether a value, read as ``int``, stays within ``±2**53`` (or is not an int at all)."""
    try:
        number = _INT.validate_python(value)
    except ValidationError:
        return True
    return abs(number) <= MODEL_INT_LIMIT


# ---------------------------------------------------------------------------
# Value pools
# ---------------------------------------------------------------------------

SENTINELS: list[Any] = ["", "--", "NaN", "nan", " -- ", "  ", "\tnan\n", None, -1, -1.0, -129, -129.0]
WRONG_TYPES: list[Any] = [None, [], {}, [1], {"a": 1}, "abc", True, 0, 1.5]

INT_VALUES: list[Any] = [0, 1, -1, 7, 42, 100, 255, 1000, 2**31 - 1, -(2**31), 2**53, -(2**53)]
INT_AWKWARD: list[Any] = [
    1.0, -0.0, 1.5, 2.0**53, 1e300, 9.3e18, -9.3e18, "1", " 12 ", "+5", "-3", "1_000", "1__0", "1e3",
    "1.0", "1.00", "1.", "1.5", "0x10", "abc", "00012", "١٢", "１", "   7　", "\x1c7",
    True, False, 2**63, 2**64 - 1,
]
FLOAT_VALUES: list[Any] = [0.0, -0.0, 1.5, 52.0, 4.0, -129.0, -1.0, 1e300, 5e-324, 1.7976931348623157e308, 0, 1, -129, 2**53]
FLOAT_AWKWARD: list[Any] = [
    "1.5", " 2.5 ", "1e3", ".5", "1.", "inf", "-Infinity", "NAN", "nan ", "+nan", "1_000.5", " 1_0 ", "+_1",
    "1e400", "-1e400", "1e-400", "0x10", "abc", True, False, 2**63, 2**64 - 1, -(2**63),
]
STR_VALUES: list[Any] = ["TESTVIN0000000001", "kW", "x", "比亚迪", "Zoë", " padded ", "0", "true", "\U0001f697"]
STR_AWKWARD: list[Any] = [1, 0, 1.5, -0.0, True, 2**64 - 1]
BOOL_VALUES: list[Any] = [True, False]
BOOL_AWKWARD: list[Any] = [
    0, 1, 2, -1, 0.0, 1.0, -0.0, 0.5, "true", "False", "YES", "off", "On", "1", "0", " true", "t", "n", "2", "",
    2**64 - 1,
]
TIMESTAMP_VALUES: list[Any] = [
    1700000000, 1700000000123, 1, 999999999999, 1000000000000, 32536850399, 32536850399999, 1.9, 1e12,
    "1700000000", " 1700000000123 ", "-5", "0", "--5", "1²", "１７００００００００",
    "١٧", "2024-01-01T00:00:00Z", "2024-01-01", "2024-01-01 10:00:00.5+05:30", "2024-W01-1T10",
    "20240101T1000Z", "2024-05-07T05:56:45.000+00:00", "2024-01-01T10:00:00-00:30:15.5", "garbage",
    "2024-13-01", "2024-01-01T24:00", True, False, 0, -1, -1.5,
]
UNKNOWN_KEYS = ["unknownKey", "extra", "Extra Key", "x-y", "比亚迪", ""]


def random_numeric_text(rng: random.Random) -> str:
    """Number-like text: signs, underscores, decimal points, exponents and odd whitespace."""
    digits = "".join(rng.choice("0123456789") for _ in range(rng.randint(0, 12)))
    if rng.random() < 0.15:
        digits = rng.choice(["0", "00", "0_", "0__"]) + rng.choice(["", "-", "+", "_"]) + digits
    if digits and rng.random() < 0.3:
        position = rng.randint(0, len(digits))
        digits = digits[:position] + rng.choice(["_", "_", "__"]) + digits[position:]
    sign = rng.choice(["", "", "", "-", "+", "--", "+-"])
    fraction = rng.choice(["", "", "", ".", ".0", ".00", ".5", ".25", ".0_0", "." + digits[:3]])
    exponent = rng.choice(["", "", "", "", "e3", "E-2", "e", "e+1_0", "e400"])
    space = ["", "", " ", "\t", "\n", " ", "　", "\x1c", "​"]
    special = rng.random() < 0.08
    body = rng.choice(["inf", "-Infinity", "nan", "NaN", "+inf", "in_f"]) if special else sign + digits + fraction + exponent
    return rng.choice(space) + body + rng.choice(space)


def random_iso_text(rng: random.Random) -> str:
    """ISO-8601-ish text, often valid, often slightly broken."""
    year = rng.choice(["2024", "2020", "0001", "9999", "0000", "1970", "2026"])
    if rng.random() < 0.2:
        week = rng.choice(["W01", "W52", "W53", "W00", "W1"])
        day = rng.choice(["", "-1", "-7", "-8", "3"])
        date = year + rng.choice(["-", ""]) + week + day
    else:
        month = rng.choice(["01", "02", "12", "13", "00", "1"])
        day = rng.choice(["01", "29", "30", "31", "00", "1"])
        dash = rng.choice(["-", "-", ""])
        date = f"{year}{dash}{month}{dash}{day}"
    if rng.random() < 0.15:
        return date
    hour = rng.choice(["00", "10", "23", "24", "1"])
    clock = [hour]
    for _ in range(rng.randint(0, 2)):
        clock.append(rng.choice(["00", "30", "59", "60"]))
    time_sep = rng.choice([":", ":", ""])
    time = time_sep.join(clock)
    if rng.random() < 0.4:
        time += rng.choice([".", ","]) + "".join(rng.choice("0123456789") for _ in range(rng.randint(0, 9)))
    zone = rng.choice(["", "", "Z", "+00:00", "-00:00", "+05:30", "-0800", "+01", "+23:59:59.999999", "+24:00",
                       "-00:30:15.5", "+00:00:00.5", "+05:75", "Z ", "+", "+5"])
    separator = rng.choice(["T", "T", " ", "t", "x", "1", "é", "比", "\U0001f697"])
    text = date + separator + time + zone
    if rng.random() < 0.1 and text:
        position = rng.randrange(len(text))
        text = text[:position] + text[position + 1 :]
    return text


def random_float(rng: random.Random) -> float:
    while True:
        value = rng.choice([
            lambda: rng.uniform(-200, 200),
            lambda: float(rng.randint(-1000, 1000)),
            lambda: rng.uniform(-1, 1) * 10.0 ** rng.randint(-320, 308),
            lambda: rng.choice([0.0, -0.0, 5e-324, 1.7976931348623157e308, 2.0**53, 2.0**63, -(2.0**63)]),
        ])()
        if math.isfinite(value):
            return value


def random_scalar(rng: random.Random) -> Any:
    """Any JSON scalar, leaning on the awkward ones."""
    return rng.choice([
        lambda: rng.randint(-(2**63), 2**63 - 1),
        lambda: rng.randint(-1000, 1000),
        lambda: rng.randint(2**63, 2**64 - 1),
        lambda: random_float(rng),
        lambda: random_numeric_text(rng),
        lambda: random_text(rng, 20),
        lambda: rng.choice(SENTINELS),
        lambda: rng.choice([True, False, None]),
    ])()


def random_json(rng: random.Random, depth: int = 0) -> Any:
    if depth >= 2 or rng.random() < 0.7:
        return random_scalar(rng)
    if rng.random() < 0.5:
        return [random_json(rng, depth + 1) for _ in range(rng.randint(0, 3))]
    return {random_text(rng, 8): random_json(rng, depth + 1) for _ in range(rng.randint(0, 3))}


# ---------------------------------------------------------------------------
# Field introspection
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Kind:
    """What a field holds: ``name`` is int, float, str, bool, any, timestamp, enum, list, dict, model, union or custom."""

    name: str
    args: tuple[Any, ...] = ()
    nullable: bool = False


def describe(annotation: Any, metadata: list[Any] | tuple[Any, ...] = ()) -> Kind:
    """The :class:`Kind` of an annotation (with its ``Annotated`` metadata)."""
    from pybyd.models._base import parse_byd_timestamp

    validators = [item for item in metadata if hasattr(item, "func")]
    if any(getattr(item, "func", None) is parse_byd_timestamp for item in validators):
        return Kind("timestamp", nullable=True)
    if validators:
        return Kind("custom", (describe(annotation),), nullable=True)
    origin = typing.get_origin(annotation)
    if origin is typing.Annotated:
        inner, *extra = typing.get_args(annotation)
        return describe(inner, extra)
    if origin in (typing.Union, types.UnionType):
        members = [arg for arg in typing.get_args(annotation) if arg is not type(None)]
        nullable = len(members) < len(typing.get_args(annotation))
        if len(members) == 1:
            inner = describe(members[0])
            return Kind(inner.name, inner.args, nullable=nullable or inner.nullable)
        return Kind("union", tuple(describe(member) for member in members), nullable=nullable)
    if annotation is Any or annotation is object:
        return Kind("any", nullable=True)
    if origin in (list, tuple, set, frozenset):
        args = typing.get_args(annotation)
        return Kind("list", (describe(args[0]) if args else Kind("any"),))
    if origin is dict or (origin is not None and getattr(origin, "__name__", "") == "Mapping"):
        args = typing.get_args(annotation)
        return Kind("dict", (describe(args[1]) if args else Kind("any"),))
    if isinstance(annotation, type):
        if issubclass(annotation, enum.Enum):
            return Kind("enum", (annotation,))
        if issubclass(annotation, BaseModel):
            return Kind("model", (annotation,))
        if annotation is datetime:
            return Kind("custom", (Kind("str"),))
        for simple in (bool, int, float, str):
            if issubclass(annotation, simple):
                return Kind(simple.__name__)
    return Kind("any", nullable=True)


@dataclass
class FieldSpec:
    """One model field: how it is found in the input and what it holds."""

    name: str
    keys: list[str]
    """Keys pydantic reads, in priority order: aliases, then the field name."""
    old_keys: list[str]
    """``_KEY_ALIASES`` keys that are renamed to one of :attr:`keys`."""
    kind: Kind
    sentinel_rule: bool = False

    @property
    def all_keys(self) -> list[str]:
        return self.keys + self.old_keys


def field_specs(cls: type[BaseModel]) -> list[FieldSpec]:
    """Every field of ``cls`` with its keys, kind and sentinel rule."""
    key_aliases: dict[str, str] = dict(getattr(cls, "_KEY_ALIASES", {}) or {})
    sentinel_rules = getattr(cls, "_SENTINEL_RULES", {}) or {}
    by_name = bool(cls.model_config.get("populate_by_name"))
    specs = []
    for name, info in cls.model_fields.items():
        alias = info.validation_alias
        if isinstance(alias, str):
            keys = [alias]
        elif isinstance(alias, AliasChoices):
            keys = [choice for choice in alias.choices if isinstance(choice, str)]
        else:
            keys = [info.alias or name]
        if (by_name or not keys) and name not in keys:
            keys.append(name)
        old_keys = [old for old, new in key_aliases.items() if new in keys]
        kind = describe(info.annotation, info.metadata)
        specs.append(FieldSpec(name, keys, old_keys, kind, name in sentinel_rules))
    return specs


def property_names(cls: type[BaseModel]) -> list[str]:
    """The ``@property`` names defined on ``cls`` and its bases below ``BaseModel``."""
    names: list[str] = []
    for klass in reversed(cls.__mro__):
        if klass is BaseModel or not (isinstance(klass, type) and issubclass(klass, BaseModel)):
            continue
        for name, attribute in vars(klass).items():
            if isinstance(attribute, property) and name not in names:
                names.append(name)
    return names


# ---------------------------------------------------------------------------
# Values per kind
# ---------------------------------------------------------------------------


def _enum_values(kind: Kind) -> list[Any]:
    members = [member.value for member in kind.args[0]]
    if all(isinstance(value, int) for value in members):
        return members + [-1, 999, str(members[-1]), f" {members[0]} ", float(members[-1]), 1.5, "x", True, False, 2**63]
    return members + [member.lower() for member in members if isinstance(member, str)] + ["", 1, " x"]


def values_for(kind: Kind, payload_for: Callable[[type[BaseModel]], list[dict[str, Any]]]) -> list[Any]:
    """Deterministic values for a field of this kind: well-formed ones first, then awkward ones."""
    name = kind.name
    if name == "int":
        return INT_VALUES + INT_AWKWARD
    if name == "float":
        return FLOAT_VALUES + FLOAT_AWKWARD
    if name == "str":
        return STR_VALUES + STR_AWKWARD
    if name == "bool":
        return BOOL_VALUES + BOOL_AWKWARD
    if name == "timestamp":
        return TIMESTAMP_VALUES
    if name == "enum":
        return _enum_values(kind)
    if name == "list":
        items = values_for(kind.args[0], payload_for)
        return [[], items[:1], items[:3], [items[0], None], "notalist", {"a": 1}]
    if name == "dict":
        items = values_for(kind.args[0], payload_for)
        return [{}, {"a": items[0]}, {"a": items[0], "b": items[-1]}, {"a": None}, [], "x"]
    if name == "model":
        return payload_for(kind.args[0]) + [5, "x", []]
    if name in ("union", "custom"):
        values = []
        for member in kind.args:
            values += values_for(member, payload_for)
        return values
    return [1, 1.5, "x", True, [1], {"a": 1}]


def random_value(rng: random.Random, kind: Kind, payload_for: Callable[[type[BaseModel]], dict[str, Any]]) -> Any:
    """A random value for a field of this kind, often of another kind on purpose."""
    roll = rng.random()
    if roll < 0.1:
        return rng.choice(SENTINELS)
    if roll < 0.2:
        return random_json(rng)
    if kind.name == "model" and roll < 0.7:
        return payload_for(kind.args[0])
    if kind.name in ("list", "dict") and roll < 0.7:
        items = [random_value(rng, kind.args[0], payload_for) for _ in range(rng.randint(0, 3))]
        if kind.name == "list":
            return items
        return {random_text(rng, 6): item for item in items}
    if kind.name == "timestamp" and roll < 0.5:
        return rng.choice([lambda: random_iso_text(rng), lambda: random_numeric_text(rng),
                           lambda: rng.randint(-10, WINDOWS_MAX_EPOCH_SECONDS * 1000)])()
    if roll < 0.35:
        return random_numeric_text(rng)
    if roll < 0.75:
        return rng.choice(values_for(kind, lambda cls: [{}]))
    return random_scalar(rng)


def in_domain(kind: Kind, value: Any) -> bool:
    """Whether a field of this kind can take ``value`` without leaving the shared domain.

    Timestamps must stay before 3001 and integers within ``±2**53``; see the module docstring.
    """
    if kind.name == "timestamp":
        return timestamp_safe(value)
    if kind.name == "int":
        return model_int_safe(value)
    if kind.name == "list" and isinstance(value, list):
        return all(in_domain(kind.args[0], item) for item in value)
    if kind.name == "dict" and isinstance(value, dict):
        return all(in_domain(kind.args[0], item) for item in value.values())
    if kind.name in ("union", "custom"):
        return all(in_domain(member, value) for member in kind.args)
    return True


# ---------------------------------------------------------------------------
# Payloads
# ---------------------------------------------------------------------------


@dataclass
class ModelCases:
    """Generates payloads for one model class."""

    cls: type[BaseModel]
    samples: list[dict[str, Any]]
    specs: list[FieldSpec] = field(init=False)

    def __post_init__(self) -> None:
        self.specs = field_specs(self.cls)

    def accepts(self, spec: FieldSpec, value: Any) -> bool:
        """Whether ``value`` stays inside the domain both sides can represent."""
        return json_safe(value) and in_domain(spec.kind, value)

    def _nested_samples(self, cls: type[BaseModel]) -> list[dict[str, Any]]:
        return [{}, {spec.keys[0]: values_for(spec.kind, lambda c: [{}])[0] for spec in field_specs(cls)}]

    def _without(self, payload: dict[str, Any], spec: FieldSpec) -> dict[str, Any]:
        return {key: value for key, value in payload.items() if key not in spec.all_keys}

    def recorded(self) -> Iterator[Any]:
        base = self.samples[0]
        yield from copy.deepcopy(self.samples)
        yield {}
        yield from [None, [], "x", 0, True, [base]]
        for spec in self.specs:
            yield self._without(base, spec)
            values = values_for(spec.kind, self._nested_samples) + SENTINELS
            for value in values:
                if self.accepts(spec, value):
                    yield {**self._without(base, spec), spec.keys[0]: value}
            good = [value for value in values if self.accepts(spec, value) and value is not None]
            first, second = good[0], good[1 % len(good)]
            for key in spec.all_keys[1:]:
                yield {**self._without(base, spec), key: first}
            # The alias and the field name (or an old key) both present, in both orders.
            for other in spec.all_keys[1:]:
                yield {**self._without(base, spec), spec.keys[0]: first, other: second}
                yield {**self._without(base, spec), other: second, spec.keys[0]: first}
            # The value as a string, as BYD often sends numbers.
            if isinstance(first, (int, float)) and not isinstance(first, bool):
                yield {**base, spec.keys[0]: str(first)}
        yield {**base, **{key: 1 for key in UNKNOWN_KEYS}}
        yield {"data": base}
        yield {"data": base, **{spec.keys[0]: None for spec in self.specs[:3]}}
        yield {**base, "data": "notadict"}
        for raw in [{"a": 1}, "", "--", None, 5, [], {}]:
            yield {**base, "raw": raw}
        yield {spec.name: base.get(spec.keys[0]) for spec in self.specs if spec.keys[0] in base}
        yield {key: str(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else value
               for key, value in base.items()}

    def fuzz(self, rng: random.Random, depth: int = 0) -> Any:
        roll = rng.random()
        if roll < 0.03 and depth == 0:
            return random_json(rng)
        payload = copy.deepcopy(rng.choice(self.samples)) if rng.random() < 0.7 else {}
        for spec in rng.sample(self.specs, k=rng.randint(0, len(self.specs))):
            if rng.random() < 0.15:
                payload = self._without(payload, spec)
                continue
            key = spec.keys[0] if rng.random() < 0.6 else rng.choice(spec.all_keys)

            def nested(cls: type[BaseModel]) -> dict[str, Any]:
                return ModelCases(cls, self._nested_samples(cls)).fuzz(rng, depth + 1) if depth < 2 else {}

            value = random_value(rng, spec.kind, nested)
            if self.accepts(spec, value):
                payload[key] = value
        if rng.random() < 0.1:
            payload[rng.choice(UNKNOWN_KEYS)] = random_json(rng)
        if rng.random() < 0.05:
            payload["raw"] = rng.choice([{"a": 1}, "", None, 5, random_json(rng)])
        if rng.random() < 0.1:
            payload = {"data": payload} if rng.random() < 0.5 else {"data": payload, **copy.deepcopy(rng.choice(self.samples))}
        return payload if json_safe(payload) else {}


def register_model(
    group: Group,
    call: str,
    cls: type[BaseModel],
    *,
    samples: list[dict[str, Any]],
    contexts: list[Any] | None = None,
) -> ModelCases:
    """Register the oracle, recorded cases and fuzz cases for one pyBYD model.

    ``samples`` are realistic payloads (synthetic VINs and locations);
    ``contexts`` are the ``model_validate(context=...)`` values to try, which
    default to ``[None]``. Returns the generator, for extra hand-written cases.
    """
    cases = ModelCases(cls, samples)
    names = property_names(cls)
    contexts = contexts or [None]

    @group.oracle(call)
    def oracle(raw: Any, context: Any = None) -> dict[str, Any]:
        model = cls.model_validate(raw, context=context)
        return {
            "dump": jsonable(model.model_dump(mode="json")),
            "properties": {name: jsonable(getattr(model, name)) for name in names},
        }

    @group.recorded
    def recorded() -> Iterator[tuple[str, dict[str, Any]]]:
        for index, payload in enumerate(cases.recorded()):
            yield call, {"raw": payload, "context": contexts[index % len(contexts)]}

    @group.fuzz
    def fuzz(rng: random.Random) -> Iterator[tuple[str, dict[str, Any]]]:
        while True:
            yield call, {"raw": cases.fuzz(rng), "context": rng.choice(contexts)}

    return cases


# ---------------------------------------------------------------------------
# Enum calls and field mutation (shared by the model groups)
# ---------------------------------------------------------------------------


def enum_values(cls: type) -> list[Any]:
    """Values worth calling an enum with: every member, near misses, other types."""
    members = [member.value for member in cls]
    extra = [
        -1, 0, 1, 2, 3, 15, 999, -999, 2**63 - 1, -(2**63), 2**64 - 1,
        *[float(value) for value in members], 2.0, 1.5, -0.0, 1e300,
        *[str(value) for value in members], "2", " 1 ", "1.0", "x", "",
        True, False, None, [], {}, [1], {"a": 1},
    ]
    return members + extra


def register_enum(group: Group, call: str, cls: type, methods: tuple[str, ...] = ()) -> None:
    """Register the oracle and cases of one ``BydEnum`` call."""

    @group.oracle(call)
    def oracle(value: Any) -> dict[str, Any]:
        member = cls(value)
        answer = {"name": member.name, "value": int(member.value)}
        for method in methods:
            answer[method] = getattr(member, method)()
        return answer

    @group.recorded
    def recorded() -> Iterator[tuple[str, dict[str, Any]]]:
        for value in enum_values(cls):
            yield call, {"value": value}

    @group.fuzz
    def fuzz(rng: random.Random) -> Iterator[tuple[str, dict[str, Any]]]:
        values = enum_values(cls)
        members = [member.value for member in cls]
        while True:
            value = rng.choice([
                lambda: rng.choice(values),
                lambda: rng.randint(-5, 20),
                lambda: random_scalar(rng),
                lambda: random_json(rng),
                lambda: rng.choice(members) + rng.choice([0.0, 0.5, -0.5]),
                lambda: rng.choice([str, lambda v: f" {v} ", lambda v: f"{v}.0"])(rng.choice(members)),
            ])()
            if json_safe(value):
                yield call, {"value": value}


def mutate_fields(
    rng: random.Random,
    cases: ModelCases,
    pools: dict[str, list[Any]] | None = None,
    nested: Callable[[type], dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """A realistic sample with one to three fields changed.

    Each changed field gets a value from ``pools`` (keyed by field name) when
    there is one, else a sentinel, another key spelling, or a random value of
    the field's kind.
    """
    pools = pools or {}
    payload = copy.deepcopy(rng.choice(cases.samples))
    for spec in rng.sample(cases.specs, k=min(len(cases.specs), rng.randint(1, 3))):
        roll = rng.random()
        if roll < 0.1:
            payload = {key: value for key, value in payload.items() if key not in spec.all_keys}
            continue
        key = rng.choice(spec.all_keys) if roll < 0.3 else spec.keys[0]
        if spec.name in pools and rng.random() < 0.6:
            value = rng.choice(pools[spec.name])
        elif rng.random() < 0.25:
            value = rng.choice(SENTINELS)
        else:
            value = random_value(rng, spec.kind, nested or (lambda cls: {}))
        if cases.accepts(spec, value):
            payload[key] = value
    return payload if json_safe(payload) else {}
