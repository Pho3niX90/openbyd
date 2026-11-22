"""The C# emitter: the typed layer of the NuGet package (`bindings/dotnet/OpenByd/Generated`).

Everything a C# program sees is in C#'s own case: `get_vehicle_realtime` is
`GetVehicleRealtimeAsync` and the field `elec_percent` is the property
`ElecPercent`. The JSON that crosses keeps pyBYD's names. The readers and
writers generated here do the mapping, and they read strictly: an unknown
field, a missing field, a null where the type has none and a value an enum
does not have all fail. That is what lets the package's tests prove every
recorded pyBYD dump fits the published types.

Files:

- ``Enums.g.cs``: every enum.
- ``Models.g.cs``: every model the core returns (records) and pyBYD's
  parameter objects (classes).
- ``Config.g.cs``: pyBYD's config.
- ``Json.g.cs``: the reader and writer of each of the above.
- ``Exceptions.g.cs``: pyBYD's exception hierarchy.
- ``Client.g.cs``, ``Car.g.cs``: the methods of the client, of a car and of
  its features. The hand-written halves of `Client` and `Car` hold
  construction, lifecycle and callbacks.
"""

from __future__ import annotations

import json
import re

from gen.common import Writer, default_text, header, wrap
from gen.ir import MISSING, Enum, ErrorClass, Feature, Field, Method, Struct, Surface, Type, camel, opt, pascal

ROOT = "bindings/dotnet/OpenByd/Generated"

#: The directories that hold this emitter's files and nothing else.
OWNED = (ROOT,)

#: Parameter names that are C# keywords, and what they are called instead.
RENAMED = {"params": "parameters"}

#: Fields a record's `ToString` does not print.
SECRETS = {"sign_token", "encry_token", "refresh_token"}

#: The kinds whose C# type is a value type, so their nullable form is `Nullable<T>`.
VALUE_KINDS = {"int", "float", "bool", "any", "union"}

#: The reader and the writer (in `Wire`) of each kind that has no parts.
LEAVES = {
    "str": ("Wire.Str", "Wire.WriteStr"),
    "datetime": ("Wire.Str", "Wire.WriteStr"),
    "time": ("Wire.Str", "Wire.WriteStr"),
    "int": ("Wire.Int", "Wire.WriteInt"),
    "float": ("Wire.Float", "Wire.WriteFloat"),
    "bool": ("Wire.Bool", "Wire.WriteBool"),
    "any": ("Wire.Any", "Wire.WriteAny"),
    "union": ("Wire.Any", "Wire.WriteAny"),
}


def exception(name: str) -> str:
    """``BydApiError`` -> ``BydApiException``."""
    return name.removesuffix("Error") + "Exception"


