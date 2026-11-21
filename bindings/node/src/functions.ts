/** What needs no client: the envelope cipher, pyBYD's models by name, the runtime's settings. */

import type { JsonObject, Models } from "./generated/models.js";
import { MODEL_CALLS } from "./generated/shapes.js";
import { callFunction, revive } from "./wire.js";

/** The version of openbyd's core. */
export function version(): string {
  return callFunction<string>("version");
}

/** Wraps plaintext (text, or bytes) in a Bangcle envelope, as BYD's app does with every request. */
export function encodeEnvelope(plaintext: string | Uint8Array): string {
  if (typeof plaintext === "string") return callFunction<string>("encode_envelope", { text: plaintext });
  return callFunction<string>("encode_envelope", { hex: Buffer.from(plaintext).toString("hex") });
}

/** Opens a Bangcle envelope and returns the plaintext bytes. Throws `BangcleError` for a bad envelope. */
export function decodeEnvelope(envelope: string): Uint8Array {
  const { hex } = callFunction<{ hex: string }>("decode_envelope", { envelope });
  return Buffer.from(hex, "hex");
}

/**
 * Parses a raw payload (a reply's `respondData`, an MQTT event's data) as
 * one of pyBYD's reply models: `Model.model_validate(raw).model_dump(mode="json")`.
 * Throws `ValidationError` when it does not validate.
 *
 * @param context - pyBYD's validation context, for the models that take one.
 */
export function parseModel<Name extends keyof Models>(
  name: Name,
  raw: JsonObject,
  context?: JsonObject,
): Models[Name] {
  const parsed = callFunction<{ dump: Models[Name] }>("parse_model", { name: MODEL_CALLS[name], raw, context });
  return revive(parsed.dump, ["struct", name]) as Models[Name];
}

/** The state of the Rust runtime every client shares. */
export interface RuntimeInfo {
  /** The runtime's worker threads. */
  readonly worker_threads: number;
  /** Whether the runtime has started: after the first client or call. */
  readonly started: boolean;
}

/** The state of the Rust runtime every client shares. */
export function runtimeInfo(): RuntimeInfo {
  return callFunction<RuntimeInfo>("runtime");
}

/**
 * Sets the worker threads of the Rust runtime (2 by default). Only before
 * the runtime starts, which the first client does.
 */
export function configureRuntime(options: { workerThreads: number }): void {
  callFunction<null>("configure", { worker_threads: options.workerThreads });
}
