"""Step detection drivers through one replay clock per account and compare their results exactly."""

from __future__ import annotations

import argparse
import asyncio
from collections import Counter, defaultdict
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from dataclasses import dataclass, field
from datetime import datetime, timezone
from hashlib import file_digest, sha256
from itertools import chain, combinations, groupby
import json
import math
import os
from pathlib import Path
from queue import Empty, Queue
import re
import secrets
from statistics import median
from subprocess import TimeoutExpired
import sys
import tempfile
from threading import Event, Thread
import time
from typing import Any, Iterator

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402
import replay_worker as rw  # noqa: E402
from replay import scenario as scenario_io  # noqa: E402

DRIVER = ("{python}", "{launcher}", "--guard-report", "{guard_report}", "--retry-local-connects", "-m", "scripts.replay_driver")
KINDS = ("drives", "charges", "open_drive", "open_charge")
PAIR_WINDOW_S = 300
MAX_PROCESSES = 8
#: The run's Postgres: a name and port of its own, so it can never collide with the real-time
#: harness's containers. One run at a time; a container left by a killed run is not removed
#: unasked (it may be a run in progress): ``docker rm -f openbyd-detect-pg``.
DETECT_PG_CONTAINER = "openbyd-detect-pg"
DETECT_PG_PORT = 55433


@dataclass(frozen=True)
class Variant(rw.Variant):
    """A guarded driver command, its settings, and where to read its closed detections."""

    expect: dict[str, Any] = field(default_factory=dict)
    detections: str = "database"
    seed_database: bool = True


VARIANTS = {
    key: Variant(key, worker.label, DRIVER, worker.env, expect)
    for key, worker, expect in (
        ("python_cipher", rw.VARIANTS["python_cipher"], {"native_cipher": False, "shadow_sample_rate": 0.0}),
        ("rust_cipher", rw.VARIANTS["rust_cipher"], {"native_cipher": True, "shadow_sample_rate": 0.0}),
        ("rust_cipher_shadow", rw.VARIANTS["rust_cipher_shadow"], {"native_cipher": True, "shadow_sample_rate": 1.0}),
    )
}

# Every other column is extracted and compared, including optional and JSON columns and the
# backend's own drive and charge ids (they are made from the start time and odometers, not random).
SKIP_COLUMNS = {
    "drives": {
        "id": "random database row id",
        "car_id": "foreign key, replaced by the scenario VIN",
        "start_address": "reverse-geocoded text (the lookup is refused by the guard)",
        "end_address": "reverse-geocoded text (the lookup is refused by the guard)",
    },
    "charges": {
        "id": "random database row id",
        "car_id": "foreign key, replaced by the scenario VIN",
        "start_address": "reverse-geocoded text (the lookup is refused by the guard)",
    },
}


@dataclass
class Account:
    """An account's scenario lines and the commands scheduled from them."""

    definition: dict[str, Any]
    cars: list[dict[str, Any]] = field(default_factory=list)
    timeline: list[dict[str, Any]] = field(default_factory=list)

    @property
    def vins(self) -> list[str]:
        """VINs in account definition order."""
        return self.definition["cars"]

    def steps(self) -> Iterator[tuple[int, int, bool]]:
        """Yield each distinct time, its push count and whether it calls for a poll."""
        for t, lines in groupby(self.timeline, key=lambda line: line["t"]):
            counts = Counter(line["type"] for line in lines)
            yield t, counts["push"], bool(counts["state"] or counts["error"])

    def write(self, path: Path, header: dict[str, Any], until: int) -> None:
        """Write a standalone scenario with unchanged line order."""
        scenario_io.write(path, iter_account(self, header, until))


def iter_account(account: Account, header: dict[str, Any], until: int) -> Iterator[dict[str, Any]]:
    """Yield the split header, definitions and timeline in scenario order."""
    yield {**header, "accounts": 1, "cars": len(account.cars), "duration_s": until}
    yield account.definition
    yield from account.cars
    yield from account.timeline


def select_accounts(tokens: list[str] | None, names: list[str]) -> set[str]:
    """Resolve a first-N count or an explicit list of account names, numbers and inclusive ranges."""
    if tokens is None:
        return set(names)
    if len(tokens) == 1 and tokens[0].isdecimal():
        count = int(tokens[0])
        if 1 <= count <= len(names):
            return set(names[:count])
        raise ValueError("--accounts N must be between 1 and the scenario's account count")

    by_number = defaultdict(list)
    for name in names:
        by_number[int(name.split("-")[1])].append(name)
    known_names = set(names)
    selected = set()
    for token in re.split(r"[,\s]+", " ".join(tokens).strip()):
        if token in known_names:
            selected.add(token)
            continue
        match = re.fullmatch(r"(?:acct-)?(\d+)(?:(?:\.\.|-)(?:acct-)?(\d+))?", token)
        if match is None:
            raise ValueError("invalid --accounts selector; use names, numbers or inclusive ranges")
        first = int(match[1])
        last = int(match[2]) if match[2] else first
        if first > last or last - first >= len(names):
            raise ValueError("--accounts range is reversed or exceeds the scenario's account count")
        for number in range(first, last + 1):
            if number not in by_number:
                raise ValueError("--accounts selects an account absent from the scenario")
            if len(by_number[number]) > 1:
                raise ValueError("--accounts number is ambiguous; use the full account name")
            selected.add(by_number[number][0])
    return selected


