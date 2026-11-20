"""Generate a deterministic fleet of synthetic drives, charges and pushes.

The shared sample uses --accounts 3 --hours 6 --seed demo --profile busy.
"""

from __future__ import annotations

import argparse
import heapq
import math
import random
from collections.abc import Iterator
from itertools import chain, takewhile
from pathlib import Path
from typing import Any

from .export_prod import MAX_COUNT, MODEL, REAL_VIN, UUID_PATTERN
from .scenario import write


def _timeline(
    vin: str, hybrid: bool, primary: bool, seed: str, duration: int, busy: bool
) -> Iterator[dict[str, Any]]:
    rng = random.Random(f"{seed}:{vin}")
    state = {
        "online": False, "soc": rng.randint(83, 90), "odometer_km": rng.randint(2000, 78000),
        "speed_kmh": 0, "vehicle_on": False, "plugged": False, "charging": False,
        "connect_state": 0, "charge_power_kw": None, "minutes_to_full": None,
        "hvac_on": False, "fuel_percent": 72 if hybrid else None, "gps": False,
    }

    def snapshot(t: int, **changes: Any) -> dict[str, Any]:
        state.update(changes)
        return {"type": "state", "vin": vin, "t": t, **state}

    def push(t: int, event: str, *fields: str) -> dict[str, Any]:
        return {"type": "push", "vin": vin, "t": t, "event": event,
                **{field: state[field] for field in fields}}

    yield snapshot(0)
    for day in range(0, duration + 1, 86400):
        if day == duration:
            break
        jitter = rng.randrange(5, 175) if busy else rng.randrange(5, 1500)
        # Keep recovery intervals inside the scenario, including truncated runs.
        for start, end, code in [(180, 240, "6002"), (300, 360, "1001"), (420, 660, "1008")]:
            t = day + start + jitter
            recovery = day + end + jitter
            if recovery <= duration and (busy or rng.random() < {"6002": 0.08, "1001": 0.04, "1008": 0.20}[code]):
                for tick in range(t, recovery, 60):
                    yield {"type": "error", "vin": vin, "t": tick, "code": code}
                yield snapshot(recovery)
        expiry = day + 690 + jitter
        if primary and expiry <= duration:
            yield {"type": "error", "vin": vin, "t": expiry, "code": "1005"}

        jobs = [(3600 * hour, "drive") for hour in (7, 12, 17)] + [(19 * 3600, "charge")]
        if busy and day == 0:
            jobs = [(900, "drive"), (1680, "charge"), (3600, "drive"), (10800, "drive")]
        for start, job in jobs:
            t = day + start + jitter
            if t > duration:
                break
            if job == "drive":
                compact = busy and day == 0 and start == 900
                minutes = rng.randint(8, 10) if compact else rng.randint(12, 27)
                yield push(t, "remoteControl")
                yield snapshot(t, online=True, vehicle_on=True, hvac_on=True, gps=True)
                yield push(t, "vehicleInfo", "online", "vehicle_on", "soc", "gps")
                odometer = state["odometer_km"]
                for minute in range(minutes):
                    speed = rng.randrange(29, 81)
                    tick = t + 60 * minute
                    yield snapshot(tick, speed_kmh=speed, odometer_km=round(odometer, 2))
                    if minute % 2 == 1:
                        yield push(tick, "vehicleInfo", "soc", "speed_kmh", "odometer_km", "fuel_percent")
                    odometer += speed / 60
                    state["soc"] = round(max(0, state["soc"] - speed / 250), 2)
                    if hybrid:
                        state["fuel_percent"] = round(max(0, state["fuel_percent"] - speed / 1200), 2)
                end = t + minutes * 60
                yield snapshot(end, speed_kmh=0, odometer_km=round(odometer, 2),
                               vehicle_on=False, hvac_on=False)
                yield push(end, "energyConsumption")
                yield snapshot(end + 120, online=False, gps=False)
            else:
                start_soc = state["soc"]
                minutes = max(1, math.ceil((100 - start_soc) * 0.48))
                yield snapshot(t, online=True, plugged=True, charging=True, connect_state=1,
                               charge_power_kw=75, minutes_to_full=minutes)
                yield push(t, "smartCharge", "plugged", "charging", "connect_state", "soc")
                for minute in range(1, minutes):
                    yield snapshot(t + minute * 60, soc=round(start_soc + (100 - start_soc) * minute / minutes, 2),
                                   minutes_to_full=minutes - minute)
                end = t + minutes * 60
                yield snapshot(end, soc=100, charging=False, charge_power_kw=0, minutes_to_full=0)
                yield push(end, "smartCharge", "plugged", "charging", "connect_state", "soc")
                yield snapshot(end + 180, online=False)
                yield snapshot(end + 360, plugged=False, connect_state=0,
                               charge_power_kw=None, minutes_to_full=None)


def scenario_lines(accounts: int, hours: float, seed: str, profile: str = "normal") -> Iterator[dict[str, Any]]:
    """Yield a scenario in file order, retaining one pending event per car."""
    if not 1 <= accounts <= MAX_COUNT * 7 // 8 or not 1 / 3600 <= hours <= MAX_COUNT / 3600:
        raise ValueError("accounts and duration must be positive and fit the replay format")
    if not MODEL.fullmatch(seed) or REAL_VIN.search(seed) or UUID_PATTERN.search(seed):
        raise ValueError("seed must be a safe token of 1–40 lowercase letters, digits, underscores or hyphens")
    if profile not in {"normal", "busy"}:
        raise ValueError("unknown generator profile")
    duration = int(hours * 3600)
    cars = []
    for index in range(1, accounts + 1):
        account = f"acct-{index:04d}"
        for position in range(2 if index % 7 == 1 else 1):
            number = len(cars) + 1
            hybrid = number % 5 == 2
            cars.append((account, f"TESTVIN{number:010d}", hybrid, position == 0))
    yield {
        "type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z",
        "duration_s": duration, "accounts": accounts, "cars": len(cars), "seed": seed, "stats": {},
    }
    vins_by_account: dict[str, list[str]] = {}
    for account, vin, _, _ in cars:
        vins_by_account.setdefault(account, []).append(vin)
    for account, vins in vins_by_account.items():
        yield {"type": "account", "account": account, "cars": vins}
    for account, vin, hybrid, _ in cars:
        yield {"type": "car", "vin": vin, "account": account,
               "model": "seal-u-dm-i" if hybrid else "seal", "energy": "hybrid" if hybrid else "ev"}
    timelines = (
        takewhile(lambda row: row["t"] <= duration,
                  _timeline(vin, hybrid, primary, seed, duration, profile == "busy"))
        for _, vin, hybrid, primary in cars
    )
    yield from heapq.merge(*timelines, key=lambda row: row["t"])


def generate(path: str | Path, accounts: int, hours: float, seed: str, profile: str = "normal") -> None:
    """Write a synthetic scenario to a UTF-8 JSONL file."""
    lines = scenario_lines(accounts, hours, seed, profile)
    header = next(lines)
    write(path, chain((header,), lines))


def main() -> None:
    """Generate a scenario using the requested fleet and time span."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--accounts", type=int, required=True)
    parser.add_argument("--hours", type=float, required=True)
    parser.add_argument("--seed", required=True)
    parser.add_argument("--profile", choices=["normal", "busy"], default="normal")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    try:
        generate(args.out, args.accounts, args.hours, args.seed, args.profile)
    except ValueError as exc:
        parser.error(str(exc))


if __name__ == "__main__":
    main()
