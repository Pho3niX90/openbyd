"""The TypeScript emitter: the typed layer of the npm package (`bindings/node/src/generated`).

What a caller passes (options, parameter objects, the config) has camelCase
fields, like the rest of a TypeScript program. What the core returns keeps
pyBYD's snake_case names: it is pyBYD's `model_dump(mode="json")` untouched,
so nothing is copied or renamed on the way out.

Files:

- ``models.ts``: every enum, every model the core returns, every object a
  caller builds.
- ``shapes.ts``: where floats sit inside what the core returns, so the
  strings JSON needs for NaN and the infinities come back as numbers.
- ``errors.ts``: pyBYD's exception hierarchy as classes.
- ``methods.ts``: the methods of the client, of a car and of its features.
"""

from __future__ import annotations

import re

from gen.common import Writer, default_text, has_float, header, wrap
from gen.ir import MISSING, Enum, ErrorClass, Feature, Field, Method, Struct, Surface, Type, camel

INDENT = "  "

#: The directories that hold this emitter's files and nothing else.
OWNED = ("bindings/node/src/generated",)


def doc(writer: Writer, *paragraphs: str) -> None:
    """A TSDoc comment of the non-empty paragraphs."""
    paragraphs = tuple(text for text in paragraphs if text)
    if not paragraphs:
        return
    lines: list[str] = []
    for index, text in enumerate(paragraphs):
        if index:
            lines.append("")
        lines.extend(wrap(text.replace("*/", "*\\/"), 92))
    if len(lines) == 1:
        writer.line(f"/** {lines[0]} */")
        return
    writer.line("/**")
    for text in lines:
        writer.line(f" * {text}".rstrip())
    writer.line(" */")


