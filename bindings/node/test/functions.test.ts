/** What needs no client: the cipher, models by name, the runtime. */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  BangcleError,
  BydCryptoError,
  DataError,
  ValidationError,
  configureRuntime,
  decodeEnvelope,
  encodeEnvelope,
  parseModel,
  runtimeInfo,
  version,
} from "openbyd";
import { parityCall } from "openbyd/testing";

test("the version is the package's", () => {
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const manifest = require("openbyd/package.json") as { version: string };
  assert.equal(version(), manifest.version);
});

test("the cipher round-trips text and bytes", () => {
  const envelope = encodeEnvelope('{"hello":"wörld"}');
  assert.match(envelope, /^F/);
  assert.equal(Buffer.from(decodeEnvelope(envelope)).toString("utf8"), '{"hello":"wörld"}');
  const bytes = Uint8Array.from([0, 255, 16, 128]);
  assert.deepEqual([...decodeEnvelope(encodeEnvelope(bytes))], [0, 255, 16, 128]);
  assert.throws(() => decodeEnvelope(""), BangcleError);
  assert.throws(() => decodeEnvelope("not an envelope"), BydCryptoError);
});

test("a model parses by name, typed", () => {
  const gps = parseModel("GpsInfo", { latitude: 52.0, longitude: "4.0", gpsTimeStamp: 1_700_000_000 });
  assert.deepEqual([gps.latitude, gps.longitude], [52, 4]);
  assert.equal(typeof gps.gps_timestamp, "string");
  assert.deepEqual(gps.raw, { latitude: 52.0, longitude: "4.0", gpsTimeStamp: 1_700_000_000 });

  const realtime = parseModel("VehicleRealtimeData", { elecPercent: 80, time: 1_700_000_000 }, { energy_type: { EnergyType: 0 } });
  assert.equal(realtime.elec_percent, 80);
  assert.throws(() => parseModel("ChargingStatus", { elecPercent: "lots" }), ValidationError);
});

test("floats JSON cannot hold come back as numbers, only where a float is", () => {
  const raw = { latitude: 52.0, speed: "inf", direction: "-inf", requestSerial: "Infinity" };
  const gps = parseModel("GpsInfo", raw);
  assert.equal(gps.speed, Infinity);
  assert.equal(gps.direction, -Infinity);
  // A string field that happens to read "Infinity" stays a string, and so does the raw reply.
  assert.equal(gps.request_serial, "Infinity");
  assert.deepEqual(gps.raw, raw);
  // pyBYD reads a NaN coordinate as no coordinate.
  assert.equal(parseModel("GpsInfo", { latitude: "nan" }).latitude, null);
});

test("the runtime reports its settings and refuses bad ones", () => {
  const info = runtimeInfo();
  assert.ok(info.worker_threads >= 1);
  assert.throws(() => configureRuntime({ workerThreads: 0 }), DataError);
});

test("a parity call reports pyBYD's error class as data", () => {
  assert.deepEqual(parityCall("hashing.md5_hex", { value: "abc" }), { ok: "900150983CD24FB0D6963F7D28E17F72" });
  const unknown = parityCall("no.such", {});
  assert.ok("error" in unknown);
  assert.equal(unknown.error.kind, "UnknownCall");
});
