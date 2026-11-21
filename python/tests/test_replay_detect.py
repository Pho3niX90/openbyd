"""Run the detection replay against synthetic traffic and guarded drivers."""

from __future__ import annotations

import asyncio
from collections import Counter
from dataclasses import replace
import errno
import json
import os
from pathlib import Path
import subprocess
import sys
from typing import Any

import pytest

pytest.importorskip("openbyd.testing", exc_type=ImportError)
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "bench"))

import replay_detect as rd  # noqa: E402
from replay import scenario as scenario_io  # noqa: E402
import replay_worker as rw  # noqa: E402
import report  # noqa: E402

ROOT = rd.common.ROOT
DEMO = rw.DEFAULT_SCENARIO
REFERENCE = rd.reference_variant()


def args_for(path, *extra):
    return rd.parse_args(["--backend", str(ROOT), "--backend-python", sys.executable,
                          "--scratch", str(path), "--quick", "--step-timeout", "10", *extra])


@pytest.fixture(scope="module")
def demo_results(tmp_path_factory):
    changed = replace(REFERENCE, key="changed", env={"OPENBYD_TEST_REPLAY_DIFFERENCE": "1"})
    return rd.run(args_for(tmp_path_factory.mktemp("replay-demo")), {"reference": REFERENCE, "changed": changed})


def test_reference_repeats_and_variant_finding(demo_results):
    assert rd.status(demo_results) == 2
    assert demo_results["coverage"]["covered"] == demo_results["coverage"]["total"] == 3
    assert demo_results["determinism"]["reference"]["identical"] is True
    assert demo_results["determinism"]["changed"]["identical"] is True
    finding, = demo_results["comparison"]
    diff, = finding["differences"]
    assert (diff["kind"], diff["type"], diff["field"], diff["absolute_difference"]) == ("drives", "field", "end_odo", 0.125)
    aggregate, = finding["summary"]
    assert aggregate["min"] == aggregate["median"] == aggregate["max"] == 0.125
    assert aggregate["cars_affected"] == 1
    for row in demo_results["rows"]:
        assert not row["problems"]
        assert row["accounts"] == 3 and len(row["detections"]) == 4
        assert row["guard"]["reports"] == 3 and row["guard"]["blocked_total"] == 0
        assert row["counts"]["drives"] > 0 and row["counts"]["charges"] > 0
        assert row["replay"]["mqtt"]["pushes_sent"] == row["counters"]["pushes_applied"] > 0
        assert row["replay"]["mqtt"]["pushes_missed"] == 0
        assert row["replay"]["logins"] > 3
        assert row["replay"]["replay"]["sessions_expired"] == 3


def test_nondeterministic_run_withholds_comparison(tmp_path, capsys):
    unstable = replace(REFERENCE, key="unstable", env={"OPENBYD_TEST_REPLAY_RANDOM": "1"})
    results = rd.run(args_for(tmp_path, "--accounts", "acct-0001", "--until", "1600"),
                     {"reference": REFERENCE, "unstable": unstable})
    assert rd.status(results) == 1
    assert results["determinism"]["reference"]["identical"] is True
    assert results["determinism"]["unstable"]["identical"] is False
    assert results["comparison"] == []
    rd._print_summary(results)
    assert "unstable: NOT DETERMINISTIC" in capsys.readouterr().out


@pytest.mark.parametrize(
    ("mode", "problem"),
    [("refuse", "open seq 0: driver answered ok:false: deliberate error"),
     ("exit", "open seq 0: driver exited early"),
     ("stall", "step seq 1: no DRIVER answer within 1 s"),
     ("mismatch", "open: settings mismatch"),
     ("sequence", "open seq 0: wrong sequence in DRIVER answer")],
)
def test_driver_protocol_failure(tmp_path, mode, problem):
    (tmp_path / "fake_driver.py").write_text(
        """
import json, os, sys, time
mode = os.environ["TEST_DRIVER_MODE"]
request = json.loads(sys.stdin.readline())
print("unrelated stdout")
if mode == "exit":
    sys.exit(0)
if mode == "refuse":
    print('DRIVER {"seq":0,"ok":false,"error":"deliberate error"}', flush=True)
    sys.exit(0)
settings = {"native_cipher": mode != "mismatch"}
print("DRIVER " + json.dumps({"seq": 1 if mode == "sequence" else request["seq"], "ok": True,
                              "settings": settings, "driver": "stub", "mqtt": True}), flush=True)
if mode == "stall":
    sys.stdin.readline()
    time.sleep(60)
""", encoding="utf-8")
    stub = replace(REFERENCE, command=(*rd.DRIVER[:-1], "fake_driver"), env={"TEST_DRIVER_MODE": mode})
    results = rd.run(args_for(tmp_path, "--backend", str(tmp_path), "--accounts", "1", "--until", "0",
                             "--step-timeout", "1"), {"reference": stub})
    assert rd.status(results) == 1
    assert problem in results["rows"][0]["problems"][0]
    assert results["comparison"] == []


