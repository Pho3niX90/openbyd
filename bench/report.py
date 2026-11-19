"""Render the JSON results in ``bench/results/`` as Markdown tables.

    python bench/report.py                 # the newest full result of each benchmark
    python bench/report.py --quick         # the newest quick result of each
    python bench/report.py FILE.json ...   # these files
    python bench/report.py --out report.md

A comparison is called "within noise" when the min..max ranges of the repeated measurements overlap;
otherwise the ratio of the medians is given.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402

NAMES = ("micro", "load", "mqtt_idle")


def duration(seconds: float) -> str:
    if seconds >= 1:
        return f"{seconds:.2f} s"
    if seconds >= 1e-3:
        return f"{seconds * 1e3:.2f} ms"
    if seconds >= 1e-6:
        return f"{seconds * 1e6:.1f} us"
    return f"{seconds * 1e9:.0f} ns"


def cell(data: dict[str, Any] | None) -> str:
    if data is None:
        return "-"
    summary = data["seconds_per_op"]
    return f"{duration(summary['median'])} (+-{summary['spread'] * 50:.0f}%)"


def versus(reference: dict[str, Any] | None, other: dict[str, Any] | None) -> str:
    """How ``other`` compares with ``reference``: a factor, or 'within noise'."""
    if reference is None or other is None:
        return "-"
    a, b = reference["seconds_per_op"], other["seconds_per_op"]
    if common.overlap(a, b):
        return "within noise"
    if b["median"] < a["median"]:
        return f"{a['median'] / b['median']:.1f}x faster"
    return f"{b['median'] / a['median']:.1f}x slower"


def table(header: list[str], rows: list[list[str]]) -> list[str]:
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join("---" for _ in header) + "|"]
    lines += ["| " + " | ".join(row) + " |" for row in rows]
    return lines


def preamble(result: dict[str, Any]) -> list[str]:
    machine, versions = result["machine"], result["versions"]
    return [
        f"Run on {result['date']}{' (quick run: small sizes, not for comparison)' if result['quick'] else ''}. "
        f"{machine['cpu']}, {machine['cores_logical']} logical cores, {machine['memory_gib']} GiB, {machine['os']}; "
        f"Python {machine['python']}, {machine['rust']}. openbyd commit `{versions['openbyd_commit'][:10]}`"
        f"{' (uncommitted changes in the tree)' if versions['openbyd_dirty'] else ''}; "
        f"pyBYD commit `{(versions['pybyd_commit_installed'] or versions['pybyd_commit_pinned'])[:10]}`.",
        "",
    ]


def render_micro(result: dict[str, Any]) -> list[str]:
    labels = result["settings"]["labels"]
    settings = result["settings"]
    lines = ["## Micro-benchmarks", ""] + preamble(result)
    background = result.get("background_cpu_percent_before")
    if background is not None:
        lines += [f"Other activity on the machine before the run: {background:.0f}% of all cores.", ""]
    lines += [
        f"Each cell is the median time per operation of {settings['repeats']} measurements of at least "
        f"{settings['min_time_s']} s, with half the spread (max - min) / median as +-. "
        f"\"openbyd vs pyBYD\" is a factor of the medians, or \"within noise\" when the measurements' ranges overlap.",
        "",
    ]
    groups: dict[str, list[dict[str, Any]]] = {}
    for row in result["rows"]:
        groups.setdefault(row["group"], []).append(row)
    for group, rows in groups.items():
        lines += [f"### {group}", ""]
        keys = [key for key in labels if any(key in row["impls"] for row in rows)]
        header = ["Row", *(labels[key] for key in keys)]
        compare = "pybyd" in keys and "openbyd" in keys
        if compare:
            header.append("openbyd vs pyBYD")
        body = []
        for row in rows:
            cells = [row["name"], *(cell(row["impls"].get(key)) for key in keys)]
            if compare:
                cells.append(versus(row["impls"].get("pybyd"), row["impls"].get("openbyd")))
            if not row["check"]["ok"]:
                cells[0] += " **(answers differ)**"
            body.append(cells)
        lines += table(header, body)
        notes = sorted({row["note"] for row in rows if row["note"]})
        if notes:
            lines += [""] + [f"*{note}*" for note in notes]
        lines.append("")
    if result.get("skipped"):
        lines += ["Not compared: " + "; ".join(result["skipped"]), ""]
    return lines


def render_load(result: dict[str, Any]) -> list[str]:
    settings = result["settings"]
    lines = ["## Load: what an account costs", ""] + preamble(result)
    lines += [
        f"Each account logs in, lists its vehicles and runs {settings['cycles']} poll cycles "
        f"(realtime, GPS, charging status, HVAC status, energy consumption; {settings['requests_per_account']} requests "
        f"in all) with {settings['think_ms']} ms think time between cycles. The fake server runs in its own process and "
        f"delays every reply by {settings['server_delay_ms']} ms; pollable reads sleep {settings['poll_interval_s']} s before their poll. "
        "CPU is user + system seconds of the client process during the run, divided by accounts x cycles (so it includes "
        "the one-off login and vehicle list); with several runs the table shows the median run and the range. Memory is the peak resident size minus the size before the run, per account.",
        "",
    ]
    sizes = sorted({row["accounts"] for row in result["rows"]})
    for size in sizes:
        rows = [row for row in result["rows"] if row["accounts"] == size]
        lines += [f"### {size} accounts", ""]
        header = [
            "Implementation", "CPU ms / cycle", "Memory / account", "Peak RSS", "Requests / s", "Wall",
            "Threads (peak)", "Loop lag median / p99 / max", "Failed calls", "Requests seen / planned",
        ]  # fmt: skip
        body = []
        for row in rows:
            derived, measured, reported = row["derived"], row["measured"], row["reported"]
            lag = reported.get("loop_lag_ms")
            lag_text = f"{lag['median']:.1f} / {lag['p99']:.0f} / {lag['max']:.0f} ms" if lag else "n/a (no event loop)"
            repeats = row.get("repeats", {})
            cpu = repeats.get("cpu_ms_per_cycle")
            cpu_text = f"{derived['cpu_ms_per_cycle']:.1f}"
            if cpu and repeats["runs"] > 1:
                cpu_text += f" (range {cpu['min']:.1f}-{cpu['max']:.1f}, {repeats['runs']} runs)"
            body.append(
                [
                    row["label"],
                    cpu_text,
                    f"{derived['memory_per_account_kib']:.0f} KiB",
                    f"{measured['rss_peak_mib']:.0f} MiB",
                    f"{derived['requests_per_s']:.0f}",
                    f"{measured['wall_s']:.1f} s",
                    f"{measured['threads_peak']:.0f}",
                    lag_text,
                    str(sum(repeats.get("all_calls_failed", [reported["calls_failed"]]))),
                    f"{row['server']['requests']} / {row['server']['expected_requests']}"
                    + ("" if row["request_count_ok"] else (" (calls failed)" if sum(repeats.get("all_calls_failed", [0])) else " **MISMATCH**")),
                ]
            )
        lines += table(header, body) + [""]
    return lines


def render_mqtt(result: dict[str, Any]) -> list[str]:
    lines = ["## Idle MQTT connections", ""] + preamble(result)
    lines += [
        "N connections to the TLS test broker (a separate process), each subscribed to its own topic, then idle "
        f"({result['settings']['idle_window_s']} s window after {result['settings']['settle_s']} s to settle, keepalive "
        f"{result['settings']['keepalive_s']} s). **Certificate verification is off on both sides** (pyBYD's paho `tls_set` is "
        "patched in the benchmark's worker process only). Memory per connection is the idle resident size minus the size "
        "before connecting, over N; threads are those added by the connections.",
        "",
    ]
    header = ["Implementation", "Connections", "Memory / connection", "Idle RSS", "Peak RSS", "Threads (idle)", "Threads added", "Idle CPU", "Connected"]
    body = []
    for row in result["rows"]:
        measured, derived = row["measured"], row["derived"]
        body.append(
            [
                row["label"],
                str(row["count"]),
                f"{derived['memory_per_connection_kib']:.0f} KiB",
                f"{measured['rss_idle_mib']:.0f} MiB",
                f"{measured['rss_peak_mib']:.0f} MiB",
                f"{measured['threads_idle']:.0f}",
                f"{measured['threads_idle'] - measured['threads_before']:.0f}",
                f"{measured['idle_cpu_percent_of_one_core']:.2f}% of a core",
                f"{row['connected']} / {row['count']}",
            ]
        )
    return lines + table(header, body) + [""]


RENDERERS = {"micro": render_micro, "load": render_load, "mqtt_idle": render_mqtt}


def newest(name: str, quick: bool) -> Path | None:
    files = sorted(common.RESULTS.glob(f"{name}-*.json"))
    files = [f for f in files if f.stem.endswith("-quick") == quick]
    return files[-1] if files else None


def render(paths: list[Path]) -> str:
    out: list[str] = []
    for path in paths:
        result = json.loads(path.read_text(encoding="utf-8"))
        out += RENDERERS[result["benchmark"]](result) + [f"*Source: `bench/results/{path.name}`*", ""]
    return "\n".join(out)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("files", nargs="*", type=Path)
    parser.add_argument("--quick", action="store_true", help="use the newest quick results")
    parser.add_argument("--out", type=Path, help="write the Markdown here instead of printing it")
    args = parser.parse_args(argv)
    paths = args.files or [p for name in NAMES if (p := newest(name, args.quick)) is not None]
    if not paths:
        print("no results found in bench/results/", file=sys.stderr)
        return 1
    text = render(paths)
    if args.out:
        args.out.write_text(text, encoding="utf-8")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
