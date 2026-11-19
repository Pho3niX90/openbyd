#!/usr/bin/env python3
"""Print the Markdown body of an "upstream moved" pull request.

Used by ``.github/workflows/upstream-watch.yml``. The workflow collects the
raw pieces into files and this script formats them, so the formatting can be
tried locally. Standard library only.

Inputs (every one except --old and --new is optional; a missing input gets a
"not collected" note in its section):

  --old, --new       pinned and new pyBYD commit (full hashes)
  --commits FILE     upstream commits, one per line, tab separated:
                     <full hash> <date> <subject>
                     (git log --format='%H%x09%cs%x09%s' old..new)
  --diffstat FILE    output of `git diff --stat old new` in a pyBYD clone
  --numstat FILE     output of `git diff --numstat` over the vector files
                     (lines: <added> <removed> <path>)
  --surface FILE     output of the strict surface audit
                     (OPENBYD_SURFACE_STRICT=1 pytest python/tests/test_surface.py)
  --cargo-log FILE   output of cargo test; --cargo-status is its exit code
  --pytest-log FILE  output of pytest;     --pytest-status is its exit code
  --agents FILE      AGENTS.md; its "Following upstream" list becomes the
                     checklist (default: the repository's AGENTS.md)
  --upstream-url     pyBYD repository URL (default https://github.com/jkaberg/pyBYD)
  --run-url          link to the workflow run, shown at the end
  --tail N           failure output is cut to its last N lines (default 60)

Example:

  python scripts/upstream_report.py --old 1111111... --new 2222222... \\
      --commits commits.tsv --diffstat diffstat.txt --numstat numstat.txt \\
      --surface surface.txt --cargo-log cargo.txt --cargo-status 0 \\
      --pytest-log pytest.txt --pytest-status 1
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

DEFAULT_UPSTREAM = "https://github.com/jkaberg/pyBYD"
MAX_CHARS_PER_BLOCK = 6000
MAX_BODY_CHARS = 60000  # GitHub allows 65536 in a pull request body
SURFACE_LINE = re.compile(r"^(?:E\s+)?\s*([A-Za-z_][\w.]*)\s+\(([\w.]+:[\w.]+)\)\s*$")
SURFACE_BARE = re.compile(r"^[A-Za-z_]\w*(?:\.\w+)+$")


def read(path: str | None) -> str | None:
    if not path:
        return None
    try:
        return Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None


def fence(text: str) -> str:
    """A code block whose fence cannot be closed by the text inside it."""
    ticks = "```"
    while ticks in text:
        ticks += "`"
    return f"{ticks}\n{text.rstrip()}\n{ticks}"


def trim(text: str, tail: int) -> str:
    """The last ``tail`` lines, and at most MAX_CHARS_PER_BLOCK characters."""
    lines = text.rstrip().splitlines()
    cut = len(lines) > tail
    out = "\n".join(lines[-tail:])
    if len(out) > MAX_CHARS_PER_BLOCK:
        out = out[-MAX_CHARS_PER_BLOCK:]
        cut = True
    return ("... (earlier output cut)\n" if cut else "") + out


def commits_section(old: str, new: str, raw: str | None, url: str) -> list[str]:
    out = ["## Upstream commits", ""]
    out += [f"[`{old[:12]}...{new[:12]}`]({url}/compare/{old}...{new})", ""]
    if raw is None:
        return out + ["_Not collected._", ""]
    rows = []
    for line in raw.splitlines():
        if not line.strip():
            continue
        full, _, rest = line.partition("\t")
        date, _, subject = rest.partition("\t")
        subject = subject.replace("|", "\\|").strip()
        rows.append(f"| [`{full[:7]}`]({url}/commit/{full}) | {date.strip()} | {subject} |")
    if not rows:
        return out + ["_No commits between the two (history may have been rewritten)._", ""]
    return out + ["| Commit | Date | Subject |", "|---|---|---|", *rows, ""]


def diffstat_section(raw: str | None) -> list[str]:
    out = ["## Upstream files changed", ""]
    if raw is None:
        return out + ["_Not collected._", ""]
    if not raw.strip():
        return out + ["_No file changes._", ""]
    lines = raw.rstrip().splitlines()
    if len(lines) > 200:
        lines = lines[:199] + [f"... {len(lines) - 200} more files", lines[-1]]
    return out + [fence("\n".join(lines)), ""]


def vectors_section(raw: str | None) -> list[str]:
    out = ["## Vector files changed", ""]
    if raw is None:
        return out + ["_Not collected._", ""]
    rows = []
    for line in raw.splitlines():
        parts = line.split("\t") if "\t" in line else line.split(None, 2)
        if len(parts) != 3:
            continue
        added, removed, path = parts
        rows.append((path.strip(), int(added) if added.isdigit() else 0, int(removed) if removed.isdigit() else 0))
    if not rows:
        return out + ["No vector file changed: pyBYD answers the recorded cases as before.", ""]
    out += ["| File | Added | Removed |", "|---|---:|---:|"]
    out += [f"| `{p}` | +{a} | -{r} |" for p, a, r in rows]
    total_a, total_r = sum(a for _, a, _ in rows), sum(r for _, _, r in rows)
    out += ["", f"{len(rows)} file(s), +{total_a} / -{total_r} lines. "
            "Each changed line is a case where pyBYD now answers differently.", ""]
    return out


def surface_names(raw: str) -> list[tuple[str, str]]:
    seen: dict[str, str] = {}
    for line in raw.splitlines():
        m = SURFACE_LINE.match(line)
        if m:
            seen.setdefault(m.group(1), m.group(2))
        elif SURFACE_BARE.match(line.strip()):
            seen.setdefault(line.strip(), "")
    return sorted(seen.items())


def surface_section(raw: str | None) -> list[str]:
    out = ["## Strict surface audit", ""]
    if raw is None:
        return out + ["_Not collected._", ""]
    out += ["pyBYD symbols with no parity call and no entry in `parity/surface_exceptions.py`:", ""]
    names = surface_names(raw)
    if not names:
        return out + ["None: every public pyBYD symbol has a parity call.", ""]
    out += [f"{len(names)} missing:", ""]
    out += [f"- `{n}`" + (f" (`{loc}`)" if loc else "") for n, loc in names]
    return out + [""]


def result_section(title: str, command: str, status: str | None, raw: str | None, tail: int) -> list[str]:
    out = [f"### {title}", "", f"`{command}`", ""]
    if status is None:
        return out + ["_Not run._", ""]
    if status == "0":
        return out + ["Passed.", ""]
    out += [f"**Failed** (exit code {status}). Last lines of the output:", ""]
    out.append(fence(trim(raw, tail)) if raw and raw.strip() else "_No output captured._")
    return out + [""]


def checklist(agents: str | None) -> list[str]:
    out = ["## Checklist", ""]
    items: list[str] = []
    m = re.search(r"^## Following upstream\s*$(.*?)(?=^## |\Z)", agents or "", re.S | re.M)
    if m:
        current: str | None = None
        for line in m.group(1).splitlines():
            n = re.match(r"^\d+\.\s+(.*)", line)
            if n:
                if current:
                    items.append(current)
                current = n.group(1).strip()
            elif current and line.strip() and line.startswith((" ", "\t")):
                current += " " + line.strip()
        if current:
            items.append(current)
    if not items:
        return out + ['_Could not read the "Following upstream" section of AGENTS.md._', ""]
    out += ['From the "Following upstream" section of `AGENTS.md`:', ""]
    return out + [f"- [ ] {item}" for item in items] + [""]


def build(args: argparse.Namespace) -> str:
    url = args.upstream_url.rstrip("/")
    lines = [
        f"pyBYD has moved from `{args.old[:12]}` to `{args.new[:12]}`. "
        "This pull request only moves the pin and re-records the vectors; nothing is ported yet.",
        "",
    ]
    lines += commits_section(args.old, args.new, read(args.commits), url)
    lines += diffstat_section(read(args.diffstat))
    lines += vectors_section(read(args.numstat))
    lines += surface_section(read(args.surface))
    lines += ["## Tests with the new pin", ""]
    lines += result_section("Rust", "cargo test --workspace --exclude openbyd-py --all-features",
                            args.cargo_status, read(args.cargo_log), args.tail)
    lines += result_section("Python", "pytest", args.pytest_status, read(args.pytest_log), args.tail)
    lines += checklist(read(args.agents))
    if args.run_url:
        lines += [f"Generated by [this workflow run]({args.run_url}). Never merged automatically.", ""]
    body = "\n".join(lines).rstrip() + "\n"
    if len(body) > MAX_BODY_CHARS:
        body = body[:MAX_BODY_CHARS] + "\n\n... (report cut to fit the pull request body limit)\n"
    return body


def main(argv: list[str] | None = None) -> int:
    default_agents = Path(__file__).resolve().parent.parent / "AGENTS.md"
    p = argparse.ArgumentParser(
        description="Print the Markdown report for a 'Follow pyBYD' pull request.",
        epilog="Each file option names a file the workflow collected; see the module docstring.",
    )
    p.add_argument("--old", required=True, help="pinned pyBYD commit")
    p.add_argument("--new", required=True, help="new pyBYD commit")
    p.add_argument("--commits", help="TSV: full hash, date, subject (git log --format='%%H%%x09%%cs%%x09%%s')")
    p.add_argument("--diffstat", help="output of `git diff --stat old new` in a pyBYD clone")
    p.add_argument("--numstat", help="output of `git diff --numstat` over the vector files")
    p.add_argument("--surface", help="output of the strict surface audit test")
    p.add_argument("--cargo-log", help="output of cargo test")
    p.add_argument("--cargo-status", help="exit code of cargo test")
    p.add_argument("--pytest-log", help="output of pytest")
    p.add_argument("--pytest-status", help="exit code of pytest")
    p.add_argument("--agents", default=str(default_agents), help="AGENTS.md (default: the repository's)")
    p.add_argument("--upstream-url", default=DEFAULT_UPSTREAM, help="pyBYD repository URL")
    p.add_argument("--run-url", help="link to the workflow run")
    p.add_argument("--tail", type=int, default=60, help="lines of failure output to keep (default 60)")
    args = p.parse_args(argv)
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stdout.write(build(args))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
