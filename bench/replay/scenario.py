"""Read, write and validate the version 1 replay JSONL format."""

from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from collections.abc import Iterable, Iterator, Mapping
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

from .export_prod import (
    ACCOUNT, CODE, DEFAULT_STATE, LINE_KEYS, MAX_COUNT, MODEL, PUSH_FIELDS, START, STAT, SYNTHETIC_VIN,
    allowed_keys, json_line, valid_state_value,
)


class ScenarioError(ValueError):
    """A format violation with a one-based line number."""

    def __init__(self, line: int, detail: str) -> None:
        super().__init__(f"line {line}: {detail}")
        self.line = line


@dataclass(frozen=True)
class Summary:
    """Counts and warnings collected while validating a scenario."""

    accounts: int
    cars: int
    days: float
    lines_by_type: dict[str, int]
    errors_by_code: dict[str, int]
    pushes_by_event: dict[str, int]
    warnings: tuple[str, ...]

    def as_dict(self) -> dict[str, Any]:
        """Return the public counts without identifiers or diagnostic text."""
        return {
            "accounts": self.accounts, "cars": self.cars, "days": self.days,
            "lines_by_type": self.lines_by_type, "errors_by_code": self.errors_by_code,
            "pushes_by_event": self.pushes_by_event,
        }