def test_split_schedule_covers_both_cars_and_all_error_times(tmp_path):
    scenario, accounts, coverage = rd.read_scenario(DEMO, None, None)
    assert coverage["window"] == {"start_s": 0, "end_s": 21600, "omitted_s": 0}
    account = accounts["acct-0001"]
    assert len(account.vins) == 2
    split = tmp_path / "account.jsonl"
    account.write(split, scenario["header"], 21600)
    summary = scenario_io.validate(split)
    rows = list(scenario_io.read(split))
    original = list(scenario_io.read(DEMO))
    assert summary.accounts == 1 and summary.cars == 2
    assert rows[0]["start"] == original[0]["start"]
    assert rows[1] == original[1]
    assert rows[2:] == [line for line in original if line.get("vin") in account.vins]
    schedule = {t: (pushes, poll) for t, pushes, poll in account.steps()}
    assert list(schedule) == sorted({line["t"] for line in account.timeline})
    assert schedule[0] == (0, True)
    assert schedule[906] == (2, True)
    expiry = next(line for line in account.timeline if line.get("code") == "1005")
    assert [line["type"] for line in account.timeline if line["t"] == expiry["t"]] == ["error"]
    assert schedule[expiry["t"]] == (0, True)
    assert Counter({t: pushes for t, (pushes, _) in schedule.items() if pushes}) == Counter(
        line["t"] for line in account.timeline if line["type"] == "push"
    )
    assert rd.select_accounts(["acct-0001..acct-0002,3"], list(accounts)) == set(accounts)
    assert rd.select_accounts(["2"], list(accounts)) == {"acct-0001", "acct-0002"}
    assert rd.select_accounts(["2-2"], list(accounts)) == {"acct-0002"}
    assert rd.select_accounts(["acct-1"], ["acct-1", "acct-0001"]) == {"acct-1"}
    with pytest.raises(ValueError, match="ambiguous"):
        rd.select_accounts(["1-1"], ["acct-1", "acct-0001"])


def test_exact_comparison_includes_nested_values_missing_extra_and_open_sessions():
    start = "2026-01-05T00:00:00Z"
    a = {"test": {"drives": [{"start": start, "energy_split": {"ac": 1.0}, "km": 1},
                             {"start": "2026-01-05T01:00:00Z"}],
                  "open_drive": [{"start": start, "start_odo": 10}]}}
    b = {"test": {"drives": [{"start": start, "energy_split": {"ac": 1.0000001}, "km": 1.0000001},
                             {"start": "2026-01-05T02:00:00Z"}],
                  "open_drive": [{"start": "2026-01-05T00:00:01Z", "start_odo": 10}]}}
    diffs = rd.compare_detections(a, b)
    assert Counter(diff["type"] for diff in diffs) == Counter(field=3, missing=1, extra=1)
    assert {diff["field"] for diff in diffs if diff["type"] == "field"} == {"km", "start", "energy_split.ac"}
    opened, = [diff for diff in diffs if diff["kind"] == "open_drive"]
    assert opened["unit"] == "s" and opened["absolute_difference"] == 1
    km, = [diff for diff in diffs if diff["field"] == "km"]
    assert km["absolute_difference"] == 1.0000001 - 1
    assert rd.compare_detections(a, a) == []


