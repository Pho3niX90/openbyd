/** A fake BYD server and a test broker wired together, and clients pointed at both. */

import { Client, type ClientOptions, type SessionTokens } from "openbyd";
import { FakeServer, TEST_TLS, TestBroker } from "openbyd/testing";

export const VIN = "TESTVIN0000000001";
export const USER = "user@example.com";
export const PASSWORD = "secret";
export const PIN = "123456";

export const BROKER_LOOKUP = "/app/emqAuth/getEmqBrokerIp";
export const VEHICLES = "/app/account/getAllListByUserId";
export const LATEST_CONFIG = "/vehicle/vehicleswitch/getLatestConfig";
export const VERIFY = "/vehicle/vehicleswitch/verifyControlPassword";
export const HVAC = "/control/getStatusNow";
export const LOGIN = "/app/account/login";
export const REMOTE = "/control/remoteControl";
export const REMOTE_RESULT = "/control/remoteControlResult";
export const REALTIME = "/vehicleInfo/vehicle/vehicleRealTimeRequest";
export const HOMEPAGE = "/control/smartCharge/homePage";

export const FUNCTIONS = ["1001", "1005", "1006", "1012", "1020", "1021", "1026", "1027", "1030", "1033"];

export function functions(numbers: string[]): { cfFixedList: { functionNo: string }[] } {
  return { cfFixedList: numbers.map((functionNo) => ({ functionNo })) };
}

export class World {
  readonly server = new FakeServer([{ username: USER, password: PASSWORD }]);
  readonly broker = new TestBroker();
  readonly #clients: Client[] = [];

  constructor() {
    this.server.reply(BROKER_LOOKUP, { data: { emqBorker: this.broker.address } });
    this.server.reply(VEHICLES, { data: [{ vin: VIN, energyType: "0", modelName: "SEAL" }] });
    this.server.reply(LATEST_CONFIG, { data: { [VIN]: functions(FUNCTIONS) } });
    this.server.reply(VERIFY, { data: { ok: true } });
    this.server.reply(HVAC, { data: { acSwitch: 1, tempInCar: 21 } });
  }

  options(extra: Partial<ClientOptions> = {}): ClientOptions {
    return {
      username: USER,
      password: PASSWORD,
      baseUrl: this.server.url,
      mqttEnabled: false,
      mqttTimeout: 5,
      controlPin: PIN,
      ...extra,
      ...{ [TEST_TLS]: true },
    };
  }

  /** A client that is closed with the world. */
  client(extra: Partial<ClientOptions> = {}): Client {
    const client = new Client(this.options(extra));
    this.#clients.push(client);
    return client;
  }

  session(client: Client): SessionTokens {
    const session = client.exportSession();
    if (session === null) throw new Error("the client has no session");
    return session;
  }

  push(client: Client, body: Record<string, unknown>): void {
    const reached = this.broker.push(this.session(client), body as never);
    if (reached !== 1) throw new Error(`the push reached ${reached} connections`);
  }

  async untilSubscribed(client: Client): Promise<void> {
    const topic = `oversea/res/${this.session(client).user_id}`;
    await until("a subscription", () => this.broker.subscribers(topic) > 0);
  }

  [Symbol.dispose](): void {
    for (const client of this.#clients) client.close();
    this.server.stop();
    this.broker.stop();
  }
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Waits until `condition()` is true, yielding to the event loop. */
export async function until(what: string, condition: () => boolean, timeoutMs = 10_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!condition()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await sleep(5);
  }
}

/** The error `run` throws; fails when it does not throw. */
export async function thrown(run: () => unknown): Promise<unknown> {
  try {
    await run();
  } catch (error) {
    return error;
  }
  throw new Error("nothing was thrown");
}
