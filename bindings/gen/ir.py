"""What the bindings are generated from: types, structs, enums, methods and errors.

`spec.py` builds one :class:`Surface` out of these, reading the shape of
every pyBYD model from pyBYD itself (:func:`struct_from_model`). The
emitters (`typescript.py`, `java.py`, `csharp.py`) turn it into source code.

Names here are pyBYD's: snake_case fields and methods, PascalCase classes.
Each emitter renames them to its language's convention where that costs
nothing on the wire.
"""

from __future__ import annotations

import enum
import re
from dataclasses import dataclass, field
from typing import Any

# ---------------------------------------------------------------------------
# Types
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Type:
    """A type of the JSON that crosses between the core and a binding.

    ``kind`` is one of:

    - ``str``, ``int``, ``float``, ``bool``;
    - ``datetime`` (an ISO 8601 string) and ``time`` (``HH:MM:SS``);
    - ``any``: any JSON value;
    - ``union``: any one of ``of``, which are plain types (``int`` or ``str``,
      say); a language without unions takes its widest type;
    - ``opt``: ``of[0]`` or null;
    - ``list``: a list of ``of[0]``;
    - ``map``: an object whose values are ``of[0]`` and whose keys are
      strings, or integers written as strings when ``key`` is ``"int"``;
    - ``ref``: the struct or enum called ``name``.
    """

    kind: str
    of: tuple[Type, ...] = ()
    name: str = ""
    key: str = "str"

    def __post_init__(self) -> None:
        assert self.kind in KINDS, self.kind

    @property
    def inner(self) -> Type:
        """The element type of an ``opt``, ``list`` or ``map``."""
        return self.of[0]

    @property
    def optional(self) -> bool:
        return self.kind == "opt"

    def to_json(self) -> Any:
        if self.kind == "ref":
            return {"ref": self.name}
        if self.kind == "map":
            return {"map": self.inner.to_json(), "key": self.key}
        if self.kind in ("opt", "list"):
            return {self.kind: self.inner.to_json()}
        if self.kind == "union":
            return {"union": [inner.to_json() for inner in self.of]}
        return self.kind


KINDS = ("str", "int", "float", "bool", "datetime", "time", "any", "union", "opt", "list", "map", "ref")

STR = Type("str")
INT = Type("int")
FLOAT = Type("float")
BOOL = Type("bool")
DATETIME = Type("datetime")
TIME = Type("time")
ANY = Type("any")


def opt(inner: Type) -> Type:
    return inner if inner.kind == "opt" else Type("opt", (inner,))


def list_of(inner: Type) -> Type:
    return Type("list", (inner,))


def map_of(inner: Type, key: str = "str") -> Type:
    return Type("map", (inner,), key=key)


def ref(name: str) -> Type:
    return Type("ref", name=name)


#: A JSON object with any values: pyBYD's ``dict[str, Any]``.
OBJECT = map_of(ANY)

# ---------------------------------------------------------------------------
# Declarations
# ---------------------------------------------------------------------------


class _Missing:
    def __repr__(self) -> str:
        return "MISSING"


#: The default of a field that has none.
MISSING: Any = _Missing()


@dataclass(frozen=True)
class Field:
    """A field of a struct, or a parameter of a method.

    ``required`` matters for input, and for an output struct a caller also
    gives back (``SessionTokens``, which ``import_session`` reads): there a
    field with ``required=False`` may be missing from a record saved by an
    older version and takes its ``default`` (a string). Openbyd always writes
    every field. ``default`` is what the core uses when an optional input is
    not given, for documentation and for languages that write defaults out.
    """

    name: str
    type: Type
    doc: str = ""
    required: bool = True
    default: Any = MISSING

    def read_back_default(self) -> str | None:
        """The string an output field takes when a record read back lacks it,
        or ``None`` for a field every record has (see the class doc)."""
        if self.required:
            return None
        assert isinstance(self.default, str), f"{self.name}: an optional output field needs a string default"
        return self.default

    def to_json(self) -> dict[str, Any]:
        out: dict[str, Any] = {"name": self.name, "type": self.type.to_json(), "required": self.required}
        if self.default is not MISSING:
            out["default"] = self.default
        return out