def test_published_names_are_names_not_data():
    assert rd._public_key("end_odo") == "end_odo"
    assert rd._public_key("energy_split.ac") == "energy_split.ac"
    assert rd._public_key("route_points[12].lat") == "route_points[].lat"
    for data in ("energy_split.1767571200", "samples.2026-01-05", "TESTVIN0000000001", "notes.acct-0001", "a b", "x" * 121):
        assert rd._public_key(data) == "(other)"
    assert rd._public_count_key("/control/getGpsInfo") == "/control/getGpsInfo"
    assert rd._public_count_key("1008") == "1008" and rd._public_count_key("byd_code_1008") == "byd_code_1008"
    for data in ("1767571200", "2026-01-05T00:00:00Z", "TESTVIN0000000001", "/acct-0001"):
        assert rd._public_count_key(data) == "(other)"


def test_a_dead_driver_is_reaped_without_losing_the_log(tmp_path):
    child = rd.common.Child([sys.executable, "-c", "pass"], "dead", log=tmp_path / "dead.log")
    child.proc.wait()
    try:
        for _ in range(3):
            child.send("x" * 70000)
    except OSError:
        pass
    child.reap()
    assert child._stderr.closed and child.proc.stdout.closed


def test_native_worker_is_one_variant_row():
    variant = rd.Variant("native", "Native worker", ("{backend}/target/release/byd-worker", "--replay-driver"),
                         expect={"protocol_engine": "native"}, detections="driver")
    variants = {**rd.VARIANTS, variant.key: variant}
    assert rw.render_command(variants["native"], {"backend": "/backend"}) == ["/backend/target/release/byd-worker", "--replay-driver"]
    assert set(rd.VARIANTS) == set(rw.VARIANTS)
    for key in rd.VARIANTS:
        assert rd.VARIANTS[key].label == rw.VARIANTS[key].label
        assert rd.VARIANTS[key].env == rw.VARIANTS[key].env


def test_private_results_and_public_document(tmp_path, capsys):
    source = tmp_path / "synthetic-export.jsonl"
    header, *lines = scenario_io.read(DEMO)
    header.update(source="export")
    del header["seed"]
    scenario_io.write(source, [header, *lines])
    # A whole account with a planted difference, so the public document has detections, counters
    # from a real driver and a finding to redact.
    changed = replace(REFERENCE, key="changed", env={"OPENBYD_TEST_REPLAY_DIFFERENCE": "1"})
    args = args_for(tmp_path / "scratch", "--scenario", str(source), "--accounts", "acct-0001")
    results = rd.run(args, {"reference": REFERENCE, "changed": changed})
    assert rd.status(results) == 2
    assert results["rows"][0]["counts"]["drives"] > 0
    results["rows"][0]["problems"].append(f"TESTVIN0000000001 {source.name} {header['start']}")
    # Keys that are data, not names: a counter and a compared field keyed by a time and by a car.
    results["rows"][0]["counters"]["TESTVIN0000000001"] = 1
    results["rows"][0]["counters"][header["start"]] = 1
    results["comparison"][0]["differences"].append(
        {"vin": "TESTVIN0000000001", "kind": "drives", "type": "field", "field": "energy_split.1767571200",
         "absolute_difference": 1.0, "unit": None, "reference": 1.0, "other": 2.0})
    output = rd.write(results, args)
    public = json.loads(output.read_text(encoding="utf-8"))
    forbidden = {"vin", "detections", "logs", "all_accounts", "missing_accounts", "after_window", "differences", "start", "file",
                 "reference_value", "sha256"}
    epoch = int(rd.datetime.fromisoformat(header["start"].replace("Z", "+00:00")).timestamp())

    def walk(item):
        if isinstance(item, dict):
            assert not forbidden.intersection(item)
            for key, value in item.items():
                walk(key)
                walk(value)
        if isinstance(item, list):
            for value in item:
                walk(value)
        if isinstance(item, str):
            assert all(value not in item for value in ("TESTVIN", "acct-0001", source.name, header["start"], str(epoch)))
        if type(item) in (int, float):
            # No time of a drive: nothing within the scenario's span, in seconds or milliseconds.
            assert not epoch <= item <= epoch + header["duration_s"]
            assert not epoch * 1000 <= item <= (epoch + header["duration_s"]) * 1000

    walk(public)
    finding, = public["comparison"]
    assert {(item["kind"], item["field"]) for item in finding["summary"]} == {("drives", "end_odo"), ("drives", "(other)")}
    assert finding["cars_differing"] == 1 and "differences" not in finding
    row = public["rows"][0]
    assert row["counters"]["(other)"] == 2 and row["counters"]["polls"] > 0
    assert row["replay"]["by_endpoint"] and all(key.startswith("/") for key in row["replay"]["by_endpoint"])
    assert row["replay"]["by_code"] and all(key.isdecimal() for key in row["replay"]["by_code"])
    assert row["counts"]["drives"] > 0 and row["steps_without_mqtt"] == 0
    with pytest.raises(SystemExit, match="complete results file"):
        report.render_replay_detect(results)
    complete, = (tmp_path / "results").glob("*.json")
    assert json.loads(complete.read_text(encoding="utf-8"))["coverage"]["accounts"] == ["acct-0001"]
    assert len(list((tmp_path / "work").rglob("driver.log"))) == 4
    assert not list((tmp_path / "work").rglob("*.jsonl"))
    assert "acct-0001" not in capsys.readouterr().out
    with pytest.raises(ValueError, match="outside the repository"):
        rw.private_directory(ROOT / "bench")


