"""The Java emitter: the typed layer of the Maven package (`bindings/jvm/src/generated`).

Java and Kotlin callers get names in their own case: a model is a record
whose components are camelCase and map to pyBYD's snake_case JSON, an enum
is a Java enum carrying its wire value, and what a caller builds (the
config, parameter objects, options) is a class with fluent setters. Every
type that may be null is marked `@Nullable` (JSpecify), so Kotlin sees
which values can be missing.

Files, one class each:

- ``model/``: every enum, every model the core returns, every object a
  caller builds and every options class.
- the exceptions: pyBYD's hierarchy, with ``Errors``, the table from Python
  class names to them.
- ``ClientMethods``, ``CarMethods`` and the feature classes: the methods of
  the client, of a car and of its features.
- ``ResultTypes`` and ``Models``: what the methods return, and the core's
  call name of each pyBYD model.
"""

from __future__ import annotations

import re

from gen.common import Writer, default_text, header
from gen.ir import MISSING, Enum, ErrorClass, Feature, Field, Method, Struct, Surface, Type, camel, words

#: The directories that hold this emitter's files and nothing else.
OWNED = ("bindings/jvm/src/generated",)

PACKAGE = "io.github.pho3nix90.openbyd"
MODEL = f"{PACKAGE}.model"
ROOT = "bindings/jvm/src/generated/java/" + PACKAGE.replace(".", "/")

#: The longest line of code the emitter writes before it breaks a statement.
WIDTH = 110

#: What a record component, a method or a parameter cannot be called.
RESERVED = frozenset(
    """abstract assert boolean break byte case catch char class const continue default do double else enum
    extends final finally float for goto if implements import instanceof int interface long native new
    package private protected public return short static strictfp super switch synchronized this throw
    throws transient try void volatile while true false null var record yield sealed permits
    clone finalize getClass hashCode notify notifyAll toString wait equals""".split()
)

#: Imports a file may need, by the name its code uses.
IMPORTS = {
    "ArrayList": "java.util.ArrayList",
    "BiFunction": "java.util.function.BiFunction",
    "Collections": "java.util.Collections",
    "CompletableFuture": "java.util.concurrent.CompletableFuture",
    "JsonAutoDetect": "com.fasterxml.jackson.annotation.JsonAutoDetect",
    "JsonCreator": "com.fasterxml.jackson.annotation.JsonCreator",
    "JsonInclude": "com.fasterxml.jackson.annotation.JsonInclude",
    "JsonProperty": "com.fasterxml.jackson.annotation.JsonProperty",
    "JsonPropertyOrder": "com.fasterxml.jackson.annotation.JsonPropertyOrder",
    "JsonValue": "com.fasterxml.jackson.annotation.JsonValue",
    "List": "java.util.List",
    "Map": "java.util.Map",
    "Nullable": "org.jspecify.annotations.Nullable",
    "Objects": "java.util.Objects",
    "Serial": "java.io.Serial",
    "TypeReference": "com.fasterxml.jackson.core.type.TypeReference",
}


def name(python: str) -> str:
    """A Python name as a Java member name: ``poll_attempts`` -> ``pollAttempts``."""
    java = camel(python)
    assert java not in RESERVED, f"{python} is not a name Java allows"
    return java


def fill(text: str, width: int) -> list[str]:
    """Words as lines no longer than ``width``. A ``\\0`` joins words that stay together."""
    lines: list[str] = []
    current = ""
    for word in text.split():
        if current and len(current) + 1 + len(word) > width:
            lines.append(current)
            current = word
        else:
            current = f"{current} {word}" if current else word
    if current:
        lines.append(current)
    return [line.replace("\0", " ") for line in lines]


def escape(text: str) -> str:
    """Prose as Javadoc takes it: no HTML, no tag, no end of comment."""
    text = text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
    return text.replace("@", "{@literal\0@}").replace("*/", "*&#47;")


