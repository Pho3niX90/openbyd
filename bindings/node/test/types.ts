/**
 * The package's types, checked by the compiler: this file is compiled with
 * the tests and never run. A line under `@ts-expect-error` must be an error;
 * if it stops being one, the build fails.
 */

import {
  BydApiError,
  Client,
  EnergyType,
  LockState,
  SeatLevel,
  SeatPosition,
  StandbyScheduleRepeat,
  parseModel,
  type Car,
  type ChargingHomepage,
  type CommandGateVerdict,
  type GpsInfo,
  type JsonObject,
  type RemoteControlResult,
  type SessionTokens,
  type StandbyPlan,
  type StandbyPlanList,
  type StandbyPlanRunResult,
  type Vehicle,
  type VehicleRealtimeData,
  type VehicleSnapshot,
} from "openbyd";

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2 ? true : false;
function exact<A, B>(_proof: Equal<A, B>): void {}

export async function checks(client: Client, car: Car): Promise<void> {
  // -- results are pyBYD's models --------------------------------------------
  exact<Awaited<ReturnType<Client["getVehicles"]>>, Vehicle[]>(true);
  exact<Awaited<ReturnType<Client["getVehicleRealtime"]>>, VehicleRealtimeData>(true);
  exact<Awaited<ReturnType<Client["lock"]>>, RemoteControlResult>(true);
  exact<Awaited<ReturnType<Client["getChargingHomepage"]>>, ChargingHomepage>(true);
  exact<Awaited<ReturnType<Client["login"]>>, void>(true);
  exact<ReturnType<Client["exportSession"]>, SessionTokens | null>(true);
  exact<Client["commandsEnabled"], boolean>(true);
  exact<Car["state"], VehicleSnapshot>(true);
  exact<Awaited<ReturnType<Car["lock"]["lock"]>>, void>(true);

  const realtime = await client.getVehicleRealtime("TESTVIN0000000001");
  exact<typeof realtime.elec_percent, number | null>(true);
  exact<typeof realtime.raw, JsonObject>(true);
  // @ts-expect-error a model has no field of that name
  void realtime.elecPercent;
  // @ts-expect-error what the core returns is read-only
  realtime.elec_percent = 5;

  // -- enums are their values, and nothing else ------------------------------
  const vehicle = (await client.getVehicles())[0];
  // @ts-expect-error a list may be empty
  void vehicle.vin;
  if (vehicle !== undefined) {
    exact<typeof vehicle.energy_type, -1 | 0 | 1 | 2>(true);
    if (vehicle.energy_type === EnergyType.EV) void vehicle;
    // @ts-expect-error 7 is not an EnergyType
    if (vehicle.energy_type === 7) void vehicle;
  }
  const lock: LockState = LockState.LOCKED;
  void lock;
  // @ts-expect-error 9 is not a LockState
  const bad: LockState = 9;
  void bad;

  // -- arguments -------------------------------------------------------------
  // @ts-expect-error the VIN is required
  await client.getVehicleRealtime();
  // @ts-expect-error options are camelCase
  await client.getVehicleRealtime("VIN", { poll_attempts: 3 });
  // @ts-expect-error an option has its type
  await client.getVehicleRealtime("VIN", { pollAttempts: "3" });
  await client.getVehicleRealtime("VIN", { pollAttempts: 3, mqttTimeout: null });
  await client.startClimate("VIN", { temperature: 21, timeSpan: 2 });
  // @ts-expect-error a parameter object has only its own fields
  await client.startClimate("VIN", { temperature: 21, warmth: 2 });
  // @ts-expect-error battery heating needs `on`
  await client.setBatteryHeat("VIN", {});
  await client.setBatteryHeat("VIN", { on: true }, { commandPwd: "123456" });
  client.seedVehicleMetadata("VIN", EnergyType.HYBRID);
  // @ts-expect-error the energy type is one of EnergyType's values
  client.seedVehicleMetadata("VIN", 5);
  await car.seat.heat(SeatPosition.DRIVER, SeatLevel.HIGH);
  await car.seat.heat("rear_left", 1);
  // @ts-expect-error there is no such seat
  await car.seat.heat("boot", 1);
  // @ts-expect-error there is no such level
  await car.seat.heat("driver", 3);

  // -- the client's options ---------------------------------------------------
  new Client({ username: "user@example.com", password: "secret", controlPin: "123456", sessionTtl: 60 });
  // @ts-expect-error the password is required
  new Client({ username: "user@example.com" });
  // @ts-expect-error the config is camelCase
  new Client({ username: "user@example.com", password: "secret", control_pin: "123456" });
  // @ts-expect-error there is no such gate policy
  new Client({ username: "user@example.com", password: "secret", gatePolicy: "lenient" });
  new Client({
    username: "user@example.com",
    password: "secret",
    gatePolicy: (vin, verdict) => {
      exact<typeof vin, string>(true);
      exact<typeof verdict, CommandGateVerdict>(true);
      return verdict.supported;
    },
    onVehicleInfo: (vin, data) => {
      exact<typeof vin, string>(true);
      exact<typeof data, VehicleRealtimeData>(true);
    },
    // refresh_token may be left out, as records saved by older versions lack it.
    relogin: async () => ({ user_id: "1", sign_token: "s", encry_token: "e", ttl_remaining: null, guest_vins: {} }),
  });
  // @ts-expect-error relogin returns a whole session
  new Client({ username: "user@example.com", password: "secret", relogin: () => ({ user_id: "1" }) });

  // -- errors and models by name ----------------------------------------------
  try {
    await client.lock("VIN");
  } catch (error) {
    if (error instanceof BydApiError) {
      exact<typeof error.code, string>(true);
      exact<typeof error.endpoint, string>(true);
    }
  }
  exact<ReturnType<typeof parseModel<"GpsInfo">>, GpsInfo>(true);
  exact<Awaited<ReturnType<Client["getStandbyPlans"]>>, StandbyPlanList>(true);
  exact<Awaited<ReturnType<Client["createStandbyPlan"]>>, StandbyPlan>(true);
  exact<Awaited<ReturnType<Client["runStandbyPlan"]>>, StandbyPlanRunResult>(true);
  await client.saveStandbyPlan("VIN", (await client.getStandbyPlans("VIN")).plans[0]!, {
    schedule: { repeat: StandbyScheduleRepeat.DAILY, at: "2026-10-24T08:00:00+02:00", zone: "Europe/Madrid" },
  });
  // @ts-expect-error repeat must be a StandbyScheduleRepeat value
  await client.createStandbyPlan("VIN", "P", [], { schedule: { repeat: "hourly" } });
  car.handleMqttStatusPush({ elecPercent: 70 }, { hvac: { acSwitch: 1 } });
  // @ts-expect-error there is no such model
  parseModel("Weather", {});
}
