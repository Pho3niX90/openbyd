"""Scan scenario text for private or non-scenario content."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from .export_prod import Leak, find_leaks

__all__ = ["Leak", "find_leaks"]


def main() -> None:
    """Print redacted findings and return a failing exit status if any occur."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("file", type=Path)
    args = parser.parse_args()
    failed = False
    try:
        with args.file.open(encoding="utf-8") as file:
            for finding in find_leaks(file):
                print(f"line {finding.line}: {finding.rule} {finding.excerpt}", file=sys.stderr)
                failed = True
    except (OSError, UnicodeError):
        print("could not read scenario as UTF-8", file=sys.stderr)
        failed = True
    if failed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
