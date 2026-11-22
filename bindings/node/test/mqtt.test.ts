/** Pushes over the TLS test broker, commands acknowledged by push, and the car layer. */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  BydControlPasswordError,
  BydEndpointNotSupportedError,
  BydError,
  Car,
  DataError,
  LockState,
  SeatLevel,
  SeatMode,
  SeatPosition,
  ValidationError,
  type CommandAckEvent,
  type JsonObject,
  type VehicleRealtimeData,
} from "openbyd";

import { HOMEPAGE, LATEST_CONFIG, REALTIME, REMOTE, REMOTE_RESULT, VIN, World, functions, sleep, thrown, until } from "./helpers.js";

function realtime(lock: number, time: number, percent = 80): Record<string, number> {
  return {
    onlineState: 1,
    time,
    elecPercent: percent,
    leftFrontDoorLock: lock,
    rightFrontDoorLock: lock,
    leftRearDoorLock: lock,
    rightRearDoorLock: lock,
  };
}

function vehicleInfo(data: Record<string, unknown>, uuid?: string): Record<string, unknown> {
  return { event: "vehicleInfo", vin: VIN, data: uuid === undefined ? { respondData: data } : { respondData: data, uuid } };
}

function doors(car: Car): (number | null | undefined)[] {
  const state = car.state.realtime;
  return [state?.left_front_door_lock, state?.right_front_door_lock, state?.left_rear_door_lock, state?.right_rear_door_lock];
}

test("pushes reach the callbacks", async () => {
  using world = new World();
  const infos: [string, VehicleRealtimeData][] = [];
  const events: [string, string, JsonObject][] = [];
  const reported: string[] = [];
  let connects = 0;
  const client = world.client({
    mqttEnabled: true,
    onVehicleInfo: (vin, data) => {
      infos.push([vin, data]);
      throw new Error("info bug");
    },
    onMqttEvent: (event, vin, data) => void events.push([event, vin, data]),
    onMqttConnect: () => void (connects += 1),
    onCallbackError: (callback) => reported.push(callback),
  });
  await client.login();
  assert.equal(client.mqttIsRunning, true);
  await world.untilSubscribed(client);
  await until("onMqttConnect", () => connects > 0);
  world.push(client, vehicleInfo({ elecPercent: 64, time: 1_700_000_000 }));
  world.push(client, { event: "smartCharge", vin: VIN, data: { respondData: { elecPercent: 64 } } });
  await until("two events", () => events.length === 2);
  // The throwing onVehicleInfo did not stop the next push.
  world.push(client, vehicleInfo({ elecPercent: 65, time: 1_700_000_001 }));
  await until("two infos", () => infos.length === 2);

  assert.deepEqual(infos.map(([vin, data]) => [vin, data.elec_percent]), [[VIN, 64], [VIN, 65]]);
  assert.deepEqual([events[0]?.[0], events[0]?.[1], events[0]?.[2].elecPercent], ["vehicleInfo", VIN, 64]);
  assert.equal(events[1]?.[0], "smartCharge");
  assert.ok(reported.filter((callback) => callback === "onVehicleInfo").length >= 2);
  await client.startMqtt();
});

test("a command is acknowledged by push", async () => {
  using world = new World();
  world.server.reply(REMOTE, { data: { requestSerial: "C1" } });
  const acks: CommandAckEvent[] = [];
  const lifecycle: string[] = [];
  const client = world.client({
    mqttEnabled: true,
    onCommandAck: (ack) => void acks.push(ack),
    onCommandLifecycle: (event) => void lifecycle.push(event.status),
  });
  await client.login();
  await world.untilSubscribed(client);
  await client.verifyCommandAccess(VIN);
  const locking = client.lock(VIN);
  await until("the command", () => world.server.requests(REMOTE).length === 1);
  await sleep(50);
  world.push(client, { event: "remoteControl", vin: VIN, data: { uuid: "C1", respondData: { res: 2 } } });
  const result = await locking;
  await until("the ack callback", () => acks.length === 1);

  assert.deepEqual([result.success, result.request_serial], [true, "C1"]);
  assert.deepEqual(world.server.requests(REMOTE_RESULT), [], "acknowledged by push, not polled");
  assert.deepEqual([acks[0]?.request_serial, acks[0]?.success], ["C1", true]);
  assert.deepEqual(lifecycle, ["registered", "matched"]);
  const diagnostics = client.getCommandAckDiagnostics();
  assert.deepEqual([diagnostics.matched, diagnostics.pending], [1, 0]);
});

