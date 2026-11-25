"""openbyd.testing.ReplayServer, driven by pyBYD's BydClient and by openbyd's client.

The scenario's values must survive the whole way: scenario line -> raw reply
-> each client's model -> the fields a backend reads them from. Both clients
must parse the same thing, and that must be what the scenario says. The
replay clock is manual, so nothing waits for real time.
"""

from __future__ import annotations

import asyncio
import json
import os
import queue
import socket
import ssl
import subprocess
import threading
import time
from pathlib import Path
from typing import Any

import pytest

import openbyd

testing = pytest.importorskip(
    "openbyd.testing", reason="extension built without the 'testing' feature", exc_type=ImportError
)
pybyd = pytest.importorskip("pybyd")
ReplayServer = testing.ReplayServer

ROOT = Path(__file__).resolve().parents[2]
DEMO = ROOT / "bench" / "replay" / "samples" / "demo.jsonl"
EV, HYBRID, OTHER = "TESTVIN0000000001", "TESTVIN0000000002", "TESTVIN0000000003"
START_UNIX = 1_767_571_200
LIBRARIES = ("pybyd", "openbyd")


# --- scenarios ---------------------------------------------------------------


def state(vin: str, t: int, **fields: Any) -> dict[str, Any]:
    line = {
        "type": "state", "vin": vin, "t": t, "online": True, "soc": 71.0, "odometer_km": 20431.4,
        "speed_kmh": 0.0, "vehicle_on": False, "plugged": False, "charging": False, "connect_state": 0,
        "charge_power_kw": None, "minutes_to_full": None, "hvac_on": False, "fuel_percent": None, "gps": True,
    }
    line.update(fields)
    return line


def write_scenario(path: Path, timeline: list[dict[str, Any]], duration: int = 10_000) -> Path:
    lines = [
        {"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z",
         "duration_s": duration, "accounts": 2, "cars": 3, "seed": "test", "stats": {}},
        {"type": "account", "account": "acct-0001", "cars": [EV, HYBRID]},
        {"type": "account", "account": "acct-0002", "cars": [OTHER]},
        {"type": "car", "vin": EV, "account": "acct-0001", "model": "seal", "energy": "ev"},
        {"type": "car", "vin": HYBRID, "account": "acct-0001", "model": "seal-6-dm-i", "energy": "hybrid"},
        {"type": "car", "vin": OTHER, "account": "acct-0002", "model": "unknown", "energy": "ev"},
        *timeline,
    ]
    path.write_text("".join(json.dumps(line) + "\n" for line in lines), encoding="utf-8")
    return path


class Expected:
    """An independent reading of a scenario file: what applies to a car at a moment."""

    UNSEEN = {**state("", 0), "online": False, "soc": None, "odometer_km": 0.0, "speed_kmh": None,
              "vehicle_on": None, "connect_state": None, "hvac_on": None}

    def __init__(self, path: Path) -> None:
        lines = [json.loads(text) for text in path.read_text(encoding="utf-8").splitlines() if text.strip()]
        self.cars = {line["vin"]: line for line in lines if line["type"] == "car"}
        self.order = list(self.cars)
        self.accounts = [line for line in lines if line["type"] == "account"]
        self.timeline = [line for line in lines if line["type"] in ("state", "error")]
        self.duration = lines[0]["duration_s"]

    def at(self, vin: str, t: int) -> tuple[dict[str, Any], str | None]:
        current, error = self.UNSEEN, None
        for line in self.timeline:
            if line["vin"] != vin or line["t"] > t:
                continue
            if line["type"] == "state":
                current, error = line, None
            elif line["code"] != "1005":
                error = line["code"]
        return current, error


# --- what a backend reads from the parsed models ----------------------------


def num(*values: Any, allow_zero: bool = False) -> float | None:
    for value in values:
        try:
            number = float(value)
        except (TypeError, ValueError):
            continue
        if number < 0 or (number == 0 and not allow_zero):
            continue
        return number
    return None


