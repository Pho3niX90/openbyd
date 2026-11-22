/**
 * The client: pyBYD's `BydClient` with a Rust core.
 *
 * ```ts
 * import { Client } from "openbyd";
 *
 * const client = new Client({ username: "user@example.com", password: "secret", controlPin: "123456" });
 * try {
 *   await client.login();
 *   for (const vehicle of await client.getVehicles()) {
 *     const realtime = await client.getVehicleRealtime(vehicle.vin);
 *     console.log(vehicle.vin, realtime.elec_percent);
 *   }
 * } finally {
 *   client.close();
 * }
 * ```
 *
 * Methods are pyBYD's, in camelCase. What they return is exactly pyBYD's
 * `model_dump(mode="json")`, so fields keep pyBYD's snake_case names; what
 * you pass (the config, options, parameter objects) is camelCase. Failures
 * are thrown as pyBYD's exception classes (`BydApiError` and its subclasses,
 * with `code` and `endpoint`).
 *
 * Every client in a process shares one HTTP connection pool and one Rust
 * runtime, so a client is cheap: thousands can be created.
 *
 * Callbacks always run on JavaScript's thread. One that throws is reported
 * to `onCallbackError` (by default as a process warning) and does not affect
 * the client.
 */

import { CarMethods, ClientMethods, encodeBydConfig, type Invoker } from "./generated/methods.js";
import type {
  BydConfig,
  CommandAckEvent,
  CommandGateVerdict,
  CommandLifecycleEvent,
  JsonObject,
  RequestRecord,
  SessionTokens,
  VehicleRealtimeData,
  VehicleSnapshot,
} from "./generated/models.js";
import { EVENT_SHAPES, type Shape } from "./generated/shapes.js";
import { native, type NativeClient } from "./native.js";
import { decode, encode, errorToJson, mayHoldNonFinite, reviveFields, translate } from "./wire.js";

/**
 * Whether a command is sent, given the command gate's verdict:
 *
 * - `"strict"` (the default, pyBYD's): only when the gate says supported.
 * - `"allow_missing_data"`: also when the car's capability data is
 *   incomplete (the reasons `function_no_missing` and `learn_info_missing`).
 * - a function deciding from the verdict. If it throws, the verdict stands.
 */
export type GatePolicy =
  | "strict"
  | "allow_missing_data"
  | ((vin: string, verdict: CommandGateVerdict) => boolean | Promise<boolean>);

/**
 * Returns the `commandPwd` to send (empty for none) for the PIN a call was
 * given and the config's `controlPin`, in place of pyBYD's rule (32 hex
 * digits are a digest, anything else is hashed).
 */
export type PinResolver = (commandPwd: string | null, controlPin: string | null) => string | Promise<string>;

/**
 * What to do when a new session is needed:
 *
 * - `"password"` (the default, pyBYD's): log in again.
 * - `"never"`: fail with `BydSessionExpiredError` (BYD's own expiry code, or `relogin_disabled` when there was no session).
 * - a function returning a session (as `exportSession` gives it). What it
 *   throws is what the call throws.
 */
export type ReloginPolicy = "password" | "never" | (() => SessionTokens | Promise<SessionTokens>);

/** The options of a client besides pyBYD's config. */
export interface ClientHooks {
  /** Every MQTT `vehicleInfo` push, parsed. */
  onVehicleInfo?: ((vin: string, data: VehicleRealtimeData) => void) | undefined;
  /** Every MQTT event that names a VIN, with its raw `respondData`. */
  onMqttEvent?: ((event: string, vin: string, data: JsonObject) => void) | undefined;
  /** Every remote-control acknowledgement. */
  onCommandAck?: ((ack: CommandAckEvent) => void) | undefined;
  /** A command was registered, matched, expired or acknowledged without a match. */
  onCommandLifecycle?: ((event: CommandLifecycleEvent) => void) | undefined;
  /** Every accepted MQTT connection. */
  onMqttConnect?: (() => void) | undefined;
  /** Every request the client sends, with its outcome and duration. */
  requestObserver?: ((record: RequestRecord) => void) | undefined;
  /** How long a command waits for its acknowledgement, in seconds. Default `300`. */
  commandAckTtlSeconds?: number | undefined;
  /** Attempts per remote command. Default `1`; more retry a command the cloud could not confirm, never one the car refused. */
  commandRetries?: number | undefined;
  /** Whether a command is sent, given the command gate's verdict. Default `"strict"`. */
  gatePolicy?: GatePolicy | undefined;
  /** Replaces pyBYD's rule for the `commandPwd` sent. */
  pinResolver?: PinResolver | undefined;
  /** What to do when a new session is needed. Default `"password"`. */
  relogin?: ReloginPolicy | undefined;
  /**
   * Hears about a callback that threw: its option's name and what it threw.
   * By default the failure is emitted as a process warning.
   */
  onCallbackError?: ((callback: string, error: unknown) => void) | undefined;
}