@dataclass(frozen=True)
class Struct:
    """A JSON object with known fields.

    ``role`` is ``"output"`` for what the core returns (every field
    present, nothing to construct) or ``"input"`` for what a caller builds
    (optional fields may be left out). ``pybyd`` names the pyBYD module of
    a pyBYD model (``"gps"``), empty for a shape openbyd defines.
    """

    name: str
    doc: str
    fields: tuple[Field, ...]
    role: str = "output"
    pybyd: str = ""

    def to_json(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "role": self.role,
            "pybyd": self.pybyd,
            "fields": [item.to_json() for item in self.fields],
        }


@dataclass(frozen=True)
class Member:
    name: str
    value: int | str
    doc: str = ""


@dataclass(frozen=True)
class Enum:
    """An enumeration whose values cross as integers (``base`` ``"int"``) or strings."""

    name: str
    doc: str
    base: str
    members: tuple[Member, ...]
    pybyd: str = ""

    def to_json(self) -> dict[str, Any]:
        return {"name": self.name, "base": self.base, "members": {m.name: m.value for m in self.members}}


@dataclass(frozen=True)
class Method:
    """A method of the client, of a car or of one of a car's features.

    ``wire`` is the method's name in ``openbyd-host``. ``params`` are
    required and positional; ``optional`` have defaults and are grouped in
    the options type ``options_name`` where a language has no optional
    arguments. Every argument crosses under its own name. ``returns`` is
    ``None`` for nothing.
    """

    name: str
    wire: str
    doc: str
    params: tuple[Field, ...] = ()
    optional: tuple[Field, ...] = ()
    options_name: str = ""
    returns: Type | None = None
    sync: bool = False
    #: A property in languages that have them (sync, no arguments).
    prop: bool = False

    def to_json(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "wire": self.wire,
            "sync": self.sync,
            "params": [item.to_json() for item in self.params],
            "optional": [item.to_json() for item in self.optional],
            "options_name": self.options_name,
            "prop": self.prop,
            "returns": None if self.returns is None else self.returns.to_json(),
        }


@dataclass(frozen=True)
class Feature:
    """One of a car's features (``car.lock``): what it offers and its commands.

    ``props`` are read from the car's feature view: the field ``name`` of
    ``features[<feature>]``.
    """

    name: str
    class_name: str
    doc: str
    props: tuple[Field, ...]
    methods: tuple[Method, ...]

    def to_json(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "class": self.class_name,
            "props": [item.to_json() for item in self.props],
            "methods": [item.to_json() for item in self.methods],
        }


@dataclass(frozen=True)
class ErrorClass:
    """An error class of the bindings.

    ``python`` lists the Python exception classes the core reports that
    become this class. ``fields`` are what it carries besides its message.
    """

    name: str
    parent: str
    doc: str
    python: tuple[str, ...] = ()
    fields: tuple[Field, ...] = ()

    def to_json(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "parent": self.parent,
            "python": list(self.python),
            "fields": [item.to_json() for item in self.fields],
        }


@dataclass(frozen=True)
class Event:
    """Something a client reports: a callback of the client or of a car.

    ``wire`` is the event's name in ``openbyd-host`` and ``fields`` its
    payload, in the order the callback takes them. ``answer`` is what a
    question's callback returns (``None`` for a notification).
    """

    name: str
    wire: str
    doc: str
    fields: tuple[Field, ...] = ()
    answer: Type | None = None

    def to_json(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "wire": self.wire,
            "fields": [item.to_json() for item in self.fields],
            "answer": None if self.answer is None else self.answer.to_json(),
        }


