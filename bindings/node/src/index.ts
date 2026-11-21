/**
 * openbyd: an unofficial client for the BYD overseas app API, with a Rust core.
 *
 * - {@link Client}: pyBYD's `BydClient`.
 * - {@link Car}: pyBYD's `BydCar`, from `client.getCar(vin)`.
 * - pyBYD's exception hierarchy as error classes ({@link BydError}, {@link BydApiError}, ...).
 * - pyBYD's reply models and enums as types, and {@link parseModel} to parse a raw payload as one.
 * - The envelope cipher: {@link encodeEnvelope}, {@link decodeEnvelope}.
 *
 * This project is not affiliated with, endorsed by or supported by BYD.
 *
 * @packageDocumentation
 */

export { Car, Client } from "./client.js";
export type { ClientHooks, ClientOptions, GatePolicy, GetCarOptions, PinResolver, ReloginPolicy } from "./client.js";
export * from "./generated/errors.js";
export {
  BatteryFeature,
  FinderFeature,
  HvacFeature,
  LockFeature,
  SeatFeature,
  SteeringFeature,
  TrunkFeature,
  WindowsFeature,
} from "./generated/methods.js";
export * from "./generated/models.js";
export { configureRuntime, decodeEnvelope, encodeEnvelope, parseModel, runtimeInfo, version } from "./functions.js";
export type { RuntimeInfo } from "./functions.js";
