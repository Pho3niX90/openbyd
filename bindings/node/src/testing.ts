/**
 * A fake BYD server and an MQTT test broker, for tests: `import { FakeServer } from "openbyd/testing"`.
 *
 * They need an add-on built with the feature `testing` (`npm run
 * build:native -- --testing`); released add-ons do not have it, and
 * {@link hasTesting} tells which one is loaded.
 *
 * ```ts
 * using server = new FakeServer([{ username: "user@example.com", password: "secret" }]);
 * server.reply("/control/getStatusNow", { data: { acSwitch: 1 } });
 * const client = new Client({ username: "user@example.com", password: "secret", baseUrl: server.url });
 * ```
 */

import { TEST_TLS } from "./client.js";
import type { JsonObject, JsonValue, SessionTokens } from "./generated/models.js";
import { native } from "./native.js";
import { callFunction, encode, translate } from "./wire.js";

export { TEST_TLS };

/** Whether the add-on loaded has the fake server and the test broker. */
export const hasTesting: boolean = callFunction<boolean>("has_testing");

function testing<T>(name: string, args: Record<string, unknown> = {}): T {
  try {
    return JSON.parse(native.testingCall(name, encode(args))) as T;
  } catch (error) {
    throw translate(error);
  }
}

/** An account the fake server knows. */
export interface Account {
  username: string;
  password: string;
  /** The account's user ID; made up when not given. */
  userId?: string | undefined;
  /** The VINs the account reaches as a guest. */
  guestVins?: string[] | undefined;
  /** Whether the server insists on the `userType` a guest's requests carry. Default `true`. */
  userTypeRule?: boolean | undefined;
}

/** What the fake server answers to one request. */
export type Reply = (
  | { /** Success: code `0` with this as the encrypted `respondData`. */ data: JsonValue }
  | { /** An API error with this code. */ code: string; message?: string | undefined }
  | { /** This outer reply as it is, inside the envelope only. */ outer: JsonValue }
  | { /** An HTTP status other than 200, with this body. */ status: number; body?: string | undefined }
  | { /** Closes the connection without answering. */ drop: true }
  | { /** What the server answers without a handler. */ default: true }
) & {
  /** Answer after this many seconds. */
  delay?: number | undefined;
  /** Add this `Set-Cookie` header. */
  cookie?: string | undefined;
};

/** One request the fake server received, decoded. */
export interface RecordedRequest {
  readonly endpoint: string;
  readonly headers: [string, string][];
  readonly cookies: [string, string][];
  readonly body: string;
  /** The outer payload, when the envelope opened. */
  readonly outer: JsonObject | null;
  /** The decrypted inner payload, when it decrypted. */
  readonly inner: JsonValue;
  /** The account the request was verified as. */
  readonly username: string | null;
  /** The API code answered; null for an HTTP-level reply. */
  readonly code: string | null;
  /** The HTTP status answered; null when the connection was dropped. */
  readonly status: number | null;
}

function accountArgs(account: Account): Record<string, unknown> {
  return {
    username: account.username,
    password: account.password,
    user_id: account.userId,
    guest_vins: account.guestVins,
    user_type_rule: account.userTypeRule,
  };
}

/** A fake BYD server on 127.0.0.1 that speaks the real wire protocol. */
export class FakeServer {
  readonly #server: number;
  /** The server's base URL, for a client's `baseUrl`. */
  readonly url: string;

  constructor(accounts: Account[] = []) {
    const started = testing<{ server: number; url: string }>("server.start", {
      accounts: accounts.map(accountArgs),
    });
    this.#server = started.server;
    this.url = started.url;
  }

  addAccount(account: Account): void {
    testing("server.add_account", { server: this.#server, ...accountArgs(account) });
  }

  /** Answers every request to `endpoint` with `reply`. */
  reply(endpoint: string, reply: Reply): void {
    testing("server.reply", { server: this.#server, endpoint, reply });
  }

  /** Answers the next requests to `endpoint` with `replies`, one each. */
  script(endpoint: string, replies: Reply[]): void {
    testing("server.script", { server: this.#server, endpoint, replies });
  }

  /** The requests received, to one endpoint or to all. */
  requests(endpoint?: string): RecordedRequest[] {
    return testing("server.requests", { server: this.#server, endpoint });
  }

  clearRequests(): void {
    testing("server.clear_requests", { server: this.#server });
  }

  /** Makes the server refuse every session of an account, as an expired one. */
  expireSessions(username: string): void {
    testing("server.expire_sessions", { server: this.#server, username });
  }

  userId(username: string): string | null {
    return testing("server.user_id", { server: this.#server, username });
  }

  stop(): void {
    testing("server.stop", { server: this.#server });
  }

  [Symbol.dispose](): void {
    this.stop();
  }
}

/** A TLS MQTT broker on 127.0.0.1, which clients made with {@link TEST_TLS} trust. */
export class TestBroker {
  readonly #broker: number;
  /** The address as BYD's broker lookup gives it: `{ data: { emqBorker: broker.address } }`. */
  readonly address: string;
  readonly port: number;

  constructor() {
    const started = testing<{ broker: number; address: string; port: number }>("broker.start");
    this.#broker = started.broker;
    this.address = started.address;
    this.port = started.port;
  }

  /**
   * Sends `body` to the account of `session` as BYD does: on its topic,
   * encrypted with the session's content key. Returns how many connections
   * it reached.
   */
  push(session: Pick<SessionTokens, "user_id" | "encry_token">, body: JsonValue): number {
    return testing("broker.push", { broker: this.#broker, session, body });
  }

  /** Connections subscribed to `topic`. */
  subscribers(topic: string): number {
    return testing("broker.subscribers", { broker: this.#broker, topic });
  }

  /** Connections still open on the broker's side. */
  get open(): number {
    return testing("broker.open", { broker: this.#broker });
  }

  /** How many connection attempts the broker has seen. */
  get connects(): number {
    return testing("broker.connects", { broker: this.#broker });
  }

  /** Refuses the next `count` connection attempts. */
  refuseNext(count: number): void {
    testing("broker.refuse_next", { broker: this.#broker, count });
  }

  /** Drops every connection. */
  dropAll(): void {
    testing("broker.drop_all", { broker: this.#broker });
  }

  stop(): void {
    testing("broker.stop", { broker: this.#broker });
  }

  [Symbol.dispose](): void {
    this.stop();
  }
}

/** The outcome of a parity call: pyBYD's answer for the same input is recorded in the repository's vectors. */
export type ParityOutcome = { ok: JsonValue } | { error: { kind: string; message: string } };

/**
 * Runs one parity call of the core, the hook the repository's tests compare
 * openbyd with pyBYD through. Not part of the public API.
 */
export function parityCall(name: string, args: JsonValue): ParityOutcome {
  return callFunction<ParityOutcome>("parity_call", { name, args });
}
