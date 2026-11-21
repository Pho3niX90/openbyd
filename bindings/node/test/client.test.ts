/** The client end to end against the fake server: reads, failures, every option, sessions. */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  BydApiError,
  BydAuthenticationError,
  BydControlPasswordError,
  BydEndpointNotSupportedError,
  BydError,
  BydSessionExpiredError,
  BydTransportError,
  Client,
  DataError,
  EnergyType,
  NotificationCategory,
  OpenBydError,
  StandbyScheduleRepeat,
  ValidationError,
  type CommandGateVerdict,
  type RequestRecord,
} from "openbyd";
import { hasTesting } from "openbyd/testing";

import {
  HOMEPAGE,
  HVAC,
  LATEST_CONFIG,
  LOGIN,
  PASSWORD,
  PIN,
  REALTIME,
  REMOTE,
  USER,
  VEHICLES,
  VIN,
  World,
  functions,
  thrown,
} from "./helpers.js";

const DONE = { controlState: 1, requestSerial: "S1" };

test("the add-on under test has the fake server", () => {
  assert.equal(hasTesting, true, "build it with: npm run build:native -- --testing");
});

test("reads return pyBYD's dumps, typed", async () => {
  using world = new World();
  const client = world.client({ countryCode: "DE", language: "de", device: { model: "Pixel" } });
  assert.equal(client.exportSession(), null);
  assert.equal(client.commandsEnabled, false);
  assert.equal(client.mqttIsRunning, false);
  await client.login();

  const vehicles = await client.getVehicles();
  assert.equal(vehicles.length, 1);
  const vehicle = vehicles[0];
  assert.ok(vehicle);
  assert.equal(vehicle.vin, VIN);
  assert.equal(vehicle.energy_type, EnergyType.EV);
  assert.equal(vehicle.model_name, "SEAL");
  assert.deepEqual(vehicle.raw, { vin: VIN, energyType: "0", modelName: "SEAL" });

  const login = world.server.requests(LOGIN)[0];
  assert.ok(login?.outer);
  assert.deepEqual([login.outer.countryCode, login.outer.language, login.outer.model], ["DE", "de", "Pixel"]);

  assert.equal((await client.getHvacStatus(VIN)).ac_switch, 1);
  assert.equal((await client.getVehicleCapabilities(VIN)).lock, true);
  assert.equal((await client.getVehicleCapabilities(VIN, { forceRefresh: true })).vin, VIN);
  const configs = await client.getLatestConfigs([VIN]);
  assert.deepEqual(Object.keys(configs), [VIN]);
  assert.ok(await client.getLatestConfig(VIN));

  world.server.reply(HOMEPAGE, { data: { elecPercent: 80, chargingState: 0 } });
  const homepage = await client.getChargingHomepage(VIN);
  assert.equal(homepage.status.soc, 80);
  assert.equal(typeof homepage.schedule.vin, "string");

  const session = await client.ensureSession();
  assert.equal(typeof session.sign_token, "string");
  assert.ok(session.ttl > 0);
  assert.equal(client.vehicleMetadata(VIN)?.model_name, "SEAL");
  assert.equal(client.vehicleMetadata("TESTVIN0000000009"), null);
  const diagnostics = client.getCommandAckDiagnostics();
  assert.deepEqual([diagnostics.pending, diagnostics.matched], [0, 0]);
});

test("failures are pyBYD's exception classes with their fields", async () => {
  using world = new World();
  const client = world.client();
  world.server.reply(HOMEPAGE, { code: "1001" });
  const unsupported = await thrown(() => client.getChargingStatus(VIN));
  assert.ok(unsupported instanceof BydEndpointNotSupportedError);
  assert.ok(unsupported instanceof BydApiError && unsupported instanceof BydError);
  assert.ok(unsupported instanceof OpenBydError && unsupported instanceof Error);
  assert.equal(unsupported.name, "BydEndpointNotSupportedError");
  assert.equal(unsupported.kind, "BydEndpointNotSupportedError");
  assert.equal(unsupported.code, "1001");
  assert.equal(unsupported.endpoint, HOMEPAGE);

  world.server.reply(HVAC, { status: 503, body: "down" });
  const transport = await thrown(() => client.getHvacStatus(VIN));
  assert.ok(transport instanceof BydTransportError);
  assert.equal(transport.statusCode, 503);
  assert.equal(transport.endpoint, HVAC);

  const invalid = await thrown(() => client.startClimate(VIN, { temperature: "hot" as never }));
  assert.ok(invalid instanceof ValidationError && invalid instanceof DataError);
  assert.deepEqual(invalid.loc, ["temperature"]);
  assert.equal(typeof invalid.errorType, "string");

  const category = await thrown(() => client.getNotifications({ category: 9 as never }));
  assert.ok(category instanceof DataError && !(category instanceof ValidationError));
  assert.equal(category.kind, "ValueError");
});