def read_scenario(path: Path, tokens: list[str] | None, until: int | None) -> tuple[dict, dict[str, Account], dict]:
    """Read selected timelines in one pass and record what the account and time filters excluded."""
    rows = scenario_io.read(path)
    header = next(rows, {})
    if header.get("type") != "scenario" or header.get("version") != 1:
        raise ValueError("expected a version 1 replay scenario")
    duration = header["duration_s"]
    until = duration if until is None else until
    if not 0 <= until <= duration:
        raise ValueError("--until must be within the scenario duration")
    accounts = {}
    for row in rows:
        if row["type"] != "account":
            break
        account = row["account"]
        if account in accounts:
            raise ValueError("duplicate scenario account")
        accounts[account] = Account(row)
    else:
        row = None
    if not accounts or header["accounts"] != len(accounts):
        raise ValueError("scenario has no accounts or its account count is wrong")
    selected = select_accounts(tokens, list(accounts))
    owners = {vin: name for name, account in accounts.items() for vin in account.vins}
    counts = Counter(account=len(accounts))
    after_window: dict[str, Counter] = defaultdict(Counter)
    previous_t = -1
    if row is not None:
        for line in chain((row,), rows):
            kind = line["type"]
            counts[kind] += 1
            owner = owners.get(line.get("vin"))
            if owner is None:
                raise ValueError("a scenario line names a car that no account owns")
            if kind == "car":
                accounts[owner].cars.append(line)
                continue
            t = line["t"]
            if kind not in ("state", "error", "push") or not previous_t <= t <= duration:
                raise ValueError("invalid scenario timeline order or type")
            previous_t = t
            if owner in selected:
                if t <= until:
                    accounts[owner].timeline.append(line)
                else:
                    after_window[owner][kind] += 1
    if counts["car"] != header["cars"]:
        raise ValueError("scenario car count is wrong")
    with path.open("rb") as handle:
        fingerprint = file_digest(handle, "sha256").hexdigest()
    scenario = {"file": path.name, "header": header, "counts": dict(counts), "sha256": fingerprint}
    coverage = {
        "all_accounts": list(accounts),
        "accounts": [name for name in accounts if name in selected],
        "window": {"start_s": 0, "end_s": until, "omitted_s": duration - until},
        "after_window": {name: dict(after_window[name]) for name in selected},
    }
    return scenario, {name: account for name, account in accounts.items() if name in selected}, coverage


class DriverError(RuntimeError):
    """A failed driver command or an invalid driver answer."""


class Driver:
    """One driver process, with a deadline and sequence check on each answer."""

    def __init__(self, child: common.Child, timeout: float) -> None:
        self.child = child
        self.timeout = timeout
        self.seq = 0

    def _send(self, line: str, deadline: float) -> None:
        if len(line) < 4096:
            self.child.send(line)
            return
        # An open with many VINs can fill a pipe before a stalled driver reads it.
        sent: Queue[OSError | None] = Queue()

        def write() -> None:
            try:
                self.child.send(line)
            except OSError as exc:
                sent.put(exc)
            else:
                sent.put(None)

        Thread(target=write, daemon=True).start()
        try:
            problem = sent.get(timeout=max(0, deadline - time.monotonic()))
        except Empty:
            raise TimeoutError from None
        if problem is not None:
            raise problem

    def ask(self, op: str, **fields: Any) -> dict[str, Any]:
        """Send one command and return the matching successful response."""
        label = f"{op} seq {self.seq}"
        deadline = time.monotonic() + self.timeout
        try:
            self._send(json.dumps({"op": op, "seq": self.seq, **fields}, allow_nan=False), deadline)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError
                line = self.child.read(remaining)
                if line.startswith("DRIVER "):
                    break
            answer = json.loads(rw._line(line, "DRIVER "), parse_constant=_bad_constant, object_pairs_hook=scenario_io._unique)
        except TimeoutError:
            raise DriverError(f"{label}: no DRIVER answer within {self.timeout:g} s") from None
        except (RuntimeError, OSError, ValueError) as exc:
            if isinstance(exc, (OSError, RuntimeError)):
                raise DriverError(f"{label}: driver exited early or pipe failed ({exc})") from exc
            raise DriverError(f"{label}: invalid JSON answer ({exc})") from exc
        if not isinstance(answer, dict) or type(answer.get("seq")) is not int or answer["seq"] != self.seq:
            raise DriverError(f"{label}: wrong sequence in DRIVER answer")
        if answer.get("ok") is False:
            raise DriverError(f"{label}: driver answered ok:false: {answer.get('error')}")
        if answer.get("ok") is not True:
            raise DriverError(f"{label}: missing boolean ok in DRIVER answer")
        self.seq += 1
        return answer

    def finish(self) -> None:
        """Require EOF shutdown, exit 0, and exactly one answer per command."""
        try:
            code = self.child.finish(self.timeout)
        except TimeoutExpired:
            raise DriverError(f"finish: driver did not exit within {self.timeout:g} s after EOF") from None
        if code:
            raise DriverError(f"finish: driver exited with status {code}")
        self.child.reader.join()
        while not self.child.lines.empty():
            line = self.child.lines.get_nowait()
            if line is not None and line.startswith("DRIVER "):
                raise DriverError("finish: unexpected extra DRIVER answer")


def _bad_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON constant {value}")


def _receive_finish(answer: dict, vins: list[str], source: str) -> tuple[dict, dict]:
    detections = {vin: {kind: [] for kind in KINDS} for vin in vins}
    opened, counters = answer.get("open"), answer.get("counters")
    if not isinstance(opened, dict) or set(opened) != set(vins):
        raise DriverError("finish: open must contain every car on the account")
    for vin, sessions in opened.items():
        if not isinstance(sessions, dict) or set(sessions) != {"drive", "charge"}:
            raise DriverError("finish: each car must have drive and charge entries in open")
        for kind, session in sessions.items():
            if session is not None:
                if not isinstance(session, dict):
                    raise DriverError("finish: an open session must be an object or null")
                detections[vin]["open_" + kind].append(session)
    if not isinstance(counters, dict) or any(type(value) is not int or value < 0 for value in counters.values()):
        raise DriverError("finish: counters must be a flat object of non-negative integers")
    closed = answer.get("detections") if source == "driver" else {}
    if source == "driver" and (not isinstance(closed, dict) or set(closed) != {"drives", "charges"}):
        raise DriverError("finish: detections must contain drives and charges")
    _add_closed(detections, closed)
    return detections, counters


def _add_closed(detections: dict, closed: dict) -> None:
    for kind, items in closed.items():
        if not isinstance(items, list) or any(not isinstance(item, dict) or item.get("vin") not in detections for item in items):
            raise DriverError(f"invalid {kind} list or a detection for a car outside the run")
        for item in items:
            detections[item["vin"]][kind].append({key: value for key, value in item.items() if key != "vin"})


