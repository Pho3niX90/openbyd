"""openbyd.Client: construction, lifecycle, every option and its effect, sessions, scale."""

from __future__ import annotations

import asyncio
import gc
import logging
import threading
import time
from typing import Any

import pytest

import openbyd
from openbyd import exceptions, runtime

from conftest import HVAC, LATEST_CONFIG, LOGIN, PASSWORD, PIN, REMOTE, USER, VEHICLES, VIN, until

DONE = {"controlState": 1, "requestSerial": "S1"}


def run(coroutine: Any) -> Any:
    return asyncio.run(coroutine)


# ---------------------------------------------------------------------------
# Construction and lifecycle
# ---------------------------------------------------------------------------


def test_construction_checks_its_arguments() -> None:
    with pytest.raises(TypeError, match="unexpected keyword"):
        openbyd.Client(username=USER, password=PASSWORD, colour="red")
    with pytest.raises(openbyd.ValidationError):
        openbyd.Client(password=PASSWORD)
    with pytest.raises(openbyd.ValidationError):
        openbyd.Client(username=USER, password=PASSWORD, device={"nope": "1"})
    with pytest.raises(TypeError, match="callable"):
        openbyd.Client(username=USER, password=PASSWORD, on_vehicle_info=5)
    with pytest.raises(ValueError, match="command_retries"):
        openbyd.Client(username=USER, password=PASSWORD, command_retries=0)
    with pytest.raises(ValueError, match="finite"):
        openbyd.Client(username=USER, password=PASSWORD, command_ack_ttl_seconds=float("nan"))
    with pytest.raises(ValueError, match="gate_policy"):
        openbyd.Client(username=USER, password=PASSWORD, gate_policy="lenient")
    with pytest.raises(ValueError, match="relogin"):
        openbyd.Client(username=USER, password=PASSWORD, relogin="sometimes")
    with pytest.raises(TypeError, match="config must be"):
        openbyd.Client(42)
    # A mapping, or pyBYD's own BydConfig, works as the config; keywords win.
    openbyd.Client({"username": USER, "password": PASSWORD}, country_code="DE", gate_policy="strict")
    pybyd = pytest.importorskip("pybyd")
    openbyd.Client(pybyd.BydConfig(username=USER, password=PASSWORD), relogin="password")


def test_a_client_must_be_started(world) -> None:
    client = world.client()

    async def main() -> None:
        with pytest.raises(exceptions.BydError, match="not initialized"):
            await client.get_hvac_status(VIN)
        async with client:
            assert (await client.get_hvac_status(VIN))["ac_switch"] == 1
        with pytest.raises(exceptions.BydError):
            await client.login()
        await client.async_start()
        await client.login()
        await client.async_close()

    run(main())


def test_config_fields_reach_the_requests(world) -> None:
    async def main() -> None:
        async with world.client(country_code="DE", language="de", time_zone="UTC", device={"model": "Pixel"}) as client:
            await client.login()

    run(main())
    login = world.requests(LOGIN)[0]
    assert (login["outer"]["countryCode"], login["outer"]["language"]) == ("DE", "de")
    assert (login["outer"]["model"], login["inner"]["timeZone"]) == ("Pixel", "UTC")


# ---------------------------------------------------------------------------
# Options
# ---------------------------------------------------------------------------


def lock_with(world, *, functions: dict | None = None, **options: Any) -> dict | BaseException:
    if functions is not None:
        world.server.reply(LATEST_CONFIG, world.Reply.data({VIN: functions}))
    world.server.reply(REMOTE, world.Reply.data(DONE))

    async def main() -> Any:
        async with world.client(**options) as client:
            await client.verify_command_access(VIN)
            try:
                return await client.lock(VIN)
            except Exception as error:  # noqa: BLE001 - the test looks at it
                return error

    return run(main())