/** What a client is made from: pyBYD's `BydConfig` fields in camelCase, and openbyd's options. */
export interface ClientOptions extends BydConfig, ClientHooks {}

/** The options of {@link Client.getCar}. */
export interface GetCarOptions {
  /** The vehicle's raw payload; the vehicle list is fetched when not given. */
  vehicle?: JsonObject | undefined;
  /** How long a command's projections stay active, in seconds. Default `30`. */
  projectionTtl?: number | undefined;
  /** Called after every change of the car's state. */
  onStateChanged?: ((vin: string, state: VehicleSnapshot) => void) | undefined;
}

/**
 * Makes a client trust the certificate of the MQTT test broker. For tests,
 * with an add-on built with the feature `testing`: `{ ...options, [TEST_TLS]: true }`.
 *
 * @internal
 */
export const TEST_TLS: unique symbol = Symbol.for("openbyd.testTls");

type StateListener = (vin: string, state: VehicleSnapshot) => void;

/** How long the timer that keeps a listening client's process alive sleeps: the longest Node allows. */
const KEEP_ALIVE_MS = 0x7fffffff;

/** A client's connection to the core: calls out, events in. */
class Core implements Invoker {
  readonly #native: NativeClient;
  readonly #options: ClientOptions;
  readonly #listens: boolean;
  /** The state listener of each car, by VIN. */
  readonly stateListeners = new Map<string, StateListener>();
  #keepAlive: NodeJS.Timeout | undefined;
  #pending = 0;

  constructor(options: ClientOptions) {
    this.#options = options;
    const events: string[] = [];
    if (options.onVehicleInfo) events.push("vehicle_info");
    if (options.onMqttEvent) events.push("mqtt_event");
    if (options.onCommandAck) events.push("command_ack");
    if (options.onCommandLifecycle) events.push("command_lifecycle");
    if (options.onMqttConnect) events.push("mqtt_connect");
    this.#listens = events.length > 0;
    if (options.requestObserver) events.push("request");
    const hostOptions = {
      events,
      command_ack_ttl_seconds: options.commandAckTtlSeconds,
      command_retries: options.commandRetries,
      gate_policy: typeof options.gatePolicy === "function" ? "host" : options.gatePolicy,
      pin_resolver: options.pinResolver ? "host" : undefined,
      relogin: typeof options.relogin === "function" ? "host" : options.relogin,
      test_tls: (options as { [TEST_TLS]?: boolean })[TEST_TLS] === true ? true : undefined,
    };
    // The add-on keeps this function for as long as its client lives, so it
    // must not hold the client: a client nobody refers to is collected, and
    // the add-on's client with it.
    let target: WeakRef<Core> | undefined;
    const onEvent = (name: string, payload: string): void => target?.deref()?.dispatch(name, payload);
    try {
      this.#native = new native.NativeClient(encode(encodeBydConfig(options)), encode(hostOptions), onEvent);
    } catch (error) {
      throw translate(error);
    }
    target = new WeakRef(this);
  }