def view(realtime: dict | tuple[str, str | None], charging: dict, gps: dict, hvac: dict, hybrid: bool) -> dict[str, Any]:
    """The values a backend derives from the dumps of the four replies: which raw
    field carries which meaning, as the owner's worker reads them."""
    if isinstance(realtime, tuple):
        assert realtime == ("BydDataUnavailableError", "not_ready")
        asleep = True
        placeholder = True
        online_state = 0
        raw: dict[str, Any] = {}
    else:
        raw = realtime["raw"]
        placeholder = "time" in raw and not float(raw.get("time") or 0) > 0 and not any(
            num(raw.get(key)) for key in ("leftFrontTirepressure", "enduranceMileage", "elecPercent")
        )
        online_state = realtime["online_state"]
        asleep = online_state != 1 or placeholder
    speed = None if asleep else num(realtime["speed"], allow_zero=True)
    gear = None if asleep else realtime["power_gear"]
    if asleep:
        vehicle_on = None
    elif speed is not None and speed >= 1:
        vehicle_on = True
    elif gear in (1, 3):
        vehicle_on = gear == 3
    elif realtime["vehicle_state"] in (0, 2):
        vehicle_on = False
    else:
        vehicle_on = None
    reported = num(raw.get("chargePower"))
    rate = None if asleep else num(realtime["rate"], allow_zero=True)
    pack = None if raw.get("gl") in (None, -999, -9) else float(raw["gl"])
    if asleep:
        power = None
    elif reported is not None:
        power = reported
    elif rate is not None and rate >= 100:
        power = rate / 1000
    elif pack is not None and pack >= 0 and (speed is None or speed < 1):
        power = pack / 1000
    else:
        power = 0.0 if rate == 0 else None
    soc = None if asleep else num(realtime["elec_percent"], realtime["power_battery"], allow_zero=True)
    status = hvac.get("status")
    fuel = None if asleep or not hybrid else num(realtime["oil_percent"])
    lat, lon = gps.get("latitude"), gps.get("longitude")
    return {
        "online": None if online_state == 1 and placeholder else online_state == 1,
        "soc": soc if soc is not None else num(charging["soc"], allow_zero=True),
        "odometer_km": None if asleep else num(realtime["total_mileage"]),
        "speed_kmh": speed,
        "vehicle_on": vehicle_on,
        "charge_power_kw": power,
        "charging": charging["charging_state"] == 1,
        "plugged": charging["connect_state"] not in (None, 0),
        "connect_state": charging["connect_state"],
        "minutes_to_full": None if charging["full_hour"] is None or charging["full_minute"] is None
        else charging["full_hour"] * 60 + charging["full_minute"],
        "hvac_on": True if status == 1 else False if status == 2 else None,
        "fuel_percent": fuel,
        "gps": None if lat == 0 and lon == 0 else (lat, lon),
    }


