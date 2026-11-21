/**
 * Loads the compiled add-on (`crates/openbyd-node`).
 *
 * It is looked for in two places:
 *
 * 1. `native/openbyd.<platform>.node` inside this package: a build from
 *    source (`npm run build:native`).
 * 2. The package `@openbyd/<platform>`: the prebuilt add-on for this
 *    platform, installed as an optional dependency.
 *
 * `<platform>` is Node's platform and architecture with the C library where
 * it matters: `win32-x64-msvc`, `linux-x64-gnu`, `linux-arm64-musl`,
 * `darwin-arm64`.
 *
 * @internal
 */

import { join } from "node:path";

/** The add-on's client: methods by name, JSON text in and out. */
export interface NativeClient {
  callSync(method: string, args?: string): string;
  call(method: string, args?: string): Promise<string>;
}

/** What the add-on exports. */
export interface Native {
  readonly ERROR_PREFIX: string;
  call(name: string, args?: string): string;
  testingCall(name: string, args?: string): string;
  readonly NativeClient: new (
    config: string,
    options: string | undefined,
    onEvent: (name: string, payload: string) => void,
  ) => NativeClient;
}

/** Whether this Linux runs on musl (Alpine) instead of glibc. */
function isMusl(): boolean {
  const report = process.report?.getReport() as { header?: { glibcVersionRuntime?: string } } | undefined;
  return report?.header?.glibcVersionRuntime === undefined;
}

/** This platform's name in the add-on's file and package names. */
export function platformName(): string {
  const base = `${process.platform}-${process.arch}`;
  if (process.platform === "win32") return `${base}-msvc`;
  if (process.platform === "linux") return `${base}-${isMusl() ? "musl" : "gnu"}`;
  return base;
}

function load(): Native {
  const platform = platformName();
  const local = join(__dirname, "..", "native", `openbyd.${platform}.node`);
  const failures: string[] = [];
  for (const candidate of [local, `@openbyd/${platform}`]) {
    try {
      // eslint-disable-next-line @typescript-eslint/no-require-imports
      return require(candidate) as Native;
    } catch (error) {
      failures.push(`${candidate}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
  throw new Error(
    `openbyd has no compiled add-on for ${platform}. Install the package @openbyd/${platform}, ` +
      `or build it from source (npm run build:native).\n${failures.join("\n")}`,
  );
}

/** The add-on. */
export const native: Native = load();