  call<T>(method: string, args: Record<string, unknown>, shape: Shape | null): Promise<T> {
    this.#pending += 1;
    // The call's promise refers to this client until it settles, so a client
    // is not collected while it has a call in flight (its callbacks may be needed).
    return this.#native.call(method, encode(args)).then(
      (text) => {
        this.#pending -= 1;
        return decode<T>(text, shape);
      },
      (error: unknown) => {
        this.#pending -= 1;
        throw translate(error);
      },
    );
  }

  callSync<T>(method: string, args: Record<string, unknown>, shape: Shape | null): T {
    try {
      return decode<T>(this.#native.callSync(method, encode(args)), shape);
    } catch (error) {
      throw translate(error);
    }
  }

  /** The invoker of the car numbered `car`: every call names it. */
  forCar(car: number): Invoker {
    return {
      call: (method, args, shape) => this.call(method, { ...args, car }, shape),
      callSync: (method, args, shape) => this.callSync(method, { ...args, car }, shape),
    };
  }

  /** Calls in flight. */
  get pending(): number {
    return this.#pending;
  }

  start(): void {
    this.callSync("start", {}, null);
    if (this.#listens) this.hold();
  }

  close(): void {
    this.stateListeners.clear();
    this.release();
    this.callSync("close", {}, null);
  }

  /**
   * Keeps the process alive until {@link release}, as an open socket would:
   * a client with push listeners is waiting for pushes. The timer also
   * refers to the client, so it is not collected while it listens.
   */
  hold(): void {
    this.#keepAlive ??= setInterval(() => void this, KEEP_ALIVE_MS);
  }

  release(): void {
    clearInterval(this.#keepAlive);
    this.#keepAlive = undefined;
  }

  dispatch(name: string, payload: string): void {
    const options = this.#options;
    const data = JSON.parse(payload) as Record<string, unknown>;
    if (mayHoldNonFinite(payload)) reviveFields(data, EVENT_SHAPES[name]);
    switch (name) {
      case "vehicle_info":
        this.#notify("onVehicleInfo", () =>
          options.onVehicleInfo?.(data.vin as string, data.data as VehicleRealtimeData),
        );
        break;
      case "mqtt_event":
        this.#notify("onMqttEvent", () =>
          options.onMqttEvent?.(data.event as string, data.vin as string, data.data as JsonObject),
        );
        break;
      case "command_ack":
        this.#notify("onCommandAck", () => options.onCommandAck?.(data.ack as CommandAckEvent));
        break;
      case "command_lifecycle":
        this.#notify("onCommandLifecycle", () =>
          options.onCommandLifecycle?.(data.event as CommandLifecycleEvent),
        );
        break;
      case "mqtt_connect":
        this.#notify("onMqttConnect", () => options.onMqttConnect?.());
        break;
      case "request":
        this.#notify("requestObserver", () => options.requestObserver?.(data.record as RequestRecord));
        break;
      case "state_changed": {
        const vin = data.vin as string;
        this.#notify("onStateChanged", () => this.stateListeners.get(vin)?.(vin, data.state as VehicleSnapshot));
        break;
      }
      case "gate_policy": {
        const policy = options.gatePolicy;
        if (typeof policy !== "function") break;
        this.#answer(data.request as number, "gatePolicy", true, () =>
          policy(data.vin as string, data.verdict as CommandGateVerdict),
        );
        break;
      }
      case "pin_resolver": {
        const resolver = options.pinResolver;
        if (resolver === undefined) break;
        this.#answer(data.request as number, "pinResolver", true, () =>
          resolver(data.command_pwd as string | null, data.control_pin as string | null),
        );
        break;
      }
      case "relogin": {
        const relogin = options.relogin;
        if (typeof relogin !== "function") break;
        // What it throws is the failure of the call that needed the session, not a stray error.
        this.#answer(data.request as number, "relogin", false, relogin);
        break;
      }
      default:
        // An event of a newer core.
        break;
    }
  }

  /** Runs a notification callback; what it throws, now or later, is reported. */
  #notify(name: string, run: () => unknown): void {
    try {
      const result = run();
      if (result instanceof Promise) result.catch((error: unknown) => this.#report(name, error));
    } catch (error) {
      this.#report(name, error);
    }
  }

  /** Runs a callback the core waits for, and hands its answer or its failure back. */
  #answer(request: number, name: string, report: boolean, run: () => unknown): void {
    Promise.resolve()
      .then(run)
      .then(
        (value) => this.#resolve({ request, value }),
        (error: unknown) => {
          if (report) this.#report(name, error);
          this.#resolve({ request, error: errorToJson(error) });
        },
      );
  }

  #resolve(answer: Record<string, unknown>): void {
    try {
      this.#native.callSync("resolve", encode(answer));
    } catch {
      // The question is no longer waiting.
    }
  }

  #report(name: string, error: unknown): void {
    const handler = this.#options.onCallbackError;
    if (handler !== undefined) {
      try {
        handler(name, error);
      } catch {
        // A failing error handler has nowhere to report to.
      }
      return;
    }
    const message = error instanceof Error ? error.message : String(error);
    const warning = new Error(`the ${name} callback threw: ${message}`, { cause: error });
    warning.name = "OpenBydCallbackWarning";
    process.emitWarning(warning);
  }
}