class CSharp:
    def __init__(self, surface: Surface) -> None:
        self.surface = surface
        self.names = self.doc_names()

    # -- documentation ---------------------------------------------------------

    def doc_names(self) -> dict[str, str]:
        """What pyBYD's names are called in C#, for the ones documentation mentions in code spans."""
        names = {"kind": "Kind"}
        for method in (*self.surface.client, *self.surface.car):
            names[method.name] = self.method_name(method)
        for error in self.surface.errors:
            if error.name.startswith("Byd"):
                names[error.name] = exception(error.name)
        for entry in self.surface.structs["BydConfig"].fields:
            names[entry.name] = pascal(entry.name)
        return names

    def xml(self, text: str) -> str:
        """Prose as XML documentation: escaped, with `code` spans as ``<c>`` and names in C#'s form."""
        escaped = text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
        return re.sub(r"`([^`]+)`", lambda span: f"<c>{self.names.get(span.group(1), span.group(1))}</c>", escaped)

    def element(self, writer: Writer, tag: str, text: str, attributes: str = "") -> None:
        if not text:
            return
        lines = wrap(self.xml(text), 96)
        inline = f"/// <{tag}{attributes}>{lines[0]}</{tag}>"
        if len(lines) == 1 and len(inline) <= 104:
            writer.line(inline)
            return
        writer.line(f"/// <{tag}{attributes}>")
        for line in lines:
            writer.line(f"/// {line}")
        writer.line(f"/// </{tag}>")

    def doc(self, writer: Writer, summary: str, remarks: str = "", params: tuple[tuple[str, str], ...] = ()) -> None:
        self.element(writer, "summary", summary)
        self.element(writer, "remarks", remarks)
        for name, text in params:
            self.element(writer, "param", text, f' name="{name}"')

    def note(self, item: Field) -> str:
        """What a field's comment adds to its documentation: its format and its default."""
        notes = []
        inner = item.type.inner if item.type.kind == "opt" else item.type
        if inner.kind == "datetime":
            notes.append("An ISO 8601 date and time.")
        if inner.kind == "time":
            notes.append("A time of day, `HH:MM:SS`.")
        if item.default is not MISSING and not item.required and item.default is not None:
            notes.append(f"Default `{self.default(inner, item.default)}`.")
        return " ".join(notes)

    def default(self, kind: Type, value: object) -> str:
        """A default as documentation shows it: an enum's by its member."""
        enum = self.surface.enums.get(kind.name) if kind.kind == "ref" else None
        if enum is not None:
            member = next(member for member in enum.members if member.value == value)
            return f"{enum.name}.{pascal(member.name)}"
        if kind.kind == "float" and isinstance(value, int):
            return default_text(float(value))
        return default_text(value)

    def field_doc(self, item: Field) -> str:
        """A field's documentation; pyBYD's name for it when pyBYD says nothing else."""
        if item.name == "raw" and item.type.kind == "map":
            text = "The reply this was parsed from, as BYD sent it."
        else:
            text = item.doc or f"pyBYD's `{item.name}`."
        return " ".join(part for part in (text, self.note(item)) if part)

    # -- types -----------------------------------------------------------------

    def is_enum(self, kind: Type) -> bool:
        return kind.kind == "ref" and kind.name in self.surface.enums

    def is_value(self, kind: Type) -> bool:
        """Whether the C# type of ``kind`` is a value type."""
        return kind.kind in VALUE_KINDS or self.is_enum(kind)

    def type(self, kind: Type) -> str:
        """The C# type of ``kind``."""
        if kind.kind in ("str", "datetime", "time"):
            return "string"
        if kind.kind == "int":
            return "long"
        if kind.kind == "float":
            return "double"
        if kind.kind == "bool":
            return "bool"
        if kind.kind in ("any", "union"):
            return "JsonElement"
        if kind.kind == "opt":
            return f"{self.type(kind.inner)}?"
        if kind.kind == "list":
            return f"IReadOnlyList<{self.type(kind.inner)}>"
        if kind.kind == "map":
            if kind.inner.kind == "any":
                return "IReadOnlyDictionary<string, JsonElement>"
            key = "int" if kind.key == "int" else "string"
            return f"IReadOnlyDictionary<{key}, {self.type(kind.inner)}>"
        return kind.name

    def read(self, kind: Type, reader: str = "reader", depth: int = 0) -> str:
        """An expression that reads a ``kind`` from ``reader``."""
        if kind.kind == "opt":
            function = "OptValue" if self.is_value(kind.inner) else "Opt"
            return f"Wire.{function}<{self.type(kind.inner)}>(ref {reader}, {self.reader(kind.inner, depth)})"
        if kind.kind == "list":
            return f"Wire.List<{self.type(kind.inner)}>(ref {reader}, {self.reader(kind.inner, depth)})"
        if kind.kind == "map" and kind.inner.kind != "any":
            function = "IntMap" if kind.key == "int" else "Map"
            return f"Wire.{function}<{self.type(kind.inner)}>(ref {reader}, {self.reader(kind.inner, depth)})"
        return f"{self.reader(kind, depth)}(ref {reader})"

    def reader(self, kind: Type, depth: int = 0) -> str:
        """A ``Reader<T>`` of ``kind``: a method, or a lambda for a type with parts."""
        if kind.kind in LEAVES:
            return LEAVES[kind.kind][0]
        if kind.kind == "ref":
            return f"{kind.name}Json.ReadValue"
        if kind.kind == "map" and kind.inner.kind == "any":
            return "Wire.Object"
        name = f"r{depth}"
        return f"static (ref Utf8JsonReader {name}) => {self.read(kind, name, depth + 1)}"

    def write(self, kind: Type, writer: str, value: str, depth: int = 0) -> str:
        """An expression that writes ``value``, a ``kind``, to ``writer``."""
        if kind.kind == "opt":
            function = "WriteOptValue" if self.is_value(kind.inner) else "WriteOpt"
            return f"Wire.{function}<{self.type(kind.inner)}>({writer}, {value}, {self.writer(kind.inner, depth)})"
        if kind.kind == "list":
            return f"Wire.WriteList<{self.type(kind.inner)}>({writer}, {value}, {self.writer(kind.inner, depth)})"
        if kind.kind == "map" and kind.inner.kind != "any":
            function = "WriteIntMap" if kind.key == "int" else "WriteMap"
            return f"Wire.{function}<{self.type(kind.inner)}>({writer}, {value}, {self.writer(kind.inner, depth)})"
        return f"{self.writer(kind, depth)}({writer}, {value})"

    def writer(self, kind: Type, depth: int = 0) -> str:
        """A ``Writer<T>`` of ``kind``: a method, or a lambda for a type with parts."""
        if kind.kind in LEAVES:
            return LEAVES[kind.kind][1]
        if kind.kind == "ref":
            return f"{kind.name}Json.WriteValue"
        if kind.kind == "map" and kind.inner.kind == "any":
            return "Wire.WriteObject"
        target, value = f"w{depth}", f"v{depth}"
        return f"static ({target}, {value}) => {self.write(kind, target, value, depth + 1)}"

    def argument(self, function: str, first: str, entry: Field, value: str) -> str:
        """A call that adds ``value`` to an object being written, unless it is null.

        ``function`` is ``.Add`` for a call's arguments or ``Wire.Field`` for
        a field of a parameter object, which takes the writer ``first``.
        """
        kind = entry.type.inner if entry.type.kind == "opt" else entry.type
        if kind.kind in ("str", "datetime", "time", "int", "float", "bool"):
            return f'{function}({first}"{entry.name}", {value})'
        suffix = "Value" if self.is_value(kind) else ""
        return f'{function}{suffix}<{self.type(kind)}>({first}"{entry.name}", {value}, {self.writer(kind)})'

    def file(self, namespace: str, usings: tuple[str, ...]) -> Writer:
        """A writer holding what opens every generated file."""
        writer = Writer()
        writer.lines(header(self.surface))
        writer.line("#nullable enable")
        writer.blank()
        for using in usings:
            writer.line(f"using {using};")
        writer.blank()
        writer.line(f"namespace {namespace};")
        writer.blank()
        return writer

    # -- Enums.g.cs --------------------------------------------------------------

    def enum(self, writer: Writer, item: Enum) -> None:
        self.doc(writer, item.doc, f"pyBYD's `{item.name}`." if item.pybyd else "")
        writer.line(f"[JsonConverter(typeof({item.name}Json))]")
        writer.line(f"public enum {item.name}")
        writer.line("{")
        writer.indent()
        for member in item.members:
            self.doc(writer, member.doc or f"`{member.name}`: `{member.value}` in JSON.")
            if item.base == "int":
                writer.line(f"{pascal(member.name)} = {member.value},")
            else:
                writer.line(f"{pascal(member.name)},")
            writer.blank()
        writer.dedent()
        writer.line("}")
        writer.blank()

    def enums(self) -> str:
        writer = self.file("OpenByd.Models", ("System.Text.Json.Serialization",))
        for item in self.surface.enums.values():
            names = [pascal(member.name) for member in item.members]
            assert len(set(names)) == len(names), f"{item.name}: members clash in PascalCase"
            self.enum(writer, item)
        return writer.text()

    # -- Models.g.cs, Config.g.cs ------------------------------------------------

    def output(self, writer: Writer, item: Struct) -> None:
        origin = f"pyBYD's `{item.name}`, as `model_dump(mode=\"json\")` gives it." if item.pybyd else ""
        secrets = [entry for entry in item.fields if entry.name in SECRETS]
        if secrets:
            origin = " ".join(part for part in (origin, "`ToString` leaves the secrets out.") if part)
        self.doc(writer, item.doc, origin)
        writer.line(f"[JsonConverter(typeof({item.name}Json))]")
        writer.line(f"public sealed record {item.name}{' : IBydModel' if item.pybyd else ''}")
        writer.line("{")
        writer.indent()
        for entry in item.fields:
            self.doc(writer, self.field_doc(entry))
            writer.line(f'[JsonPropertyName("{entry.name}")]')
            fallback = entry.read_back_default()
            if fallback is None:
                writer.line(f"public required {self.type(entry.type)} {pascal(entry.name)} {{ get; init; }}")
            else:
                writer.line(f"public {self.type(entry.type)} {pascal(entry.name)} {{ get; init; }} = {json.dumps(fallback)};")
            writer.blank()
        if secrets:
            writer.line("private bool PrintMembers(StringBuilder builder)")
            writer.line("{")
            writer.indent()
            for index, entry in enumerate(item.fields):
                label = f"{', ' if index else ''}{pascal(entry.name)} = "
                if entry.name in SECRETS:
                    writer.line(f'builder.Append("{label}***");')
                else:
                    writer.line(f'builder.Append("{label}").Append((object?){pascal(entry.name)});')
            writer.line("return true;")
            writer.dedent()
            writer.line("}")
        writer.dedent()
        writer.line("}")
        writer.blank()

    def input(self, writer: Writer, item: Struct) -> None:
        origin = f"The fields of pyBYD's `{item.name}`. " if item.pybyd else ""
        self.doc(writer, item.doc, origin + "A member left null is not sent, so pyBYD's default applies.")
        writer.line(f"public sealed class {item.name}")
        writer.line("{")
        writer.indent()
        for entry in item.fields:
            self.doc(writer, self.field_doc(entry))
            if entry.required:
                writer.line(f"public required {self.type(entry.type)} {pascal(entry.name)} {{ get; init; }}")
            else:
                writer.line(f"public {self.type(opt(entry.type))} {pascal(entry.name)} {{ get; init; }}")
            writer.blank()
        writer.dedent()
        writer.line("}")
        writer.blank()

    def check_names(self, item: Struct) -> None:
        names = [pascal(entry.name) for entry in item.fields]
        assert len(set(names)) == len(names), f"{item.name}: fields clash in PascalCase"
        assert item.name not in names, f"{item.name} has a field of its own name, which C# does not allow"

    def models(self) -> str:
        writer = self.file("OpenByd.Models", ("System.Text", "System.Text.Json", "System.Text.Json.Serialization"))
        self.doc(
            writer,
            "One of pyBYD's reply models, which `OpenBydCore.ParseModel` parses a raw payload as.",
        )
        writer.line("public interface IBydModel")
        writer.line("{")
        writer.line("}")
        writer.blank()
        for item in self.surface.structs.values():
            self.check_names(item)
            if item.pybyd == "config":
                continue
            if item.role == "output":
                self.output(writer, item)
            else:
                self.input(writer, item)
        return writer.text()

    def config(self) -> str:
        writer = self.file("OpenByd", ())
        for item in self.surface.structs.values():
            if item.pybyd == "config":
                assert item.role == "input", item.name
                self.input(writer, item)
        return writer.text()

    # -- Json.g.cs ---------------------------------------------------------------

    def enum_json(self, writer: Writer, item: Enum) -> None:
        name = item.name
        self.doc(writer, f"`{name}` in JSON: its values, and no other.")
        writer.line(f"internal sealed class {name}Json() : WireConverter<{name}>(ReadValue, WriteValue)")
        writer.line("{")
        writer.indent()
        read = "Wire.Int" if item.base == "int" else "Wire.Str"
        writer.line(f"internal static {name} ReadValue(ref Utf8JsonReader reader) => {read}(ref reader) switch")
        writer.line("{")
        writer.indent()
        for member in item.members:
            value = str(member.value) if item.base == "int" else f'"{member.value}"'
            writer.line(f"{value} => {name}.{pascal(member.name)},")
        writer.line(f'var other => throw Wire.NotAMember(other, "{name}"),')
        writer.dedent()
        writer.line("};")
        writer.blank()
        if item.base == "int":
            writer.line(f"internal static void WriteValue(Utf8JsonWriter writer, {name} value) =>")
            writer.line("    writer.WriteNumberValue((long)value);")
        else:
            writer.line(f"internal static void WriteValue(Utf8JsonWriter writer, {name} value) => writer.WriteStringValue(value switch")
            writer.line("{")
            writer.indent()
            for member in item.members:
                writer.line(f'{name}.{pascal(member.name)} => "{member.value}",')
            writer.line(f'_ => throw new DataException($"{{value}} is not a valid {name}"),')
            writer.dedent()
            writer.line("});")
        writer.dedent()
        writer.line("}")
        writer.blank()

    def output_json(self, writer: Writer, item: Struct) -> None:
        name = item.name
        self.doc(writer, f"`{name}` in JSON: every field, under pyBYD's names, and no other.")
        writer.line(f"internal sealed class {name}Json() : WireConverter<{name}>(ReadValue, WriteValue)")
        writer.line("{")
        writer.indent()
        writer.line("private static readonly string[] Names =")
        writer.line("[")
        writer.indent()
        for entry in item.fields:
            writer.line(f'"{entry.name}",')
        writer.dedent()
        writer.line("];")
        writer.blank()

        writer.line(f"internal static {name} ReadValue(ref Utf8JsonReader reader)")
        writer.line("{")
        writer.indent()
        writer.line(f'Wire.BeginObject(ref reader, "{name}");')
        writer.line(f"Span<bool> seen = stackalloc bool[{len(item.fields)}];")
        writer.line("seen.Clear();")
        for entry in item.fields:
            initial = "default" if self.is_value(entry.type) or entry.type.kind == "opt" else "null!"
            if entry.read_back_default() is not None:
                initial = json.dumps(entry.read_back_default())
            writer.line(f"{self.type(entry.type)} {self.local(entry)} = {initial};")
        writer.line("while (Wire.NextName(ref reader, out var name))")
        writer.line("{")
        writer.indent()
        writer.line("switch (name)")
        writer.line("{")
        writer.indent()
        for index, entry in enumerate(item.fields):
            writer.line(f'case "{entry.name}":')
            writer.indent()
            writer.line(f"Wire.Mark(seen, {index}, name);")
            fallback = entry.read_back_default()
            if fallback is None:
                writer.line(f"{self.local(entry)} = {self.read(entry.type)};")
            else:
                # Absent or null in an older record: the default, as every other layer reads it.
                read = f"Wire.Opt<{self.type(entry.type)}>(ref reader, {self.reader(entry.type)})"
                writer.line(f"{self.local(entry)} = {read} ?? {json.dumps(fallback)};")
            writer.line("break;")
            writer.dedent()
        writer.line("default:")
        writer.indent()
        writer.line(f'throw Wire.UnknownField("{name}", name);')
        writer.dedent()
        writer.dedent()
        writer.line("}")
        writer.dedent()
        writer.line("}")
        for index, entry in enumerate(item.fields):
            if entry.read_back_default() is not None:
                writer.line(f"seen[{index}] = true; // optional: an older record may lack it")
        writer.line(f'Wire.Complete(seen, Names, "{name}");')
        writer.line(f"return new {name}")
        writer.line("{")
        writer.indent()
        for entry in item.fields:
            writer.line(f"{pascal(entry.name)} = {self.local(entry)},")
        writer.dedent()
        writer.line("};")
        writer.dedent()
        writer.line("}")
        writer.blank()

        writer.line(f"internal static void WriteValue(Utf8JsonWriter writer, {name} value)")
        writer.line("{")
        writer.indent()
        writer.line("writer.WriteStartObject();")
        for entry in item.fields:
            writer.line(f'writer.WritePropertyName("{entry.name}"u8);')
            writer.line(f"{self.write(entry.type, 'writer', f'value.{pascal(entry.name)}')};")
        writer.line("writer.WriteEndObject();")
        writer.dedent()
        writer.line("}")
        writer.dedent()
        writer.line("}")
        writer.blank()

    def local(self, entry: Field) -> str:
        """The local variable a reader keeps a field in, named so that it clashes with no other."""
        return f"_{camel(entry.name)}"

    def input_json(self, writer: Writer, item: Struct) -> None:
        name = item.name
        self.doc(writer, f"`{name}` as the core takes it: under pyBYD's names, without what was not given.")
        writer.line(f"internal static class {name}Json")
        writer.line("{")
        writer.indent()
        writer.line(f"internal static void WriteValue(Utf8JsonWriter writer, {name} value)")
        writer.line("{")
        writer.indent()
        writer.line("writer.WriteStartObject();")
        for entry in item.fields:
            writer.line(f"{self.argument('Wire.Field', 'writer, ', entry, f'value.{pascal(entry.name)}')};")
        writer.line("writer.WriteEndObject();")
        writer.dedent()
        writer.line("}")
        writer.dedent()
        writer.line("}")
        writer.blank()

    def json(self) -> str:
        writer = self.file("OpenByd", ("System.Text.Json", "OpenByd.Models"))
        for item in self.surface.enums.values():
            self.enum_json(writer, item)
        for item in self.surface.structs.values():
            if item.role == "output":
                self.output_json(writer, item)
            else:
                self.input_json(writer, item)
        self.doc(writer, "The core's call name and the reader of each of pyBYD's reply models, for `ParseModel`.")
        writer.line("internal static class ModelCalls")
        writer.line("{")
        writer.indent()
        writer.line("private static readonly Dictionary<Type, (string Call, Delegate Read)> Known = new()")
        writer.line("{")
        writer.indent()
        for item in self.surface.structs.values():
            if item.pybyd and item.role == "output":
                call = f"models.{item.pybyd}.{item.name}"
                writer.line(f'[typeof({item.name})] = ("{call}", (Reader<{item.name}>){item.name}Json.ReadValue),')
        writer.dedent()
        writer.line("};")
        writer.blank()
        writer.line("internal static (string Call, Reader<T> Read) Of<T>()")
        writer.line("    where T : class, IBydModel =>")
        writer.line("    Known.TryGetValue(typeof(T), out var known)")
        writer.line("        ? (known.Call, (Reader<T>)known.Read)")
        writer.line('        : throw new DataException($"{typeof(T).Name} is not one of pyBYD\'s models", "TypeError");')
        writer.dedent()
        writer.line("}")
        return writer.text()

    # -- Exceptions.g.cs ---------------------------------------------------------

    def error_fields(self, item: ErrorClass) -> list[Field]:
        """What an error class carries besides its message and its kind, its parents' fields first."""
        by_name = {error.name: error for error in self.surface.errors}
        inherited = self.error_fields(by_name[item.parent]) if item.parent else []
        return [*inherited, *(entry for entry in item.fields if entry.name != "kind")]

    def error_parameter(self, entry: Field) -> str:
        name = camel(entry.name)
        if entry.type.kind == "opt":
            return f"{self.type(entry.type)} {name} = null"
        if entry.type.kind == "list":
            return f"{self.type(entry.type)}? {name} = null"
        assert entry.type.kind == "str", f"an error field of kind {entry.type.kind} has no default"
        return f'string {name} = ""'

    def error(self, writer: Writer, item: ErrorClass) -> None:
        name = exception(item.name)
        own = [entry for entry in item.fields if entry.name != "kind"]
        fields = self.error_fields(item)
        inherited = fields[: len(fields) - len(own)]
        # The Python class a new instance stands for: the class's own, where it has one.
        kind = item.name if item.name in item.python or not item.parent else item.python[0]
        self.doc(writer, item.doc)
        writer.line(f"public class {name} : {exception(item.parent) if item.parent else 'Exception'}")
        writer.line("{")
        writer.indent()

        params = [("message", "What went wrong.")]
        params.extend((camel(entry.name), entry.doc) for entry in fields)
        self.doc(writer, "An error made by the application, such as one a relogin callback throws.", "", tuple(params))
        # Written as it is: the Python class keeps its Python name here.
        writer.line(f'/// <param name="kind">The Python exception class this stands for; <c>{kind}</c> when null.</param>')
        signature = ", ".join(["string message", *(self.error_parameter(entry) for entry in fields), "string? kind = null"])
        writer.line(f"public {name}({signature})")
        if item.parent:
            passed = ", ".join(["message", *(camel(entry.name) for entry in inherited), f'kind ?? "{kind}"'])
            writer.line(f"    : base({passed})")
        else:
            writer.line("    : base(message)")
        writer.line("{")
        writer.indent()
        if not item.parent:
            writer.line(f'Kind = kind ?? "{kind}";')
        for entry in own:
            value = camel(entry.name) + (" ?? []" if entry.type.kind == "list" else "")
            writer.line(f"{pascal(entry.name)} = {value};")
        writer.dedent()
        writer.line("}")
        writer.blank()

        writer.line(f"internal {name}(in ErrorDetails details)")
        writer.line("    : base(details)" if item.parent else "    : base(details.Message)")
        writer.line("{")
        writer.indent()
        if not item.parent:
            writer.line("Kind = details.Kind;")
        for entry in own:
            fallback = {"opt": "", "list": " ?? []"}.get(entry.type.kind, ' ?? ""')
            writer.line(f"{pascal(entry.name)} = details.{pascal(entry.name)}{fallback};")
        writer.dedent()
        writer.line("}")
        writer.blank()

        for entry in item.fields:
            self.doc(writer, entry.doc)
            writer.line(f"public {self.type(entry.type)} {pascal(entry.name)} {{ get; }}")
            writer.blank()
        writer.dedent()
        writer.line("}")
        writer.blank()

    def exceptions(self) -> str:
        writer = self.file("OpenByd", ())
        for item in self.surface.errors:
            self.error(writer, item)
        self.doc(writer, "The exception class for each Python exception class the core reports.")
        writer.line("internal static class ErrorClasses")
        writer.line("{")
        writer.indent()
        writer.line("internal static readonly Dictionary<string, Func<ErrorDetails, OpenBydException>> ByPythonName = new()")
        writer.line("{")
        writer.indent()
        for item in self.surface.errors:
            for python in item.python:
                writer.line(f'["{python}"] = static details => new {exception(item.name)}(details),')
        writer.dedent()
        writer.line("};")
        writer.dedent()
        writer.line("}")
        return writer.text()

    # -- Client.g.cs, Car.g.cs ---------------------------------------------------

    def method_name(self, method: Method) -> str:
        return pascal(method.name) + ("" if method.sync else "Async")

    def parameter(self, entry: Field) -> str:
        """A parameter's name in C#."""
        return RENAMED.get(entry.name, camel(entry.name))

    def method(self, writer: Writer, method: Method, invoker: str) -> None:
        params = []
        documented = []
        added = []
        for entry in (*method.params, *method.optional):
            name = self.parameter(entry)
            optional = entry in method.optional
            if optional:
                params.append(f"{self.type(opt(entry.type))} {name} = null")
            else:
                params.append(f"{self.type(entry.type)} {name}")
            documented.append((name, self.field_doc(entry) if optional else entry.doc or f"pyBYD's `{entry.name}`."))
            added.append(self.argument(".Add", "", entry, name))
        self.doc(writer, method.doc, "", tuple(documented))

        returns = None if method.returns is None else self.type(method.returns)
        call = f"{invoker}.{'CallSync' if method.sync else 'CallAsync'}" + ("" if returns is None else f"<{returns}>")
        read = [] if method.returns is None else [self.reader(method.returns)]
        if method.prop:
            assert method.sync and returns is not None and not params, method.wire
            writer.line(f"public {returns} {pascal(method.name)} =>")
            writer.line(f'    {call}("{method.wire}", {invoker}.NewArgs(), {read[0]});')
            writer.blank()
            return

        if method.sync:
            result = returns or "void"
        else:
            result = "async Task" if returns is None else f"async Task<{returns}>"
        opening = f"public {result} {self.method_name(method)}("
        if len(opening) + sum(len(text) + 2 for text in params) <= 110:
            writer.line(f"{opening}{', '.join(params)})")
        else:
            writer.line(opening)
            writer.indent()
            for index, text in enumerate(params):
                writer.line(text + ("," if index < len(params) - 1 else ")"))
            writer.dedent()
        writer.line("{")
        writer.indent()
        target = "args"
        if added:
            writer.line(f"var args = {invoker}.NewArgs()")
            writer.indent()
            for index, text in enumerate(added):
                writer.line(text + (";" if index == len(added) - 1 else ""))
            writer.dedent()
        else:
            target = f"{invoker}.NewArgs()"
        passed = [f'"{method.wire}"', target, *read]
        before = ("return " if returns else "") + ("" if method.sync else "await ") + call
        after = ");" if method.sync else ").ConfigureAwait(false);"
        if len(before) + sum(len(text) + 2 for text in passed) + len(after) <= 112:
            writer.line(f"{before}({', '.join(passed)}{after}")
        else:
            writer.line(f"{before}(")
            writer.indent()
            for index, text in enumerate(passed):
                writer.line(text + ("," if index < len(passed) - 1 else after))
            writer.dedent()
        writer.dedent()
        writer.line("}")
        writer.blank()

    def client(self) -> str:
        writer = self.file("OpenByd", ("System.Text.Json", "OpenByd.Models"))
        writer.line("public sealed partial class Client")
        writer.line("{")
        writer.indent()
        for method in self.surface.client:
            self.method(writer, method, "Invoker")
        writer.dedent()
        writer.line("}")
        return writer.text()

    def seat_availability(self, writer: Writer) -> None:
        """`SeatFeature.IsAvailable`: the seat's entry of the car's features, by position and mode."""
        positions = self.surface.enums["SeatPosition"]
        modes = self.surface.enums["SeatMode"]
        seats = {entry.name for entry in self.surface.structs["SeatFeatures"].fields}
        offers = {entry.name for entry in self.surface.structs["SeatAvailability"].fields}
        assert {member.value for member in positions.members} == seats, "SeatFeatures has a field for each seat"
        assert {member.value for member in modes.members} == offers, "SeatAvailability has a field for each mode"
        self.doc(writer, "Whether a seat can be heated or ventilated.", "", (("position", "The seat."), ("mode", "Heat or ventilation.")))
        writer.line("public bool IsAvailable(SeatPosition position, SeatMode mode)")
        writer.line("{")
        writer.indent()
        writer.line("var seats = _car.Features.Seat;")
        writer.line("var seat = position switch")
        writer.line("{")
        writer.indent()
        for member in positions.members:
            writer.line(f"SeatPosition.{pascal(member.name)} => seats.{pascal(str(member.value))},")
        writer.line('_ => throw new DataException($"{position} is not a valid SeatPosition"),')
        writer.dedent()
        writer.line("};")
        writer.line("return mode switch")
        writer.line("{")
        writer.indent()
        for member in modes.members:
            writer.line(f"SeatMode.{pascal(member.name)} => seat.{pascal(str(member.value))},")
        writer.line('_ => throw new DataException($"{mode} is not a valid SeatMode"),')
        writer.dedent()
        writer.line("};")
        writer.dedent()
        writer.line("}")
        writer.blank()

    def feature(self, writer: Writer, feature: Feature) -> None:
        self.doc(writer, f"`car.{pascal(feature.name)}`: {feature.doc[0].lower()}{feature.doc[1:]}")
        writer.line(f"public sealed class {feature.class_name}")
        writer.line("{")
        writer.indent()
        writer.line("private readonly Car _car;")
        writer.blank()
        writer.line(f"internal {feature.class_name}(Car car) => _car = car;")
        writer.blank()
        for entry in feature.props:
            self.doc(writer, entry.doc)
            writer.line(
                f"public {self.type(entry.type)} {pascal(entry.name)} => "
                f"_car.Features.{pascal(feature.name)}.{pascal(entry.name)};"
            )
            writer.blank()
        if feature.name == "seat":
            self.seat_availability(writer)
        for method in feature.methods:
            self.method(writer, method, "_car.Invoker")
        writer.dedent()
        writer.line("}")
        writer.blank()

    def car(self) -> str:
        writer = self.file("OpenByd", ("System.Text.Json", "OpenByd.Models"))
        writer.line("public sealed partial class Car")
        writer.line("{")
        writer.indent()
        writer.line("internal Car(IInvoker invoker, string vin)")
        writer.line("{")
        writer.indent()
        writer.line("Invoker = invoker;")
        writer.line("Vin = vin;")
        for feature in self.surface.features:
            writer.line(f"{pascal(feature.name)} = new {feature.class_name}(this);")
        writer.dedent()
        writer.line("}")
        writer.blank()
        writer.line("internal IInvoker Invoker { get; }")
        writer.blank()
        for feature in self.surface.features:
            self.doc(writer, feature.doc)
            writer.line(f"public {feature.class_name} {pascal(feature.name)} {{ get; }}")
            writer.blank()
        for method in self.surface.car:
            self.method(writer, method, "Invoker")
        writer.dedent()
        writer.line("}")
        writer.blank()
        for feature in self.surface.features:
            self.feature(writer, feature)
        return writer.text()

    def files(self) -> dict[str, str]:
        return {
            f"{ROOT}/Enums.g.cs": self.enums(),
            f"{ROOT}/Models.g.cs": self.models(),
            f"{ROOT}/Config.g.cs": self.config(),
            f"{ROOT}/Json.g.cs": self.json(),
            f"{ROOT}/Exceptions.g.cs": self.exceptions(),
            f"{ROOT}/Client.g.cs": self.client(),
            f"{ROOT}/Car.g.cs": self.car(),
        }


def files(surface: Surface) -> dict[str, str]:
    """Every generated file of the NuGet package, by path from the repository's root."""
    return CSharp(surface).files()
