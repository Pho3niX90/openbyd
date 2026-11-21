/**
 * The recorded pyBYD vectors, through this package.
 *
 * - Every model dump pyBYD produced conforms to the type this package
 *   publishes for it (`bindings/surface.json`, which the generated
 *   interfaces are written from).
 * - `parseModel` returns that dump.
 * - Every vector JavaScript can carry without changing it gives pyBYD's
 *   answer through the add-on.
 */

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";

import { OpenBydError, parseModel, type JsonObject, type JsonValue, type Models } from "openbyd";
import { parityCall } from "openbyd/testing";

const ROOT = join(__dirname, "..", "..", "..", "..");

// -- the surface, as data ---------------------------------------------------

type TypeJson =
  | string
  | { ref: string }
  | { opt: TypeJson }
  | { list: TypeJson }
  | { map: TypeJson; key: string }
  | { union: TypeJson[] };

interface Surface {
  structs: { name: string; role: string; pybyd: string; fields: { name: string; type: TypeJson }[] }[];
  enums: { name: string; members: Record<string, string | number> }[];
}

const surface = JSON.parse(readFileSync(join(ROOT, "bindings", "surface.json"), "utf8")) as Surface;
const structs = new Map(surface.structs.map((item) => [item.name, item]));
const enums = new Map(surface.enums.map((item) => [item.name, new Set(Object.values(item.members))]));

const NON_FINITE = new Set(["NaN", "Infinity", "-Infinity"]);

/** Why `value` is not a `type`, or `null` when it is. */
function mismatch(value: unknown, type: TypeJson, path: string): string | null {
  const wrong = (expected: string): string => `${path}: ${JSON.stringify(value)} is not ${expected}`;
  if (typeof type === "string") {
    switch (type) {
      case "str":
      case "datetime":
      case "time":
        return typeof value === "string" ? null : wrong(type);
      case "int":
        return typeof value === "number" && Number.isInteger(value) ? null : wrong("an int");
      case "float":
        // In the core's JSON a float that JSON cannot hold is one of three strings.
        return typeof value === "number" || (typeof value === "string" && NON_FINITE.has(value))
          ? null
          : wrong("a float");
      case "bool":
        return typeof value === "boolean" ? null : wrong("a bool");
      default:
        return null; // any
    }
  }
  if ("opt" in type) return value === null ? null : mismatch(value, type.opt, path);
  if ("union" in type) {
    return type.union.some((member) => mismatch(value, member, path) === null) ? null : wrong("in the union");
  }
  if ("list" in type) {
    if (!Array.isArray(value)) return wrong("a list");
    for (const [index, item] of value.entries()) {
      const problem = mismatch(item, type.list, `${path}[${index}]`);
      if (problem !== null) return problem;
    }
    return null;
  }
  if ("ref" in type) {
    const members = enums.get(type.ref);
    if (members !== undefined) return members.has(value as string | number) ? null : wrong(`a ${type.ref}`);
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) return wrong("an object");
  const object = value as Record<string, unknown>;
  if ("map" in type) {
    for (const [key, item] of Object.entries(object)) {
      if (type.key === "int" && !/^-?\d+$/.test(key)) return `${path}: key ${key} is not an int`;
      const problem = mismatch(item, type.map, `${path}.${key}`);
      if (problem !== null) return problem;
    }
    return null;
  }
  const struct = structs.get(type.ref);
  if (struct === undefined) return `${path}: no struct ${type.ref}`;
  // An output has every field, in pyBYD's order, and no other.
  const expected = struct.fields.map((field) => field.name);
  if (JSON.stringify(Object.keys(object)) !== JSON.stringify(expected)) {
    return `${path}: fields ${Object.keys(object).join(",")} are not ${expected.join(",")}`;
  }
  for (const field of struct.fields) {
    const problem = mismatch(object[field.name], field.type, `${path}.${field.name}`);
    if (problem !== null) return problem;
  }
  return null;
}

// -- the vectors ------------------------------------------------------------

interface Case {
  call: string;
  args: JsonValue;
  ok?: JsonValue;
  error?: string;
}

/** A number of a vector's text that JavaScript would write back differently. */
class Lossy {
  constructor(readonly value: number) {}
}

/**
 * A vector file with every number JavaScript cannot carry unchanged marked
 * {@link Lossy}: an integer too large for a double, or a float written with
 * a whole value (`1.0`), which JavaScript writes back as an integer.
 */
function load(path: string): { cases?: Case[] } {
  const reviver = function (this: unknown, _key: string, value: unknown, context?: { source?: string }): unknown {
    if (typeof value !== "number" || context?.source === undefined) return value;
    const source = context.source;
    const whole = /^-?\d+$/.test(source);
    const kept = whole ? Number.isSafeInteger(value) && source !== "-0" : !Number.isInteger(value);
    return kept ? value : new Lossy(value);
  };
  return JSON.parse(readFileSync(path, "utf8"), reviver as (key: string, value: unknown) => unknown) as { cases?: Case[] };
}

/** Whether JavaScript would change `value` on its way to the core: a marked number, or keys it reorders. */
function changes(value: unknown): boolean {
  if (value instanceof Lossy) return true;
  if (Array.isArray(value)) return value.some(changes);
  if (value !== null && typeof value === "object") {
    return Object.entries(value).some(([key, item]) => /^(0|[1-9]\d*)$/.test(key) || changes(item));
  }
  return false;
}

