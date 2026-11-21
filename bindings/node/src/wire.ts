/**
 * What crosses between TypeScript and the core: JSON text, and errors.
 *
 * @internal
 */

import { DataError, ERROR_CLASSES, OpenBydError } from "./generated/errors.js";
import { STRUCT_SHAPES, type Shape } from "./generated/shapes.js";
import { native } from "./native.js";

/** An error as the core reports it. */
interface ErrorJson {
  class: string;
  bases: string[];
  message: string;
  code: string | null;
  endpoint: string | null;
  status_code: number | null;
  error_type: string | null;
  loc: string[] | null;
}

/** The error class for what the core reported: its own, or the nearest parent's this package knows. */
function errorFromJson(error: ErrorJson): OpenBydError {
  const known = [error.class, ...error.bases].map((name) => ERROR_CLASSES[name]).find((cls) => cls !== undefined);
  return new (known ?? OpenBydError)(error.message, {
    kind: error.class,
    code: error.code,
    endpoint: error.endpoint,
    statusCode: error.status_code,
    errorType: error.error_type,
    loc: error.loc,
  });
}

/** What the add-on threw as this package's error; anything else unchanged. */
export function translate(thrown: unknown): unknown {
  if (thrown instanceof Error && thrown.message.startsWith(native.ERROR_PREFIX)) {
    try {
      return errorFromJson(JSON.parse(thrown.message.slice(native.ERROR_PREFIX.length)) as ErrorJson);
    } catch {
      return thrown;
    }
  }
  return thrown;
}

/**
 * An error a callback threw, as the core takes it back: an openbyd error
 * keeps its class and fields, anything else is named by its own class.
 */
export function errorToJson(error: unknown): Record<string, unknown> {
  if (error instanceof OpenBydError) {
    const fields = error as OpenBydError & { code?: string; endpoint?: string; statusCode?: number | null };
    return {
      class: error.kind,
      message: error.message,
      code: fields.code,
      endpoint: fields.endpoint,
      status_code: fields.statusCode,
    };
  }
  if (error instanceof Error) return { class: error.name, message: error.message };
  return { class: "Error", message: String(error) };
}

const NON_FINITE: Readonly<Record<string, number>> = { NaN: NaN, Infinity: Infinity, "-Infinity": -Infinity };

/**
 * Turns the strings JSON needs for NaN and the infinities back into numbers,
 * in place, wherever `shape` says a float is.
 */
export function revive(value: unknown, shape: Shape): unknown {
  if (value === null || value === undefined) return value;
  if (shape === "float") {
    return typeof value === "string" ? (NON_FINITE[value] ?? value) : value;
  }
  const [kind, inner] = shape;
  if (kind === "list") {
    const items = value as unknown[];
    for (let index = 0; index < items.length; index++) items[index] = revive(items[index], inner);
  } else if (kind === "map") {
    const entries = value as Record<string, unknown>;
    for (const key of Object.keys(entries)) entries[key] = revive(entries[key], inner);
  } else {
    reviveFields(value as Record<string, unknown>, STRUCT_SHAPES[inner]);
  }
  return value;
}

/** {@link revive} for an object whose fields have these shapes. */
export function reviveFields(
  value: Record<string, unknown>,
  fields: Readonly<Record<string, Shape>> | undefined,
): void {
  if (fields === undefined) return;
  for (const [field, shape] of Object.entries(fields)) {
    if (field in value) value[field] = revive(value[field], shape);
  }
}

/** Whether JSON text may hold a float written as a string. */
export function mayHoldNonFinite(text: string): boolean {
  return text.includes('"NaN"') || text.includes('Infinity"');
}

/** The core's JSON text as a value, with the floats `shape` describes read back as numbers. */
export function decode<T>(text: string, shape: Shape | null): T {
  const value: unknown = JSON.parse(text);
  if (shape !== null && mayHoldNonFinite(text)) return revive(value, shape) as T;
  return value as T;
}

/**
 * Arguments as JSON text. A field that is `undefined` is left out: not
 * given. A number JSON cannot hold (NaN, an infinity) is refused, as pyBYD
 * refuses it, instead of silently becoming "not given".
 */
export function encode(args: Record<string, unknown>): string {
  return JSON.stringify(args, (key, value: unknown) => {
    if (typeof value === "number" && !Number.isFinite(value)) {
      throw new DataError(`${key} must be a finite number, not ${value}`, { kind: "ValueError" });
    }
    return value;
  });
}

/** Runs a function of the core that needs no client. */
export function callFunction<T>(name: string, args: Record<string, unknown> = {}): T {
  try {
    return JSON.parse(native.call(name, encode(args))) as T;
  } catch (error) {
    throw translate(error);
  }
}