def test_gate_policy(world, caplog) -> None:
    missing = {"cfFixedList": []}
    refused = lock_with(world, functions=missing)
    assert isinstance(refused, exceptions.BydEndpointNotSupportedError) and refused.code == "command_gate_blocked"
    assert world.requests(REMOTE) == []
    assert lock_with(world, functions=missing, gate_policy="allow_missing_data")["success"] is True
    assert len(world.requests(REMOTE)) == 1

    verdicts: list[tuple[str, dict, int]] = []

    def policy(vin: str, verdict: dict) -> bool:
        verdicts.append((vin, verdict, threading.get_ident()))
        return verdict["reason"] == "function_no_missing"

    assert lock_with(world, functions=missing, gate_policy=policy)["success"] is True
    assert verdicts[0][0] == VIN and verdicts[0][1]["supported"] is False
    assert verdicts[0][2] == threading.get_ident(), "the policy runs on the client's loop"

    def broken(vin: str, verdict: dict) -> bool:
        raise RuntimeError("policy bug")

    with caplog.at_level(logging.ERROR, logger="openbyd"):
        refused = lock_with(world, functions=missing, gate_policy=broken)
    assert isinstance(refused, exceptions.BydEndpointNotSupportedError), "a failing policy falls back to the verdict"
    assert "gate_policy" in caplog.text and "policy bug" in caplog.text


def test_command_retries(world) -> None:
    world.server.reply(LATEST_CONFIG, world.Reply.data({VIN: {"cfFixedList": [{"functionNo": "1005"}]}}))
    world.server.reply(REMOTE, world.Reply.data({"controlState": 2, "requestSerial": "S1"}))

    async def main() -> Any:
        async with world.client(command_retries=1) as client:
            await client.verify_command_access(VIN)
            with pytest.raises(exceptions.BydRemoteControlError) as raised:
                await client.lock(VIN)
            return raised.value

    error = run(main())
    assert error.code == "2"
    assert len(world.requests(REMOTE)) == 1, "one attempt, no retry"


def test_pin_resolver(world, caplog) -> None:
    calls: list[tuple] = []

    def resolver(command_pwd: str | None, control_pin: str | None) -> str:
        calls.append((command_pwd, control_pin))
        return "0123456789abcdef0123456789abcdef"

    lock_with(world, pin_resolver=resolver)
    assert calls[0] == (None, PIN)
    sent = world.requests(REMOTE)[0]["inner"]["commandPwd"]
    assert sent == "0123456789abcdef0123456789abcdef", "a resolver that keeps the digest's case"

    world.server.clear_requests()
    with caplog.at_level(logging.WARNING, logger="openbyd"):
        lock_with(world, pin_resolver=lambda given, pin: 1234)
    import hashlib

    assert world.requests(REMOTE)[0]["inner"]["commandPwd"] == hashlib.md5(PIN.encode()).hexdigest().upper()
    assert "pin_resolver" in caplog.text


def test_request_observer(world, caplog) -> None:
    records: list[dict] = []
    world.server.reply("/control/smartCharge/homePage", world.Reply.code("1001"))

    async def main() -> None:
        async with world.client(request_observer=records.append) as client:
            await client.login()
            await client.get_hvac_status(VIN)
            with pytest.raises(exceptions.BydEndpointNotSupportedError):
                await client.get_charging_status(VIN)

    run(main())
    assert [record["endpoint"] for record in records] == [LOGIN, HVAC, "/control/smartCharge/homePage"]
    assert [record["code"] for record in records] == ["0", "0", "1001"]
    assert all(record["outcome"] == "reply" and record["duration"] >= 0 for record in records)

    def broken(record: dict) -> None:
        raise ValueError("observer bug")

    async def still_works() -> Any:
        async with world.client(request_observer=broken) as client:
            return await client.get_hvac_status(VIN)

    with caplog.at_level(logging.ERROR, logger="openbyd"):
        assert run(still_works())["ac_switch"] == 1
    assert "request_observer" in caplog.text


def test_request_observer_sees_transport_failures() -> None:
    records: list[dict] = []

    async def main() -> None:
        async with openbyd.Client(
            username=USER, password=PASSWORD, base_url="http://127.0.0.1:9", request_observer=records.append
        ) as client:
            with pytest.raises(exceptions.BydTransportError):
                await client.login()
            await asyncio.sleep(0.05)

    run(main())
    assert records[0]["outcome"] == "transport" and records[0]["status"] is None