test("a bad config is refused when the client is made", () => {
  assert.throws(() => new Client({ username: USER } as never), ValidationError);
  assert.throws(() => new Client({ username: USER, password: PASSWORD, sessionTtl: "long" as never }), ValidationError);
  assert.throws(() => new Client({ username: USER, password: PASSWORD, commandRetries: 0 }), DataError);
  assert.throws(() => new Client({ username: USER, password: PASSWORD, commandAckTtlSeconds: Infinity }), DataError);
  assert.throws(() => new Client({ username: USER, password: PASSWORD, gatePolicy: "lenient" as never }), DataError);
  assert.throws(() => new Client({ username: USER, password: PASSWORD, relogin: "sometimes" as never }), DataError);
  new Client({ username: USER, password: PASSWORD, gatePolicy: "strict", relogin: "password" }).close();
});

test("a client fails after close and works again after start", async () => {
  using world = new World();
  const client = world.client();
  assert.equal(client.closed, false);
  assert.equal((await client.getHvacStatus(VIN)).ac_switch, 1);
  client.close();
  assert.equal(client.closed, true);
  const closed = await thrown(() => client.getHvacStatus(VIN));
  assert.ok(closed instanceof BydError);
  assert.match(closed.message, /not initialized/);
  client.start();
  assert.equal((await client.getHvacStatus(VIN)).ac_switch, 1);
  {
    using scoped = world.client();
    await scoped.login();
    void scoped;
  }
});

async function lockWith(world: World, options: Parameters<World["client"]>[0], missing = false): Promise<unknown> {
  world.server.reply(LATEST_CONFIG, { data: { [VIN]: missing ? functions([]) : functions(["1005"]) } });
  world.server.reply(REMOTE, { data: DONE });
  const client = world.client(options);
  await client.verifyCommandAccess(VIN);
  assert.equal(client.commandsEnabled, true);
  try {
    return await client.lock(VIN);
  } catch (error) {
    return error;
  }
}

test("the gate policy decides whether a command is sent", async () => {
  using world = new World();
  const refused = await lockWith(world, {}, true);
  assert.ok(refused instanceof BydEndpointNotSupportedError);
  assert.equal(refused.code, "command_gate_blocked");
  assert.deepEqual(world.server.requests(REMOTE), []);

  const allowed = await lockWith(world, { gatePolicy: "allow_missing_data" }, true);
  assert.equal((allowed as { success: boolean }).success, true);

  const verdicts: [string, CommandGateVerdict][] = [];
  const decided = await lockWith(
    world,
    {
      gatePolicy: (vin, verdict) => {
        verdicts.push([vin, verdict]);
        return verdict.reason === "function_no_missing";
      },
    },
    true,
  );
  assert.equal((decided as { success: boolean }).success, true);
  assert.equal(verdicts[0]?.[0], VIN);
  assert.equal(verdicts[0]?.[1].supported, false);

  const later = await lockWith(world, { gatePolicy: async () => true }, true);
  assert.equal((later as { success: boolean }).success, true);

  // A policy that throws falls back to the verdict, and the failure is reported.
  const reported: [string, unknown][] = [];
  const broken = await lockWith(
    world,
    {
      gatePolicy: () => {
        throw new Error("policy bug");
      },
      onCallbackError: (callback, error) => reported.push([callback, error]),
    },
    true,
  );
  assert.ok(broken instanceof BydEndpointNotSupportedError);
  assert.equal(reported[0]?.[0], "gatePolicy");
  assert.match(String(reported[0]?.[1]), /policy bug/);
});

test("the pin resolver replaces pyBYD's rule", async () => {
  using world = new World();
  const digest = "0123456789abcdef0123456789abcdef";
  const calls: [string | null, string | null][] = [];
  await lockWith(world, {
    pinResolver: (commandPwd, controlPin) => {
      calls.push([commandPwd, controlPin]);
      return digest;
    },
  });
  assert.deepEqual(calls[0], [null, PIN]);
  assert.equal((world.server.requests(REMOTE)[0]?.inner as { commandPwd: string }).commandPwd, digest);

  world.server.clearRequests();
  const reported: string[] = [];
  await lockWith(world, {
    pinResolver: () => 1234 as never,
    onCallbackError: (callback) => reported.push(callback),
  });
  // An answer that is no string falls back to pyBYD's rule: MD5, upper case.
  assert.equal(
    (world.server.requests(REMOTE)[0]?.inner as { commandPwd: string }).commandPwd,
    "E10ADC3949BA59ABBE56E057F20F883E",
  );
});