/** `value` with its marked numbers as plain ones, for comparing. */
function plain(value: unknown): unknown {
  if (value instanceof Lossy) return value.value;
  if (Array.isArray(value)) return value.map(plain);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, plain(item)]));
  }
  return value;
}

/** `value` as the vectors record it: NaN and the infinities as their three strings. */
function asRecorded(value: unknown, found: () => void): unknown {
  if (typeof value === "number" && !Number.isFinite(value)) {
    found();
    return Number.isNaN(value) ? "NaN" : value > 0 ? "Infinity" : "-Infinity";
  }
  if (Array.isArray(value)) return value.map((item) => asRecorded(item, found));
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, asRecorded(item, found)]));
  }
  return value;
}

function vectorFiles(): string[] {
  const files: string[] = [];
  for (const crate of ["openbyd-protocol", "openbyd-client"]) {
    const directory = join(ROOT, "crates", crate, "tests", "vectors");
    for (const name of readdirSync(directory).sort()) {
      if (name.endsWith(".json")) files.push(join(directory, name));
    }
  }
  return files;
}

const files = vectorFiles().map((path) => ({ path, cases: load(path).cases ?? [] }));

test("there are vectors to check", () => {
  assert.ok(files.length >= 40, `${files.length} vector files`);
});

test("every model dump pyBYD recorded conforms to its published type", () => {
  const models = new Map<string, string>();
  for (const struct of surface.structs) {
    if (struct.pybyd !== "" && struct.role === "output") models.set(`models.${struct.pybyd}.${struct.name}`, struct.name);
  }
  const seen = new Map<string, number>();
  const problems: string[] = [];
  for (const { cases } of files) {
    for (const item of cases) {
      const name = models.get(item.call);
      const dump = (plain(item.ok) as { dump?: unknown } | undefined)?.dump;
      if (name === undefined || dump === undefined) continue;
      seen.set(name, (seen.get(name) ?? 0) + 1);
      const problem = mismatch(dump, { ref: name }, name);
      if (problem !== null && problems.length < 20) problems.push(problem);
    }
  }
  assert.deepEqual(problems, []);
  const unchecked = [...models.values()].filter((name) => !seen.has(name));
  assert.deepEqual(unchecked, [], "models without a recorded dump");
  const total = [...seen.values()].reduce((sum, count) => sum + count, 0);
  assert.ok(total > 10_000, `${total} dumps checked`);
});

test("parseModel returns pyBYD's dump, with floats as numbers", () => {
  const names = new Map<string, keyof Models>();
  for (const struct of surface.structs) {
    if (struct.pybyd !== "" && struct.role === "output") {
      names.set(`models.${struct.pybyd}.${struct.name}`, struct.name as keyof Models);
    }
  }
  let parsed = 0;
  let revived = 0;
  for (const { cases } of files) {
    for (const item of cases) {
      const name = names.get(item.call);
      if (name === undefined || changes(item.args)) continue;
      const { raw, context } = item.args as { raw: JsonValue; context: JsonValue };
      if (raw === null || typeof raw !== "object" || Array.isArray(raw)) continue;
      let dump: unknown;
      try {
        dump = parseModel(name, raw, (context as JsonObject | null) ?? undefined);
      } catch (error) {
        assert.ok(error instanceof OpenBydError, `${item.call}: ${String(error)}`);
        assert.equal(error.kind, item.error, item.call);
        continue;
      }
      // The recorded dump writes NaN and the infinities as strings; here they are numbers.
      const expected = (plain(item.ok) as { dump: unknown }).dump;
      const withStrings = asRecorded(dump, () => (revived += 1));
      assert.deepEqual(withStrings, expected, item.call);
      parsed += 1;
    }
  }
  assert.ok(parsed > 5_000, `${parsed} models parsed`);
  assert.ok(revived > 0, "no vector had a float JSON cannot hold");
});

test("every vector JavaScript can carry gives pyBYD's answer", () => {
  let ran = 0;
  let skipped = 0;
  const problems: string[] = [];
  for (const { path, cases } of files) {
    for (const [index, item] of cases.entries()) {
      if (changes(item.args)) {
        skipped += 1;
        continue;
      }
      ran += 1;
      const got = parityCall(item.call, item.args);
      let problem: string | null = null;
      if (item.error !== undefined) {
        if (!("error" in got) || got.error.kind !== item.error) problem = `expected error ${item.error}`;
      } else if ("error" in got) {
        problem = `got error ${got.error.kind}: ${got.error.message}`;
      } else {
        try {
          assert.deepEqual(got.ok, plain(item.ok));
        } catch {
          problem = "the answer differs";
        }
      }
      if (problem !== null && problems.length < 20) problems.push(`${path} #${index} ${item.call}: ${problem}`);
    }
  }
  assert.deepEqual(problems, []);
  assert.ok(ran > 20_000, `${ran} vectors ran, ${skipped} skipped`);
  console.log(`${ran} vectors ran through the add-on; ${skipped} need numbers or keys JavaScript cannot carry`);
});