class Java:
    def __init__(self, surface: Surface) -> None:
        self.surface = surface
        #: The result types the methods use: constant name -> Java type.
        self.results: dict[str, str] = {}
        #: Python names a method's documentation mentions, as a Java caller knows them.
        self.renamed: dict[str, str] = {}
        for method in surface.methods():
            self.renamed[method.name] = camel(method.name)
        for struct in surface.structs.values():
            if struct.role == "input":
                for entry in struct.fields:
                    self.renamed[entry.name] = camel(entry.name)
        for error in surface.errors:
            self.renamed[error.name] = self.exception(error.name)

    # -- documentation -------------------------------------------------------

    def prose(self, text: str, rename: bool = False) -> str:
        """Documentation as Javadoc: `code` spans become ``{@code}``, the rest is escaped.

        With ``rename``, a span that names a method, an input field or an
        error class is written as Java has it.
        """
        parts = re.split(r"`([^`]+)`", text)
        out = []
        for index, part in enumerate(parts):
            if index % 2 == 0:
                out.append(escape(part))
                continue
            code = self.renamed.get(part, part) if rename else part
            assert "*/" not in code and code.count("{") == code.count("}"), code
            out.append("{@code\0" + code.replace(" ", "\0") + "}")
        return "".join(out)

    def doc(
        self,
        writer: Writer,
        *paragraphs: str,
        tags: tuple[tuple[str, str], ...] = (),
        rename: bool = False,
    ) -> None:
        """A Javadoc comment of the non-empty paragraphs and the tags (``("@param vin", text)``)."""
        lines: list[str] = []
        for text in (text for text in paragraphs if text):
            if lines:
                lines.append("")
                text = "<p>" + self.prose(text, rename)
            else:
                text = self.prose(text, rename)
            lines.extend(fill(text, 92))
        tag_lines: list[str] = []
        for tag, text in tags:
            first, *rest = fill(f"{tag} {self.prose(text, rename)}", 92)
            tag_lines.append(first)
            tag_lines.extend(f"    {line}" for line in rest)
        if lines and tag_lines:
            lines.append("")
        lines.extend(tag_lines)
        if not lines:
            return
        if len(lines) == 1 and not tags:
            writer.line(f"/** {lines[0]} */")
            return
        writer.line("/**")
        for text in lines:
            writer.line(f" * {text}".rstrip())
        writer.line(" */")

    def note(self, item: Field) -> str:
        """What a field's documentation adds: its format and its default."""
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
        """A default as documentation shows it: an enum's member by name."""
        enum = self.surface.enums.get(kind.name) if kind.kind == "ref" else None
        if enum is not None:
            for member in enum.members:
                if member.value == value:
                    return f"{enum.name}.{member.name}"
        return default_text(value)

    # -- types ---------------------------------------------------------------

    def type(self, kind: Type, boxed: bool = False) -> str:
        """The Java type of ``kind``; ``boxed`` where a primitive cannot stand (a type argument)."""
        if kind.kind in ("str", "datetime", "time"):
            return "String"
        if kind.kind == "int":
            return "Long" if boxed else "long"
        if kind.kind == "float":
            return "Double" if boxed else "double"
        if kind.kind == "bool":
            return "Boolean" if boxed else "boolean"
        if kind.kind == "any":
            return "@Nullable Object"
        if kind.kind == "union":
            return "Object"
        if kind.kind == "opt":
            inner = self.type(kind.inner, boxed=True)
            return inner if inner.startswith("@Nullable ") else f"@Nullable {inner}"
        if kind.kind == "list":
            return f"List<{self.type(kind.inner, boxed=True)}>"
        if kind.kind == "map":
            key = "Integer" if kind.key == "int" else "String"
            return f"Map<{key}, {self.type(kind.inner, boxed=True)}>"
        return kind.name

    def parameter(self, kind: Type) -> str:
        """The Java type of a parameter of ``kind``: a map of any values takes a map of any type."""
        java = self.type(kind)
        if kind.kind == "map":
            key, _, value = java.partition(", ")
            return f"{key}, ? extends {value}"
        return java

    def nullable(self, kind: Type) -> bool:
        return self.type(kind).startswith("@Nullable ")

    def exception(self, python: str) -> str:
        """The Java name of an error class: ``BydApiError`` -> ``BydApiException``."""
        return python.removesuffix("Error") + "Exception"

    def result(self, kind: Type | None) -> str:
        """The constant of ``ResultTypes`` for what a method returns."""
        constant, java = self.result_name(kind)
        self.results[constant] = java
        return f"ResultTypes.{constant}"

    def result_name(self, kind: Type | None) -> tuple[str, str]:
        if kind is None:
            return "NOTHING", "Void"
        if kind.kind == "opt":
            return self.result_name(kind.inner)
        if kind.kind == "list":
            constant, java = self.result_name(kind.inner)
            return f"LIST_OF_{constant}", f"List<{java}>"
        if kind.kind == "map":
            constant, java = self.result_name(kind.inner)
            if kind.key == "int":
                return f"INT_MAP_OF_{constant}", f"Map<Integer, {java}>"
            return f"MAP_OF_{constant}", f"Map<String, {java}>"
        if kind.kind == "ref":
            return "_".join(words(kind.name)).upper(), kind.name
        plain = {"bool": ("BOOL", "Boolean"), "str": ("STRING", "String"), "int": ("LONG", "Long")}
        plain |= {"float": ("DOUBLE", "Double"), "any": ("ANY", "Object")}
        return plain[kind.kind]

    # -- files ---------------------------------------------------------------

    def file(self, package: str, body: Writer) -> str:
        """A source file: the header, the package, the imports its body needs, the body."""
        text = body.text()
        # Names are looked for in code, not in comments.
        code = "\n".join(line for line in text.splitlines() if not line.lstrip().startswith(("*", "/**")))
        imports = {path for used, path in IMPORTS.items() if re.search(rf"\b{used}\b", code)}
        if package != MODEL:
            for used in (*self.surface.structs, *self.surface.enums, *self.surface.option_groups()):
                if re.search(rf"\b{used}\b", code):
                    imports.add(f"{MODEL}.{used}")
        top = Writer()
        top.lines(header(self.surface))
        top.line(f"package {package};")
        if imports:
            top.line()
            top.lines(f"import {path};" for path in sorted(imports))
        return top.text() + "\n" + text

    def strings(self, writer: Writer, opening: str, items: list[str], closing: str) -> None:
        """``opening "a", "b" closing`` over as many lines as it needs."""
        quoted = [f'"{item}"' for item in items]
        one_line = opening + ", ".join(quoted) + closing
        if len(one_line) + 4 <= WIDTH:
            writer.line(one_line)
            return
        writer.line(opening)
        writer.indent()
        for line in fill(", ".join(quoted), WIDTH - 12):
            writer.line(line)
        writer.dedent()
        writer.line(closing)

    # -- model: enums --------------------------------------------------------

    def enum(self, item: Enum) -> str:
        writer = Writer()
        value = "String" if item.base == "str" else "int"
        self.doc(writer, item.doc)
        writer.line(f"public enum {item.name} {{")
        writer.indent()
        for index, member in enumerate(item.members):
            assert re.fullmatch(r"[A-Z][A-Z0-9_]*", member.name), member.name
            if item.base == "str":
                literal = f'"{member.value}"'
            else:
                assert -(2**31) <= int(member.value) < 2**31, f"{item.name}.{member.name} is not an int"
                literal = str(member.value)
            self.doc(writer, member.doc)
            writer.line(f"{member.name}({literal}){';' if index == len(item.members) - 1 else ','}")
        writer.line()
        writer.line(f"private final {value} value;")
        writer.line()
        writer.line(f"{item.name}({value} value) {{")
        writer.indent()
        writer.line("this.value = value;")
        writer.dedent()
        writer.line("}")
        writer.line()
        self.doc(writer, "The value as it crosses in JSON, which is pyBYD's.")
        writer.line("@JsonValue")
        writer.line(f"public {value} value() {{")
        writer.indent()
        writer.line("return value;")
        writer.dedent()
        writer.line("}")
        writer.line()
        self.doc(
            writer,
            "The member with this value.",
            tags=(("@throws IllegalArgumentException", "when no member has it"),),
        )
        writer.line("@JsonCreator(mode = JsonCreator.Mode.DELEGATING)")
        writer.line(f"public static {item.name} fromValue({value} value) {{")
        writer.indent()
        writer.line(f"for ({item.name} member : values()) {{")
        writer.indent()
        writer.line("if (member.value.equals(value)) {" if item.base == "str" else "if (member.value == value) {")
        writer.indent()
        writer.line("return member;")
        writer.dedent()
        writer.line("}")
        writer.dedent()
        writer.line("}")
        writer.line(f'throw new IllegalArgumentException(value + " is not a valid {item.name}");')
        writer.dedent()
        writer.line("}")
        writer.dedent()
        writer.line("}")
        return self.file(MODEL, writer)

    # -- model: records ------------------------------------------------------

    def output(self, item: Struct) -> str:
        writer = Writer()
        origin = f"pyBYD's `{item.name}`, as `model_dump(mode=\"json\")` gives it." if item.pybyd else ""
        tags = []
        for entry in item.fields:
            text = "The reply this was parsed from, as BYD sent it." if entry.name == "raw" else entry.doc
            text = " ".join(part for part in (text, self.note(entry)) if part)
            if text:
                tags.append((f"@param {name(entry.name)}", text))
        self.doc(writer, item.doc, origin, tags=tuple(tags))
        self.strings(writer, "@JsonPropertyOrder({", [entry.name for entry in item.fields], "})")
        writer.line(f"public record {item.name}(")
        writer.indent()
        writer.indent()
        for index, entry in enumerate(item.fields):
            end = ") {" if index == len(item.fields) - 1 else ","
            writer.line(f'@JsonProperty("{entry.name}") {self.type(entry.type)} {name(entry.name)}{end}')
        writer.dedent()
        required = [entry for entry in item.fields if not self.nullable(entry.type) and self.type(entry.type)[0].isupper()]
        if required:
            writer.line()
            self.doc(writer, "Refuses null where a value must be.")
            writer.line(f"public {item.name} {{")
            writer.indent()
            for entry in required:
                writer.line(f'Objects.requireNonNull({name(entry.name)}, "{entry.name}");')
            writer.dedent()
            writer.line("}")
        writer.dedent()
        writer.line("}")
        # A method takes at most 255 slots, and a long or a double takes two.
        slots = sum(2 if self.type(entry.type) in ("long", "double") else 1 for entry in item.fields)
        assert slots < 255, f"{item.name} has too many fields for a record"
        return self.file(MODEL, writer)

    # -- model: what a caller builds -----------------------------------------

    def setter_doc(self, entry: Field) -> str:
        text = entry.doc or f"pyBYD's `{entry.name}`."
        text = text.replace(" when null", " when not set").replace("; null keeps", "; not set, it keeps")
        return " ".join(part for part in (text, self.note(entry)) if part)

    def builder(self, class_name: str, fields: tuple[Field, ...], *paragraphs: str, what: str) -> str:
        """A class a caller builds: required fields in the constructor, a fluent setter for each other."""
        writer = Writer()
        self.doc(writer, *paragraphs, rename=True)
        writer.line("@JsonInclude(JsonInclude.Include.NON_NULL)")
        writer.line("@JsonAutoDetect(")
        writer.indent()
        writer.indent()
        writer.line("fieldVisibility = JsonAutoDetect.Visibility.NONE,")
        writer.line("getterVisibility = JsonAutoDetect.Visibility.NONE,")
        writer.line("isGetterVisibility = JsonAutoDetect.Visibility.NONE)")
        writer.dedent()
        writer.dedent()
        writer.line(f"public final class {class_name} {{")
        writer.indent()
        required = [entry for entry in fields if entry.required]
        for entry in fields:
            inner = entry.type.inner if entry.type.kind == "opt" else entry.type
            writer.line(f'@JsonProperty("{entry.name}")')
            if entry.required:
                writer.line(f"private final {self.type(entry.type)} {name(entry.name)};")
            else:
                boxed = self.type(inner, boxed=True).removeprefix("@Nullable ")
                writer.line(f"private @Nullable {boxed} {name(entry.name)};")
            writer.line()
        tags = tuple((f"@param {name(entry.name)}", self.setter_doc(entry)) for entry in required)
        every = "every other value" if required else "every value"
        self.doc(writer, f"{what} with {every} left to its default.", tags=tags)
        parameters = ", ".join(f"{self.type(entry.type)} {name(entry.name)}" for entry in required)
        if required:
            writer.line(f"public {class_name}({parameters}) {{")
            writer.indent()
            for entry in required:
                java = name(entry.name)
                if self.type(entry.type)[0].isupper():
                    writer.line(f'this.{java} = Objects.requireNonNull({java}, "{java}");')
                else:
                    writer.line(f"this.{java} = {java};")
            writer.dedent()
            writer.line("}")
        else:
            writer.line(f"public {class_name}() {{}}")
        for entry in fields:
            if entry.required:
                continue
            inner = entry.type.inner if entry.type.kind == "opt" else entry.type
            java = name(entry.name)
            writer.line()
            self.doc(writer, self.setter_doc(entry), tags=(("@return", "this, to chain"),), rename=True)
            writer.line(f"public {class_name} {java}({self.type(inner).removeprefix('@Nullable ')} {java}) {{")
            writer.indent()
            if self.type(inner)[0].islower():
                writer.line(f"this.{java} = {java};")
            else:
                writer.line(f'this.{java} = Objects.requireNonNull({java}, "{java}");')
            writer.line("return this;")
            writer.dedent()
            writer.line("}")
        writer.dedent()
        writer.line("}")
        return self.file(MODEL, writer)

    def input(self, item: Struct) -> str:
        origin = f"The fields of pyBYD's `{item.name}`, in camelCase. A field that is not set keeps pyBYD's default."
        doc = item.doc or f"pyBYD's `{item.name}`."
        return self.builder(item.name, item.fields, doc, origin, what=f"A `{item.name}`")

    def options(self, class_name: str, fields: tuple[Field, ...], methods: list[str]) -> str:
        used = ", ".join(f"`{camel(method)}`" for method in methods)
        doc = f"The options of {used}. An option that is not set takes its default."
        return self.builder(class_name, fields, doc, what="Options")

    def model_package(self) -> str:
        writer = Writer()
        self.doc(
            writer,
            "What openbyd's calls take and return: pyBYD's models as records, its enums, and the "
            "objects a caller builds (the config, parameter objects, options).",
            "A record is exactly pyBYD's `model_dump(mode=\"json\")`: its components are pyBYD's fields "
            "in camelCase. Nothing here is null unless its type is marked `@Nullable`.",
        )
        writer.line("@NullMarked")
        writer.line(f"package {MODEL};")
        writer.line()
        writer.line("import org.jspecify.annotations.NullMarked;")
        return "\n".join(header(self.surface)) + "\n" + writer.text()

    # -- exceptions ----------------------------------------------------------

    def lineage(self, item: ErrorClass) -> list[ErrorClass]:
        """An error class and its parents, the root first."""
        by_name = {error.name: error for error in self.surface.errors}
        chain = [item]
        while chain[0].parent:
            chain.insert(0, by_name[chain[0].parent])
        return chain

    def error_field(self, entry: Field) -> tuple[str, str]:
        """An exception's field: the type it is kept as, and the type its accessor returns."""
        java = self.type(entry.type)
        # An exception is serializable, and so is what it holds.
        return (f"ArrayList<{java[5:-1]}>" if java.startswith("List<") else java), java

    def details(self) -> str:
        writer = Writer()
        self.doc(writer, "What an exception is built from: its fields, whichever its class carries.")
        writer.line("final class ErrorDetails {")
        writer.indent()
        writer.line("final String kind;")
        fields: dict[str, Field] = {}
        for item in self.surface.errors:
            for entry in item.fields:
                fields.setdefault(entry.name, entry)
        del fields["kind"]
        for entry in fields.values():
            writer.line(f"@Nullable {self.type(entry.type, boxed=True).removeprefix('@Nullable ')} {name(entry.name)};")
        writer.line()
        writer.line("ErrorDetails(String kind) {")
        writer.indent()
        writer.line("this.kind = kind;")
        writer.dedent()
        writer.line("}")
        for entry in fields.values():
            java = name(entry.name)
            boxed = self.type(entry.type, boxed=True).removeprefix("@Nullable ")
            writer.line()
            writer.line(f"ErrorDetails {java}(@Nullable {boxed} {java}) {{")
            writer.indent()
            writer.line(f"this.{java} = {java};")
            writer.line("return this;")
            writer.dedent()
            writer.line("}")
        writer.dedent()
        writer.line("}")
        return self.file(PACKAGE, writer)

    def error(self, item: ErrorClass) -> str:
        writer = Writer()
        class_name = self.exception(item.name)
        parent = self.exception(item.parent) if item.parent else "RuntimeException"
        self.doc(writer, item.doc)
        writer.line(f"public class {class_name} extends {parent} {{")
        writer.indent()
        writer.line("@Serial private static final long serialVersionUID = 1L;")
        for entry in item.fields:
            writer.line()
            writer.line(f"private final {self.error_field(entry)[0]} {name(entry.name)};")

        # What an application throws itself: from a relogin callback, say.
        carried = [entry for error in self.lineage(item) for entry in error.fields if entry.name != "kind"]
        asks_kind = len(item.python) != 1 or item.python[0] != item.name
        parameters = ["String message"]
        tags = [("@param message", "What went wrong.")]
        if asks_kind:
            parameters.append("String kind")
            tags.append(("@param kind", "The name of the Python exception class pyBYD raises."))
            details = "new ErrorDetails(kind)"
        else:
            details = f'new ErrorDetails("{item.name}")'
        for entry in carried:
            parameters.append(f"{self.type(entry.type)} {name(entry.name)}")
            tags.append((f"@param {name(entry.name)}", entry.doc))
            details += f".{name(entry.name)}({name(entry.name)})"
        writer.line()
        python = "" if asks_kind else f", as pyBYD's `{item.name}`"
        self.doc(writer, f"An exception for an application to throw{python}.", tags=tuple(tags))
        writer.line(f"public {class_name}({', '.join(parameters)}) {{")
        writer.indent()
        writer.line(f"this(message, {details});")
        writer.dedent()
        writer.line("}")
        writer.line()
        writer.line(f"{class_name}(String message, ErrorDetails details) {{")
        writer.indent()
        writer.line("super(message, details);" if item.parent else "super(message);")
        for entry in item.fields:
            java = name(entry.name)
            if java == "kind":
                writer.line("this.kind = details.kind;")
            elif entry.type.kind == "opt":
                writer.line(f"this.{java} = details.{java};")
            elif entry.type.kind == "list":
                writer.line(f"this.{java} = new ArrayList<>(details.{java} != null ? details.{java} : List.of());")
            else:
                assert entry.type.kind == "str", f"{item.name}.{entry.name}: no default for {entry.type.kind}"
                writer.line(f'this.{java} = details.{java} != null ? details.{java} : "";')
        writer.dedent()
        writer.line("}")
        for entry in item.fields:
            java = name(entry.name)
            kept, returned = self.error_field(entry)
            writer.line()
            self.doc(writer, entry.doc)
            writer.line(f"public {returned} {java}() {{")
            writer.indent()
            writer.line(f"return Collections.unmodifiableList({java});" if kept != returned else f"return {java};")
            writer.dedent()
            writer.line("}")
        writer.dedent()
        writer.line("}")
        return self.file(PACKAGE, writer)

    def errors(self) -> str:
        writer = Writer()
        self.doc(writer, "The exception class for each Python exception class the core reports.")
        writer.line("final class Errors {")
        writer.indent()
        writer.line("static final Map<String, BiFunction<String, ErrorDetails, OpenBydException>> CLASSES =")
        writer.indent()
        writer.indent()
        writer.line("Map.ofEntries(")
        writer.indent()
        entries = [(python, self.exception(item.name)) for item in self.surface.errors for python in item.python]
        for index, (python, java) in enumerate(entries):
            end = ");" if index == len(entries) - 1 else ","
            writer.line(f'Map.entry("{python}", {java}::new){end}')
        writer.dedent()
        writer.dedent()
        writer.dedent()
        writer.line()
        writer.line("private Errors() {}")
        writer.dedent()
        writer.line("}")
        return self.file(PACKAGE, writer)

    # -- methods -------------------------------------------------------------

    def broken(self, writer: Writer, start: str, items: list[str], end: str, room: int = 8) -> None:
        """``start(items)end`` on one line, or with each item on a line of its own.

        An item that is itself too long for a line, a call with arguments, is broken the same way.
        """
        one_line = f"{start}({', '.join(items)}){end}"
        if len(one_line) + room <= WIDTH:
            writer.line(one_line)
            return
        writer.line(f"{start}(")
        writer.indent()
        writer.indent()
        for index, item in enumerate(items):
            after = f"){end}" if index == len(items) - 1 else ","
            if len(item) + room + 8 > WIDTH and item.endswith(")") and "(" in item:
                call, _, inside = item[:-1].partition("(")
                pairs = inside.split(", ")
                paired = [", ".join(pairs[at : at + 2]) for at in range(0, len(pairs), 2)]
                self.broken(writer, call, paired, after, room + 8)
            else:
                writer.line(item + after)
        writer.dedent()
        writer.dedent()

    def statement(self, writer: Writer, start: str, arguments: list[str]) -> None:
        """``start(arguments);`` on one line, or with the arguments on lines of their own."""
        self.broken(writer, start, arguments, ";")

    def method(self, writer: Writer, method: Method) -> None:
        """A method, twice when it has options: without them, and with."""
        java = name(method.name)
        if method.returns is None:
            returns = "void" if method.sync else "CompletableFuture<Void>"
        elif method.sync:
            returns = self.type(method.returns)
        else:
            returns = f"CompletableFuture<{self.type(method.returns, boxed=True)}>"
        parameters = [f"{self.parameter(entry.type)} {name(entry.name)}" for entry in method.params]
        tags = [(f"@param {name(entry.name)}", entry.doc) for entry in method.params if entry.doc]
        pairs = [text for entry in method.params for text in (f'"{entry.name}"', name(entry.name))]
        arguments = [f'"{method.wire}"', f"Invoker.args({', '.join(pairs)})" if pairs else "Map.of()"]
        function = f"invoker.{'callSync' if method.sync else 'call'}"
        start = function if method.sync and method.returns is None else f"return {function}"

        if method.optional:
            assert not method.prop, f"{method.wire} is a property with options"
            self.doc(writer, method.doc, tags=tuple(tags), rename=True)
            self.broken(writer, f"public {returns} {java}", parameters, " {", room=4)
            writer.indent()
            passed = ", ".join([*(name(entry.name) for entry in method.params), f"new {method.options_name}()"])
            writer.line(f"{'' if returns == 'void' else 'return '}{java}({passed});")
            writer.dedent()
            writer.line("}")
            writer.line()
            parameters.append(f"{method.options_name} options")
            tags.append(("@param options", "What to change from the defaults."))
        self.doc(writer, method.doc, tags=tuple(tags), rename=True)
        self.broken(writer, f"public {returns} {java}", parameters, " {", room=4)
        writer.indent()
        options = "options" if method.optional else "null"
        self.statement(writer, start, [*arguments, options, self.result(method.returns)])
        writer.dedent()
        writer.line("}")
        writer.line()

    def client_methods(self) -> str:
        writer = Writer()
        self.doc(
            writer,
            "The client's methods: pyBYD's `BydClient`. `Client` completes it.",
            "A method that waits on the network returns a future; what fails completes it with one of "
            "`OpenBydException`'s subclasses. A method that answers at once returns its value and throws.",
        )
        writer.line("public abstract class ClientMethods {")
        writer.indent()
        writer.line("final Invoker invoker;")
        writer.line()
        writer.line("ClientMethods(Invoker invoker) {")
        writer.indent()
        writer.line("this.invoker = invoker;")
        writer.dedent()
        writer.line("}")
        writer.line()
        for method in self.surface.client:
            self.method(writer, method)
        writer.dedent()
        writer.line("}")
        return self.file(PACKAGE, writer)

    def feature(self, feature: Feature) -> str:
        writer = Writer()
        accessor = name(feature.name)
        self.doc(writer, f"`car.{accessor}()`: {feature.doc[0].lower()}{feature.doc[1:]}")
        writer.line(f"public final class {feature.class_name} {{")
        writer.indent()
        writer.line("private final Invoker invoker;")
        writer.line()
        writer.line(f"{feature.class_name}(Invoker invoker) {{")
        writer.indent()
        writer.line("this.invoker = invoker;")
        writer.dedent()
        writer.line("}")
        writer.line()
        if feature.props or feature.name == "seat":
            writer.line("private CarFeatures features() {")
            writer.indent()
            writer.line(f'return invoker.callSync("car.features", Map.of(), null, {self.result(Type("ref", name="CarFeatures"))});')
            writer.dedent()
            writer.line("}")
            writer.line()
        for entry in feature.props:
            self.doc(writer, entry.doc)
            writer.line(f"public {self.type(entry.type)} {name(entry.name)}() {{")
            writer.indent()
            writer.line(f"return features().{accessor}().{name(entry.name)}();")
            writer.dedent()
            writer.line("}")
            writer.line()
        if feature.name == "seat":
            self.doc(writer, "Whether a seat can be heated or ventilated.")
            writer.line("public boolean isAvailable(SeatPosition position, SeatMode mode) {")
            writer.indent()
            writer.line("SeatFeatures seats = features().seat();")
            writer.line("SeatAvailability seat = switch (position) {")
            writer.indent()
            for member in self.surface.enums["SeatPosition"].members:
                writer.line(f"case {member.name} -> seats.{name(str(member.value))}();")
            writer.dedent()
            writer.line("};")
            writer.line("return switch (mode) {")
            writer.indent()
            for member in self.surface.enums["SeatMode"].members:
                writer.line(f"case {member.name} -> seat.{name(str(member.value))}();")
            writer.dedent()
            writer.line("};")
            writer.dedent()
            writer.line("}")
            writer.line()
        for method in feature.methods:
            self.method(writer, method)
        writer.dedent()
        writer.line("}")
        return self.file(PACKAGE, writer)

    def car_methods(self) -> str:
        surface = self.surface
        taken = {name(method.name) for method in surface.car}
        writer = Writer()
        self.doc(writer, "A car's methods: pyBYD's `BydCar`. `Car` completes it.")
        writer.line("public abstract class CarMethods {")
        writer.indent()
        writer.line("final Invoker invoker;")
        for feature in surface.features:
            writer.line(f"private final {feature.class_name} {name(feature.name)};")
        writer.line()
        writer.line("CarMethods(Invoker invoker) {")
        writer.indent()
        writer.line("this.invoker = invoker;")
        for feature in surface.features:
            writer.line(f"this.{name(feature.name)} = new {feature.class_name}(invoker);")
        writer.dedent()
        writer.line("}")
        writer.line()
        for feature in surface.features:
            assert name(feature.name) not in taken, f"car.{feature.name} is a method and a feature"
            self.doc(writer, feature.doc)
            writer.line(f"public {feature.class_name} {name(feature.name)}() {{")
            writer.indent()
            writer.line(f"return {name(feature.name)};")
            writer.dedent()
            writer.line("}")
            writer.line()
        for method in surface.car:
            self.method(writer, method)
        writer.dedent()
        writer.line("}")
        return self.file(PACKAGE, writer)

    def result_types(self) -> str:
        writer = Writer()
        self.doc(writer, "What the methods return, for reading the core's JSON as it.")
        writer.line("final class ResultTypes {")
        writer.indent()
        for constant, java in sorted(self.results.items()):
            declaration = f"static final TypeReference<{java}> {constant} ="
            if len(declaration) + 30 <= WIDTH:
                writer.line(f"{declaration} new TypeReference<>() {{}};")
            else:
                writer.line(declaration)
                writer.line("        new TypeReference<>() {};")
        writer.line()
        writer.line("private ResultTypes() {}")
        writer.dedent()
        writer.line("}")
        return self.file(PACKAGE, writer)

    def models(self) -> str:
        writer = Writer()
        self.doc(writer, "The core's call name of each pyBYD model, for `OpenByd.parseModel`.")
        writer.line("final class Models {")
        writer.indent()
        writer.line("static final Map<Class<?>, String> CALLS =")
        writer.indent()
        writer.indent()
        writer.line("Map.ofEntries(")
        writer.indent()
        models = [item for item in self.surface.structs.values() if item.pybyd and item.role == "output"]
        for index, item in enumerate(models):
            end = ");" if index == len(models) - 1 else ","
            writer.line(f'Map.entry({item.name}.class, "models.{item.pybyd}.{item.name}"){end}')
        writer.dedent()
        writer.dedent()
        writer.dedent()
        writer.line()
        writer.line("private Models() {}")
        writer.dedent()
        writer.line("}")
        return self.file(PACKAGE, writer)

    def files(self) -> dict[str, str]:
        surface = self.surface
        files = {f"{ROOT}/model/package-info.java": self.model_package()}
        for item in surface.enums.values():
            files[f"{ROOT}/model/{item.name}.java"] = self.enum(item)
        for item in surface.structs.values():
            files[f"{ROOT}/model/{item.name}.java"] = self.output(item) if item.role == "output" else self.input(item)
        for class_name, (fields, methods) in surface.option_groups().items():
            files[f"{ROOT}/model/{class_name}.java"] = self.options(class_name, fields, methods)
        files[f"{ROOT}/ErrorDetails.java"] = self.details()
        files[f"{ROOT}/Errors.java"] = self.errors()
        for item in surface.errors:
            files[f"{ROOT}/{self.exception(item.name)}.java"] = self.error(item)
        files[f"{ROOT}/ClientMethods.java"] = self.client_methods()
        files[f"{ROOT}/CarMethods.java"] = self.car_methods()
        for feature in surface.features:
            files[f"{ROOT}/{feature.class_name}.java"] = self.feature(feature)
        # After the methods: they say which result types there are.
        files[f"{ROOT}/ResultTypes.java"] = self.result_types()
        files[f"{ROOT}/Models.java"] = self.models()
        return files


def files(surface: Surface) -> dict[str, str]:
    """Every generated file of the Maven package, by path from the repository's root."""
    return Java(surface).files()