test("a command is retried as many times as asked", async () => {
  using world = new World();
  world.server.reply(LATEST_CONFIG, { data: { [VIN]: functions(["1005"]) } });
  world.server.reply(REMOTE, { data: { controlState: 2, requestSerial: "S1" } });
  const client = world.client({ commandRetries: 1 });
  await client.verifyCommandAccess(VIN);
  const failed = await thrown(() => client.lock(VIN));
  assert.ok(failed instanceof BydApiError);
  assert.equal(failed.code, "2");
  assert.equal(world.server.requests(REMOTE).length, 1, "one attempt, no retry");

  world.server.script(REMOTE, [{ code: "5005" }]);
  const refused = await thrown(() => client.lock(VIN, { commandPwd: "654321" }));
  assert.ok(refused instanceof BydControlPasswordError);
});

test("the request observer hears every request", async () => {
  using world = new World();
  const records: RequestRecord[] = [];
  world.server.reply(HOMEPAGE, { code: "1001" });
  const client = world.client({ requestObserver: (record) => records.push(record) });
  await client.login();
  await client.getHvacStatus(VIN);
  await thrown(() => client.getChargingStatus(VIN));
  assert.deepEqual(
    records.map((record) => [record.endpoint, record.outcome, record.code]),
    [
      [LOGIN, "reply", "0"],
      [HVAC, "reply", "0"],
      [HOMEPAGE, "reply", "1001"],
    ],
  );
  assert.ok(records.every((record) => record.duration >= 0));

  // An observer that throws is reported and does not affect the client.
  const reported: string[] = [];
  const noisy = world.client({
    requestObserver: () => {
      throw new Error("observer bug");
    },
    onCallbackError: (callback) => reported.push(callback),
  });
  assert.equal((await noisy.getHvacStatus(VIN)).ac_switch, 1);
  assert.ok(reported.includes("requestObserver"));
});

test("a callback's failure is a process warning by default", async () => {
  using world = new World();
  const warnings: Error[] = [];
  const listener = (warning: Error): void => void warnings.push(warning);
  process.on("warning", listener);
  try {
    const client = world.client({
      requestObserver: () => {
        throw new Error("observer bug");
      },
    });
    await client.login();
    await new Promise((resolve) => setImmediate(resolve));
  } finally {
    process.off("warning", listener);
  }
  const warning = warnings.find((item) => item.name === "OpenBydCallbackWarning");
  assert.ok(warning);
  assert.match(warning.message, /requestObserver.*observer bug/);
});

test("sessions are exported, restored and renewed", async () => {
  using world = new World();
  const first = world.client();
  await first.login();
  const saved = world.session(first);
  assert.deepEqual(Object.keys(saved), ["user_id", "sign_token", "encry_token", "ttl_remaining", "guest_vins"]);
  assert.ok(saved.ttl_remaining !== null && saved.ttl_remaining > 0);

  const { password: _password, ...withoutPassword } = world.options();
  const restored = await Client.fromSession(saved, withoutPassword);
  try {
    assert.equal(restored.exportSession()?.sign_token, saved.sign_token);
    assert.equal((await restored.getHvacStatus(VIN)).ac_switch, 1);
    assert.equal(world.server.requests(LOGIN).length, 1, "the restored client never logged in");

    world.server.expireSessions(USER);
    const expired = await thrown(() => restored.getHvacStatus(VIN));
    assert.ok(expired instanceof BydSessionExpiredError);
    assert.equal(expired.code, "relogin_disabled");
    restored.invalidateSession();
  } finally {
    restored.close();
  }
  await assert.rejects(
    Client.fromSession({ ...saved, user_id: "" }, withoutPassword),
    DataError,
  );

  // A relogin function hands over a new session when one is needed.
  let asked = 0;
  const renewing = world.client({
    relogin: async () => {
      asked += 1;
      const helper = world.client();
      await helper.login();
      return world.session(helper);
    },
  });
  assert.equal((await renewing.getHvacStatus(VIN)).ac_switch, 1);
  assert.equal(asked, 1);

  // What it throws is what the call throws.
  const refusing = world.client({
    relogin: () => {
      throw new BydAuthenticationError("no", { kind: "BydAuthenticationError", code: "1002", endpoint: "x" });
    },
  });
  const refused = await thrown(() => refusing.getHvacStatus(VIN));
  assert.ok(refused instanceof BydAuthenticationError);
  assert.deepEqual([refused.message, refused.code], ["no", "1002"]);

  const broken = world.client({
    relogin: () => {
      throw new RangeError("bug");
    },
  });
  const failure = await thrown(() => broken.getHvacStatus(VIN));
  assert.ok(failure instanceof BydError);
  assert.match(failure.message, /RangeError: bug/);

  const nonsense = world.client({ relogin: () => 42 as never });
  assert.match(String(await thrown(() => nonsense.getHvacStatus(VIN))), /relogin returned no session/);
});

