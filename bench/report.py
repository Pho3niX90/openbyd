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

NAMES = ("micro", "load", "mqtt_idle", "replay_worker")


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


def _seconds(value: float | None) -> str:
    return "-" if value is None else duration(value)


def _lag(summary: dict[str, Any]) -> str:
    if not summary.get("count"):
        return "-"
    bound = summary.get("max_bound")
    top = f"{bound[0]} {duration(bound[1])}" if bound else "-"
    return f"{_seconds(summary['p50'])} / {_seconds(summary['p99'])} / {top}"


def _range(figures: dict[str, Any], key: str, fmt: str) -> str:
    figure = figures.get(key)
    if figure is None:
        return "-"
    text = format(figure["median"], fmt)
    if figure["n"] > 1:
        text += f" ({format(figure['min'], fmt)}-{format(figure['max'], fmt)})"
    return text


def _median_run(rows: list[dict[str, Any]]) -> dict[str, Any]:
    ranked = sorted(rows, key=lambda row: row["measured"]["cpu_s"])
    return ranked[len(ranked) // 2]


def _cell(value: Any) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:g}"
    return str(value)


def _difference_row(diff: dict[str, Any]) -> list[str]:
    if diff["type"] == "field":
        what = f"{diff['field']} (start {_cell(diff['start'])})"
        return [diff["kind"], diff["vin"], "field", what, _cell(diff["reference"]), _cell(diff["other"]), f"{diff['tolerance']:g}"]
    if diff["type"] == "count":
        return [diff["kind"], diff["vin"], "count", "-", _cell(diff["reference"]), _cell(diff["other"]), "0"]
    item = diff["reference"] or diff["other"]
    what = f"start {_cell(item.get('start'))}, end {_cell(item.get('end'))}"
    return [
        diff["kind"], diff["vin"], diff["type"], what,
        "present" if diff["reference"] else "-", "present" if diff["other"] else "-", "-",
    ]  # fmt: skip