/**
 * One vehicle: pyBYD's `BydCar`. Get one from {@link Client.getCar}.
 *
 * It holds the vehicle's merged state and offers its commands by feature:
 *
 * ```ts
 * const car = await client.getCar(vin);
 * await car.updateRealtime();
 * await car.lock.lock();
 * await car.hvac.start({ temperature: 21, duration: 20 });
 * car.state.realtime?.left_front_door_lock; // 2 (locked), projected
 * ```
 *
 * A command lays the state it expects over the current state at once (a
 * projection) and rolls it back when it fails; the next push or update that
 * agrees confirms it. Each feature tells whether the car supports it; a
 * command the car does not support throws `BydEndpointNotSupportedError`
 * without sending anything. A car whose client is closed fails its calls
 * with `BydError`.
 */
export class Car extends CarMethods {
  /** The vehicle identification number. */
  readonly vin: string;

  /** @internal */
  constructor(invoker: Invoker, vin: string) {
    super(invoker);
    this.vin = vin;
  }
}

/** The async client for the BYD API: pyBYD's `BydClient`. See the module's documentation. */
export class Client extends ClientMethods {
  readonly #core: Core;
  readonly #cars = new Map<number, Car>();
  #closed = false;

  /**
   * A client for an account, ready to use. A bad config value throws
   * `ValidationError`.
   *
   * A client with push listeners (`onVehicleInfo`, `onMqttEvent`,
   * `onCommandAck`, `onCommandLifecycle`, `onMqttConnect`, or a car's
   * `onStateChanged`) keeps the process alive until {@link close}, as an
   * open socket would.
   */
  constructor(options: ClientOptions) {
    const core = new Core(options);
    super(core);
    this.#core = core;
    core.start();
  }

  /**
   * A client that resumes a session from {@link exportSession}, without a
   * password login. Without a `password` it never logs in by itself
   * (`relogin` defaults to `"never"`): when the session expires, calls throw
   * `BydSessionExpiredError` until the application imports a new one.
   */
  static async fromSession(
    session: SessionTokens,
    options: Omit<ClientOptions, "password"> & { password?: string | undefined },
  ): Promise<Client> {
    const password = options.password ?? "";
    const client = new Client({
      ...options,
      password,
      relogin: options.relogin ?? (password === "" ? "never" : "password"),
    });
    try {
      await client.importSession(session);
    } catch (error) {
      client.close();
      throw error;
    }
    return client;
  }

  /** Whether {@link close} was called and {@link start} was not called since. */
  get closed(): boolean {
    return this.#closed;
  }

  /** Opens the client again after {@link close} (pyBYD's `async_start`). A new client is open already. */
  start(): void {
    this.#core.start();
    this.#closed = false;
  }

  /**
   * Closes the cars, stops MQTT and makes later calls fail with `BydError`
   * (pyBYD's `async_close`). Calls already in flight finish; one waiting
   * for an MQTT push stops waiting and polls over HTTP instead, as when the
   * wait times out.
   */
  close(): void {
    this.#cars.clear();
    this.#core.close();
    this.#closed = true;
  }

  /** Closes the client: `using client = new Client(...)`. */
  [Symbol.dispose](): void {
    this.close();
  }

  /**
   * The {@link Car} of a vehicle, built once and kept.
   *
   * As pyBYD: a kept car is returned as it is, whatever the options.
   * Otherwise the vehicle list is fetched unless `vehicle` is given, then
   * the capabilities.
   *
   * @param vin - The vehicle identification number.
   */
  async getCar(vin: string, options: GetCarOptions = {}): Promise<Car> {
    const listener = options.onStateChanged;
    const built = await this.#core.call<{ car: number; vin: string }>(
      "get_car",
      {
        vin,
        vehicle: options.vehicle,
        projection_ttl: options.projectionTtl,
        state_events: listener !== undefined,
      },
      null,
    );
    let car = this.#cars.get(built.car);
    if (car === undefined) {
      car = new Car(this.#core.forCar(built.car), built.vin);
      this.#cars.set(built.car, car);
      if (listener !== undefined) {
        this.#core.stateListeners.set(built.vin, listener);
        this.#core.hold();
      }
    }
    return car;
  }
}