def _push_counts(stats: dict) -> tuple[int, int]:
    mqtt = stats["mqtt"]
    # The broker counts a push once however many connections it reached, so the count is the
    # driver's only with a single connection.
    if mqtt["open"] > 1:
        raise DriverError(f"{mqtt['open']} MQTT connections are open for one account")
    return mqtt["pushes_sent"], mqtt["pushes_missed"]


def _lane(
    variant: Variant, account: Account, directory: Path, scenario: dict, options: dict, args: argparse.Namespace,
    env: dict, stopped: Event, progress: dict,
) -> dict[str, Any]:
    from openbyd.testing import ReplayServer

    if stopped.is_set():
        return {}
    directory.mkdir()
    account_file = directory / "scenario.jsonl"
    ca = directory / "ca.pem"
    guard_file = directory / "guard.json"
    until = options["window"]["end_s"]
    row: dict[str, Any] = {"steps": 0, "steps_without_mqtt": 0, "parent_cpu_ns": 0, "server_step_ns": 0,
                           "log": str(directory / "driver.log")}
    try:
        account.write(account_file, scenario["header"], until)
        with ReplayServer(account_file, corpus=args.corpus, manual=True, epoch="scenario") as replay:
            ca.write_text(replay.ca_pem, encoding="utf-8")
            values = {"python": str(args.backend_python), "launcher": str(rw.LAUNCHER),
                      "guard_report": str(guard_file), "backend": str(args.backend)}
            command = rw.render_command(variant, values)
            # The driver gives up on a command before this harness does, so its own account of
            # what it was doing arrives instead of a bare timeout.
            child_env = {**env, **variant.env, "BYD_BASE_URL_OVERRIDE": replay.url, "SSL_CERT_FILE": str(ca),
                         "REPLAY_DRIVER_TIMEOUT_S": f"{args.step_timeout * 0.75:g}"}
            child = common.Child(command, "replay driver", cwd=args.backend, env=child_env, log=Path(row["log"]))
            driver = Driver(child, args.step_timeout)
            try:
                opened = driver.ask("open", protocol=1, account=account.definition["account"], vins=account.vins,
                                    epoch_unix_ms=replay.epoch_ms, duration_s=until)
                settings = opened.get("settings")
                if not isinstance(settings, dict) or any(
                    key not in settings or not _equal(value, settings[key]) for key, value in variant.expect.items()
                ):
                    raise DriverError(f"open: settings mismatch, expected {variant.expect!r}, got {settings!r}")
                if not isinstance(opened.get("driver"), str) or opened.get("mqtt") is not True:
                    raise DriverError("open: driver must identify itself and acknowledge an MQTT subscription")
                sent, missed = _push_counts(replay.stats())
                if sent or missed:
                    raise DriverError("open: replay processed pushes before the first step")
                for t, pushes, poll in account.steps():
                    if stopped.is_set():
                        return row
                    cpu_start = time.thread_time_ns()
                    start = time.perf_counter_ns()
                    replay.set_time(t)
                    next_sent, next_missed = _push_counts(replay.stats())
                    row["server_step_ns"] += time.perf_counter_ns() - start
                    delivered, dropped = next_sent - sent, next_missed - missed
                    if delivered < 0 or dropped < 0 or delivered + dropped != pushes:
                        raise DriverError(f"step t={t}: expected {pushes} pushes, replay counted {delivered} sent and {dropped} missed")
                    sent, missed = next_sent, next_missed
                    step = driver.ask("step", t=t, pushes=delivered, poll=poll)
                    if type(step.get("mqtt")) is not bool:
                        raise DriverError("step: mqtt must be a boolean")
                    row["steps"] += 1
                    row["steps_without_mqtt"] += not step["mqtt"]
                    progress[account.definition["account"]] = row["steps"]
                    row["parent_cpu_ns"] += time.thread_time_ns() - cpu_start
                finished = driver.ask("finish", t=until)
                driver.finish()
                detections, counters = _receive_finish(finished, account.vins, variant.detections)
                if str(rw.LAUNCHER) in command and not guard_file.exists():
                    raise DriverError("finish: missing launcher guard report")
                row.update(detections=detections, counters=counters)
            finally:
                child.reap()
                row["replay"] = replay.stats()
                if guard_file.is_file():
                    row["guard"] = json.loads(guard_file.read_text(encoding="utf-8"))
        failures = row.get("guard", {}).get("connect_failures", 0)
        if failures and "problem" not in row:
            # The worker saw an outage the scenario does not hold: its detections depend on the machine.
            row["problem"] = f"{failures} connection(s) to the replay server failed on this machine after every retry (local ports exhausted?)"
    except Exception as exc:
        stopped.set()
        row["problem"] = f"{type(exc).__name__}: {exc}"
    finally:
        if rw.is_private({"scenario": scenario}):
            account_file.unlink(missing_ok=True)
    return row


def _iso(value: Any) -> Any:
    if not isinstance(value, datetime):
        return value
    if value.tzinfo is None:
        value = value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


async def extract(database_url: str) -> dict[str, list[dict[str, Any]]]:
    """Read every detection value from the backend's closed drive and charge rows."""
    from sqlalchemy import select
    from sqlalchemy.ext.asyncio import AsyncSession, create_async_engine

    from app.db_models import Car, CloudChargeRow, CloudDriveRow

    engine = create_async_engine(database_url)
    try:
        async with AsyncSession(engine) as db:
            out = {}
            for kind, model in (("drives", CloudDriveRow), ("charges", CloudChargeRow)):
                columns = [column for column in model.__table__.columns if column.key not in SKIP_COLUMNS[kind]]
                query = select(Car.vin, *columns).join(Car, Car.id == model.car_id)
                out[kind] = [{key: _iso(value) for key, value in row.items()} for row in (await db.execute(query)).mappings()]
            return out
    finally:
        await engine.dispose()


def _sum_counts(target: dict, counts: dict) -> None:
    for key, value in counts.items():
        if isinstance(value, dict):
            _sum_counts(target.setdefault(key, {}), value)
        else:
            target[key] = target.get(key, 0) + value