test("closing ends a wait for a push like a timeout", async () => {
  // pyBYD #98: the read goes on to poll over HTTP instead of being cancelled.
  using world = new World();
  world.server.reply("/control/getGpsInfo", { data: { requestSerial: "G1" } });
  world.server.reply("/control/getGpsInfoResult", { data: { requestSerial: "G1", latitude: 52.0, longitude: 4.0 } });
  const client = world.client({ mqttEnabled: true });
  await client.login();
  await world.untilSubscribed(client);
  const waiting = client.getGpsInfo(VIN, { mqttTimeout: 30, pollAttempts: 1, pollInterval: 0 });
  await until("the trigger", () => world.server.requests("/control/getGpsInfo").length === 1);
  await sleep(50);
  client.close();
  assert.equal((await waiting).latitude, 52.0);
  assert.equal(world.server.requests("/control/getGpsInfoResult").length, 1);
});

test("a car merges, projects and rolls back", async () => {
  using world = new World();
  world.server.reply(REALTIME, { data: realtime(1, 1_700_000_000) });
  world.server.reply("/control/getGpsInfo", { data: { latitude: 52.0, longitude: 4.0 } });
  world.server.reply(HOMEPAGE, { data: { elecPercent: 80, chargingState: 0 } });
  world.server.reply("/vehicleInfo/vehicle/getEnergyConsumption", { data: { totalEnergy: "15.2kW·h/100km" } });
  world.server.reply("/control/smartCharge/getAcCurrentLimit", { data: { acCurrentLimit: 16 } });
  const changes: string[] = [];
  const client = world.client({ mqttEnabled: true });
  await client.login();
  await world.untilSubscribed(client);
  await client.verifyCommandAccess(VIN);

  const car = await client.getCar(VIN, { onStateChanged: (vin) => void changes.push(vin) });
  assert.equal(await client.getCar(VIN), car, "kept");
  assert.ok(car instanceof Car);
  assert.equal(car.vin, VIN);
  assert.equal(car.capabilities.lock, true);
  assert.equal(car.lock.available, true);
  assert.equal(car.hvac.available, true);
  assert.deepEqual(car.hvac.tempRange, [15, 31]);
  assert.equal(car.hvac.tempOffset, 14);
  assert.equal(typeof car.seat.isAvailable(SeatPosition.DRIVER, SeatMode.HEAT), "boolean");
  assert.deepEqual(Object.keys(car.features), ["lock", "hvac", "seat", "steering", "battery", "finder", "windows", "trunk"]);

  // Every section.
  assert.equal((await car.updateRealtime()).elec_percent, 80);
  await car.updateHvac();
  assert.equal((await car.updateGps()).latitude, 52);
  await car.updateCharging();
  await car.updateEnergy();
  assert.ok(await car.getAcCurrentLimit());
  const state = car.state;
  assert.equal(state.vehicle.vin, VIN);
  for (const section of ["realtime", "hvac", "gps", "charging", "energy", "charging_schedule"] as const) {
    assert.notEqual(state[section], null, section);
  }
  await until("state change callbacks", () => changes.length >= 6);
  assert.deepEqual(doors(car), [LockState.UNLOCKED, LockState.UNLOCKED, LockState.UNLOCKED, LockState.UNLOCKED]);

  // A command acknowledged by push: projected at once, then confirmed.
  world.server.reply(REMOTE, { data: { requestSerial: "C1" } });
  const locking = car.lock.lock();
  await until("the command", () => world.server.requests(REMOTE).length === 1);
  assert.deepEqual(doors(car), [2, 2, 2, 2], "projected while in flight");
  await sleep(50);
  world.push(client, { event: "remoteControl", vin: VIN, data: { uuid: "C1", respondData: { res: 2 } } });
  await locking;
  world.push(client, vehicleInfo(realtime(2, 1_700_000_200)));
  await until("confirmation", () => doors(car).every((door) => door === LockState.LOCKED));

  // A refused command: rolled back.
  world.server.script(REMOTE, [{ code: "5005" }]);
  assert.ok((await thrown(() => car.lock.unlock())) instanceof BydControlPasswordError);
  assert.deepEqual(doors(car), [2, 2, 2, 2], "rolled back");

  // Pushes handed to the car directly, and their validation.
  car.handleMqttCharging({ elecPercent: 81, chargingState: 1 });
  car.handleMqttEnergy({ time: 1_700_000_300 });
  car.handleMqttRealtime(realtime(2, 1_700_000_400, 79));
  await until("the charging push", () => car.state.charging?.soc === 81);
  await until("the realtime push", () => car.state.realtime?.elec_percent === 79);
  car.handleMqttStatusPush(
    { elecPercent: 77 },
    { hvac: { acSwitch: 1, tempInCar: 22 }, charging: { elecPercent: 77, chargingState: 1 } },
  );
  await until(
    "the status push",
    () =>
      car.state.realtime?.elec_percent === 77 &&
      car.state.hvac?.temp_in_car === 22 &&
      car.state.charging?.soc === 77,
  );
  assert.throws(() => car.handleMqttCharging({ elecPercent: "lots" }), ValidationError);
  assert.throws(
    () => car.handleMqttStatusPush({ elecPercent: 55 }, { hvac: { tempInCar: "warm" } }),
    ValidationError,
  );

  // The client closed: the car's calls fail as a closed client's do.
  client.close();
  assert.ok((await thrown(() => car.updateHvac())) instanceof BydError);
  car.close();
});