def wanted(line: dict[str, Any], hybrid: bool, index: int) -> dict[str, Any]:
    """What `view` must give for a scenario state of car number `index`."""
    online = line["online"]
    gps = None
    if line["gps"]:
        from math import cos, radians, sin

        anchor_lat, anchor_lon = 52.0 + (index % 100) * 0.01, 4.0 + (index // 100) * 0.01
        angle = (line["odometer_km"] or 0.0) / 5.0
        gps = (
            round(anchor_lat + 5.0 * sin(angle) / 111.32, 6),
            round(anchor_lon + 5.0 * cos(angle) / (111.32 * cos(radians(anchor_lat))), 6),
        )
    soc = line["soc"]
    speed = line["speed_kmh"]
    return {
        "online": online,
        # Asleep, a backend falls back to the charging reply's whole percent.
        "soc": soc if online or soc is None else float(int(soc + 0.5)),
        "odometer_km": num(line["odometer_km"]) if online else None,
        "speed_kmh": speed if online else None,
        "vehicle_on": (True if speed is not None and speed >= 1 else line["vehicle_on"]) if online else None,
        "charge_power_kw": line["charge_power_kw"] if online else None,
        "charging": line["charging"],
        "plugged": line["plugged"],
        "connect_state": line["connect_state"],
        "minutes_to_full": line["minutes_to_full"],
        "hvac_on": line["hvac_on"] if online else None,
        "fuel_percent": num(line["fuel_percent"]) if online and hybrid else None,
        "gps": gps,
    }


# --- clients --------------------------------------------------------------------


POLL = {"poll_attempts": 2, "poll_interval": 0.0, "mqtt_timeout": 0.0}


def dump(value: Any) -> dict[str, Any]:
    return value.model_dump(mode="json") if hasattr(value, "model_dump") else value


def make_client(library: str, replay: Any, account: str, mqtt: bool = False, **callbacks: Any) -> Any:
    config = {"username": f"{account}@example.com", "password": "secret", "base_url": replay.url,
              "mqtt_enabled": mqtt, "mqtt_timeout": 0.0}
    if library == "pybyd":
        return pybyd.BydClient(pybyd.BydConfig(**config), **callbacks)
    return openbyd.Client(**config, _test_tls=True, **callbacks)


async def read_car(client: Any, vin: str) -> dict[str, Any]:
    try:
        realtime: Any = dump(await client.get_vehicle_realtime(vin, **POLL))
    except Exception as error:  # noqa: BLE001 -- asleep placeholder raises not_ready since pyBYD #110
        if type(error).__name__ == "BydDataUnavailableError" and getattr(error, "code", None) == "not_ready":
            realtime = ("BydDataUnavailableError", "not_ready")
        else:
            raise
    return {
        "realtime": realtime,
        "charging": dump(await client.get_charging_status(vin)),
        "gps": dump(await client.get_gps_info(vin, **POLL)),
        "hvac": dump(await client.get_hvac_status(vin)),
        "energy": dump(await client.get_energy_consumption(vin)),
    }


def outcome(call: Any) -> Any:
    """The result of an awaitable, or the error's class name and code."""

    async def run() -> Any:
        try:
            return await call
        except Exception as error:  # noqa: BLE001 -- the class is what is compared
            return (type(error).__name__, getattr(error, "code", None))

    return run()


async def until(condition: Any, what: str, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    while not condition():
        assert time.monotonic() < deadline, f"timed out waiting for {what}"
        await asyncio.sleep(0.01)


# --- tests ----------------------------------------------------------------------


def test_both_clients_read_the_scenario_back(tmp_path: Path) -> None:
    path = write_scenario(tmp_path / "read.jsonl", [
        state(EV, 100),
        state(HYBRID, 100, fuel_percent=95.0, soc=54.0, odometer_km=168.0),
        state(EV, 200, speed_kmh=62.0, vehicle_on=True, odometer_km=20440.0, hvac_on=True),
        state(HYBRID, 250, plugged=True, charging=True, connect_state=1, charge_power_kw=6.3,
              minutes_to_full=95, soc=60.5, fuel_percent=12.0, gps=False),
        state(EV, 300, online=False, soc=70.0),
        state(EV, 400, soc=None, odometer_km=None, speed_kmh=None, vehicle_on=None, hvac_on=None,
              charge_power_kw=0.0),
    ])
    expected = Expected(path)
    times = [0, 99, 100, 150, 200, 250, 299, 300, 400, 10_000]
    seen: dict[str, dict] = {}

    async def main(library: str, replay: Any) -> None:
        async with make_client(library, replay, "acct-0001") as client:
            await client.login()
            vehicles = [dump(vehicle) for vehicle in await client.get_vehicles()]
            seen[(library, "vehicles")] = vehicles
            for t in times:
                replay.set_time(t)
                for vin in (EV, HYBRID):
                    seen[(library, vin, t)] = await read_car(client, vin)
            seen[(library, "refused")] = await outcome(client.get_charging_status(OTHER))

    for library in LIBRARIES:
        with ReplayServer(str(path)) as replay:
            asyncio.run(main(library, replay))

    vehicles = seen[("pybyd", "vehicles")]
    assert vehicles == seen[("openbyd", "vehicles")]
    assert [(v["vin"], v["model_name"], v["energy_type"]) for v in vehicles] == [
        (EV, "SEAL", 0), (HYBRID, "SEAL 6 DM I", 2)]
    for t in times:
        for index, vin in enumerate((EV, HYBRID)):
            replies = seen[("pybyd", vin, t)]
            assert replies == seen[("openbyd", vin, t)], f"pyBYD and openbyd differ for {vin} at {t}"
            line, error = expected.at(vin, t)
            assert error is None
            hybrid = vin == HYBRID
            got = view(replies["realtime"], replies["charging"], replies["gps"], replies["hvac"], hybrid)
            assert got == wanted(line, hybrid, index), f"{vin} at {t}"
            if line["online"]:
                assert replies["realtime"]["raw"]["time"] == START_UNIX + t
    assert seen[("pybyd", "refused")] == ("BydEndpointNotSupportedError", "1001")
    assert seen[("openbyd", "refused")] == ("BydEndpointNotSupportedError", "1001")


def test_errors_and_expiry_at_their_moments(tmp_path: Path) -> None:
    path = write_scenario(tmp_path / "errors.jsonl", [
        state(EV, 0, soc=80.0),
        {"type": "error", "vin": EV, "t": 100, "code": "1008"},
        state(EV, 110, soc=79.0),
        {"type": "error", "vin": EV, "t": 200, "code": "6002"},
        state(EV, 210, soc=78.0),
        {"type": "error", "vin": EV, "t": 300, "code": "1001"},
        state(EV, 310, soc=77.0),
        {"type": "error", "vin": EV, "t": 400, "code": "1005"},
    ])
    steps = [99, 100, 109, 110, 199, 200, 210, 299, 300, 310, 399, 400, 450]
    results: dict[str, list] = {}
    logins: dict[str, list] = {}

    async def main(library: str, replay: Any) -> None:
        async with make_client(library, replay, "acct-0001") as client:
            await client.login()
            await client.get_vehicles()
            for t in steps:
                replay.set_time(t)
                realtime = await outcome(client.get_vehicle_realtime(EV, **POLL))
                charging = await outcome(client.get_charging_status(EV))
                results.setdefault(library, []).append((
                    t,
                    realtime if isinstance(realtime, tuple) else dump(realtime)["elec_percent"],
                    charging if isinstance(charging, tuple) else dump(charging)["soc"],
                ))
                logins.setdefault(library, []).append(replay.stats()["logins"])

    for library in LIBRARIES:
        with ReplayServer(str(path)) as replay:
            asyncio.run(main(library, replay))
            stats = replay.stats()
            assert stats["replay"]["sessions_expired"] == 1
            assert stats["replay"]["errors_by_code"] == {"1001": 1, "1008": 4, "6002": 1}

    busy = ("BydServiceBusyError", "1008")
    assert results["pybyd"] == results["openbyd"] == [
        (99, 80.0, 80),
        (100, busy, busy),
        (109, busy, busy),
        (110, 79.0, 79),
        (199, 79.0, 79),
        (200, ("BydDataUnavailableError", "6002"), 79),
        (210, 78.0, 78),
        (299, 78.0, 78),
        (300, ("BydEndpointNotSupportedError", "1001"), 78),
        (310, 77.0, 77),
        (399, 77.0, 77),
        (400, 77.0, 77),
        (450, 77.0, 77),
    ]
    # One login, then exactly one more at the 1005 (not a second before).
    assert logins["pybyd"] == logins["openbyd"] == [1] * 11 + [2, 2]


def test_pushes_arrive_with_their_content_at_their_moment(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    path = write_scenario(tmp_path / "pushes.jsonl", [
        state(EV, 0, soc=80.0),
        {"type": "push", "vin": EV, "t": 100, "event": "vehicleInfo", "soc": 64.0, "speed_kmh": 30.0},
        {"type": "push", "vin": EV, "t": 200, "event": "smartCharge", "charging": True, "connect_state": 1},
        {"type": "push", "vin": EV, "t": 300, "event": "vehicleInfo", "soc": 63.0},
        {"type": "push", "vin": EV, "t": 400, "event": "vehicleInfo", "soc": 62.0},
    ])
    # pyBYD's paho verifies the broker against the system's store; give it the test CA instead.
    import paho.mqtt.client as mqtt

    ca = tmp_path / "ca.pem"
    original = mqtt.Client.tls_set

    def tls_set(self: Any, *args: Any, **kwargs: Any) -> None:
        original(self, ca_certs=str(ca))

    monkeypatch.setattr(mqtt.Client, "tls_set", tls_set)
    received: dict[str, list] = {}

    async def main(library: str, replay: Any) -> None:
        events: list = []
        received[library] = events

        def record(event: str, vin: str, data: dict) -> None:
            events.append((replay.now(), event, vin, data))

        async with make_client(library, replay, "acct-0001", mqtt=True, on_mqtt_event=record) as client:
            await client.login()
            await until(lambda: replay.stats()["mqtt"]["subscriptions"] >= 1, "a subscription")
            replay.set_time(99)
            await asyncio.sleep(0.2)
            assert events == [], "not a second early"
            replay.set_time(100)
            await until(lambda: len(events) == 1, "the vehicleInfo push")
            replay.set_time(200)
            await until(lambda: len(events) == 2, "the smartCharge push")
        # Disconnected: the next push is missed.
        await until(lambda: replay.stats()["mqtt"]["open"] == 0, "the disconnection")
        replay.set_time(300)
        async with make_client(library, replay, "acct-0001", mqtt=True, on_mqtt_event=record) as client:
            await client.login()
            await until(lambda: replay.stats()["mqtt"]["subscriptions"] >= 2, "a new subscription")
            replay.set_time(400)
            await until(lambda: len(events) == 3, "the push after reconnecting")

    for library in LIBRARIES:
        with ReplayServer(str(path)) as replay:
            ca.write_text(replay.ca_pem, encoding="utf-8")
            asyncio.run(main(library, replay))
            mqtt_stats = replay.stats()["mqtt"]
            assert (mqtt_stats["pushes_sent"], mqtt_stats["pushes_missed"]) == (3, 1)

    for library in LIBRARIES:
        events = received[library]
        assert [(t, event, vin) for t, event, vin, _ in events] == [
            (100.0, "vehicleInfo", EV), (200.0, "smartCharge", EV), (400.0, "vehicleInfo", EV)]
        info, charge, last = (data for *_, data in events)
        assert (info["elecPercent"], info["speed"], info["totalMileage"]) == (64.0, 30.0, 20431.4)
        assert info["time"] == START_UNIX + 100
        assert (charge["chargingState"], charge["connectState"], charge["elecPercent"]) == (1, 1, 80)
        assert last["elecPercent"] == 62.0
    assert [data for *_, data in received["pybyd"]] == [data for *_, data in received["openbyd"]]


def test_the_broker_certificate_verifies_for_127_0_0_1(tmp_path: Path) -> None:
    path = write_scenario(tmp_path / "tls.jsonl", [])
    with ReplayServer(str(path)) as replay:
        assert "BEGIN CERTIFICATE" in replay.ca_pem
        context = ssl.create_default_context(cadata=replay.ca_pem)
        host, port = replay.broker_address.split(":")
        with socket.create_connection((host, int(port)), timeout=10) as raw:
            with context.wrap_socket(raw, server_hostname="127.0.0.1") as tls:
                assert tls.version()


def test_broker_counters_do_not_depend_on_recording(world: Any, tmp_path: Path) -> None:
    """`TestBroker.connects` and the replay's MQTT stats count with recording off."""
    world.broker.set_recording(False)

    async def main() -> None:
        async with world.client(mqtt_enabled=True) as client:
            await client.login()
            await world.until_subscribed(client)

    asyncio.run(main())
    assert world.broker.connects >= 1

    path = write_scenario(tmp_path / "counters.jsonl", [state(EV, 0)])
    with ReplayServer(str(path)) as replay:  # records nothing by default
        async def login() -> None:
            async with make_client("openbyd", replay, "acct-0001", mqtt=True) as client:
                await client.login()
                await until(lambda: replay.stats()["mqtt"]["subscriptions"] >= 1, "a subscription")

        asyncio.run(login())
        assert replay.requests() == []
        stats = replay.stats()
        assert stats["requests"] >= 2 and stats["logins"] == 1 and stats["connections"] >= 1
        assert stats["mqtt"]["connects"] >= 1 and stats["mqtt"]["subscriptions"] >= 1


def test_manual_clock_and_management(tmp_path: Path) -> None:
    path = write_scenario(tmp_path / "manage.jsonl", [state(EV, 0)], duration=600)
    replay = ReplayServer(str(path), epoch="now", start_at=60, corpus=str(tmp_path / "no-corpus"), record=True)
    try:
        assert replay.url.startswith("http://127.0.0.1:")
        assert replay.broker_address.startswith("127.0.0.1:")
        assert replay.now() == 60.0
        clock = replay.clock()
        assert (clock["t"], clock["started"], clock["finished"]) == (60.0, False, False)
        started = replay.start()
        assert set(started) == {"wall_unix_ms", "start_at", "speed", "epoch_unix_ms"}
        assert started["epoch_unix_ms"] == started["wall_unix_ms"] - 60_000 == replay.epoch_ms
        assert replay.start() == started
        replay.advance(30)
        assert replay.now() == 90.0
        replay.set_time(10_000)
        assert replay.clock()["finished"] is True and replay.now() == 600.0
        with pytest.raises(ValueError):
            replay.set_time(float("nan"))

        async def main() -> float:
            async with make_client("openbyd", replay, "acct-0001") as client:
                await client.login()
                return (await client.get_vehicle_realtime(EV, **POLL))["elec_percent"]

        assert asyncio.run(main()) == 71.0
        assert [request["endpoint"] for request in replay.requests("/app/account/login")] == ["/app/account/login"]
        assert len(replay.requests()) >= 3
        stats = replay.stats()
        assert set(stats) == {"requests", "by_endpoint", "by_code", "connections", "logins", "mqtt", "replay"}
        assert set(stats["mqtt"]) == {"open", "connects", "subscriptions", "pushes_sent", "pushes_missed"}
        assert stats["replay"]["accounts"] == 2 and stats["replay"]["cars"] == 3
        assert stats["logins"] == 1 and stats["by_code"] == {"0": stats["requests"]}
        replay.reset_stats()
        assert replay.stats()["requests"] == 0
    finally:
        replay.stop()
        replay.stop()
    with pytest.raises(RuntimeError, match="stopped"):
        replay.stats()
    with pytest.raises(ValueError, match="epoch"):
        ReplayServer(str(path), epoch="later")
    with pytest.raises(ValueError, match="speed"):
        ReplayServer(str(path), speed=0)
    with pytest.raises(ValueError, match="cannot open"):
        ReplayServer(str(tmp_path / "missing.jsonl"))
    bad = tmp_path / "bad.jsonl"
    bad.write_text('{"type": "scenario", "version": 2}\n', encoding="utf-8")
    with pytest.raises(ValueError, match="line 1"):
        ReplayServer(str(bad))
    wall = ReplayServer(str(path), manual=False, speed=100.0)
    with wall:
        with pytest.raises(ValueError, match="manual"):
            wall.set_time(5)
        wall.start()
        deadline = time.monotonic() + 10
        while wall.now() < 60 and time.monotonic() < deadline:
            time.sleep(0.01)
        assert wall.now() >= 60


def test_the_demo_scenario_with_both_clients() -> None:
    if not DEMO.exists():
        pytest.skip("bench/replay/samples/demo.jsonl is not there")
    expected = Expected(DEMO)
    steps = [expected.duration * step // 6 for step in range(7)]
    seen: dict[tuple, Any] = {}

    async def main(library: str, replay: Any) -> None:
        clients = [make_client(library, replay, account["account"]) for account in expected.accounts]
        for client in clients:
            await client.__aenter__()
            await client.login()
        try:
            for t in steps:
                replay.set_time(t)
                for account, client in zip(expected.accounts, clients):
                    for vin in account["cars"]:
                        seen[(library, vin, t)] = await outcome(read_car(client, vin))
        finally:
            for client in clients:
                await client.__aexit__(None, None, None)

    for library in LIBRARIES:
        with ReplayServer(str(DEMO)) as replay:
            asyncio.run(main(library, replay))

    for t in steps:
        for index, vin in enumerate(expected.order):
            replies = seen[("pybyd", vin, t)]
            assert replies == seen[("openbyd", vin, t)], f"{vin} at {t}"
            line, error = expected.at(vin, t)
            if error is not None:
                assert isinstance(replies, tuple) and replies[1] == error, f"{vin} at {t}"
                continue
            hybrid = expected.cars[vin]["energy"] == "hybrid"
            got = view(replies["realtime"], replies["charging"], replies["gps"], replies["hvac"], hybrid)
            assert got == wanted(line, hybrid, index), f"{vin} at {t}"


def test_a_generated_scenario_replays(tmp_path: Path) -> None:
    generate = pytest.importorskip("bench.replay.generate", reason="the scenario generator is not there")
    path = tmp_path / "generated.jsonl"
    generate.generate(path, accounts=3, hours=1, seed="replay-test")
    expected = Expected(path)
    readings: list = []

    async def main(replay: Any) -> None:
        for t in (0, expected.duration // 2, expected.duration):
            replay.set_time(t)
            for account in expected.accounts:
                async with make_client("openbyd", replay, account["account"]) as client:
                    await client.login()
                    for vin in account["cars"]:
                        readings.append((vin, t, await outcome(read_car(client, vin))))

    with ReplayServer(str(path)) as replay:
        asyncio.run(main(replay))
    assert readings
    for vin, t, replies in readings:
        line, error = expected.at(vin, t)
        if error is not None:
            assert replies[1] == error
            continue
        hybrid = expected.cars[vin]["energy"] == "hybrid"
        got = view(replies["realtime"], replies["charging"], replies["gps"], replies["hvac"], hybrid)
        assert got == wanted(line, hybrid, expected.order.index(vin)), f"{vin} at {t}"


# --- the replay_server example as a process ------------------------------------


def replay_binary() -> Path | None:
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    name = "replay_server.exe" if os.name == "nt" else "replay_server"
    for profile in ("release", "debug"):
        candidate = target / profile / "examples" / name
        if candidate.exists():
            return candidate
    return None


class Process:
    """The replay_server example, spoken to over its line protocol. Every read
    has a deadline, so a line that never comes fails the test instead of
    hanging it."""

    DEADLINE = 10.0

    def __init__(self, *args: str) -> None:
        binary = replay_binary()
        if binary is None:
            pytest.skip("the replay_server example is not built (cargo build --example replay_server --features testing)")
        self.process = subprocess.Popen(
            [str(binary), *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
        )
        self.lines: queue.Queue[str | None] = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        assert self.process.stdout is not None
        for text in self.process.stdout:
            self.lines.put(text.rstrip("\n"))
        self.lines.put(None)

    def line(self) -> str:
        try:
            text = self.lines.get(timeout=self.DEADLINE)
        except queue.Empty:
            pytest.fail(f"no line from replay_server within {self.DEADLINE} s")
        if text is None:
            pytest.fail("replay_server closed its output")
        return text

    def ask(self, command: str) -> str:
        assert self.process.stdin is not None
        self.process.stdin.write(command + "\n")
        self.process.stdin.flush()
        return self.line()

    def close(self) -> int:
        """Ends the process (by closing its input, if it is still running) and returns its status."""
        if self.process.poll() is None and self.process.stdin is not None:
            try:
                self.process.stdin.close()
            except OSError:
                pass
        try:
            return self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            return self.process.wait()


def test_the_replay_server_process_speaks_its_protocol(tmp_path: Path) -> None:
    if not DEMO.exists():
        pytest.skip("bench/replay/samples/demo.jsonl is not there")
    ca = tmp_path / "ca.pem"
    process = Process("--scenario", str(DEMO), "--speed", "60", "--ca-out", str(ca), "--workers", "1")
    try:
        url, broker, ca_line, ready = (process.line() for _ in range(4))
        assert url.startswith("URL http://127.0.0.1:")
        assert broker.startswith("MQTT 127.0.0.1:")
        assert ca_line == f"CA {ca}"
        assert ready == "READY"
        assert "BEGIN CERTIFICATE" in ca.read_text(encoding="utf-8")
        clock = json.loads(process.ask("clock"))
        assert set(clock) == {"t", "started", "speed", "wall_unix_ms", "finished"}
        assert (clock["t"], clock["started"], clock["speed"], clock["finished"]) == (0.0, False, 60.0, False)
        started = process.ask("start")
        assert started.startswith("STARTED ")
        info = json.loads(started.removeprefix("STARTED "))
        assert set(info) == {"wall_unix_ms", "start_at", "speed", "epoch_unix_ms"}
        assert info["epoch_unix_ms"] == START_UNIX * 1000
        assert (info["start_at"], info["speed"]) == (0.0, 60.0)
        assert process.ask("start") == started, "a second start answers the same"
        assert json.loads(process.ask("clock"))["started"] is True

        async def poll() -> float:
            async with openbyd.Client(username="acct-0001@example.com", password="secret",
                                      base_url=url.removeprefix("URL "), mqtt_enabled=False) as client:
                await client.login()
                return (await client.get_charging_status(EV))["vin"]

        assert asyncio.run(poll()) == EV
        stats = json.loads(process.ask("stats"))
        assert set(stats) == {"requests", "by_endpoint", "by_code", "connections", "logins", "mqtt", "replay"}
        assert stats["logins"] == 1 and stats["requests"] == 2
        assert stats["replay"]["accounts"] == 3 and stats["replay"]["cars"] == 4
        assert set(stats["mqtt"]) == {"open", "connects", "subscriptions", "pushes_sent", "pushes_missed"}
        assert process.ask("reset") == "OK"
        assert json.loads(process.ask("stats"))["requests"] == 0
        assert process.ask("frobnicate").startswith("ERR ")
        assert json.loads(process.ask("clock"))["t"] > 0
        assert process.ask("quit") == "OK"
        assert process.process.wait(timeout=10) == 0
    finally:
        assert process.close() == 0


def test_the_replay_server_process_autostarts_and_refuses_bad_arguments(tmp_path: Path) -> None:
    if not DEMO.exists():
        pytest.skip("bench/replay/samples/demo.jsonl is not there")
    process = Process("--scenario", str(DEMO), "--autostart", "--epoch", "now", "--start-at", "100",
                      "--delay-ms", "1")
    try:
        lines = [process.line(), process.line(), process.line()]
        assert lines[2] == "READY", "no CA line without --ca-out"
        assert json.loads(process.ask("clock"))["started"] is True
        info = json.loads(process.ask("start").removeprefix("STARTED "))
        assert info["start_at"] == 100.0
        assert info["epoch_unix_ms"] == info["wall_unix_ms"] - 100_000
    finally:
        # End of input exits too, without a word.
        assert process.process.stdin is not None
        process.process.stdin.close()
        assert process.process.wait(timeout=10) == 0
        assert process.lines.get(timeout=Process.DEADLINE) is None, "nothing after the end of input"
    for args in (
        [],
        ["--scenario", str(DEMO), "--speed", "0"],
        ["--scenario", str(DEMO), "--epoch", "later"],
        ["--scenario", str(DEMO), "--start-at", "999999999"],
        ["--scenario", str(DEMO), "--bogus"],
        ["--scenario", str(tmp_path / "missing.jsonl")],
    ):
        bad = Process(*args)
        assert bad.process.wait(timeout=10) == 2, args
        assert bad.process.stderr is not None and bad.process.stderr.read().strip()