def _guard_total(target: dict, report: dict) -> None:
    target["reports"] += report.get("reports", 1)
    target["blocked_total"] += report["blocked_total"]
    for key in ("connect_retries", "connect_failures"):
        target[key] = target.get(key, 0) + report.get(key, 0)
    by_site = {
        (item["via"], item["host"], item["port"]): item for item in target["blocked"]
    }
    for item in report["blocked"]:
        key = item["via"], item["host"], item["port"]
        if key in by_site:
            by_site[key]["count"] += item["count"]
        else:
            by_site[key] = dict(item)
    target["blocked"] = [item for _, item in sorted(by_site.items())]


def _new_row(variant: str, repeat: int) -> dict:
    return {
        "variant": variant, "repeat": repeat, "accounts": 0, "steps": 0, "steps_without_mqtt": 0,
        "detections": {}, "counters": {}, "replay": {}, "guard": {"reports": 0, "blocked_total": 0, "connect_retries": 0, "connect_failures": 0, "blocked": []},
        "problems": [], "logs": [], "wall_s": 0, "orchestrator_cpu_s": 0, "parent_cpu_ns": 0, "server_step_ns": 0,
    }


def _summarize_row(row: dict) -> None:
    detections = row["detections"]
    row["counts"] = {kind: sum(len(car[kind]) for car in detections.values()) for kind in KINDS}
    row["cars_with_detections"] = {
        **{kind: sum(bool(car[kind]) for car in detections.values()) for kind in KINDS},
        "any": sum(any(car.values()) for car in detections.values()),
    }


def _seed_accounts(path: Path, accounts: dict[str, Account], header: dict, until: int) -> None:
    # The backend seeds accounts and cars only; this file needs no timeline.
    cars = [car for account in accounts.values() for car in account.cars]
    scenario_io.write(path, chain(
        ({**header, "accounts": len(accounts), "cars": len(cars), "duration_s": until},),
        (account.definition for account in accounts.values()), cars,
    ))


def _progress(row: dict, done: int, total: int, steps: dict, total_steps: int, elapsed: float) -> None:
    print(
        f"  {row['variant']}#{row['repeat']}: {done}/{total} accounts, "
        f"{sum(steps.values())}/{total_steps} steps, {elapsed:.1f} s elapsed", flush=True,
    )


def _run_accounts(variant: Variant, accounts: dict, directory: Path, scenario: dict, coverage: dict,
                  args: argparse.Namespace, env: dict, row: dict, start: float) -> None:
    names = sorted(accounts, key=lambda name: -len(accounts[name].timeline))
    total_steps = sum(sum(1 for _ in account.steps()) for account in accounts.values())
    steps = dict.fromkeys(names, 0)
    stopped = Event()
    with ThreadPoolExecutor(max_workers=args.processes) as pool:
        pending = {
            pool.submit(_lane, variant, accounts[name], directory / f"lane-{index:04}", scenario, coverage, args, env, stopped, steps): name
            for index, name in enumerate(names, 1)
        }
        next_report = start
        try:
            while pending:
                done, _ = wait(pending, timeout=max(0, next_report - time.monotonic()), return_when=FIRST_COMPLETED)
                for future in done:
                    name = pending.pop(future)
                    lane = future.result()
                    if not lane:
                        continue
                    if lane.get("problem"):
                        row["problems"].append(f"{name}: {lane['problem']}")
                    if "detections" in lane:
                        row["accounts"] += 1
                        row["detections"].update(lane["detections"])
                    for key in ("steps", "steps_without_mqtt", "parent_cpu_ns", "server_step_ns"):
                        row[key] += lane[key]
                    for key in ("counters", "replay"):
                        _sum_counts(row[key], lane.get(key, {}))
                    if "guard" in lane:
                        _guard_total(row["guard"], lane["guard"])
                    row["logs"].append(lane["log"])
                now = time.monotonic()
                if now >= next_report or not pending:
                    _progress(row, row["accounts"], len(names), steps, total_steps, now - start)
                    next_report = now + 5
        finally:
            stopped.set()


def run_one(variant: Variant, repeat: int, accounts: dict, scenario: dict, coverage: dict,
            args: argparse.Namespace, directory: Path) -> dict[str, Any]:
    """Replay each account and collect detections once every driver has exited."""
    directory.mkdir()
    row = _new_row(variant.key, repeat)
    start = time.monotonic()
    cpu_start = time.process_time()
    seed_file = directory / "seed.jsonl"
    store = None
    try:
        if variant.seed_database:
            store = rw.LocalStore(directory) if args.local_store else rw.DockerStore(
                pg_name=DETECT_PG_CONTAINER, pg_port=DETECT_PG_PORT,
                redis_name=None, replace_existing=False,
            )
            store.start()
        env = rw.child_environment(directory, {
            "BYD_SETTINGS_ENV_FILE": "", "BYD_BASE_URL_OVERRIDE": "http://127.0.0.1:1",
            "DATABASE_URL": "sqlite+aiosqlite:///:memory:", "DATABASE_SSL": "disable", "REDIS_URL": "memory://",
            **(store.env if store is not None else {}),
            "CREDENTIAL_ENCRYPTION_KEY": secrets.token_urlsafe(32), "JWT_SECRET": secrets.token_urlsafe(48),
            "SENTRY_DSN": "", "POD_NAME": "replay-detect", "PYTHONDONTWRITEBYTECODE": "1",
        })
        if store is not None:
            _seed_accounts(seed_file, accounts, scenario["header"], coverage["window"]["end_s"])
            guard_path = directory / "seed-guard.json"
            seed = rw.run_backend_python(args, [
                str(rw.LAUNCHER), "--guard-report", str(guard_path), "-m", "scripts.replay_seed",
                "--scenario", str(seed_file),
            ], env, 600)
            json.loads(rw._line(seed, "SEEDED "))
            _guard_total(row["guard"], json.loads(guard_path.read_text(encoding="utf-8")))
        _run_accounts(variant, accounts, directory, scenario, coverage, args, env, row, start)
        if variant.detections == "database" and not row["problems"]:
            guard_path = directory / "extract-guard.json"
            output = rw.run_backend_python(args, [
                str(rw.LAUNCHER), "--guard-report", str(guard_path), "-m", "bench.replay_detect", "--extract",
            ], {**env, "PYTHONPATH": str(common.ROOT)}, 600)
            _add_closed(row["detections"], json.loads(rw._line(output, "EXTRACT ")))
            _guard_total(row["guard"], json.loads(guard_path.read_text(encoding="utf-8")))
    except (Exception, SystemExit) as exc:
        row["problems"].append(f"{type(exc).__name__}: {exc}")
    finally:
        if rw.is_private({"scenario": scenario}):
            seed_file.unlink(missing_ok=True)
        if store is not None:
            try:
                store.stop()
            except Exception as exc:
                row["problems"].append(f"database cleanup: {exc}")
    row["wall_s"] = time.monotonic() - start
    row["orchestrator_cpu_s"] = time.process_time() - cpu_start
    _summarize_row(row)
    return row


