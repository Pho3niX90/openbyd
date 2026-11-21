"""Writes the typed layer of every binding from `spec.py` and the pyBYD installed.

    python bindings/generate.py            # write every generated file
    python bindings/generate.py --check    # fail if any is out of date
    python bindings/generate.py node       # only one package: node, jvm or dotnet

Run it with pyBYD installed at the commit in `parity/UPSTREAM_COMMIT`: the
models' shapes are read from it. `bindings/surface.json` is the surface as
data, which the tests of `openbyd-host` compare with what the core dispatches.

Each package keeps its generated files in directories that hold nothing
else (`OWNED`), so a file the generator no longer writes is removed.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import spec  # noqa: E402
from gen import csharp, java, typescript  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]

#: Each package's emitter: the surface in, every generated file's text out, by path from the root.
EMITTERS = {"node": typescript.files, "jvm": java.files, "dotnet": csharp.files}

#: The directories each package's generated files fill, and nothing else does.
OWNED = {"node": typescript.OWNED, "jvm": java.OWNED, "dotnet": csharp.OWNED}


def outputs(targets: list[str]) -> dict[str, str]:
    """Every generated file's text, by path from the repository's root."""
    surface = spec.build()
    files = {"bindings/surface.json": json.dumps(surface.to_json(), indent=1, ensure_ascii=False) + "\n"}
    for target in targets:
        files.update(EMITTERS[target](surface))
    return files


def leftovers(targets: list[str], files: dict[str, str]) -> list[str]:
    """Files in the generated directories that the generator did not write."""
    found = []
    for target in targets:
        for directory in OWNED[target]:
            for path in sorted((ROOT / directory).rglob("*")):
                relative = path.relative_to(ROOT).as_posix()
                if path.is_file() and relative not in files:
                    found.append(relative)
    return found


#: Where each package states its version, and the text around it.
VERSIONS = {
    "bindings/node/package.json": '"version": "{}"',
    "bindings/jvm/pom.xml": "<version>{}</version>",
    "bindings/dotnet/OpenByd/OpenByd.csproj": "<Version>{}</Version>",
}


def wrong_versions() -> list[str]:
    """The package manifests whose version is not the workspace's."""
    version = spec.version()
    return [
        path
        for path, pattern in VERSIONS.items()
        if (ROOT / path).exists() and pattern.format(version) not in (ROOT / path).read_text(encoding="utf-8")
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("targets", nargs="*", help=f"the packages to generate: {', '.join(EMITTERS)} (all by default)")
    parser.add_argument("--check", action="store_true", help="write nothing; fail if a file is out of date")
    args = parser.parse_args()
    unknown = [target for target in args.targets if target not in EMITTERS]
    if unknown:
        parser.error(f"no package {', '.join(unknown)}; there are {', '.join(EMITTERS)}")
    targets = args.targets or list(EMITTERS)
    behind = wrong_versions()
    if behind:
        print(f"not at the workspace's version {spec.version()} (Cargo.toml):", *behind, sep="\n  ")
        return 1
    files = outputs(targets)
    stale = []
    for path, text in files.items():
        target = ROOT / path
        current = target.read_text(encoding="utf-8") if target.exists() else None
        if current == text:
            continue
        stale.append(path)
        if not args.check:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text, encoding="utf-8", newline="\n")
    removed = leftovers(targets, files)
    if not args.check:
        for path in removed:
            (ROOT / path).unlink()
    if args.check and (stale or removed):
        print("out of date (run `python bindings/generate.py`):", *stale, sep="\n  ")
        if removed:
            print("no longer generated:", *removed, sep="\n  ")
        return 1
    verb = "out of date" if args.check else "written"
    print(f"{len(files)} files, {len(stale)} {verb}, {len(removed)} removed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