def test_command_ack_ttl(world) -> None:
    world.server.reply(REMOTE, world.Reply.data(DONE))

    async def pending_after(ttl: float) -> dict:
        async with world.client(command_ack_ttl_seconds=ttl) as client:
            await client.verify_command_access(VIN)
            await client.lock(VIN)
            await asyncio.sleep(0.1)
            return client.get_command_ack_diagnostics()

    kept = run(pending_after(300.0))
    assert (kept["pending"], kept["expired"], kept["pending_by_vin"]) == (1, 0, {VIN: 1})
    expired = run(pending_after(0.05))
    assert (expired["pending"], expired["expired"]) == (0, 1)


def test_seeded_metadata_saves_the_vehicle_list(world) -> None:
    world.server.reply("/vehicleInfo/vehicle/vehicleRealTimeRequest", world.Reply.data({"elecPercent": 50, "time": 1700000000}))

    async def main() -> Any:
        async with world.client() as client:
            assert client.vehicle_metadata(VIN) is None
            client.seed_vehicle_metadata(VIN, 2, "SONG")
            assert client.vehicle_metadata(VIN)["energy_type"] == 2
            client.seed_vehicle_metadata("TESTVIN0000000002", 7)
            assert client.vehicle_metadata("TESTVIN0000000002")["energy_type"] == -1, "UNKNOWN, as pyBYD's EnergyType(7)"
            return await client.get_vehicle_realtime(VIN, poll_attempts=0, mqtt_timeout=0)

    assert run(main())["elec_percent"] == 50
    assert world.requests(VEHICLES) == []


# ---------------------------------------------------------------------------
# Sessions and re-login
# ---------------------------------------------------------------------------


def test_export_and_restore_without_a_password_login(world) -> None:
    async def first() -> dict:
        async with world.client() as client:
            assert client.export_session() is None
            await client.login()
            return client.export_session()

    saved = run(first())
    assert set(saved) == {"user_id", "sign_token", "encry_token", "ttl_remaining", "guest_vins"}
    assert 0 < saved["ttl_remaining"] <= 12 * 3600

    async def restored() -> Any:
        client = openbyd.Client.from_session(saved, username=USER, base_url=world.server.url, mqtt_enabled=False)
        async with client:
            assert client.export_session()["sign_token"] == saved["sign_token"]
            return await client.get_hvac_status(VIN)

    assert run(restored())["ac_switch"] == 1
    assert len(world.requests(LOGIN)) == 1, "the restored client never logged in"

    async def imported() -> Any:
        async with world.client() as client:
            await client.import_session(saved)
            return await client.ensure_session()

    assert run(imported())["sign_token"] == saved["sign_token"]
    assert len(world.requests(LOGIN)) == 1
    with pytest.raises(ValueError, match="user_id"):
        run(openbyd.Client(username=USER, password=PASSWORD, session={"user_id": ""}).start())


def test_relogin_never(world) -> None:
    async def main() -> Any:
        async with world.client(relogin="never") as client:
            await client.login()
            world.server.expire_sessions(USER)
            with pytest.raises(exceptions.BydSessionExpiredError) as raised:
                await client.get_hvac_status(VIN)
            return raised.value

    error = run(main())
    assert error.code == "relogin_disabled"
    assert len(world.requests(LOGIN)) == 1


def test_relogin_handler(world) -> None:
    async def fresh_session() -> dict:
        async with world.client() as helper:
            await helper.login()
            return helper.export_session()

    calls: list[int] = []

    async def handler() -> dict:
        calls.append(threading.get_ident())
        return await fresh_session()

    async def main() -> Any:
        async with world.client(relogin=handler) as client:
            await client.login()
            world.server.expire_sessions(USER)
            return await client.get_hvac_status(VIN)

    assert run(main())["ac_switch"] == 1
    assert calls == [threading.get_ident()]

    async def failing() -> dict:
        raise exceptions.BydAuthenticationError("no", code="1002", endpoint="x")

    async def refused(handler: Any) -> BaseException:
        async with world.client(relogin=handler) as client:
            with pytest.raises(exceptions.BydError) as raised:
                await client.get_hvac_status(VIN)
            return raised.value

    error = run(refused(failing))
    assert type(error) is exceptions.BydAuthenticationError and error.code == "1002"

    async def nonsense() -> Any:
        return 42

    assert "relogin returned no session" in str(run(refused(nonsense)))

    async def broken() -> dict:
        raise KeyError("bug")

    assert "KeyError" in str(run(refused(broken)))