def _equal(a: Any, b: Any) -> bool:
    if type(a) is not type(b):
        return False
    if isinstance(a, dict):
        return a.keys() == b.keys() and all(_equal(value, b[key]) for key, value in a.items())
    if isinstance(a, list):
        return len(a) == len(b) and all(_equal(x, y) for x, y in zip(a, b))
    return a == b


def _date(value: Any) -> datetime | None:
    if not isinstance(value, str) or "T" not in value:
        return None
    try:
        parsed = datetime.fromisoformat(value)
    except ValueError:
        return None
    return parsed if parsed.tzinfo is not None else None


def _start(item: dict) -> float | None:
    value = item.get("start_at", item.get("start"))
    if type(value) in (float, int):
        return value
    date = _date(value)
    return date.timestamp() if date is not None else None


def _absolute(a: Any, b: Any) -> tuple[int | float | None, str | None]:
    if type(a) in (float, int) and type(b) in (float, int):
        return abs(a - b), None
    first, second = _date(a), _date(b)
    if first is not None and second is not None:
        return abs((first - second).total_seconds()), "s"
    return None, None


_MISSING = object()


def _fields(a: Any, b: Any, field: str = "") -> Iterator[dict]:
    if isinstance(a, dict) and isinstance(b, dict):
        for key in sorted(a.keys() | b.keys()):
            yield from _fields(a.get(key, _MISSING), b.get(key, _MISSING), f"{field}.{key}" if field else key)
    elif isinstance(a, list) and isinstance(b, list):
        for index, (x, y) in enumerate(zip(a, b)):
            yield from _fields(x, y, f"{field}[{index}]")
        for index in range(min(len(a), len(b)), max(len(a), len(b))):
            yield from _fields(a[index] if index < len(a) else _MISSING, b[index] if index < len(b) else _MISSING,
                               f"{field}[{index}]")
    elif not _equal(a, b):
        delta, unit = _absolute(a, b)
        yield {
            "type": "field", "field": field, "absolute_difference": delta, "unit": unit,
            **({"reference": a} if a is not _MISSING else {}),
            **({"other": b} if b is not _MISSING else {}),
        }


def _canonical(item: dict) -> str:
    return json.dumps(item, sort_keys=True, separators=(",", ":"))


def _pairs(reference: list[dict], other: list[dict], kind: str) -> Iterator[tuple[dict | None, dict | None]]:
    # Remove equal copies first, independent of database order or duplicate starts.
    left, right = defaultdict(list), defaultdict(list)
    for items, index in ((reference, left), (other, right)):
        for item in items:
            index[_canonical(item)].append(item)
    a, b = [], []
    for key in sorted(left.keys() | right.keys()):
        same = min(len(left[key]), len(right[key]))
        a.extend(left[key][same:])
        b.extend(right[key][same:])
    if kind.startswith("open_") and a and b:
        yield a[0], b[0]
        return
    # With no start to pair by, keep unmatched rows as missing or extra.
    starts_a = sorted((item for item in a if _start(item) is not None), key=_start)
    starts_b = sorted((item for item in b if _start(item) is not None), key=_start)
    yield from ((item, None) for item in a if _start(item) is None)
    yield from ((None, item) for item in b if _start(item) is None)
    for i, j in rw._match([{"start": _start(item)} for item in starts_a], [{"start": _start(item)} for item in starts_b], PAIR_WINDOW_S):
        yield starts_a[i] if i is not None else None, starts_b[j] if j is not None else None


def compare_detections(reference: dict, other: dict) -> list[dict[str, Any]]:
    """List every unmatched detection and every changed field of paired detections."""
    out = []
    for vin in sorted(reference.keys() | other.keys()):
        for kind in KINDS:
            before, after = reference.get(vin, {}).get(kind, []), other.get(vin, {}).get(kind, [])
            for a, b in _pairs(before, after, kind):
                common_fields = {"vin": vin, "kind": kind}
                if a is None:
                    out.append({**common_fields, "type": "extra", "field": None, "other": b})
                elif b is None:
                    out.append({**common_fields, "type": "missing", "field": None, "reference": a})
                else:
                    out.extend({**common_fields, **difference} for difference in _fields(a, b))
    return out


def _public_key(name: str) -> str:
    """A field path or counter name as it may be published: names only, never data.

    A path part must be a name (a letter or underscore first); a list index becomes ``[]``. A
    part that is a number, a date or anything else could be a value used as a key, so the whole
    name is withheld.
    """
    name = re.sub(r"\[\d+\]", "[]", name)
    parts = name.split(".")
    if len(name) > 120 or re.search(r"testvin|acct-\d", name, re.I):
        return "(other)"
    return name if all(re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*(\[\])*", part) for part in parts) else "(other)"


def _public_count_key(name: str) -> str:
    """A counter's key as it may be published: a name, an endpoint path, or a reply code of up to five digits."""
    if re.search(r"testvin|acct-\d", name, re.I):
        return "(other)"
    return name if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,79}|(/[A-Za-z][A-Za-z0-9_]{0,59}){1,6}|\d{1,5}", name) else "(other)"