@dataclass
class Surface:
    """Everything the bindings offer."""

    version: str
    pybyd_commit: str
    structs: dict[str, Struct] = field(default_factory=dict)
    enums: dict[str, Enum] = field(default_factory=dict)
    client: list[Method] = field(default_factory=list)
    car: list[Method] = field(default_factory=list)
    features: list[Feature] = field(default_factory=list)
    errors: list[ErrorClass] = field(default_factory=list)
    events: list[Event] = field(default_factory=list)
    #: The fields of the client's options that are plain values, not callbacks.
    client_options: tuple[Field, ...] = ()

    def add(self, item: Struct | Enum) -> None:
        table = self.structs if isinstance(item, Struct) else self.enums
        assert item.name not in self.structs and item.name not in self.enums, f"{item.name} is declared twice"
        table[item.name] = item

    def methods(self) -> list[Method]:
        """Every method, the features' included."""
        return [*self.client, *self.car, *(method for feature in self.features for method in feature.methods)]

    def option_groups(self) -> dict[str, tuple[tuple[Field, ...], list[str]]]:
        """The options types: each name with its fields and the methods that take it."""
        groups: dict[str, tuple[tuple[Field, ...], list[str]]] = {}
        for method in self.methods():
            if not method.optional:
                continue
            assert method.options_name, f"{method.wire} has options without a name for them"
            fields, users = groups.setdefault(method.options_name, (method.optional, []))
            assert fields == method.optional, f"{method.options_name} differs between methods"
            if method.name not in users:
                users.append(method.name)
        return groups

    def check(self) -> None:
        """Every name a type refers to is declared, and names do not clash."""

        def visit(used: Type, where: str) -> None:
            if used.kind == "ref":
                assert used.name in self.structs or used.name in self.enums, f"{where}: {used.name} is not declared"
            for inner in used.of:
                visit(inner, where)

        for struct in self.structs.values():
            for item in struct.fields:
                visit(item.type, f"{struct.name}.{item.name}")
        for method in self.methods():
            for item in (*method.params, *method.optional):
                visit(item.type, f"{method.wire}({item.name})")
            if method.returns is not None:
                visit(method.returns, f"{method.wire} returns")
        for event in self.events:
            for item in event.fields:
                visit(item.type, f"{event.name}.{item.name}")
        for name in self.option_groups():
            assert name not in self.structs and name not in self.enums, f"{name} is declared twice"
        wires = [method.wire for method in self.methods()]
        assert len(wires) == len(set(wires)), "a host method is bound twice"
        known = {error.name for error in self.errors}
        for error in self.errors:
            assert not error.parent or error.parent in known, f"{error.name}: no parent {error.parent}"

    def to_json(self) -> dict[str, Any]:
        """The surface as data: `bindings/surface.json`, which the tests of
        ``openbyd-host`` compare with the methods it dispatches."""
        return {
            "version": self.version,
            "pybyd_commit": self.pybyd_commit,
            "structs": [item.to_json() for item in self.structs.values()],
            "enums": [item.to_json() for item in self.enums.values()],
            "client": [item.to_json() for item in self.client],
            "car": [item.to_json() for item in self.car],
            "features": [item.to_json() for item in self.features],
            "errors": [item.to_json() for item in self.errors],
            "events": [item.to_json() for item in self.events],
            "client_options": [item.to_json() for item in self.client_options],
            "options": {
                name: [item.to_json() for item in fields] for name, (fields, _) in self.option_groups().items()
            },
        }


# ---------------------------------------------------------------------------
# Reading pyBYD
# ---------------------------------------------------------------------------


def clean_doc(doc: str | None) -> str:
    """The first paragraph of a Python docstring as plain prose.

    reStructuredText markup is reduced to backticks, which every emitter
    turns into its language's inline code.
    """
    if not doc:
        return ""
    paragraph = doc.strip().split("\n\n")[0]
    text = " ".join(line.strip() for line in paragraph.splitlines())
    text = re.sub(r":[a-z]+:`~?([^`]+)`", r"`\1`", text)  # :func:`x` -> `x`
    text = re.sub(r"``([^`]+)``", r"`\1`", text)
    return text.strip()


def enum_from_class(cls: type[enum.Enum], module: str = "") -> Enum:
    """An :class:`Enum` for a Python enum class with integer or string values."""
    base = "str" if issubclass(cls, str) else "int"
    members = tuple(Member(member.name, member.value) for member in cls)
    for member in members:
        assert isinstance(member.value, str if base == "str" else int), f"{cls.__name__}.{member.name}"
    return Enum(cls.__name__, clean_doc(cls.__doc__), base, members, module)