# ---------------------------------------------------------------------------
# Scale, cancellation, closing
# ---------------------------------------------------------------------------


def test_a_thousand_clients_are_cheap(world) -> None:
    async def main() -> None:
        clients = [world.client() for _ in range(1000)]
        started = time.perf_counter()
        for client in clients:
            await client.start()
        for client in clients:
            await client.close()
        assert time.perf_counter() - started < 10
        await clients[0].start()
        await clients[0].login()
        await clients[0].close()

    run(main())
    gc.collect()
    assert len(world.requests(LOGIN)) == 1


def test_two_hundred_clients_poll_concurrently(world) -> None:
    world.server.reply("/vehicleInfo/vehicle/vehicleRealTimeRequest", world.Reply.data({"requestSerial": "R1"}))
    world.server.reply(
        "/vehicleInfo/vehicle/vehicleRealTimeResult",
        world.Reply.data({"requestSerial": "R1", "elecPercent": 70, "time": 1700000000}).delayed(0.02),
    )

    async def one() -> Any:
        async with world.client() as client:
            client.seed_vehicle_metadata(VIN, 0)
            return await client.get_vehicle_realtime(VIN, poll_attempts=3, poll_interval=0.01, mqtt_timeout=0)

    async def main() -> list:
        return await asyncio.gather(*(one() for _ in range(200)))

    results = run(main())
    assert [result["elec_percent"] for result in results] == [70] * 200
    assert len(world.requests(LOGIN)) == 200


def test_cancelling_an_in_flight_call(world) -> None:
    world.server.reply(HVAC, world.Reply.data({"acSwitch": 0}).delayed(5.0))

    async def main() -> Any:
        async with world.client() as client:
            await client.login()
            before = runtime.stats()["dropped"]
            task = asyncio.create_task(client.get_hvac_status(VIN))
            await until(lambda: len(world.requests(HVAC)) == 1, "the request")
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            await until(lambda: runtime.stats()["dropped"] > before, "the Rust future to be dropped")
            world.server.reply(HVAC, world.Reply.data({"acSwitch": 1}))
            return await client.get_hvac_status(VIN)

    started = time.perf_counter()
    assert run(main())["ac_switch"] == 1
    assert time.perf_counter() - started < 4, "the cancelled call did not hold the client"


def test_closing_with_calls_in_flight(world) -> None:
    world.server.reply(HVAC, world.Reply.data({"acSwitch": 1}).delayed(0.3))

    async def main() -> tuple:
        client = world.client()
        await client.start()
        await client.login()
        in_flight = asyncio.create_task(client.get_hvac_status(VIN))
        await until(lambda: len(world.requests(HVAC)) == 1, "the request")
        await client.close()
        with pytest.raises(exceptions.BydError, match="not initialized"):
            await client.get_hvac_status(VIN)
        return await asyncio.wait_for(in_flight, 5)

    assert run(main())["ac_switch"] == 1


def test_closing_cancels_mqtt_waits(world) -> None:
    world.server.reply("/control/getGpsInfo", world.Reply.data({"requestSerial": "G1"}))

    async def main() -> None:
        async with world.client(mqtt_enabled=True) as client:
            await client.login()
            await world.until_subscribed(client)
            waiting = asyncio.create_task(client.get_gps_info(VIN, mqtt_timeout=30))
            await until(lambda: len(world.requests("/control/getGpsInfo")) == 1, "the trigger")
            await asyncio.sleep(0.05)
            await client.close()
            with pytest.raises(asyncio.CancelledError):
                await asyncio.wait_for(waiting, 5)

    run(main())


def test_the_extension_keeps_working_across_event_loops(world) -> None:
    client = world.client()

    async def use() -> Any:
        await client.start()
        return await client.get_hvac_status(VIN)

    assert run(use())["ac_switch"] == 1
    assert run(use())["ac_switch"] == 1
    assert world.requests(LOGIN), "logged in"
