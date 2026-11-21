// Builds the add-on (crates/openbyd-node) and puts it where the package
// loads it from: native/openbyd.<platform>.node.
//
//   node scripts/build-native.mjs              a release build
//   node scripts/build-native.mjs --debug      a debug build (quick to compile)
//   node scripts/build-native.mjs --testing    with the fake server and test broker
//   node scripts/build-native.mjs --target <triple> --platform <name>
//                                               cross-compile for another platform

import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const value = (name) => (args.includes(name) ? args[args.indexOf(name) + 1] : undefined);

const packageDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const debug = flag("--debug");
const target = value("--target");

function isMusl() {
  return process.report?.getReport()?.header?.glibcVersionRuntime === undefined;
}

/** This platform's name in the add-on's file name; the same rule as src/native.ts. */
function platformName() {
  const base = `${process.platform}-${process.arch}`;
  if (process.platform === "win32") return `${base}-msvc`;
  if (process.platform === "linux") return `${base}-${isMusl() ? "musl" : "gnu"}`;
  return base;
}

function run(command, commandArgs, options = {}) {
  const result = spawnSync(command, commandArgs, { stdio: "inherit", ...options });
  if (result.status !== 0) process.exit(result.status ?? 1);
  return result;
}

const cargo = ["build", "-p", "openbyd-node"];
if (!debug) cargo.push("--release");
if (flag("--testing")) cargo.push("--features", "testing");
if (target) cargo.push("--target", target);
run("cargo", cargo, { cwd: packageDir });

const metadata = spawnSync("cargo", ["metadata", "--format-version", "1", "--no-deps"], {
  cwd: packageDir,
  encoding: "utf8",
  maxBuffer: 64 * 1024 * 1024,
});
if (metadata.status !== 0) process.exit(metadata.status ?? 1);
const targetDir = JSON.parse(metadata.stdout).target_directory;

// The library's file name follows the platform it was built for.
const builtFor = target ?? process.platform;
const library = builtFor.includes("windows") || builtFor === "win32"
  ? "openbyd_node.dll"
  : builtFor.includes("apple") || builtFor === "darwin"
    ? "libopenbyd_node.dylib"
    : "libopenbyd_node.so";
const built = join(targetDir, ...(target ? [target] : []), debug ? "debug" : "release", library);
const platform = value("--platform") ?? platformName();
const destination = join(packageDir, "native", `openbyd.${platform}.node`);
mkdirSync(dirname(destination), { recursive: true });
copyFileSync(built, destination);
console.log(`${built} -> ${destination}`);
