// Prepares a release of the npm packages. Each platform's add-on goes in a
// package of its own, `@openbyd/<platform>`, which the main package lists as
// an optional dependency: npm installs the one that fits the machine and
// skips the rest, and src/native.ts loads it. The packages are scoped because
// npm's spam filter refuses unscoped names of this shape (it refused
// `openbyd-win32-x64-msvc`).
//
//   node scripts/platform-packages.mjs <add-ons directory> <output directory>
//
// The add-ons directory holds `openbyd.<platform>.node` files, as
// scripts/build-native.mjs names them. For each, a package directory is
// written under the output directory, and package.json of the main package
// gains the matching optional dependency. Run it in a release checkout only:
// the packages it names exist on npm once that release is published.

import { copyFileSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const [addOns, output] = process.argv.slice(2);
if (!addOns || !output) {
  console.error("usage: node scripts/platform-packages.mjs <add-ons directory> <output directory>");
  process.exit(2);
}

const packageDir = join(dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = join(packageDir, "package.json");
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));

/** `win32-x64-msvc` as npm's `os`, `cpu` and `libc` fields. */
function fits(platform) {
  const [os, cpu, flavour] = platform.split("-");
  const fields = { os: [os], cpu: [cpu] };
  if (os === "linux") fields.libc = [flavour === "musl" ? "musl" : "glibc"];
  return fields;
}

const optional = {};
for (const file of readdirSync(addOns).sort()) {
  const match = /^openbyd\.(.+)\.node$/.exec(file);
  if (!match) continue;
  const platform = match[1];
  const name = `@openbyd/${platform}`;
  const directory = join(output, platform);
  mkdirSync(directory, { recursive: true });
  copyFileSync(join(addOns, file), join(directory, file));
  const platformManifest = {
    name,
    version: manifest.version,
    description: `The compiled add-on of openbyd for ${platform}. Installed by the package openbyd; not for use by itself.`,
    license: manifest.license,
    repository: manifest.repository,
    main: file,
    files: [file],
    publishConfig: { access: "public" },
    engines: manifest.engines,
    ...fits(platform),
  };
  writeFileSync(join(directory, "package.json"), JSON.stringify(platformManifest, null, 2) + "\n");
  writeFileSync(
    join(directory, "README.md"),
    `# ${name}\n\nThe compiled add-on of [openbyd](https://www.npmjs.com/package/openbyd) for ${platform}. ` +
      "Install `openbyd`, which installs this package on the platforms it fits.\n",
  );
  optional[name] = manifest.version;
  console.log(`${name}@${manifest.version}`);
}

if (Object.keys(optional).length === 0) {
  console.error(`no openbyd.<platform>.node in ${addOns}`);
  process.exit(1);
}
manifest.optionalDependencies = optional;
// The main package carries no add-on of its own in a release.
manifest.files = manifest.files.filter((entry) => !entry.startsWith("native/"));
writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + "\n");