class SchemaReader:
    """Turns pydantic's JSON schema of pyBYD's models into :class:`Type` and :class:`Struct`.

    The serialization schema describes exactly what ``model_dump(mode="json")``
    gives, which is what the core returns; the validation schema describes
    what a parameter model accepts. Field names are Python's (``by_alias=False``).
    """

    def __init__(self) -> None:
        #: The names of the models and enums the schemas referred to.
        self.referenced: set[str] = set()
        #: Inline string enums generated from ``{"type": "string", "enum": [...]}``.
        self.enums: dict[str, Enum] = {}

    def inline_string_enum(self, schema: dict[str, Any], where: str) -> Type:
        values = schema["enum"]
        assert isinstance(values, list) and values and all(isinstance(value, str) for value in values), (
            f"{where}: inline string enum must list non-empty strings"
        )
        enum_name = pascal(where)
        members = tuple(Member(snake(value.replace("-", "_")).upper(), value) for value in values)
        names = [member.name for member in members]
        assert all(name for name in names) and len(set(names)) == len(names), (
            f"{where}: inline string enum member names clash: {values}"
        )
        doc = clean_doc(schema.get("description")) or f"Values of `{where}`."
        generated = Enum(enum_name, doc, "str", members, "")
        existing = self.enums.get(enum_name)
        assert existing is None or existing.members == generated.members, f"{enum_name}: conflicting inline enums"
        self.enums[enum_name] = generated
        self.referenced.add(enum_name)
        return ref(enum_name)

    def type_of(self, schema: dict[str, Any], where: str) -> Type:
        if "$ref" in schema:
            name = schema["$ref"].rsplit("/", 1)[-1]
            self.referenced.add(name)
            return ref(name)
        if "anyOf" in schema:
            options = [option for option in schema["anyOf"] if option.get("type") != "null"]
            nullable = len(options) != len(schema["anyOf"])
            members = tuple(self.type_of(option, where) for option in options)
            if len(members) == 1:
                inner = members[0]
            else:
                plain = all(member.kind in ("str", "int", "float", "bool") for member in members)
                assert plain, f"{where}: a union of {[member.kind for member in members]} is not supported"
                inner = Type("union", members)
            return opt(inner) if nullable else inner
        kind = schema.get("type")
        if kind == "string":
            if "enum" in schema:
                return self.inline_string_enum(schema, where)
            return {"date-time": DATETIME, "time": TIME}.get(schema.get("format", ""), STR)
        if kind == "integer":
            assert "enum" not in schema, f"{where}: an inline integer enum is not supported"
            return INT
        if kind == "number":
            return FLOAT
        if kind == "boolean":
            return BOOL
        if kind == "array":
            return list_of(self.type_of(schema.get("items", {}), where))
        if kind == "object":
            extra = schema.get("additionalProperties", True)
            assert "properties" not in schema, f"{where}: an inline object is not supported"
            return OBJECT if extra is True else map_of(self.type_of(extra, where))
        assert kind is None and not (set(schema) - {"title", "default", "description"}), f"{where}: {schema}"
        return ANY

    def struct(self, cls: Any, role: str, module: str) -> Struct:
        """The struct of the pydantic model ``cls``, as output or as input."""
        mode = "serialization" if role == "output" else "validation"
        schema = cls.model_json_schema(mode=mode, by_alias=False)
        if "$ref" in schema:
            # A model that contains itself is described under `$defs`.
            schema = schema["$defs"][schema["$ref"].rsplit("/", 1)[-1]]
        required = set(schema.get("required", ()))
        fields = []
        for name, prop in schema.get("properties", {}).items():
            where = f"{cls.__name__}.{name}"
            kind = self.type_of(prop, where)
            if role == "output":
                fields.append(Field(name, kind, prop.get("description", "")))
            else:
                fields.append(
                    Field(
                        name,
                        kind,
                        prop.get("description", ""),
                        required=name in required,
                        default=prop.get("default", MISSING),
                    )
                )
        return Struct(cls.__name__, clean_doc(cls.__doc__), tuple(fields), role, module)


# ---------------------------------------------------------------------------
# Naming
# ---------------------------------------------------------------------------


def words(name: str) -> list[str]:
    """The words of a snake_case, dotted or PascalCase name, in lower case."""
    spaced = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", name.replace(".", "_"))
    return [word.lower() for word in spaced.split("_") if word]


def camel(name: str) -> str:
    """``poll_attempts`` -> ``pollAttempts``."""
    first, *rest = words(name)
    return first + "".join(word.capitalize() for word in rest)


def pascal(name: str) -> str:
    """``poll_attempts`` -> ``PollAttempts``."""
    return "".join(word.capitalize() for word in words(name))


def snake(name: str) -> str:
    """``pollAttempts`` -> ``poll_attempts``."""
    return "_".join(words(name))