def test_merge_disjoint_chunks_recomputes_results(tmp_path):
    changed = replace(REFERENCE, key="changed", env={"OPENBYD_TEST_REPLAY_DIFFERENCE": "1"})
    variants = {"reference": REFERENCE, "changed": changed}

    def run_chunk(*selectors):
        return rd.run(args_for(tmp_path, "--until", "1600", *selectors), variants)

    full = run_chunk()
    first = run_chunk("--accounts", "acct-0001")
    second = run_chunk("--accounts", "2-3")
    merged = rd.merge_results([second, first])
    assert merged["coverage"] == full["coverage"]
    assert merged["coverage"]["covered"] == merged["coverage"]["total"] == 3
    assert merged["determinism"] == full["determinism"]
    assert merged["comparison"] == full["comparison"]
    assert rd.status(merged) == 2
    for expected, actual in zip(full["rows"], merged["rows"]):
        for key in ("steps", "accounts", "detections", "counters", "replay", "guard"):
            assert actual[key] == expected[key]
        assert actual["counts"]["drives"] > 0
    partial = rd.merge_results([second])
    assert partial["coverage"]["covered"] == 2 and partial["coverage"]["missing_accounts"] == ["acct-0001"]
    assert partial["coverage"]["window"]["end_s"] == 1600
    with pytest.raises(ValueError, match="overlaps"):
        rd.merge_results([first, first])
    with pytest.raises(ValueError, match="settings differ"):
        rd.merge_results([first, {**second, "settings": {**second["settings"], "store": "other"}}])
    with pytest.raises(ValueError, match="backend packages"):
        rd.merge_results([first, {**second, "backend": {**second["backend"], "packages": {}}}])
    with pytest.raises(ValueError, match="backend backend_dirty"):
        rd.merge_results([first, {**second, "backend": {**second["backend"], "backend_dirty": not second["backend"]["backend_dirty"]}}])
    with pytest.raises(ValueError, match="openbyd_commit differs"):
        rd.merge_results([{**first, "versions": {"openbyd_commit": "a"}}, {**second, "versions": {"openbyd_commit": "b"}}])
    with pytest.raises(ValueError, match="redacted"):
        rd.merge_results([rd.publishable(first), second])
    for i, chunk in enumerate((first, second), 1):
        (tmp_path / f"chunk-{i}.json").write_text(json.dumps(chunk), encoding="utf-8")
    assert rd.main(["--quick", "--scratch", str(tmp_path), "--merge",
                    str(tmp_path / "chunk-1.json"), str(tmp_path / "chunk-2.json")]) == 2
    complete, = (tmp_path / "complete").glob("*.json")
    assert json.loads(complete.read_text(encoding="utf-8"))["coverage"]["covered"] == 3


def test_report_and_cli(tmp_path, demo_results):
    published = rd.publishable(demo_results)
    results_file = tmp_path / "results.json"
    results_file.write_text(json.dumps({"benchmark": "replay_detect", **published}), encoding="utf-8")
    markdown = report.render([results_file])
    assert "## Deterministic detection replay" in markdown
    assert "Identical across repeats" in markdown and "| Yes |" in markdown
    assert "Differences between variants" in markdown and "end_odo" in markdown and "0.125" in markdown
    done = subprocess.run([sys.executable, str(ROOT / "bench" / "replay_detect.py"), "--reference", "--quick",
                           "--scratch", str(tmp_path), "--until", "0"], text=True, capture_output=True, timeout=30)
    assert done.returncode == 0, done.stderr + done.stdout
    assert "reference: identical across repeats" in done.stdout
    assert len(list((tmp_path / "results").glob("replay_detect-*.json"))) == 1