def _difference_summary(differences: list[dict]) -> list[dict]:
    grouped = defaultdict(list)
    for diff in differences:
        field = diff["field"]
        grouped[diff["kind"], diff["type"], _public_key(field) if field is not None else "", diff.get("unit") or ""].append(diff)
    out = []
    for (kind, change, field, unit), items in sorted(grouped.items()):
        magnitudes = [item["absolute_difference"] for item in items if item.get("absolute_difference") is not None]
        out.append({
            "kind": kind, "type": change, "field": field or None, "unit": unit or None,
            "count": len(items), "cars_affected": len({item["vin"] for item in items}),
            "min": min(magnitudes) if magnitudes else None, "median": median(magnitudes) if magnitudes else None,
            "max": max(magnitudes) if magnitudes else None,
        })
    return out


def _compare_runs(reference: dict, other: dict) -> dict:
    if reference["detections"].keys() != other["detections"].keys():
        raise ValueError("successful runs must cover the same cars")
    diffs = compare_detections(reference["detections"], other["detections"])
    return {
        "reference": f"{reference['variant']}#{reference['repeat']}", "other": f"{other['variant']}#{other['repeat']}",
        "equal": not diffs, "difference_count": len(diffs), "cars_differing": len({diff["vin"] for diff in diffs}),
        "summary": _difference_summary(diffs), "differences": diffs,
    }


def evaluate(results: dict) -> None:
    """Check repeats first and only publish variant findings once all repeats agree."""
    keys = results["settings"]["variants"]
    rows_by_key = {key: [row for row in results["rows"] if row["variant"] == key] for key in keys}
    determinism = {}
    for key, rows in rows_by_key.items():
        if len(rows) != results["settings"]["repeats"] or any(row["problems"] for row in rows) or results["settings"]["no_repeat"]:
            determinism[key] = {"identical": None, "repeats": len(rows), "checks": []}
            continue
        checks = [_compare_runs(rows[0], row) for row in rows[1:]]
        determinism[key] = {"identical": all(check["equal"] for check in checks), "repeats": len(rows), "checks": checks}
    results["determinism"] = determinism
    results["comparison"] = [
        _compare_runs(rows_by_key[a][0], rows_by_key[b][0]) for a, b in combinations(keys, 2)
    ] if all(check["identical"] is True for check in determinism.values()) else []


def _coverage(coverage: dict) -> dict:
    return {
        **coverage, "covered": len(coverage["accounts"]), "total": len(coverage["all_accounts"]),
        "missing_accounts": [name for name in coverage["all_accounts"] if name not in coverage["accounts"]],
        "lines_after_window": dict(sum((Counter(counts) for counts in coverage["after_window"].values()), Counter())),
    }


def _corpus_hash(path: Path) -> str:
    if not path.is_dir():
        raise ValueError("the replay corpus must be a directory")
    digest = sha256()
    for file in sorted(path.rglob("*")):
        if file.is_file():
            digest.update(file.relative_to(path).as_posix().encode() + b"\0")
            with file.open("rb") as handle:
                digest.update(file_digest(handle, "sha256").digest())
    return digest.hexdigest()


def run(args: argparse.Namespace, variants: dict[str, Variant] = VARIANTS) -> dict[str, Any]:
    """Replay each selected variant and repeat over the same account schedule."""
    scenario, accounts, coverage = read_scenario(args.scenario, args.accounts, args.until)
    is_private = rw.is_private({"scenario": scenario})
    keys = args.variants or (list(rw.QUICK_VARIANTS) if args.quick and variants is VARIANTS else list(variants))
    if len(keys) != len(set(keys)) or any(key not in variants for key in keys):
        raise ValueError(f"choose distinct variants from {', '.join(variants)}")
    if any(variants[key].detections not in ("database", "driver")
           or (variants[key].detections == "database" and not variants[key].seed_database) for key in keys):
        raise ValueError("variants must read detections from the driver or a seeded database")
    settings = {
        "variants": keys, "variant_table": {
            key: {"label": variants[key].label, "command": list(variants[key].command), "env": variants[key].env,
                  "expect": variants[key].expect, "detections": variants[key].detections, "seed_database": variants[key].seed_database}
            for key in keys
        },
        "repeats": args.repeats, "no_repeat": args.no_repeat, "protocol": 1, "pair_window_s": PAIR_WINDOW_S,
        "corpus_sha256": _corpus_hash(args.corpus), "store": "driver",
    }
    if any(variants[key].seed_database for key in keys):
        settings["store"] = "SQLite" if args.local_store else rw.PG_IMAGE
    work = args.scratch / "runs"
    if is_private:
        if args.private_dir is not None:
            work = rw.private_directory(rw.private_directory(args.private_dir) / "work")
        else:
            rw.private_directory(args.scenario.resolve().parent / "results")
            work = rw.private_directory(args.scenario.resolve().parent / "work")
    work.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="replay-detect-", dir=work))
    backend = rw.backend_versions(args)
    results = {"scenario": scenario, "settings": settings, "coverage": _coverage(coverage), "backend": backend, "rows": [],
               "execution": {"processes": args.processes, "step_timeout_s": args.step_timeout}}
    print(f"replay_detect: {len(accounts)}/{results['coverage']['total']} accounts, "
          f"t=0..{coverage['window']['end_s']} s, {len(keys)} variant(s) x {args.repeats}", flush=True)
    for key, repeat in rw.interleaved(keys, args.repeats):
        row = run_one(variants[key], repeat, accounts, scenario, coverage, args, directory / f"{key}-{repeat}")
        results["rows"].append(row)
        if row["problems"]:
            break
    evaluate(results)
    return results


