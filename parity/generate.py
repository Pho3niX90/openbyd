"""Record what pyBYD answers for every group's fixed cases.

    python parity/generate.py            # rewrite the vector files
    python parity/generate.py --check    # fail if any file is out of date
    python parity/generate.py hashing    # only the named groups

Run it with the pyBYD commit named in ``parity/UPSTREAM_COMMIT`` installed.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from parity.framework import VECTOR_DIR, load_groups  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("groups", nargs="*", help="groups to process (default: all)")
    parser.add_argument("--check", action="store_true", help="do not write; exit 1 if a file would change")
    args = parser.parse_args()

    groups = load_groups()
    unknown = sorted(set(args.groups) - set(groups))
    if unknown:
        parser.error(f"unknown group(s): {', '.join(unknown)}")
    selected = [groups[name] for name in (args.groups or sorted(groups))]

    stale = []
    for group in selected:
        if not group.recorded_cases():
            continue
        text = group.vector_document()
        path = group.vector_path()
        current = path.read_text(encoding="utf-8") if path.exists() else None
        if current == text:
            continue
        stale.append(group.name)
        if not args.check:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8", newline="\n")
            print(f"wrote {path.relative_to(VECTOR_DIR.parents[3])}")

    if not args.groups:
        known = {group.vector_path() for group in selected if group.recorded_cases()}
        for orphan in sorted(set(VECTOR_DIR.glob("*.json")) - known):
            stale.append(orphan.stem)
            if not args.check:
                orphan.unlink()
                print(f"removed {orphan.name}: no group writes it")

    if args.check and stale:
        print(f"out of date: {', '.join(stale)}. Run python parity/generate.py", file=sys.stderr)
        return 1
    if not stale:
        print("vectors are up to date")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