def render_replay_worker(result: dict[str, Any]) -> list[str]:
    settings, scenario, backend = result["settings"], result["scenario"], result["backend"]
    header, counts = scenario["header"], scenario["counts"]
    packages = backend.get("packages", {})
    lines = ["## Replay: the backend's mqtt-worker against recorded BYD behaviour", ""] + preamble(result)
    lines += [
        f"Scenario `{scenario['file']}` ({header.get('source')}, {counts.get('account', 0)} accounts, {counts.get('car', 0)} cars, "
        f"{counts.get('state', 0)} states, {counts.get('push', 0)} pushes, {counts.get('error', 0)} errors over "
        f"{header.get('duration_s', 0) / 3600:.1f} h), replayed from second {settings['start_at']} at speed {settings['speed']:g} "
        f"for {settings['minutes']:g} min of wall time per run, {settings['repeats']} run(s) per variant, interleaved. "
        f"Backend `{backend.get('backend_commit', '?')[:10]}` ({backend.get('backend_branch')})"
        f"{' with uncommitted changes' if backend.get('backend_dirty') else ''}, Python {backend.get('backend_python')}, "
        f"pyBYD {packages.get('pybyd')}, openbyd {packages.get('openbyd')}. Store: {settings['store']}.",
        "",
        "CPU is user + system seconds of the worker process from the replay's start to the end of the window (logins "
        "before the start are not in it), per account. Memory per account is the peak resident size over accounts. "
        "Loop lag is the worker's own `bydconnect_worker_event_loop_lag_seconds` histogram over the window: median / p99 "
        "interpolated within buckets, and the bucket bound of the largest wake-up. With several runs the cells show the "
        "median and the range; the other columns are from the median run.",
        "",
    ]
    by_variant: dict[str, list[dict[str, Any]]] = {}
    for row in result["rows"]:
        by_variant.setdefault(row["variant"], []).append(row)
    header_row = [
        "Variant", "CPU s / account", "CPU ms / account / min", "Peak RSS (MiB)", "Memory / account (KiB)", "Threads",
        "Loop lag p50 / p99 / max", "Hydrates (mean)", "BYD requests", "Logins", "Pushes sent / missed",
        "Polls stored", "MQTT rows", "Drives", "Charges", "Round trip", "Guard blocked",
    ]  # fmt: skip
    body = []
    for key, rows in by_variant.items():
        figures = result["variants"][key]["figures"]
        run = _median_run(rows)
        metrics, stats = run["metrics"], run["replay"]["stats"]
        window = run["replay"].get("window", stats)
        hydrates = metrics["hydrate_seconds"]["all"]
        mqtt = window.get("mqtt", {})
        rate = run["roundtrip"]["match_rate"]
        body.append(
            [
                run["label"] + (" **(problems)**" if any(r["problems"] for r in rows) else ""),
                _range(figures, "derived.cpu_s_per_account", ".2f"),
                _range(figures, "derived.cpu_ms_per_account_minute", ".1f"),
                _range(figures, "measured.rss_peak_mib", ".0f"),
                _range(figures, "derived.memory_per_account_kib", ".0f"),
                _range(figures, "measured.threads_peak", ".0f"),
                _lag(metrics["loop_lag_s"]),
                f"{hydrates['count']:.0f} ({_seconds(hydrates['mean'])})",
                str(window.get("requests")),
                str(stats.get("logins")),
                f"{mqtt.get('pushes_sent')} / {mqtt.get('pushes_missed')}",
                str(run["store"]["polls_ok"]),
                str(run["store"]["mqtt_rows"]),
                str(run["detection_counts"]["drives"]),
                str(run["detection_counts"]["charges"]),
                "-" if rate is None else f"{rate:.1%} of {run['roundtrip']['values_checked']}",
                str(run["guard"].get("blocked_total")),
            ]
        )
    lines += table(header_row, body) + [""]

    lines += ["### Detected drives and charges", ""]
    tolerances = settings["tolerances"]
    lines += [
        "Every run's closed drives and charges, per VIN, compared with the first run's. Tolerances (replay seconds; km, "
        "kWh and SoC in their units): " + ", ".join(f"{name} {value:g}" for name, value in tolerances.items()) + ". "
        "Each difference is listed; none is averaged away.",
        "",
    ]
    for entry in result["comparison"]:
        if entry["equal"]:
            lines += [f"- {entry['other']} vs {entry['reference']}: **same** drives and charges.", ""]
            continue
        count = entry.get("difference_count", len(entry["differences"]))
        lines += [f"- {entry['other']} vs {entry['reference']}: **{count} difference(s)**", ""]
        if "difference_summary" in entry:
            # A scenario exported from production: how many of each sort, never which car.
            lines += table(
                ["Difference", "Count"], [[name, str(number)] for name, number in entry["difference_summary"].items()]
            ) + ["", f"  On {entry['cars_differing']} car(s).", ""]
        else:
            lines += table(
                ["Kind", "VIN", "Difference", "What", "Reference", "Other", "Tolerance"],
                [_difference_row(diff) for diff in entry["differences"]],
            ) + [""]
    if not result["comparison"]:
        lines += ["Only one run: nothing to compare.", ""]
    first = result["rows"][0] if result["rows"] else None
    if result.get("redacted"):
        lines += [
            "The scenario was exported from production, so the drives and charges themselves are not published: "
            "the counts are in the table above, the lists stay with the scenario.",
            "",
        ]
    elif first:
        lines += [f"Detections of {first['variant']}#{first['repeat']} (replay seconds):", ""]
        rows = []
        for kind, per_vin in first["detections"].items():
            for vin, items in per_vin.items():
                for item in items:
                    figures = ", ".join(f"{k} {_cell(v)}" for k, v in item.items() if k not in ("start", "end"))
                    rows.append([kind[:-1], vin, _cell(item["start"]), _cell(item["end"]), figures])
        lines += (table(["Kind", "VIN", "Start", "End", "Figures"], rows) if rows else ["None."]) + [""]

    lines += [
        "### BYD requests by endpoint (median run)",
        "",
        "Requests during the measured window; the logins and first polls before the replay started are not in it.",
        "",
    ]
    keys = list(by_variant)
    medians = {key: (lambda replay: replay.get("window", replay["stats"]))(_median_run(by_variant[key])["replay"]) for key in keys}
    endpoints = sorted({name for stats in medians.values() for name in stats.get("by_endpoint", {})})
    codes = sorted({code for stats in medians.values() for code in stats.get("by_code", {})})
    if endpoints:
        lines += table(
            ["Endpoint", *keys],
            [[f"`{name}`", *(str(medians[key].get("by_endpoint", {}).get(name, 0)) for key in keys)] for name in endpoints],
        ) + [""]
        lines += table(
            ["Code", *keys],
            [[code, *(str(medians[key].get("by_code", {}).get(code, 0)) for key in keys)] for code in codes],
        ) + [""]

    lines += ["### Replay round trip", ""]
    lines += [
        f"Stored successful polls (a sample of up to {settings['poll_sample']}) compared with the scenario's state "
        f"within {settings['roundtrip_window_s']:g} s before they were stored; offline cars only on `online`.",
        "",
    ]
    rows = []
    for row in result["rows"]:
        rt = row["roundtrip"]
        fields = ", ".join(f"{name} {v['matched']}/{v['checked']}" for name, v in rt["by_field"].items() if v["checked"])
        rate = "-" if rt["match_rate"] is None else f"{rt['match_rate']:.1%}"
        rows.append([f"{row['variant']}#{row['repeat']}", str(rt["polls_checked"]), rate, fields or "-"])
    lines += table(["Run", "Polls checked", "Match", "By field (matched / checked)"], rows) + [""]
    mismatches = [(row, m) for row in result["rows"] for m in row["roundtrip"]["mismatches"]]
    if mismatches:
        lines += table(
            ["Run", "VIN", "t", "Endpoint", "Field", "Stored", "Scenario"],
            [
                [f"{row['variant']}#{row['repeat']}", m["vin"], _cell(m["t"]), m["endpoint"], m["field"], _cell(m["stored"]), _cell(m["expected"])]
                for row, m in mismatches[:30]
            ],
        ) + [""]

    lines += ["### Network guard", ""]
    blocked = [(row, item) for row in result["rows"] for item in row["guard"].get("blocked", [])]
    if blocked:
        lines += ["The worker tried to reach these non-loopback addresses; every attempt was refused:", ""]
        lines += table(
            ["Run", "Via", "Host", "Port", "Attempts"],
            [[f"{row['variant']}#{row['repeat']}", i["via"], i["host"], i["port"], str(i["count"])] for row, i in blocked],
        ) + [""]
    else:
        lines += ["No attempt to reach a non-loopback address.", ""]
    problems = [f"{row['variant']}#{row['repeat']}: {p}" for row in result["rows"] for p in row["problems"]]
    if problems:
        lines += ["**Problems:**", ""] + [f"- {p}" for p in problems] + [""]
    return lines


RENDERERS = {"micro": render_micro, "load": render_load, "mqtt_idle": render_mqtt, "replay_worker": render_replay_worker}


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