test("every car command reaches the client", async () => {
  using world = new World();
  world.server.reply(REMOTE, { data: { controlState: 1, requestSerial: "C1" } });
  world.server.reply("/control/remoteControlBatteryHeat", { data: { controlState: 1, requestSerial: "C2" } });
  world.server.reply("/control/smartCharge/changeChargeStatue", { data: { res: 2, requestSerial: "S" } });
  world.server.reply("/control/smartCharge/saveOrUpdate", { data: { res: 2, requestSerial: "S" } });
  world.server.reply(HOMEPAGE, { data: { elecPercent: 80 } });
  world.server.reply(LATEST_CONFIG, {
    data: {
      [VIN]: functions([
        "1001", "1005", "1006", "1007", "1008", "1009", "1012", "1020", "1021", "1026",
        "10300002", "10030010", "10030002", "10030001", "10030004",
      ]),
    },
  });
  const client = world.client();
  await client.verifyCommandAccess(VIN);
  const car = await client.getCar(VIN, {
    vehicle: { vin: VIN, energyType: "0", vehicleFunLearnInfo: { openWindowLearnInfo: 1 } },
    projectionTtl: 10,
  });
  const commands: [string, () => Promise<unknown>][] = [
    ["lock", () => car.lock.lock()],
    ["unlock", () => car.lock.unlock()],
    ["hvac.start", () => car.hvac.start({ temperature: 22, duration: 10 })],
    ["hvac.start with defaults", () => car.hvac.start()],
    ["hvac.stop", () => car.hvac.stop()],
    ["hvac.schedule", () => car.hvac.schedule({ bookingId: 1, bookingTime: 1_700_000_000 })],
    ["seat.heat", () => car.seat.heat(SeatPosition.DRIVER, SeatLevel.HIGH)],
    ["seat.ventilation", () => car.seat.ventilation("copilot", 1)],
    ["steering", () => car.steering.heat(true)],
    ["battery", () => car.battery.heat(true)],
    ["find", () => car.finder.find()],
    ["flash", () => car.finder.flashLights()],
    ["windows.close", () => car.windows.close()],
    ["windows.open", () => car.windows.open()],
    ["trunk.open", () => car.trunk.open()],
    ["trunk.close", () => car.trunk.close()],
    ["startCharging", () => car.startCharging()],
    ["saveChargingSchedule", () => car.saveChargingSchedule("22:00", "06:00", "e", { enabled: true })],
  ];
  const unsupported: string[] = [];
  for (const [name, run] of commands) {
    const error = await run().then(() => undefined, (failure: unknown) => failure);
    if (error instanceof BydEndpointNotSupportedError) unsupported.push(name);
    else assert.equal(error, undefined, name);
  }
  assert.deepEqual(unsupported, [], "every feature the car has goes through");
  assert.ok(world.server.requests(REMOTE).length >= 12);
  assert.ok((await thrown(() => car.stopCharging())) instanceof BydEndpointNotSupportedError);
  assert.ok((await thrown(() => car.seat.heat("boot" as never, 1))) instanceof DataError);
  assert.ok((await thrown(() => car.seat.heat("driver", 7 as never))) instanceof DataError);
  assert.equal(car.windows.openAvailable, true);
  assert.equal(car.finder.findAvailable, true);
  assert.equal(car.trunk.closeAvailable, true);

  // The client's own commands, with their parameter objects in camelCase.
  const started = await client.startClimate(VIN, { temperature: 21, timeSpan: 2 });
  assert.equal(started.success, true);
  const sent = world.server.requests(REMOTE).at(-1)?.inner as { controlParamsMap: string };
  assert.match(sent.controlParamsMap, /"timeSpan":2/);
  // This car's seats are unknown to the gate; that the parameters arrive is the point.
  const seats = await thrown(() => client.setSeatClimate(VIN, { mainHeat: 2 }));
  assert.ok(seats instanceof BydEndpointNotSupportedError);
  assert.match(seats.message, /seat_target/);
  assert.ok((await thrown(() => client.setSeatClimate(VIN, { mainHeat: 9 }))) instanceof ValidationError);
  assert.equal((await client.setBatteryHeat(VIN, { on: true })).success, true);
  assert.equal((await client.scheduleClimate(VIN, { bookingId: 1, bookingTime: 1_700_000_000 })).success, true);
  for (const run of [
    () => client.stopClimate(VIN),
    () => client.flashLights(VIN),
    () => client.closeWindows(VIN),
    () => client.openWindows(VIN),
    () => client.findCar(VIN),
    () => client.openTrunk(VIN),
    () => client.closeTrunk(VIN),
  ]) {
    assert.equal((await run()).success, true);
  }
  assert.equal((await client.startCharging(VIN, { pollAttempts: 1 })).res, 2);
  assert.equal(
    (await client.saveChargingSchedule(VIN, "22:00", "06:00", "e", { enabled: false, pollAttempts: 1 })).res,
    2,
  );
  assert.ok((await thrown(() => client.stopCharging(VIN))) instanceof BydEndpointNotSupportedError);
});

test("a client with listeners keeps the process alive until it is closed", async () => {
  using world = new World();
  const before = process.getActiveResourcesInfo().filter((kind) => kind === "Timeout").length;
  const quiet = world.client();
  assert.equal(process.getActiveResourcesInfo().filter((kind) => kind === "Timeout").length, before);
  const listening = world.client({ onMqttConnect: () => undefined });
  assert.equal(process.getActiveResourcesInfo().filter((kind) => kind === "Timeout").length, before + 1);
  listening.close();
  assert.equal(process.getActiveResourcesInfo().filter((kind) => kind === "Timeout").length, before);
  quiet.close();
});