def merge_results(documents: list[dict[str, Any]]) -> dict[str, Any]:
    """Combine disjoint account chunks and recompute determinism and variant comparisons."""
    base = documents[0]
    if any(document.get("redacted") for document in documents):
        raise ValueError("cannot merge redacted results; use the complete files")
    settings = base["settings"]
    expected = rw.interleaved(settings["variants"], settings["repeats"])
    combined = {key: _new_row(*key) for key in expected}
    coverage = {**base["coverage"], "accounts": [], "after_window": {}}
    seen = set()
    for index, document in enumerate(documents, 1):
        label = f"input {index}"
        if (document["scenario"]["sha256"] != base["scenario"]["sha256"]
                or document["coverage"]["all_accounts"] != coverage["all_accounts"]):
            raise ValueError(f"{label}: scenario differs")
        if document["coverage"]["window"] != coverage["window"] or document["settings"] != settings:
            raise ValueError(f"{label}: replay settings differ")
        for key in ("backend_commit", "backend_dirty", "backend_python", "packages"):
            if document["backend"].get(key) != base["backend"].get(key):
                raise ValueError(f"{label}: backend {key} differs")
        # The replay server and the Rust cipher come from this repository: every chunk must have run the same code.
        for key in ("openbyd_commit", "openbyd_dirty", "openbyd_package_version", "pybyd_commit_installed"):
            if (document.get("versions") or {}).get(key) != (base.get("versions") or {}).get(key):
                raise ValueError(f"{label}: {key} differs")
        names = set(document["coverage"]["accounts"])
        if names & seen:
            raise ValueError(f"{label}: overlaps {len(names & seen)} already covered account(s)")
        seen.update(names)
        coverage["after_window"].update(document["coverage"]["after_window"])
        rows = {(row["variant"], row["repeat"]): row for row in document["rows"]}
        if len(rows) != len(document["rows"]) or rows.keys() != combined.keys() or any(
            row["problems"] or row["accounts"] != len(names) for row in rows.values()
        ):
            raise ValueError(f"{label}: incomplete or failed variant runs")
        for key, row in rows.items():
            target = combined[key]
            if target["detections"].keys() & row["detections"].keys():
                raise ValueError(f"{label}: car present in more than one chunk")
            target["detections"].update(row["detections"])
            for field_name in ("accounts", "steps", "steps_without_mqtt", "wall_s", "orchestrator_cpu_s", "parent_cpu_ns", "server_step_ns"):
                target[field_name] += row[field_name]
            for field_name in ("counters", "replay"):
                _sum_counts(target[field_name], row[field_name])
            _guard_total(target["guard"], row["guard"])
            target["logs"].extend(row["logs"])
    coverage["accounts"] = [name for name in coverage["all_accounts"] if name in seen]
    rows = list(combined.values())
    for row in rows:
        _summarize_row(row)
    results = {"scenario": base["scenario"], "settings": settings, "backend": base["backend"],
               "coverage": _coverage(coverage), "rows": rows,
               "execution": {"merged_chunks": len(documents), "chunk_versions": base.get("versions"),
                             "chunk_dates": [document.get("date") for document in documents]}}
    evaluate(results)
    return results


def publishable(results: dict[str, Any]) -> dict[str, Any]:
    """Build a public document from aggregate data, excluding timelines and all per-car data."""
    private = rw.is_private(results)
    coverage = results["coverage"]
    scenario = results["scenario"]
    header = {key: scenario["header"][key] for key in ("version", "duration_s", "accounts", "cars")}
    header["source"] = "export" if private else "synthetic"
    public_scenario = {"header": header, "counts": scenario["counts"]}
    if not private:
        public_scenario["file"] = scenario["file"]
    sensitive = (scenario["file"], scenario["header"]["start"]) if private else ()

    def clean_key(name: str, public: Any = _public_key) -> str:
        return "(other)" if any(value and value in name for value in sensitive) else public(name)

    def clean_counts(counts: dict) -> dict:
        out: dict = {}
        for key, value in counts.items():
            name = clean_key(key, _public_count_key)
            _sum_counts(out, {name: clean_counts(value) if isinstance(value, dict) else value})
        return out

    def clean_pair(pair: dict) -> dict:
        diffs = [{**diff, "field": clean_key(diff["field"]) if diff["field"] is not None else None} for diff in pair["differences"]]
        return {**{key: value for key, value in pair.items() if key != "differences"}, "summary": _difference_summary(diffs)}

    rows = []
    for row in results["rows"]:
        public = {key: row[key] for key in ("variant", "repeat", "accounts", "steps", "steps_without_mqtt", "counts", "cars_with_detections",
                                           "wall_s", "orchestrator_cpu_s", "parent_cpu_ns", "server_step_ns")}
        public["problems"] = ["run failed (see complete results)"] * len(row["problems"]) if private else row["problems"]
        public["guard"] = {key: row["guard"].get(key, 0) for key in ("reports", "blocked_total", "connect_retries", "connect_failures")}
        public["counters"] = clean_counts(row["counters"])
        public["replay"] = clean_counts(row["replay"])
        rows.append(public)
    return {
        "redacted": True, "scenario": public_scenario, "rows": rows,
        "coverage": {key: coverage[key] for key in ("covered", "total", "window", "lines_after_window")},
        "execution": results["execution"],
        "settings": {key: results["settings"][key] for key in ("variants", "repeats", "no_repeat", "protocol", "pair_window_s", "store")},
        "backend": {key: value for key, value in results["backend"].items() if not private or key != "backend_branch"},
        "determinism": {
            key: {**check, "checks": [clean_pair(pair) for pair in check["checks"]]}
            for key, check in results["determinism"].items()
        },
        "comparison": [clean_pair(pair) for pair in results["comparison"]],
    }


def write(results: dict, args: argparse.Namespace) -> Path:
    """Keep the complete file privately and send the aggregate to the report directory."""
    if rw.is_private(results):
        default = args.merge[0].resolve().parent if args.merge else args.scenario.resolve().parent / "results"
        complete_directory = rw.private_directory(args.private_dir or default)
    else:
        complete_directory = args.private_dir or args.scratch / "complete"
    complete = common.write_results("replay_detect", args.quick, results, directory=complete_directory, unique=True)
    public = common.write_results("replay_detect", args.quick, publishable(results),
                                  directory=args.scratch / "results" if args.quick else None, unique=True)
    print(f"wrote complete results to {complete}")
    print(f"wrote aggregate results to {public}")
    return public


def status(results: dict[str, Any]) -> int:
    """Use exit 1 for execution or determinism failures and exit 2 for variant findings."""
    if any(row["problems"] for row in results["rows"]) or any(
        check["identical"] is False for check in results["determinism"].values()
    ):
        return 1
    return 2 if any(not pair["equal"] for pair in results["comparison"]) else 0