def _unique(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    row = dict(pairs)
    if len(row) != len(pairs):
        raise ValueError("duplicate JSON keys")
    return row


def _invalid_constant(_: str) -> None:
    raise ValueError("invalid JSON constant")


def _numbered(path: str | Path) -> Iterator[tuple[int, dict[str, Any]]]:
    with Path(path).open("rb") as stream:
        for number, line in enumerate(stream, 1):
            try:
                row = json.loads(line.decode("utf-8"), object_pairs_hook=_unique, parse_constant=_invalid_constant)
            except (ValueError, RecursionError):
                raise ScenarioError(number, "expected valid UTF-8 JSON with unique object keys") from None
            if not isinstance(row, dict):
                raise ScenarioError(number, "expected a JSON object")
            yield number, row


def read(path: str | Path) -> Iterator[dict[str, Any]]:
    """Yield parsed JSON objects without retaining or validating the timeline."""
    for _, row in _numbered(path):
        yield row


def write(path: str | Path, rows: Iterable[Mapping[str, Any]]) -> None:
    """Write JSON objects in the supplied order, one per line."""
    with Path(path).open("w", encoding="utf-8", newline="\n") as stream:
        for row in rows:
            stream.write(json_line(row))


def _match(pattern: Any, value: Any) -> bool:
    return isinstance(value, str) and pattern.fullmatch(value) is not None


def _keys(
    number: int, row: dict[str, Any], required: set[str] | frozenset[str],
    allowed: set[str] | frozenset[str],
) -> None:
    missing = required - row.keys()
    if missing:
        raise ScenarioError(number, f"missing keys: {', '.join(sorted(missing))}")
    if row.keys() - allowed:
        raise ScenarioError(number, "unexpected keys")


def _header(number: int, row: dict[str, Any]) -> None:
    keys = LINE_KEYS["scenario"]
    _keys(number, row, keys - {"seed", "stats"}, keys)
    if type(row["version"]) is not int or row["version"] != 1:
        raise ScenarioError(number, "unsupported scenario version")
    if row["source"] not in ("synthetic", "export"):
        raise ScenarioError(number, "invalid scenario source")
    if "seed" in row:
        if row["source"] != "synthetic":
            raise ScenarioError(number, "seed is only allowed on a synthetic scenario")
        if not _match(MODEL, row["seed"]):
            raise ScenarioError(number, "invalid synthetic seed")
    try:
        if not _match(START, row["start"]):
            raise ValueError
        datetime.fromisoformat(row["start"])
    except ValueError:
        raise ScenarioError(number, "start must be UTC to the second") from None
    for key in ("duration_s", "accounts", "cars"):
        if type(row[key]) is not int or not 0 <= row[key] <= MAX_COUNT:
            raise ScenarioError(number, f"{key} must be a non-negative 32-bit integer")
    stats = row.get("stats", {})
    if not isinstance(stats, dict) or any(
        not STAT.fullmatch(key) or type(value) is not int or value < 0 for key, value in stats.items()
    ):
        raise ScenarioError(number, "stats must be a flat object of non-negative integers with lowercase keys")


_ORDER = {"account": 1, "car": 2, "state": 3, "error": 3, "push": 3}


def validate(path: str | Path) -> Summary:
    """Validate the scenario in one pass and report the first violation, if any."""
    with_rows = _numbered(path)
    try:
        first = next(with_rows, None)
        if first is None or first[1].get("type") != "scenario":
            raise ScenarioError(1, "expected scenario header")
        header = first[1]
        _header(1, header)
        return _validate_rows(header, with_rows)
    finally:
        with_rows.close()


def _validate_rows(header: dict[str, Any], rows: Iterator[tuple[int, dict[str, Any]]]) -> Summary:
    counts: Counter[str] = Counter(scenario=1)
    errors: Counter[str] = Counter()
    pushes: Counter[str] = Counter()
    accounts: set[str] = set()
    owners: dict[str, tuple[str, int]] = {}
    cars: dict[str, str] = {}
    pending_errors: dict[str, int] = {}
    phase = 0
    previous_t = -1

    for number, row in rows:
        kind = row.get("type")
        if not isinstance(kind, str) or kind not in _ORDER:
            raise ScenarioError(number, "unexpected line type")
        if _ORDER[kind] < phase:
            raise ScenarioError(number, "account and car definitions must precede the timeline in order")
        phase = _ORDER[kind]
        event = row.get("event")
        _keys(number, row, LINE_KEYS[kind], allowed_keys(kind, event))
        counts[kind] += 1

        if kind == "account":
            account, vins = row["account"], row["cars"]
            if not _match(ACCOUNT, account) or account in accounts:
                raise ScenarioError(number, "invalid or duplicate account")
            if not isinstance(vins, list) or not all(_match(SYNTHETIC_VIN, vin) for vin in vins):
                raise ScenarioError(number, "cars must be a list of synthetic VINs")
            accounts.add(account)
            for vin in vins:
                if vin in owners:
                    raise ScenarioError(number, "VIN listed more than once")
                owners[vin] = (account, number)
            continue

        vin = row["vin"]
        if not _match(SYNTHETIC_VIN, vin):
            raise ScenarioError(number, "expected a synthetic VIN")
        if kind == "car":
            account, model, energy = row["account"], row["model"], row["energy"]
            if not _match(ACCOUNT, account) or account not in accounts:
                raise ScenarioError(number, "car refers to an undefined account")
            if vin in cars:
                raise ScenarioError(number, "duplicate car definition")
            owner = owners.get(vin)
            if owner is None or owner[0] != account:
                raise ScenarioError(number, "VIN does not belong to the car's account")
            if not _match(MODEL, model) or energy not in ("ev", "hybrid"):
                raise ScenarioError(number, "invalid car model or energy")
            cars[vin] = energy
            continue

        if vin not in cars:
            raise ScenarioError(number, "timeline refers to an undefined car")
        t = row["t"]
        if type(t) is not int or not 0 <= t <= header["duration_s"]:
            raise ScenarioError(number, "t must be an integer within the scenario duration")
        if t < previous_t:
            raise ScenarioError(number, "timeline t decreases")
        previous_t = t
        if kind == "error":
            code = row["code"]
            if not _match(CODE, code):
                raise ScenarioError(number, "error code must be a string of digits")
            errors[code] += 1
            if code != "1005":
                pending_errors[vin] = number
            continue
        if kind == "state":
            pending_errors.pop(vin, None)
        else:
            if not isinstance(event, str) or event not in PUSH_FIELDS:
                raise ScenarioError(number, "invalid push event")
            pushes[event] += 1
        for key in DEFAULT_STATE.keys() & row.keys():
            value = row[key]
            if not valid_state_value(key, value) or (
                key == "fuel_percent" and value is not None and cars[vin] != "hybrid"
            ):
                raise ScenarioError(number, f"invalid {key}")

    for vin, (_, number) in owners.items():
        if vin not in cars:
            raise ScenarioError(number, "account lists an undefined car")
    for kind in ("account", "car"):
        if header[kind + "s"] != counts[kind]:
            raise ScenarioError(1, f"header {kind} count does not match definitions")
    warnings = tuple(f"WARNING line {line}: car ends with an unrecovered error"
                     for line in sorted(pending_errors.values()))
    return Summary(
        counts["account"], counts["car"], header["duration_s"] / 86400,
        {kind: counts[kind] for kind in LINE_KEYS}, dict(errors), dict(pushes), warnings,
    )


def main() -> None:
    """Validate a scenario or print its aggregate counts."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["validate", "summary"])
    parser.add_argument("file", type=Path)
    args = parser.parse_args()
    try:
        result = validate(args.file)
    except ScenarioError as exc:
        parser.exit(1, f"{exc}\n")
    except OSError:
        parser.exit(1, "could not read the scenario file\n")
    if args.command == "summary":
        print(json_line(result.as_dict()), end="")
    else:
        for warning in result.warnings:
            print(warning, file=sys.stderr)
        print("valid scenario")


if __name__ == "__main__":
    main()
