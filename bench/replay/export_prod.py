"""Export a replay scenario inside the API pod, or fetch and verify it locally.

The pod executes this file from stdin. Keep its top-level imports in the
standard library and keep all repository imports on the local fetch path.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import logging
import os
import re
import secrets
import signal
import subprocess
import sys
from collections import Counter, defaultdict
from collections.abc import AsyncIterator, Callable, Iterable, Iterator, Mapping
from contextlib import aclosing
from datetime import UTC, datetime, timedelta
from decimal import Decimal, InvalidOperation
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any, NamedTuple, TextIO


ACCOUNT = re.compile(r"acct-[0-9]+")
SYNTHETIC_VIN = re.compile(r"TESTVIN[0-9]{10}")
MODEL = re.compile(r"[a-z0-9_-]{1,40}")
CODE = re.compile(r"[0-9]+")
START = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z")
STAT = re.compile(r"[a-z_]{1,40}")
REAL_VIN = re.compile(r"(?<![^\W_])[A-HJ-NPR-Z0-9a-hj-npr-z]{17}(?![^\W_])")
UUID_PATTERN = re.compile(r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}")
EMAIL = re.compile(r"""(?<![\w.+-])(?:[\w.!#$%&'*+/=?^_{}|~-]+|"[^"\r\n]+")@"""
                   r"""(?:[\w-]+(?:\.[\w-]+)*|\[[^\]\s]+\])""")
DECIMAL = re.compile(
    r"(?<![A-Za-z0-9.])[-+]?(?:[0-9]*\.[0-9]{4,}(?:[eE][-+]?[0-9]+)?|"
    r"[0-9]+(?:\.[0-9]+)?[eE]-[0-9]+)(?![A-Za-z0-9.])"
)
MAX_COUNT = 2**32 - 1

HEADER_KEYS = frozenset(("type", "version", "source", "start", "duration_s", "accounts", "cars", "seed", "stats"))
TIMELINE_KEYS = frozenset(("type", "vin", "t"))
DEFAULT_STATE = {
    "online": False, "soc": None, "odometer_km": None, "speed_kmh": None,
    "vehicle_on": None, "plugged": False, "charging": False, "connect_state": None,
    "charge_power_kw": None, "minutes_to_full": None, "hvac_on": None,
    "fuel_percent": None, "gps": False,
}
PUSH_FIELDS = {
    "vehicleInfo": frozenset(DEFAULT_STATE),
    "smartCharge": frozenset(("plugged", "charging", "connect_state", "soc")),
    "remoteControl": frozenset(),
    "energyConsumption": frozenset(),
}
LINE_KEYS = {
    "scenario": HEADER_KEYS,
    "account": frozenset(("type", "account", "cars")),
    "car": frozenset(("type", "vin", "account", "model", "energy")),
    "state": TIMELINE_KEYS | DEFAULT_STATE.keys(),
    "error": TIMELINE_KEYS | {"code"},
    "push": TIMELINE_KEYS | {"event"},
}
NUMBER_RANGES = {
    "soc": (0, 100), "fuel_percent": (0, 100), "odometer_km": (0, 10_000_000),
    "speed_kmh": (0, 400), "charge_power_kw": (0, 1_000),
}
INTEGER_RANGES = {"connect_state": (-1, 255), "minutes_to_full": (0, 10_080)}
BOOL_FIELDS = frozenset(("online", "plugged", "charging", "gps", "vehicle_on", "hvac_on"))
NULLABLE_BOOLS = frozenset(("vehicle_on", "hvac_on"))


def valid_state_value(key: str, value: Any) -> bool:
    """Check the value type and the telemetry bounds shared by the replay tools."""
    if value is None:
        return key not in BOOL_FIELDS or key in NULLABLE_BOOLS
    if key in BOOL_FIELDS:
        return type(value) is bool
    if key in INTEGER_RANGES:
        low, high = INTEGER_RANGES[key]
        return type(value) is int and low <= value <= high
    low, high = NUMBER_RANGES[key]
    return type(value) in (int, float) and low <= value <= high


def allowed_keys(kind: str, event: Any) -> set[str] | frozenset[str]:
    """Return the keys allowed on a particular line type."""
    keys = LINE_KEYS.get(kind, frozenset())
    if kind == "push" and isinstance(event, str):
        return keys | PUSH_FIELDS.get(event, frozenset())
    return keys


def json_line(row: Mapping[str, Any]) -> str:
    """Encode a compact, finite JSON object and its terminating newline."""
    return json.dumps(row, separators=(",", ":"), allow_nan=False) + "\n"


class Leak(NamedTuple):
    """A privacy finding whose excerpt contains no input text."""

    line: int
    rule: str
    excerpt: str = "[redacted]"


class _Object(list):
    """Preserve duplicate JSON keys so their values can also be scanned."""


class _PreciseNumber(str):
    """Keep the spelling of a JSON float for the coordinate check."""


def _is_precise(match: re.Match[str] | None) -> bool:
    if match is None:
        return False
    text = match.group()
    mantissa, _, exponent = text.lower().partition("e")
    if exponent:
        try:
            return Decimal(text).as_tuple().exponent < -3
        except InvalidOperation:
            return True
    return len(mantissa.partition(".")[2]) >= 4


def _precise_numbers(text: str) -> Iterator[re.Match[str]]:
    return (match for match in DECIMAL.finditer(text) if _is_precise(match))


def _coordinate_gaps(text: str, decimals: Iterable[re.Match[str]]) -> Iterator[str]:
    previous = None
    for match in decimals:
        if previous is not None:
            gap = text[previous.end():match.start()]
            if not any(char.isalnum() for char in gap):
                yield "coordinate_pair"
        previous = match


def _patterns(value: str) -> Iterator[str]:
    yield from ("vin" for _ in REAL_VIN.finditer(value))
    if "-" in value:
        yield from ("uuid" for _ in UUID_PATTERN.finditer(value))
    if "@" in value:
        yield from ("email" for _ in EMAIL.finditer(value))


def _safe_string(kind: str, key: str, value: str, source: Any) -> bool:
    if key == "type":
        return value == kind and value in LINE_KEYS
    if kind == "scenario":
        if key == "source":
            return value in ("synthetic", "export")
        if key == "start":
            if not START.fullmatch(value):
                return False
            try:
                datetime.fromisoformat(value)
            except ValueError:
                return False
            return True
        return key == "seed" and source == "synthetic" and MODEL.fullmatch(value) is not None
    if key == "account":
        return ACCOUNT.fullmatch(value) is not None
    if key == "vin" or (kind == "account" and key == "car_item"):
        return SYNTHETIC_VIN.fullmatch(value) is not None
    if key == "model":
        return MODEL.fullmatch(value) is not None
    if key == "energy":
        return value in ("ev", "hybrid")
    if key == "code":
        return CODE.fullmatch(value) is not None
    return key == "event" and value in PUSH_FIELDS


def _scan_node(
    node: Any, kind: str, keys: set[str] | frozenset[str], source: Any,
    context: str = "line", *, escaped: bool = False,
) -> Iterator[str]:
    if context == "stats" and not isinstance(node, _Object):
        yield "unexpected_stats_value"
    if isinstance(node, _Object):
        if context not in ("line", "stats"):
            yield "unexpected_object"
        seen = set()
        precise = 0
        for key, value in node:
            if key in seen:
                yield "duplicate_key"
            seen.add(key)
            if escaped:
                yield from _patterns(key)
                yield from _coordinate_gaps(key, _precise_numbers(key))
            if context == "line":
                if key not in keys:
                    yield "unexpected_key"
                child = key if key in keys else "nested"
            elif context == "stats":
                if not STAT.fullmatch(key):
                    yield "unexpected_key"
                child = "nested"
                if type(value) is not int or value < 0:
                    yield "unexpected_stats_value"
            else:
                yield "unexpected_key"
                child = "nested"
            if isinstance(value, str) and _is_precise(DECIMAL.fullmatch(value)):
                precise += 1
            yield from _scan_node(value, kind, keys, source, child, escaped=escaped)
        if precise >= 2:
            yield "coordinate_pair"
    elif isinstance(node, list):
        if context != "cars" or kind != "account":
            yield "unexpected_array"
        if sum(isinstance(value, str) and _is_precise(DECIMAL.fullmatch(value)) for value in node) >= 2:
            yield "coordinate_pair"
        child = "car_item" if context == "cars" and kind == "account" else "nested"
        for value in node:
            yield from _scan_node(value, kind, keys, source, child, escaped=escaped)
    elif type(node) is str:
        if escaped:
            yield from _patterns(node)
            yield from _coordinate_gaps(node, _precise_numbers(node))
        if not _safe_string(kind, context, node, source):
            yield "unexpected_string"


def _leaks(text: str, decimals: list[re.Match[str]]) -> Iterator[str]:
    yield from _patterns(text)
    yield from _coordinate_gaps(text, decimals)
    try:
        row = json.loads(text, object_pairs_hook=_Object, parse_float=_PreciseNumber if decimals else float)
    except (ValueError, RecursionError):
        if text.strip():
            yield "invalid_json"
        return
    if not isinstance(row, _Object):
        yield "invalid_line"
        return
    fields = dict(row)
    kind = fields.get("type")
    if not isinstance(kind, str) or kind not in LINE_KEYS:
        yield "unexpected_type"
        kind = ""
    yield from _scan_node(row, kind, allowed_keys(kind, fields.get("event")), fields.get("source"),
                         escaped="\\" in text)


def find_leaks(text_or_lines: str | Iterable[str]) -> Iterator[Leak]:
    """Yield every privacy finding with a one-based line number and redacted excerpt."""
    lines = text_or_lines.splitlines() if isinstance(text_or_lines, str) else text_or_lines
    pending_coordinate = False
    for number, text in enumerate(lines, 1):
        try:
            decimals = list(_precise_numbers(text))
            if decimals:
                if pending_coordinate and not any(char.isalnum() for char in text[:decimals[0].start()]):
                    yield Leak(number, "coordinate_pair")
                pending_coordinate = not any(char.isalnum() for char in text[decimals[-1].end():])
            elif pending_coordinate:
                pending_coordinate = not any(char.isalnum() for char in text)
            for rule in _leaks(text, decimals):
                yield Leak(number, rule)
        except (RecursionError, ValueError, OverflowError):
            yield Leak(number, "invalid_json")


class _ExportError(Exception):
    """A deliberately data-free error safe to send to the operator."""


def _write_checked(output: TextIO, row: dict[str, Any]) -> None:
    line = json_line(row)
    finding = next(find_leaks((line,)), None)
    if finding is not None:
        kind = row["type"]
        if not isinstance(kind, str) or kind not in LINE_KEYS:
            kind = "unknown"
        raise _ExportError(f"{finding.rule} on {kind} line")
    output.write(line)


POLL_FIELDS = {
    "currentSoc": "soc", "odometer": "odometer_km", "speed": "speed_kmh",
    "online": "online", "vehicleOn": "vehicle_on", "chargePowerKw": "charge_power_kw",
    "minutesToFull": "minutes_to_full", "fuelPercent": "fuel_percent",
}
CHARGING_FIELDS = {
    "isCharging": "charging", "isConnected": "plugged", "connectState": "connect_state",
    "soc": "soc", "minutesToFull": "minutes_to_full",
}
# What a null means depends on where it was recorded. A poll's snapshot says null for a reading
# the car did not give (speed, power, climate while asleep), which is part of the state; but the
# battery, the odometer and the tank do not become unknown, so there the last value stands. A
# push names only what it carries, and the consumer ignores its nulls, except that a null charge
# power clears the last one.
POLL_NULLS = frozenset(("speed_kmh", "vehicle_on", "connect_state", "charge_power_kw", "minutes_to_full", "hvac_on"))
PUSH_NULLS = {"vehicleInfo": frozenset(("charge_power_kw",)), "smartCharge": frozenset()}
ERROR_CODE = re.compile(r"[0-9]{1,5}")
HALF_OFFSET = 30 * 86400
HYBRID_TYPES = frozenset(("hybrid", "phev", "hev"))
Pair = tuple[Any, str]


def _salted_order(salt: bytes, domain: str, *parts: Any) -> bytes:
    identity = json.dumps([domain, *(str(part) for part in parts)], separators=(",", ":"))
    return hashlib.blake2b(identity.encode(), key=salt, digest_size=32).digest()


def _map_patch(
    payload: Any, fields: frozenset[str], hybrid: bool, keep_null: frozenset[str], stats: Counter[str],
) -> dict[str, Any]:
    """Map one production payload to scenario keys.

    A null is copied only for the keys in ``keep_null``; for every other key it
    means "not reported" and the car's last known value stands.
    """
    if isinstance(payload, str):
        # A driver that hands JSON back as text. The location was removed in SQL.
        try:
            payload = json.loads(payload)
        except ValueError:
            payload = None
    if not isinstance(payload, dict):
        stats["dropped_fields"] += 1
        return {}
    patch: dict[str, Any] = {}

    def add_values(data: dict[str, Any], names: Mapping[str, str]) -> None:
        for key, value in data.items():
            field = names.get(key)
            if field not in fields or (field == "fuel_percent" and not hybrid):
                stats["dropped_fields"] += 1
            elif value is None and field not in keep_null:
                stats["unreported_fields"] += 1
            elif not valid_state_value(field, value):
                stats["dropped_fields"] += 1
            elif value is not None or field not in patch:
                patch[field] = round(value, 2) if type(value) is float else value

    top_level = {}
    for key in payload:
        if key in POLL_FIELDS:
            top_level[key] = payload[key]
        elif key not in ("charging", "climate", "gps", "gpsFresh"):
            stats["dropped_fields"] += 1

    if "charging" in payload:
        charging = payload["charging"]
        if isinstance(charging, dict):
            add_values(charging, CHARGING_FIELDS)
        else:
            stats["dropped_fields"] += 1
    add_values(top_level, POLL_FIELDS)
    if "climate" in payload:
        climate = payload["climate"]
        if isinstance(climate, dict):
            add_values(climate, {"hvacOn": "hvac_on"})
        else:
            stats["dropped_fields"] += 1
    if "gps" in payload or "gpsFresh" in payload:
        fresh = payload.get("gpsFresh")
        if "gpsFresh" in payload and type(fresh) is not bool:
            stats["dropped_fields"] += 1
        if "gps" in fields:
            # Never descend into the location object, even when counting drops.
            patch["gps"] = isinstance(payload.get("gps"), dict) and fresh is not False
        else:
            stats["dropped_fields"] += int("gps" in payload) + int("gpsFresh" in payload)
    return patch


class Export:
    """Transform bounded, ordered production rows into a checked scenario stream."""

    def __init__(
        self, output: TextIO, inventory: Mapping[Pair, Mapping[str, Any]],
        start: datetime, until: datetime, *, salt: bytes, offset_s: int,
        max_accounts: int | None = None,
    ) -> None:
        self.output = output
        self.start_time = start
        self.until = until
        self.shift = timedelta(seconds=offset_s)
        self.accounts = _select_accounts(inventory, salt, max_accounts)
        self.cars: dict[Pair, tuple[str, bool]] = {}
        self.states: dict[str, dict[str, Any]] = {}
        self.errors: set[str] = set()
        self.stats: Counter[str] = Counter()
        selected = sorted(
            (pair for pair in inventory if pair[0] in self.accounts),
            key=lambda pair: _salted_order(salt, "vin", *pair),
        )
        by_account: dict[Any, list[str]] = defaultdict(list)
        models: dict[str, str] = {}
        for index, pair in enumerate(selected, 1):
            metadata = inventory[pair]
            model = metadata.get("model_key")
            if (not isinstance(model, str) or not MODEL.fullmatch(model)
                    or REAL_VIN.search(model) or UUID_PATTERN.search(model)):
                if model is not None:
                    self.stats["dropped_models"] += 1
                model = "unknown"
            powertrain = metadata.get("powertrain")
            hybrid = metadata.get("is_hybrid") is True or (
                isinstance(powertrain, str) and powertrain.strip().lower() in HYBRID_TYPES
            )
            vin = f"TESTVIN{index:010d}"
            self.cars[pair] = (vin, hybrid)
            models[vin] = model
            by_account[pair[0]].append(vin)
            # Only synthetic data is passed to the writer.
        for user_id, account in self.accounts.items():
            _write_checked(output, {"type": "account", "account": account, "cars": by_account[user_id]})
        for pair, (vin, hybrid) in self.cars.items():
            _write_checked(output, {"type": "car", "vin": vin, "account": self.accounts[pair[0]],
                                    "model": models[vin], "energy": "hybrid" if hybrid else "ev"})
        self.last_time = -1
        self.shifted_start = _second(start + self.shift)

    def _timeline(self, row: dict[str, Any]) -> None:
        _write_checked(self.output, row)
        self.stats[row["type"]] += 1

    def _state(self, vin: str, t: int, patch: dict[str, Any], *, from_push: bool = False) -> None:
        previous = self.states.get(vin)
        state = {**(DEFAULT_STATE if previous is None else previous), **patch}
        if from_push and vin in self.errors:
            self.states[vin] = state
        elif state != previous or vin in self.errors:
            self._timeline({"type": "state", "vin": vin, "t": t, **state})
            self.states[vin] = state
            self.errors.discard(vin)

    def add(self, row: Mapping[str, Any]) -> None:
        """Project one production row, emitting its timeline lines immediately."""
        car = self.cars.get((row["user_id"], row["vin"]))
        if car is None:
            self.stats["dropped_rows"] += 1
            return
        vin, hybrid = car
        received_at = row["received_at"]
        if not self.start_time <= received_at < self.until:
            raise _ExportError("timeline row outside the requested window")
        t = (_second(received_at + self.shift) - self.shifted_start) // timedelta(seconds=1)
        if t < self.last_time:
            raise _ExportError("timeline rows are out of order")
        self.last_time = t
        source, kind = row["source"], row["kind"]
        if source == "poll":
            if kind == "ok":
                patch = _map_patch(row.get("payload"), PUSH_FIELDS["vehicleInfo"], hybrid, POLL_NULLS, self.stats)
                self._state(vin, t, patch)
            elif kind == "error":
                code = row.get("detail")
                if code == "session_expired":
                    code = "1005"
                if not isinstance(code, str) or not ERROR_CODE.fullmatch(code):
                    self.stats["dropped_errors"] += 1
                    return
                self._timeline({"type": "error", "vin": vin, "t": t, "code": code})
                if code != "1005":
                    self.errors.add(vin)
            elif kind == "skip":
                self.stats["skipped_polls"] += 1
            else:
                self.stats["dropped_events"] += 1
        elif source == "mqtt" and kind in PUSH_FIELDS:
            fields = PUSH_FIELDS[kind]
            patch = {}
            if fields:
                patch = _map_patch(row.get("payload"), fields, hybrid, PUSH_NULLS[kind], self.stats)
            elif isinstance(row.get("payload"), dict):
                self.stats["dropped_fields"] += len(row["payload"])
            self._timeline({"type": "push", "vin": vin, "t": t, "event": kind, **patch})
            if fields:
                self._state(vin, t, patch, from_push=True)
        else:
            self.stats["dropped_events"] += 1

    def finish(self) -> None:
        """Write the final header as the last line of the pod stream."""
        start = self.shifted_start.isoformat(timespec="seconds").replace("+00:00", "Z")
        _write_checked(self.output, {
            "type": "scenario", "version": 1, "source": "export", "start": start,
            "duration_s": (_second(self.until + self.shift) - self.shifted_start) // timedelta(seconds=1),
            "accounts": len(self.accounts), "cars": len(self.cars), "stats": dict(self.stats),
        })


def _select_accounts(
    inventory: Mapping[Pair, Mapping[str, Any]], salt: bytes, max_accounts: int | None,
) -> dict[Any, str]:
    users = sorted({pair[0] for pair in inventory}, key=lambda user_id: _salted_order(salt, "account", user_id))
    return {user_id: f"acct-{index:04d}" for index, user_id in enumerate(users[:max_accounts], 1)}


def _second(stamp: datetime) -> datetime:
    return (stamp.astimezone(UTC) + timedelta(microseconds=500_000)).replace(microsecond=0)


READ_ONLY = "SET TRANSACTION READ ONLY"
TIMEOUT = "SET LOCAL statement_timeout = '30s'"
IDLE_TIMEOUT = "SET LOCAL idle_in_transaction_session_timeout = '10s'"
LOCK_TIMEOUT = "SET LOCAL lock_timeout = '2s'"
WINDOW = "received_at >= :lower AND received_at < :upper"
ROW_KINDS = {
    "poll_ok": "source = 'poll' AND kind = 'ok'",
    "poll_error": "source = 'poll' AND kind = 'error'",
    "poll_skip": "source = 'poll' AND kind = 'skip'",
    "poll_other": "source = 'poll' AND kind NOT IN ('ok', 'error', 'skip')",
    "mqtt_vehicleInfo": "source = 'mqtt' AND kind = 'vehicleInfo'",
    "mqtt_remoteControl": "source = 'mqtt' AND kind = 'remoteControl'",
    "mqtt_smartCharge": "source = 'mqtt' AND kind = 'smartCharge'",
    "mqtt_energyConsumption": "source = 'mqtt' AND kind = 'energyConsumption'",
    "mqtt_other": (
        "source = 'mqtt' AND kind NOT IN ('vehicleInfo', 'remoteControl', 'smartCharge', 'energyConsumption')"
    ),
    "other": "source NOT IN ('poll', 'mqtt')",
}
COUNTS = "".join(
    f", COUNT(*) FILTER (WHERE {condition}) AS \"{name}\"" for name, condition in ROW_KINDS.items()
)
CAR_METADATA = """
    LEFT JOIN LATERAL (
        SELECT CASE WHEN model_key_source IN ('vin', 'name') THEN model_key END AS model_key,
               is_hybrid, powertrain
        FROM cars
        WHERE cars.user_id = identities.user_id AND cars.vin = identities.vin
          AND cars.deleted_at IS NULL
        ORDER BY model_key NULLS LAST, is_hybrid DESC, powertrain NULLS LAST
        LIMIT 1
    ) metadata ON TRUE
"""
ROW_PAYLOAD = """
    CASE WHEN (source = 'poll' AND kind = 'ok') OR source = 'mqtt' THEN
        CASE WHEN json_typeof(payload) = 'object' THEN
            (CAST(payload AS jsonb) - 'gps') ||
            CASE WHEN json_typeof(payload -> 'gps') = 'object' THEN jsonb_build_object('gps', '{}'::jsonb)
                 WHEN (payload -> 'gps') IS NOT NULL THEN jsonb_build_object('gps', NULL::jsonb)
                 ELSE '{}'::jsonb END
        END
    END AS payload
"""


def _inventory_sql(resume: bool, *, count_only: bool) -> str:
    cursor = "AND (user_id, vin) > (:after_first, :after_second)" if resume else ""
    if count_only:
        return f"""
            SELECT user_id, vin {COUNTS}
            FROM ingest_events WHERE {WINDOW} {cursor}
            GROUP BY user_id, vin ORDER BY user_id, vin LIMIT :limit
        """
    return f"""
        WITH identities AS (
            SELECT user_id, vin FROM ingest_events WHERE {WINDOW} {cursor}
            GROUP BY user_id, vin ORDER BY user_id, vin LIMIT :limit
        )
        SELECT identities.user_id, identities.vin, metadata.model_key,
               metadata.is_hybrid, metadata.powertrain
        FROM identities {CAR_METADATA}
        ORDER BY identities.user_id, identities.vin
    """


def _events_sql(resume: bool, *, filter_users: bool) -> str:
    cursor = "AND (received_at, id) > (:after_first, :after_second)" if resume else ""
    selected = "AND user_id = ANY(CAST(:users AS uuid[]))" if filter_users else ""
    return f"""
        SELECT received_at, id, user_id, vin, source, kind,
               CASE WHEN source = 'poll' AND kind = 'error'
                          AND (detail ~ '^[0-9]{{1,5}}$' OR detail = 'session_expired')
                    THEN detail END AS detail,
               {ROW_PAYLOAD}
        FROM ingest_events WHERE {WINDOW} {cursor} {selected}
        ORDER BY received_at, id LIMIT :limit
    """


PLAN_SHAPES = ("inventory", "dry_run", "events")


def _plan_nodes(raw: Any) -> list[dict[str, Any]]:
    plan = json.loads(raw) if isinstance(raw, str) else raw
    pending = [plan[0]["Plan"]]
    nodes = []
    while pending:
        node = pending.pop()
        nodes.append({"node": node["Node Type"], "relation": node.get("Relation Name"),
                      "index": node.get("Index Name"), "rows": node["Plan Rows"]})
        pending.extend(reversed(node.get("Plans", [])))
    return nodes


class ExportQueries:
    """Read bounded windows in short, independent read-only transactions."""

    def __init__(
        self, sessions: Any, statement: Callable[[str], Any], page_rows: int, pause_ms: int,
    ) -> None:
        self.sessions = sessions
        self.statement = statement
        self.page_rows = page_rows
        self.pause_s = pause_ms / 1000
        self.started = False

    async def _query(self, sql: str, params: Mapping[str, Any]) -> list[Mapping[str, Any]]:
        if self.started:
            await asyncio.sleep(self.pause_s)
        self.started = True
        async with self.sessions.begin() as session:
            for setting in (READ_ONLY, TIMEOUT, IDLE_TIMEOUT, LOCK_TIMEOUT):
                await session.execute(self.statement(setting))
            result = await session.execute(self.statement(sql), params)
            return list(result.mappings())

    async def _rows(
        self, start: datetime, until: datetime, keys: tuple[str, str],
        query: Callable[[bool], str], filters: Mapping[str, Any],
    ) -> AsyncIterator[Mapping[str, Any]]:
        lower = start
        while lower < until:
            upper = min(lower + timedelta(hours=1), until)
            cursor = None
            while True:
                params = {"lower": lower, "upper": upper, "limit": self.page_rows, **filters}
                if cursor is not None:
                    params.update(after_first=cursor[0], after_second=cursor[1])
                page = await self._query(query(cursor is not None), params)
                for row in page:
                    yield row
                full_page = len(page) == self.page_rows
                del page
                if not full_page:
                    break
                cursor = (row[keys[0]], row[keys[1]])
            lower = upper

    def inventory(
        self, start: datetime, until: datetime, *, count_only: bool,
    ) -> AsyncIterator[Mapping[str, Any]]:
        """Yield car identities per window, with metadata or dry-run counts."""
        return self._rows(start, until, ("user_id", "vin"),
                          lambda resume: _inventory_sql(resume, count_only=count_only), {})

    def events(
        self, start: datetime, until: datetime, users: Iterable[Any] | None,
    ) -> AsyncIterator[Mapping[str, Any]]:
        """Yield ingest rows in stable chronological order without GPS coordinates."""
        filters = {"users": list(users)} if users is not None else {}

        return self._rows(start, until, ("received_at", "id"),
                          lambda resume: _events_sql(resume, filter_users=bool(filters)), filters)

    async def explain(self, start: datetime, until: datetime) -> dict[str, list[dict[str, Any]]]:
        """Return only the plan nodes and estimates for the three first-page queries."""
        statements = (
            _inventory_sql(False, count_only=False),
            _inventory_sql(False, count_only=True),
            _events_sql(False, filter_users=False),
        )
        params = {"lower": start, "upper": min(start + timedelta(hours=1), until), "limit": self.page_rows}
        plans = {}
        for shape, sql in zip(PLAN_SHAPES, statements, strict=True):
            result = await self._query("EXPLAIN (FORMAT JSON) " + sql, params)
            plans[shape] = _plan_nodes(result[0]["QUERY PLAN"])
        return plans


def _default_until(now: datetime) -> datetime:
    return (now.astimezone(UTC) - timedelta(minutes=10)).replace(second=0, microsecond=0)


async def _run_pod(args: argparse.Namespace, output: TextIO) -> None:
    from sqlalchemy import text
    from sqlalchemy.ext.asyncio import async_sessionmaker

    from app.config import get_settings
    from app.db import create_engine

    until = args.until if args.until is not None else _default_until(datetime.now(UTC))
    start = until - timedelta(days=args.days)
    engine = create_engine(get_settings())
    try:
        queries = ExportQueries(async_sessionmaker(engine), text, args.page_rows, args.pause_ms)
        if args.explain:
            output.write(json_line(_checked_plans(await queries.explain(start, until))))
            return
        salt = secrets.token_bytes(32)
        inventory: dict[Pair, Mapping[str, Any]] = {}
        counts: dict[Any, Counter[str]] = defaultdict(Counter)
        async with aclosing(queries.inventory(start, until, count_only=args.dry_run)) as rows:
            async for row in rows:
                uid, vin = row["user_id"], row["vin"]
                if args.dry_run:
                    counts[uid].update({key: row[key] for key in ROW_KINDS})
                    inventory[(uid, vin)] = {}
                else:
                    inventory.setdefault((uid, vin), row)
        if args.dry_run:
            selected = _select_accounts(inventory, salt, args.max_accounts)
            total: Counter[str] = Counter()
            for uid in selected:
                total.update(counts[uid])
            output.write(json_line(_checked_counts({
                "accounts": len(selected), "cars": sum(pair[0] in selected for pair in inventory),
                "rows": {key: total[key] for key in ROW_KINDS},
            })))
            return

        offset_s = secrets.randbelow(2 * HALF_OFFSET + 1) - HALF_OFFSET
        export = Export(output, inventory, start, until, salt=salt, offset_s=offset_s,
                        max_accounts=args.max_accounts)
        if export.accounts:
            users = export.accounts if args.max_accounts is not None else None
            async with aclosing(queries.events(start, until, users)) as rows:
                async for row in rows:
                    export.add(row)
        export.finish()
    finally:
        await engine.dispose()


EXPORT_ROOT = Path(r"E:\Projects\openbyd-replay-data")


def _export_directory(path: Path) -> Path:
    directory = path.resolve()
    if any((ancestor / ".git").exists() for ancestor in (directory, *directory.parents)):
        raise _ExportError("output directory is inside a git work tree")
    if not directory.is_relative_to(EXPORT_ROOT.resolve()):
        raise _ExportError("output directory is outside the replay data directory")
    existing = next(ancestor for ancestor in (directory, *directory.parents) if ancestor.is_dir())
    check = subprocess.run(
        ["git", "-C", str(existing), "rev-parse", "--is-inside-work-tree"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
        env={key: value for key, value in os.environ.items() if not key.upper().startswith("GIT_")},
    )
    if check.returncode == 0:
        raise _ExportError("output directory is inside a git repository")
    if check.returncode != 128 or b"not a git repository" not in check.stderr.lower():
        raise _ExportError("could not determine whether the output directory is in a git work tree")
    directory.mkdir(parents=True, exist_ok=True)
    return directory


def _fold(stream: Path, destination: Path) -> dict[str, Any]:
    with stream.open(encoding="utf-8") as source:
        total = 0
        trailer = ""
        for total, trailer in enumerate(source, 1):
            pass
        try:
            header = json.loads(trailer)
        except ValueError:
            raise _ExportError("missing or invalid scenario trailer") from None
        if not isinstance(header, dict) or header.get("type") != "scenario" or header.get("source") != "export":
            raise _ExportError("missing or invalid scenario trailer")
        source.seek(0)
        with destination.open("w", encoding="utf-8", newline="\n") as output:
            # Preserve the source bytes' JSON spelling so the local privacy check
            # sees the original trailer keys, including any accidental duplicates.
            output.write(trailer.rstrip("\n") + "\n")
            for _ in range(total - 1):
                output.write(source.readline())
    return header


def _dry_run_counts(path: Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as stream:
        result = json.loads(stream.read(4096), object_pairs_hook=_unique_counts)
        if stream.read(1):
            raise _ExportError("invalid dry-run counts")
    return _checked_counts(result)


def _unique_counts(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result = dict(pairs)
    if len(result) != len(pairs):
        raise _ExportError("duplicate key in exporter response")
    return result


def _checked_counts(result: Any) -> dict[str, Any]:
    if (
        not isinstance(result, dict)
        or set(result) != {"accounts", "cars", "rows"}
        or not isinstance(result["rows"], dict)
        or set(result["rows"]) != ROW_KINDS.keys()
        or any(type(value) is not int or not 0 <= value < 10**16
               for value in (result["accounts"], result["cars"], *result["rows"].values()))
    ):
        raise _ExportError("invalid dry-run counts")
    return result


_PLAN_NODE = re.compile(r"[A-Za-z][A-Za-z0-9 ]{0,79}")
_PLAN_INDEX = re.compile(r"[A-Za-z_][A-Za-z0-9_]{0,62}")


def _checked_plans(result: Any) -> dict[str, list[dict[str, Any]]]:
    if not isinstance(result, dict) or set(result) != set(PLAN_SHAPES):
        raise _ExportError("invalid export plans")
    for nodes in result.values():
        if not isinstance(nodes, list) or not nodes:
            raise _ExportError("invalid export plans")
        for node in nodes:
            if not isinstance(node, dict) or set(node) != {"node", "relation", "index", "rows"}:
                raise _ExportError("invalid export plan node")
            name, relation, index, rows = node["node"], node["relation"], node["index"], node["rows"]
            if (
                not isinstance(name, str) or not _PLAN_NODE.fullmatch(name)
                or relation not in (None, "ingest_events", "cars")
                or (index is not None and (not isinstance(index, str) or not _PLAN_INDEX.fullmatch(index)))
                or type(rows) is not int or rows < 0
            ):
                raise _ExportError("invalid export plan node")
    return result


_SAFE_POD_LEAK = re.compile(
    rb"(vin|uuid|email|coordinate_pair|duplicate_key|unexpected_key|unexpected_stats_value|"
    rb"unexpected_array|unexpected_object|unexpected_string|invalid_json|invalid_line|unexpected_type)"
    rb" on (scenario|account|car|state|error|push|unknown) line"
)
_SAFE_POD_FAILURE = re.compile(rb"replay export failed: [A-Za-z_][A-Za-z_0-9]{0,79}")


def _pod_error(stderr: bytes, status: int) -> _ExportError:
    for line in stderr[:4096].splitlines():
        if _SAFE_POD_LEAK.fullmatch(line) or _SAFE_POD_FAILURE.fullmatch(line):
            return _ExportError(line.decode("ascii"))
    return _ExportError(f"kubectl export failed (exit {status})")


def _fetch(args: argparse.Namespace) -> dict[str, Any]:
    # Direct script execution puts bench/replay, rather than the repo, on sys.path.
    if not __package__:
        sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
    from bench.replay import privacy, scenario

    options = ["--days", str(args.days), "--page-rows", str(args.page_rows),
               "--pause-ms", str(args.pause_ms), "--max-minutes", str(args.max_minutes)]
    if args.until is not None:
        options.extend(("--until", args.until.isoformat()))
    if args.max_accounts is not None:
        options.extend(("--max-accounts", str(args.max_accounts)))
    if args.dry_run:
        options.append("--dry-run")
    if args.explain:
        options.append("--explain")
    # The script shares the API container's CPU allowance, so it runs at the lowest priority.
    command = ["kubectl", "-n", "bydconnect", "exec", "-i", "deploy/api", "--",
               "nice", "-n", "19", "python", "-", *options]
    if args.explain:
        with Path(__file__).open("rb") as script:
            result = subprocess.run(command, stdin=script, capture_output=True, check=False)
        if result.returncode:
            raise _pod_error(result.stderr, result.returncode)
        return _checked_plans(json.loads(result.stdout, object_pairs_hook=_unique_counts))

    directory = _export_directory(args.out_dir)
    with TemporaryDirectory(prefix=".replay-", dir=directory) as scratch:
        stream = Path(scratch) / "stream.jsonl"
        with Path(__file__).open("rb") as script, stream.open("wb") as output:
            result = subprocess.run(command, stdin=script, stdout=output, stderr=subprocess.PIPE, check=False)
        if result.returncode:
            raise _pod_error(result.stderr, result.returncode)
        if args.dry_run:
            return _dry_run_counts(stream)

        folded = Path(scratch) / "scenario.jsonl"
        header = _fold(stream, folded)
        with folded.open(encoding="utf-8") as file:
            leak = next(privacy.find_leaks(file), None)
        if leak is not None:
            raise _ExportError(f"{leak.rule} on line {leak.line}")
        try:
            report = scenario.validate(folded)
        except scenario.ScenarioError as exc:
            raise _ExportError(str(exc)) from None
        destination = directory / f"export-{header['start'][:10]}-{args.days}d.jsonl"
        if destination.exists():
            raise _ExportError("an export for this shifted start date and duration already exists")
        folded.rename(destination)
        return report.as_dict()


def _until(value: str) -> datetime:
    try:
        parsed = datetime.fromisoformat(value)
        if parsed.tzinfo is not None:
            return parsed.astimezone(UTC)
    except (ValueError, OverflowError):
        pass
    raise argparse.ArgumentTypeError("until must be an ISO timestamp with a timezone")


def main() -> None:
    """Run the pod exporter, or fetch a stream and verify it before publishing."""
    local = sys.argv[1:2] == ["fetch"]
    parser = argparse.ArgumentParser(description=__doc__)
    # One day less than the table's retention, whose oldest rows are deleted while an export runs.
    parser.add_argument("--days", type=int, default=13)
    parser.add_argument("--until", type=_until)
    parser.add_argument("--max-accounts", type=int)
    parser.add_argument("--page-rows", type=int, default=5000)
    parser.add_argument("--pause-ms", type=int, default=50)
    parser.add_argument("--max-minutes", type=int, default=45)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--dry-run", action="store_true")
    mode.add_argument("--explain", action="store_true")
    if local:
        parser.add_argument("--out-dir", type=Path)
    args = parser.parse_args(sys.argv[2:] if local else None)
    if local and not args.explain and args.out_dir is None:
        parser.error("--out-dir is required except for --explain")
    if not 1 <= args.days <= MAX_COUNT // 86400 or not 1 <= args.page_rows <= 5000 or not 1 <= args.pause_ms:
        parser.error("days must fit the scenario duration, page-rows must be 1–5000 and pause-ms must be positive")
    if args.max_accounts is not None and args.max_accounts < 1:
        parser.error("max-accounts must be positive")
    if not 1 <= args.max_minutes <= (2**31 - 1) // 60:
        parser.error("max-minutes must be positive and fit in the process alarm")
    logging.disable(logging.CRITICAL)
    try:
        if local:
            print(json_line(_fetch(args)), end="")
        else:
            _pod_with_deadline(args)
    except _ExportError as exc:
        print(exc, file=sys.stderr)
        raise SystemExit(1) from None
    except BaseException as exc:  # noqa: BLE001 - nothing but the class name may be printed
        # SQLAlchemy exceptions and Kubernetes diagnostics can contain row data.
        print(f"replay export failed: {type(exc).__name__}", file=sys.stderr)
        raise SystemExit(1) from None


def _pod_with_deadline(args: argparse.Namespace) -> None:
    alarm = getattr(signal, "alarm", None)
    if alarm is not None:
        previous_handler = signal.signal(signal.SIGALRM, signal.SIG_DFL)
        alarm(args.max_minutes * 60)
    try:
        asyncio.run(_run_pod(args, sys.stdout))
        sys.stdout.flush()
    finally:
        if alarm is not None:
            alarm(0)
            signal.signal(signal.SIGALRM, previous_handler)


if __name__ == "__main__":
    main()