class TypeScript:
    def __init__(self, surface: Surface) -> None:
        self.surface = surface

    # -- types ---------------------------------------------------------------

    def type(self, kind: Type) -> str:
        """The TypeScript type of ``kind``."""
        if kind.kind == "str":
            return "string"
        if kind.kind in ("int", "float"):
            return "number"
        if kind.kind == "bool":
            return "boolean"
        if kind.kind in ("datetime", "time"):
            return "string"
        if kind.kind == "any":
            return "JsonValue"
        if kind.kind == "union":
            return " | ".join(self.type(inner) for inner in kind.of)
        if kind.kind == "opt":
            return f"{self.type(kind.inner)} | null"
        if kind.kind == "list":
            inner = self.type(kind.inner)
            return f"({inner})[]" if " " in inner else f"{inner}[]"
        if kind.kind == "map":
            if kind.inner.kind == "any":
                return "JsonObject"
            key = "number" if kind.key == "int" else "string"
            return f"Record<{key}, {self.type(kind.inner)}>"
        return kind.name

    def is_input(self, kind: Type) -> bool:
        """Whether ``kind`` is an object the caller builds, which is encoded on the way in."""
        return kind.kind == "ref" and kind.name in self.surface.structs and self.surface.structs[kind.name].role == "input"

    def note(self, item: Field) -> str:
        """What a field's comment adds to its documentation: its format and its default."""
        notes = []
        inner = item.type.inner if item.type.kind == "opt" else item.type
        if inner.kind == "datetime":
            notes.append("An ISO 8601 date and time.")
        if inner.kind == "time":
            notes.append("A time of day, `HH:MM:SS`.")
        if item.default is not MISSING and not item.required and item.default is not None:
            notes.append(f"Default `{default_text(item.default)}`.")
        return " ".join(notes)

    def shape(self, kind: Type) -> str | None:
        """The shape literal of ``kind``, or ``None`` when it holds no float."""
        if not has_float(self.surface, kind):
            return None
        if kind.kind == "float":
            return '"float"'
        if kind.kind == "opt":
            return self.shape(kind.inner)
        if kind.kind in ("list", "map"):
            return f'["{kind.kind}", {self.shape(kind.inner)}]'
        return f'["struct", "{kind.name}"]'

    # -- models.ts -----------------------------------------------------------

    def enum(self, writer: Writer, item: Enum) -> None:
        doc(writer, item.doc)
        writer.line(f"export const {item.name} = {{")
        writer.indent()
        for member in item.members:
            value = f'"{member.value}"' if isinstance(member.value, str) else str(member.value)
            writer.line(f"{member.name}: {value},")
        writer.dedent()
        writer.line("} as const;")
        doc(writer, f"A value of `{item.name}`.")
        writer.line(f"export type {item.name} = (typeof {item.name})[keyof typeof {item.name}];")
        writer.blank()

    def output(self, writer: Writer, item: Struct) -> None:
        origin = f"pyBYD's `{item.name}`, as `model_dump(mode=\"json\")` gives it." if item.pybyd else ""
        doc(writer, item.doc, origin)
        writer.line(f"export interface {item.name} {{")
        writer.indent()
        for entry in item.fields:
            text = "The reply this was parsed from, as BYD sent it." if entry.name == "raw" else entry.doc
            doc(writer, " ".join(part for part in (text, self.note(entry)) if part))
            writer.line(f"readonly {entry.name}: {self.type(entry.type)};")
        writer.dedent()
        writer.line("}")
        writer.blank()

    def input_field(self, writer: Writer, entry: Field) -> None:
        doc(writer, " ".join(part for part in (entry.doc, self.note(entry)) if part))
        if entry.required:
            writer.line(f"{camel(entry.name)}: {self.type(entry.type)};")
        else:
            writer.line(f"{camel(entry.name)}?: {self.type(entry.type)} | undefined;")

    def input(self, writer: Writer, item: Struct) -> None:
        origin = f"The fields of pyBYD's `{item.name}`, in camelCase." if item.pybyd else ""
        doc(writer, item.doc, origin)
        writer.line(f"export interface {item.name} {{")
        writer.indent()
        for entry in item.fields:
            self.input_field(writer, entry)
        writer.dedent()
        writer.line("}")
        writer.blank()

    def models(self) -> str:
        writer = Writer(INDENT)
        writer.lines(header(self.surface))
        writer.blank()
        doc(writer, "Any JSON value.")
        writer.line("export type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };")
        doc(writer, "A JSON object.")
        writer.line("export type JsonObject = { [key: string]: JsonValue };")
        writer.blank()
        for item in self.surface.enums.values():
            self.enum(writer, item)
        for item in self.surface.structs.values():
            if item.role == "output":
                self.output(writer, item)
            else:
                self.input(writer, item)
        doc(writer, "pyBYD's reply models by name: what `parseModel` returns for each.")
        writer.line("export interface Models {")
        writer.indent()
        for item in self.surface.structs.values():
            if item.pybyd and item.role == "output":
                writer.line(f"{item.name}: {item.name};")
        writer.dedent()
        writer.line("}")
        writer.blank()
        for name, (fields, methods) in self.surface.option_groups().items():
            used = ", ".join(f"`{camel(method)}`" for method in methods)
            doc(writer, f"The options of {used}.")
            writer.line(f"export interface {name} {{")
            writer.indent()
            for entry in fields:
                self.input_field(writer, entry)
            writer.dedent()
            writer.line("}")
            writer.blank()
        return writer.text()

    # -- shapes.ts -----------------------------------------------------------

    def shapes(self) -> str:
        writer = Writer(INDENT)
        writer.lines(header(self.surface))
        writer.blank()
        doc(
            writer,
            'Where the floats are inside a value the core returns: a float itself, a list or a map of '
            "shapes, or a struct by name. JSON has no NaN and no infinities, so the core writes them as "
            "strings; `revive` (wire.ts) follows a shape to turn them back into numbers.",
        )
        writer.line("export type Shape =")
        writer.indent()
        writer.line('| "float"')
        writer.line('| readonly ["list", Shape]')
        writer.line('| readonly ["map", Shape]')
        writer.line('| readonly ["struct", string];')
        writer.dedent()
        writer.blank()
        doc(writer, "The fields of each struct that hold a float, with their shapes.")
        writer.line("export const STRUCT_SHAPES: Readonly<Record<string, Readonly<Record<string, Shape>>>> = {")
        writer.indent()
        for item in self.surface.structs.values():
            fields = [(entry.name, self.shape(entry.type)) for entry in item.fields]
            fields = [(name, shape) for name, shape in fields if shape is not None]
            if item.role != "output" or not fields:
                continue
            writer.line(f"{item.name}: {{")
            writer.indent()
            for name, shape in fields:
                writer.line(f"{name}: {shape},")
            writer.dedent()
            writer.line("},")
        writer.dedent()
        writer.line("};")
        writer.blank()
        doc(writer, "The payload of each event that holds a float, by the event's name in the core.")
        writer.line("export const EVENT_SHAPES: Readonly<Record<string, Readonly<Record<string, Shape>>>> = {")
        writer.indent()
        for event in self.surface.events:
            fields = [(entry.name, self.shape(entry.type)) for entry in event.fields]
            fields = [(name, shape) for name, shape in fields if shape is not None]
            if fields:
                writer.line(f"{event.wire}: {{ " + ", ".join(f"{name}: {shape}" for name, shape in fields) + " },")
        writer.dedent()
        writer.line("};")
        writer.blank()
        doc(writer, "The core's call name of each pyBYD model, for `parseModel`.")
        writer.line("export const MODEL_CALLS = {")
        writer.indent()
        for item in self.surface.structs.values():
            if item.pybyd and item.role == "output":
                writer.line(f'{item.name}: "models.{item.pybyd}.{item.name}",')
        writer.dedent()
        writer.line("} as const;")
        return writer.text()

    # -- errors.ts -----------------------------------------------------------

    def error_default(self, entry: Field) -> str:
        if entry.type.kind == "opt":
            return "null"
        return "[]" if entry.type.kind == "list" else '""'

    def error(self, writer: Writer, item: ErrorClass) -> None:
        doc(writer, item.doc)
        parent = item.parent or "Error"
        writer.line(f"export class {item.name} extends {parent} {{")
        writer.indent()
        writer.line(f'override readonly name: string = "{item.name}";')
        for entry in item.fields:
            doc(writer, entry.doc)
            writer.line(f"readonly {camel(entry.name)}: {self.type(entry.type)};")
        if item.fields:
            writer.blank()
            writer.line("constructor(message: string, details: ErrorDetails) {")
            writer.indent()
            writer.line("super(message);" if not item.parent else "super(message, details);")
            for entry in item.fields:
                name = camel(entry.name)
                if name == "kind":
                    writer.line("this.kind = details.kind;")
                else:
                    writer.line(f"this.{name} = details.{name} ?? {self.error_default(entry)};")
            writer.dedent()
            writer.line("}")
        writer.dedent()
        writer.line("}")
        writer.blank()

    def errors(self) -> str:
        writer = Writer(INDENT)
        writer.lines(header(self.surface))
        writer.blank()
        doc(writer, "What an error is built from: its fields, whichever its class carries.")
        writer.line("export interface ErrorDetails {")
        writer.indent()
        writer.line("readonly kind: string;")
        seen = {"kind"}
        for item in self.surface.errors:
            for entry in item.fields:
                if entry.name not in seen:
                    seen.add(entry.name)
                    writer.line(f"readonly {camel(entry.name)}?: {self.type(entry.type).removesuffix(' | null')} | null | undefined;")
        writer.dedent()
        writer.line("}")
        writer.blank()
        for item in self.surface.errors:
            self.error(writer, item)
        doc(writer, "The error class for each Python exception class the core reports.")
        writer.line("export const ERROR_CLASSES: Readonly<")
        writer.line("  Record<string, new (message: string, details: ErrorDetails) => OpenBydError>")
        writer.line("> = {")
        writer.indent()
        for item in self.surface.errors:
            for python in item.python:
                writer.line(f"{python}: {item.name},")
        writer.dedent()
        writer.line("};")
        return writer.text()

    # -- methods.ts ----------------------------------------------------------

    def encoder(self, writer: Writer, item: Struct) -> None:
        doc(writer, f"`{item.name}` as the core takes it: under pyBYD's field names.", "@internal")
        writer.line(f"export function encode{item.name}(value: {item.name}): Record<string, unknown> {{")
        writer.indent()
        writer.line("return {")
        writer.indent()
        for entry in item.fields:
            name = camel(entry.name)
            inner = entry.type.inner if entry.type.kind == "opt" else entry.type
            if self.is_input(inner):
                optional = not entry.required or entry.type.kind == "opt"
                value = f"value.{name}"
                encoded = f"encode{inner.name}({value})"
                writer.line(f"{entry.name}: {value} == null ? {value} : {encoded}," if optional else f"{entry.name}: {encoded},")
            else:
                writer.line(f"{entry.name}: value.{name},")
        writer.dedent()
        writer.line("};")
        writer.dedent()
        writer.line("}")
        writer.blank()

    def returns(self, method: Method) -> str:
        return "void" if method.returns is None else self.type(method.returns)

    def signature(self, method: Method) -> str:
        params = [f"{camel(entry.name)}: {self.type(entry.type)}" for entry in method.params]
        if method.optional:
            params.append(f"options: {method.options_name} = {{}}")
        return ", ".join(params)

    def arguments(self, method: Method) -> list[str]:
        entries = []
        for entry in method.params:
            name = camel(entry.name)
            value = f"encode{entry.type.name}({name})" if self.is_input(entry.type) else name
            entries.append(entry.name if value == entry.name else f"{entry.name}: {value}")
        for entry in method.optional:
            value = f"options.{camel(entry.name)}"
            inner = entry.type.inner if entry.type.kind == "opt" else entry.type
            if self.is_input(inner):
                value = f"{value} == null ? {value} : encode{inner.name}({value})"
            entries.append(f"{entry.name}: {value}")
        return entries

    def method(self, writer: Writer, method: Method, invoker: str) -> None:
        paragraphs = [method.doc]
        for entry in method.params:
            if entry.doc:
                paragraphs.append(f"@param {camel(entry.name)} - {entry.doc}")
        doc(writer, *paragraphs)
        shape = (self.shape(method.returns) if method.returns is not None else None) or "null"
        function = f'{invoker}.{"callSync" if method.sync else "call"}'
        entries = self.arguments(method)
        returns = self.returns(method)
        keyword = "return " if returns != "void" or not method.sync else ""
        inline = "{ " + ", ".join(entries) + " }" if entries else "{}"
        one_line = f'{keyword}{function}("{method.wire}", {inline}, {shape});'
        if method.prop:
            writer.line(f"get {camel(method.name)}(): {returns} {{")
        elif method.sync:
            writer.line(f"{camel(method.name)}({self.signature(method)}): {returns} {{")
        else:
            writer.line(f"{camel(method.name)}({self.signature(method)}): Promise<{returns}> {{")
        writer.indent()
        if len(one_line) <= 96:
            writer.line(one_line)
        else:
            writer.line(f"{keyword}{function}(")
            writer.indent()
            writer.line(f'"{method.wire}",')
            writer.line("{")
            writer.indent()
            for entry in entries:
                writer.line(f"{entry},")
            writer.dedent()
            writer.line("},")
            writer.line(f"{shape},")
            writer.dedent()
            writer.line(");")
        writer.dedent()
        writer.line("}")
        writer.blank()

    def feature(self, writer: Writer, feature: Feature) -> None:
        doc(writer, f"`car.{feature.name}`: {feature.doc[0].lower()}{feature.doc[1:]}")
        writer.line(f"export class {feature.class_name} {{")
        writer.indent()
        writer.line("readonly #invoker: Invoker;")
        writer.line("readonly #features: () => CarFeatures;")
        writer.blank()
        doc(writer, "@internal")
        writer.line("constructor(invoker: Invoker, features: () => CarFeatures) {")
        writer.indent()
        writer.line("this.#invoker = invoker;")
        writer.line("this.#features = features;")
        writer.dedent()
        writer.line("}")
        writer.blank()
        for entry in feature.props:
            doc(writer, entry.doc)
            writer.line(f"get {camel(entry.name)}(): {self.type(entry.type)} {{")
            writer.indent()
            writer.line(f"return this.#features().{feature.name}.{entry.name};")
            writer.dedent()
            writer.line("}")
            writer.blank()
        if feature.name == "seat":
            doc(writer, "Whether a seat can be heated or ventilated.")
            writer.line("isAvailable(position: SeatPosition, mode: SeatMode): boolean {")
            writer.indent()
            writer.line("return this.#features().seat[position][mode];")
            writer.dedent()
            writer.line("}")
            writer.blank()
        for method in feature.methods:
            self.method(writer, method, "this.#invoker")
        writer.dedent()
        writer.line("}")
        writer.blank()

    def methods(self) -> str:
        surface = self.surface
        writer = Writer(INDENT)
        doc(
            writer,
            "Runs the core's methods by name. `shape` says where floats sit in the result (see shapes.ts).",
            "@internal",
        )
        writer.line("export interface Invoker {")
        writer.indent()
        writer.line("call<T>(method: string, args: Record<string, unknown>, shape: Shape | null): Promise<T>;")
        writer.line("callSync<T>(method: string, args: Record<string, unknown>, shape: Shape | null): T;")
        writer.dedent()
        writer.line("}")
        writer.blank()
        for item in surface.structs.values():
            if item.role == "input":
                self.encoder(writer, item)

        doc(writer, "The client's methods: pyBYD's `BydClient`. `Client` (client.ts) completes it.")
        writer.line("export abstract class ClientMethods {")
        writer.indent()
        writer.line("readonly #invoker: Invoker;")
        writer.blank()
        doc(writer, "@internal")
        writer.line("protected constructor(invoker: Invoker) {")
        writer.indent()
        writer.line("this.#invoker = invoker;")
        writer.dedent()
        writer.line("}")
        writer.blank()
        for method in surface.client:
            self.method(writer, method, "this.#invoker")
        writer.dedent()
        writer.line("}")
        writer.blank()

        for feature in surface.features:
            self.feature(writer, feature)

        doc(writer, "A car's methods: pyBYD's `BydCar`. `Car` (car.ts) completes it.")
        writer.line("export abstract class CarMethods {")
        writer.indent()
        writer.line("readonly #invoker: Invoker;")
        for feature in surface.features:
            doc(writer, feature.doc)
            writer.line(f"readonly {feature.name}: {feature.class_name};")
        writer.blank()
        doc(writer, "@internal")
        writer.line("protected constructor(invoker: Invoker) {")
        writer.indent()
        writer.line("this.#invoker = invoker;")
        writer.line("const features = (): CarFeatures => this.features;")
        for feature in surface.features:
            writer.line(f"this.{feature.name} = new {feature.class_name}(invoker, features);")
        writer.dedent()
        writer.line("}")
        writer.blank()
        for method in surface.car:
            self.method(writer, method, "this.#invoker")
        writer.dedent()
        writer.line("}")
        body = writer.text()

        top = Writer(INDENT)
        top.lines(header(surface))
        top.blank()
        names = sorted(["JsonObject", "JsonValue", *surface.structs, *surface.enums, *surface.option_groups()])
        top.line("import type {")
        top.indent()
        for name in names:
            if re.search(rf"\b{name}\b", body):
                top.line(f"{name},")
        top.dedent()
        top.line('} from "./models.js";')
        top.line('import type { Shape } from "./shapes.js";')
        return top.text() + "\n" + body

    def files(self) -> dict[str, str]:
        root = "bindings/node/src/generated"
        return {
            f"{root}/models.ts": self.models(),
            f"{root}/shapes.ts": self.shapes(),
            f"{root}/errors.ts": self.errors(),
            f"{root}/methods.ts": self.methods(),
        }


def files(surface: Surface) -> dict[str, str]:
    """Every generated file of the npm package, by path from the repository's root."""
    return TypeScript(surface).files()