test("seeded metadata saves the vehicle list", async () => {
  using world = new World();
  world.server.reply(REALTIME, { data: { elecPercent: 50, time: 1_700_000_000 } });
  const client = world.client();
  client.seedVehicleMetadata(VIN, EnergyType.HYBRID, { modelName: "SONG" });
  assert.equal(client.vehicleMetadata(VIN)?.energy_type, EnergyType.HYBRID);
  const realtime = await client.getVehicleRealtime(VIN, { pollAttempts: 0, mqttTimeout: 0 });
  assert.equal(realtime.elec_percent, 50);
  assert.deepEqual(world.server.requests(VEHICLES), []);
  assert.deepEqual(await client.getNotifications({ category: NotificationCategory.SYSTEM }).catch(() => []), []);
});

test("many clients are cheap and run at once", async () => {
  using world = new World();
  world.server.reply(HVAC, { data: { acSwitch: 1 }, delay: 0.02 });
  const clients = Array.from({ length: 100 }, () => world.client());
  const statuses = await Promise.all(clients.map((client) => client.getHvacStatus(VIN)));
  assert.deepEqual(new Set(statuses.map((status) => status.ac_switch)), new Set([1]));
  assert.equal(world.server.requests(LOGIN).length, 100);
});

test("standby plans round-trip, including daily schedules with zone", async () => {
  const LIST = "/control/standby-vehicles/list";
  const SAVE = "/control/standby-vehicles/save";
  const SET_DEFAULT = "/control/standby-vehicles/setDefault";
  const DELETE = "/control/standby-vehicles/delete";
  const SEND = "/control/standby-vehicles/send";
  const SEND_RESULT = "/control/standby-vehicles/send-result";

  using world = new World();
  const preset = {
    id: "101",
    planName: "Preset",
    planType: 1,
    defaultPlan: 1,
    enable: 1,
    executeType: "2",
    startTime: "",
    weeklyDays: [],
    controlParams: [
      { commandType: "OPENAIR", commandId: "1", controlParamsMap: "{\"temperature\":21,\"timeSpan\":2}" },
    ],
  };
  const created = {
    ...preset,
    id: "202",
    planName: "Morning",
    planType: 2,
    defaultPlan: 0,
    executeType: "1",
  };
  world.server.reply(SAVE, { data: {} });
  world.server.reply(SET_DEFAULT, { data: {} });
  world.server.reply(DELETE, { data: {} });
  world.server.reply(SEND, { data: { requestSerial: "RUN1" } });
  world.server.reply(SEND_RESULT, { data: { requestSerial: "RUN1", res: 2, message: "ok" } });
  world.server.script(LIST, [
    { data: [preset] },
    { data: [preset] },
    { data: [preset] },
    { data: [preset, created] },
    { data: [preset, created] },
    { data: [preset] },
  ]);

  const client = world.client();
  const list = await client.getStandbyPlans(VIN);
  assert.equal(list.plans.length, 1);
  const plan = list.plans[0];
  assert.ok(plan);

  const saved = await client.saveStandbyPlan(VIN, plan, {
    schedule: {
      repeat: StandbyScheduleRepeat.DAILY,
      at: "2026-10-24T08:00:00+02:00",
      zone: "Europe/Madrid",
    },
  });
  assert.equal(saved.plans[0]?.id, "101");
  const saveInner = world.server.requests(SAVE)[0]?.inner as { executeType: string; executeTime: number };
  assert.equal(saveInner.executeType, "1");
  assert.ok(saveInner.executeTime > 0);

  const badZone = await thrown(() =>
    client.saveStandbyPlan(VIN, plan, {
      schedule: {
        repeat: StandbyScheduleRepeat.DAILY,
        at: "2026-10-24T08:00:00+02:00",
        zone: "Mars/Olympus_Mons",
      },
    }),
  );
  assert.ok(badZone instanceof DataError);
  assert.equal(badZone.kind, "ZoneInfoNotFoundError");

  const newPlan = await client.createStandbyPlan(VIN, "Morning", plan.commands, {
    schedule: {
      repeat: StandbyScheduleRepeat.WEEKLY,
      startTime: "07:30",
      weeklyDays: [0, 1, 2, 3, 4],
    },
  });
  assert.equal(newPlan.id, "202");

  const defaulted = await client.setDefaultStandbyPlan(VIN, "202");
  assert.equal(defaulted.plans.length, 2);

  const remaining = await client.deleteStandbyPlan(VIN, "202");
  assert.equal(remaining.plans.length, 1);

  const run = await client.runStandbyPlan(VIN, "101", { pollAttempts: 2, pollInterval: 0.01 });
  assert.equal(run.res, 2);
  assert.equal(run.vin, VIN);
});