def test_backend_driver_on_demo(tmp_path):
    backend = os.environ.get("OPENBYD_REPLAY_BACKEND")
    interpreter = os.environ.get("OPENBYD_REPLAY_BACKEND_PYTHON")
    if not backend or not interpreter or not (Path(backend) / "scripts" / "replay_driver.py").is_file():
        pytest.skip("requires the backend checkout and Python with scripts/replay_driver.py")
    args = args_for(tmp_path, "--backend", backend, "--backend-python", interpreter, "--local-store",
                    "--variants", "python_cipher", "rust_cipher", "--repeats", "2", "--step-timeout", "60")
    results = rd.run(args)
    assert not any(row["problems"] for row in results["rows"]), [row["problems"] for row in results["rows"]]
    assert all(entry["identical"] is True for entry in results["determinism"].values()), rd.publishable(results)["determinism"]
    assert all(row["counts"]["drives"] for row in results["rows"])


# ---------------------------------------------------------------------------
# The launcher's retry of local connects the machine failed (--retry-local-connects)
# ---------------------------------------------------------------------------


def _winerror(code: int) -> OSError:
    exc = OSError(f"[WinError {code}] simulated")
    exc.winerror = code  # type: ignore[attr-defined]
    return exc


def _retrying(monkeypatch, outcomes: list) -> tuple[Any, Any, list]:
    """The wrapped start_connection over a fake that fails with ``outcomes`` in turn."""
    aiohappyeyeballs = pytest.importorskip("aiohappyeyeballs")
    from replay import launcher

    calls: list = []

    async def fake(addr_infos, *args, **kwargs):
        calls.append(addr_infos[0][4])
        outcome = outcomes.pop(0)
        if isinstance(outcome, BaseException):
            raise outcome
        return outcome

    monkeypatch.setattr(aiohappyeyeballs, "start_connection", fake)
    monkeypatch.setattr(launcher, "CONNECT_ATTEMPTS", 3)
    guard = launcher.Guard()
    assert launcher.install_connect_retry(guard)
    return aiohappyeyeballs.start_connection, guard, calls


LOOPBACK = [(2, 1, 6, "", ("127.0.0.1", 50123))]


def test_a_local_connect_the_machine_failed_is_tried_again(monkeypatch):
    start, guard, calls = _retrying(monkeypatch, [_winerror(52), _winerror(10048), "socket"])
    assert asyncio.run(start(LOOPBACK)) == "socket"
    assert len(calls) == 3
    assert (guard.connect_retries, guard.connect_failures) == (2, 0)
    assert guard.report()["connect_retries"] == 2


def test_a_local_connect_that_keeps_failing_is_counted_and_raised(monkeypatch):
    start, guard, calls = _retrying(monkeypatch, [_winerror(52)] * 3)
    with pytest.raises(OSError, match="WinError 52"):
        asyncio.run(start(LOOPBACK))
    assert len(calls) == 3
    assert (guard.connect_retries, guard.connect_failures) == (2, 1)


@pytest.mark.parametrize(
    "addr_infos, error",
    [
        (LOOPBACK, ConnectionRefusedError(errno.ECONNREFUSED, "refused by the peer")),  # the server said no
        ([(2, 1, 6, "", ("192.0.2.1", 443))], _winerror(52)),  # not this machine
    ],
)
def test_only_local_resource_failures_on_loopback_are_retried(monkeypatch, addr_infos, error):
    start, guard, calls = _retrying(monkeypatch, [error, "socket"])
    with pytest.raises(OSError):
        asyncio.run(start(addr_infos))
    assert len(calls) == 1
    assert (guard.connect_retries, guard.connect_failures) == (0, 0)


def test_the_backend_driver_runs_with_the_connect_retry():
    assert "--retry-local-connects" in rd.DRIVER
    assert rd.DRIVER.index("--retry-local-connects") < rd.DRIVER.index("-m")