def _print_summary(results: dict) -> None:
    public = publishable(results)
    coverage = public["coverage"]
    print(f"Coverage {coverage['covered']}/{coverage['total']} accounts, t=0..{coverage['window']['end_s']} s")
    if results["coverage"]["missing_accounts"] and not rw.is_private(results):
        print(f"Accounts omitted: {', '.join(results['coverage']['missing_accounts'])}")
    if coverage["window"]["omitted_s"]:
        print(f"Window omitted {coverage['window']['omitted_s']} s and {sum(coverage['lines_after_window'].values())} selected-account timeline lines")
    for row in public["rows"]:
        if row["guard"]["blocked_total"]:
            print(f"GUARD {row['variant']}#{row['repeat']}: refused {row['guard']['blocked_total']} non-loopback attempt(s)")
        if row["guard"]["connect_retries"]:
            print(f"GUARD {row['variant']}#{row['repeat']}: retried {row['guard']['connect_retries']} local connect(s) the machine failed")
        for problem in row["problems"]:
            print(f"PROBLEM {row['variant']}#{row['repeat']}: {problem}")
    for key, check in public["determinism"].items():
        labels = {True: "identical across repeats", False: "NOT DETERMINISTIC", None: "determinism not checked"}
        print(f"{key}: {labels[check['identical']]}")
    for pair in public["comparison"]:
        outcome = "agree exactly" if pair["equal"] else f"{pair['difference_count']} finding(s) on {pair['cars_differing']} car(s)"
        print(f"{pair['reference']} and {pair['other']}: {outcome}")


def reference_variant() -> Variant:
    """The guarded synthetic test driver, available outside the production variants table."""
    return Variant("reference", "Synthetic reference driver",
                   (*DRIVER[:-1], "bench.replay.reference_driver"),
                   expect={"native_cipher": True}, detections="driver", seed_database=False)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """Read the replay and chunk-merge command-line options."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", type=Path, default=os.environ.get("OPENBYD_REPLAY_BACKEND"))
    parser.add_argument("--backend-python", type=Path, default=os.environ.get("OPENBYD_REPLAY_BACKEND_PYTHON") or sys.executable)
    parser.add_argument("--scenario", type=Path, default=rw.DEFAULT_SCENARIO)
    parser.add_argument("--variants", nargs="+", help=f"default: {' '.join(VARIANTS)}")
    parser.add_argument("--repeats", type=int, default=2)
    parser.add_argument("--no-repeat", action="store_true", help="run once for sizing, without drawing detection conclusions")
    parser.add_argument("--processes", type=int, default=MAX_PROCESSES, help="maximum concurrent accounts, 1–8")
    parser.add_argument("--accounts", nargs="+", help="first N accounts, or names, comma-separated numbers and inclusive ranges")
    parser.add_argument("--until", type=int, metavar="SECONDS", help="stop at this scenario second, inclusive")
    parser.add_argument("--corpus", type=Path, default=common.CORPUS)
    parser.add_argument("--private-dir", type=Path)
    parser.add_argument("--scratch", type=Path, default=common.SCRATCH / "replay-detect")
    parser.add_argument("--local-store", action="store_true", help="use SQLite for a backend smoke run")
    parser.add_argument("--quick", action="store_true", help="two cipher variants and results under scratch")
    parser.add_argument("--step-timeout", type=float, default=180, metavar="SECONDS",
                        help="real-time limit of one driver command; the driver is given three quarters of it")
    parser.add_argument("--merge", nargs="+", type=Path, metavar="FILE", help="combine complete results for disjoint account chunks")
    parser.add_argument("--reference", action="store_true", help="exercise the protocol with the synthetic test driver")
    parser.add_argument("--extract", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if not 1 <= args.processes <= MAX_PROCESSES:
        parser.error("--processes must be between 1 and 8")
    if not math.isfinite(args.step_timeout) or args.step_timeout <= 0:
        parser.error("--step-timeout must be positive and finite")
    if args.no_repeat:
        args.repeats = 1
    elif args.repeats < 2:
        parser.error("--repeats must be at least 2 unless --no-repeat is given")
    for key in ("scratch", "scenario", "corpus", "private_dir", "backend"):
        if (path := getattr(args, key)) is not None:
            setattr(args, key, path.resolve())
    if args.backend_python.is_file() or args.backend_python.parent != Path("."):
        # absolute(), not resolve(): a virtual environment's python is a symlink to the base
        # interpreter, and following it would run the driver outside the environment.
        args.backend_python = args.backend_python.absolute()
    return args


def main(argv: list[str] | None = None) -> int:
    """Run or merge replays and report their outcome."""
    args = parse_args(argv)
    if args.extract:
        print("EXTRACT " + json.dumps(asyncio.run(extract(os.environ["DATABASE_URL"])), allow_nan=False), flush=True)
        return 0
    try:
        if args.merge:
            results = merge_results([json.loads(path.read_text(encoding="utf-8")) for path in args.merge])
        elif args.reference:
            reference = reference_variant()
            args.quick = True  # the test double's results never go to bench/results
            args.backend, args.backend_python = common.ROOT, Path(sys.executable)
            if next(scenario_io.read(args.scenario), {}).get("source") != "synthetic":
                raise ValueError("--reference is only for synthetic scenarios")
            results = run(args, {reference.key: reference})
        else:
            if args.backend is None or not args.backend.is_dir():
                raise ValueError("--backend must be the backend directory (or set OPENBYD_REPLAY_BACKEND)")
            keys = args.variants or (rw.QUICK_VARIANTS if args.quick else VARIANTS)
            if any(key in VARIANTS and VARIANTS[key].command == DRIVER for key in keys):
                if not (args.backend / "scripts" / "replay_driver.py").is_file():
                    raise ValueError("selected variants need scripts/replay_driver.py in the backend")
            results = run(args)
        write(results, args)
        _print_summary(results)
        return status(results)
    except (ValueError, OSError) as exc:
        print(f"replay_detect: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
