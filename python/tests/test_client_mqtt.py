"""openbyd.Client and openbyd.Car over MQTT from Python, through the TLS test broker.

Pushes reach the callbacks on the client's event loop, a command is
acknowledged by push, and a car merges pushes, projects its commands and
rolls back a refused one.
"""

from __future__ import annotations

import asyncio
import logging
import threading
from typing import Any

import pytest

import openbyd
from openbyd import exceptions

from conftest import LATEST_CONFIG, REMOTE, REMOTE_RESULT, VIN, until

DOORS = ("left_front_door_lock", "right_front_door_lock", "left_rear_door_lock", "right_rear_door_lock")


def run(coroutine: Any) -> Any:
    return asyncio.run(coroutine)


def realtime(lock: int, time: int, **more: Any) -> dict:
    payload = {"onlineState": 1, "time": time, "elecPercent": 80, **more}
    for name in ("leftFrontDoorLock", "rightFrontDoorLock", "leftRearDoorLock", "rightRearDoorLock"):
        payload[name] = lock
    return payload


def vehicle_info(data: dict, uuid: str | None = None) -> dict:
    body: dict[str, Any] = {"respondData": data}
    if uuid is not None:
        body["uuid"] = uuid
    return {"event": "vehicleInfo", "vin": VIN, "data": body}


def doors(car: openbyd.Car) -> list:
    return [car.state["realtime"][name] for name in DOORS]


def test_pushes_reach_the_callbacks_on_the_loop(world, caplog) -> None:
    seen: list[tuple] = []
    threads: set[int] = set()

    def record(name: str, fail: bool = False):
        def callback(*args: Any) -> None:
            threads.add(threading.get_ident())
            seen.append((name, *args))
            if fail:
                raise RuntimeError(f"{name} bug")

        return callback

    async def main() -> None:
        async with world.client(
            mqtt_enabled=True,
            on_vehicle_info=record("info", fail=True),
            on_mqtt_event=record("event"),
            on_mqtt_connect=record("connect"),
        ) as client:
            await client.login()
            assert client.mqtt_is_running
            await world.until_subscribed(client)
            await until(lambda: any(item[0] == "connect" for item in seen), "on_mqtt_connect")
            world.push(client, vehicle_info({"elecPercent": 64, "time": 1700000000}))
            world.push(client, {"event": "smartCharge", "vin": VIN, "data": {"respondData": {"elecPercent": 64}}})
            await until(lambda: len([item for item in seen if item[0] == "event"]) == 2, "two events")
            # The raising on_vehicle_info did not stop the next push.
            world.push(client, vehicle_info({"elecPercent": 65, "time": 1700000001}))
            await until(lambda: len([item for item in seen if item[0] == "info"]) == 2, "two infos")

    with caplog.at_level(logging.ERROR, logger="openbyd"):
        run(main())
    infos = [item for item in seen if item[0] == "info"]
    assert [(vin, data["elec_percent"]) for _, vin, data in infos] == [(VIN, 64), (VIN, 65)]
    events = [item for item in seen if item[0] == "event"]
    assert events[0][1:3] == ("vehicleInfo", VIN) and events[0][3]["elecPercent"] == 64
    assert events[1][1] == "smartCharge"
    assert threads == {threading.get_ident()}, "every callback ran on the loop's thread"
    assert caplog.text.count("on_vehicle_info") >= 2 and "info bug" in caplog.text


def test_a_command_is_acknowledged_by_push(world) -> None:
    world.server.reply(REMOTE, world.Reply.data({"requestSerial": "C1"}))
    acks: list[dict] = []
    lifecycle: list[str] = []

    async def main() -> Any:
        async with world.client(
            mqtt_enabled=True,
            on_command_ack=acks.append,
            on_command_lifecycle=lambda event: lifecycle.append(event["status"]),
        ) as client:
            await client.login()
            await world.until_subscribed(client)
            await client.verify_command_access(VIN)
            locking = asyncio.create_task(client.lock(VIN))
            await until(lambda: len(world.requests(REMOTE)) == 1, "the command")
            await asyncio.sleep(0.05)
            world.push(client, {"event": "remoteControl", "vin": VIN, "data": {"uuid": "C1", "respondData": {"res": 2}}})
            result = await asyncio.wait_for(locking, 10)
            await until(lambda: len(acks) == 1, "the ack callback")
            return result, client.get_command_ack_diagnostics()

    result, diagnostics = run(main())
    assert result["success"] is True and result["request_serial"] == "C1"
    assert world.requests(REMOTE_RESULT) == [], "acknowledged by push, not polled"
    assert acks[0]["request_serial"] == "C1" and acks[0]["success"] is True
    assert lifecycle == ["registered", "matched"]
    assert (diagnostics["matched"], diagnostics["pending"]) == (1, 0)


def test_realtime_answered_by_push(world) -> None:
    world.server.reply("/vehicleInfo/vehicle/vehicleRealTimeRequest", world.Reply.data({"requestSerial": "R7"}))

    async def main() -> Any:
        async with world.client(mqtt_enabled=True) as client:
            await client.login()
            await world.until_subscribed(client)
            client.seed_vehicle_metadata(VIN, 0)
            reading = asyncio.create_task(client.get_vehicle_realtime(VIN))
            await until(lambda: len(world.requests("/vehicleInfo/vehicle/vehicleRealTimeRequest")) == 1, "the trigger")
            await asyncio.sleep(0.05)
            world.push(client, vehicle_info({"elecPercent": 55, "time": 1700000000}, uuid="R7"))
            return await asyncio.wait_for(reading, 10)

    assert run(main())["elec_percent"] == 55
    assert world.requests("/vehicleInfo/vehicle/vehicleRealTimeResult") == []


def test_mqtt_recovers_after_refused_connections(world) -> None:
    connects: list[int] = []

    async def main() -> None:
        async with world.client(mqtt_enabled=True, on_mqtt_connect=lambda: connects.append(1)) as client:
            await client.login()
            await until(lambda: connects, "the first connection")
            world.broker.refuse_next(1)
            world.broker.drop_all()
            await until(lambda: len(connects) >= 2, "a reconnection")
            assert world.broker.connects >= 3 and world.broker.open >= 1
            await client.start_mqtt()

    run(main())


def test_a_car_merges_projects_and_rolls_back(world) -> None:
    world.server.reply("/vehicleInfo/vehicle/vehicleRealTimeRequest", world.Reply.data(realtime(1, 1700000000)))
    world.server.reply("/control/getGpsInfo", world.Reply.data({"latitude": 52.0, "longitude": 4.0}))
    world.server.reply("/control/smartCharge/homePage", world.Reply.data({"elecPercent": 80, "chargingState": 0}))
    world.server.reply("/vehicleInfo/vehicle/getEnergyConsumption", world.Reply.data({"totalEnergy": "15.2kW·h/100km"}))
    world.server.reply("/control/smartCharge/getAcCurrentLimit", world.Reply.data({"acCurrentLimit": 16}))
    changes: list[str] = []

    async def main() -> openbyd.Car:
        async with world.client(mqtt_enabled=True) as client:
            await client.login()
            await world.until_subscribed(client)
            await client.verify_command_access(VIN)
            car = await client.get_car(VIN, on_state_changed=lambda vin, state: changes.append(vin))
            assert await client.get_car(VIN) is car, "kept"
            assert car.vin == VIN and repr(car) == f"<openbyd.Car vin='{VIN}'>"
            assert car.capabilities["lock"] is True and car.lock.available
            assert car.hvac.available and car.hvac.temp_range == (15.0, 31.0) and car.hvac.temp_offset == 14.0
            assert car.seat.is_available(openbyd.SeatPosition.DRIVER, "heat") in (True, False)
            assert set(car.features) == {"lock", "hvac", "seat", "steering", "battery", "finder", "windows", "trunk"}
            assert "LockFeature" in repr(car.lock)

            # every section
            assert (await car.update_realtime())["elec_percent"] == 80
            await car.update_hvac()
            await car.update_gps()
            await car.update_charging()
            await car.update_energy()
            assert (await car.get_ac_current_limit()) is not None
            state = car.state
            assert all(state[section] is not None for section in ("realtime", "hvac", "gps", "charging", "energy", "charging_schedule"))
            await until(lambda: len(changes) >= 6, "state change callbacks")
            assert doors(car) == [1, 1, 1, 1]

            # a command acknowledged by push: projected at once, then confirmed
            world.server.reply(REMOTE, world.Reply.data({"requestSerial": "C1"}))
            locking = asyncio.create_task(car.lock.lock())
            await until(lambda: len(world.requests(REMOTE)) == 1, "the command")
            assert doors(car) == [2, 2, 2, 2], "projected while in flight"
            await asyncio.sleep(0.05)
            world.push(client, {"event": "remoteControl", "vin": VIN, "data": {"uuid": "C1", "respondData": {"res": 2}}})
            await asyncio.wait_for(locking, 10)
            world.push(client, vehicle_info(realtime(1, 1700000100)))
            await asyncio.sleep(0.1)
            assert doors(car) == [2, 2, 2, 2], "an older reading is held off"
            world.push(client, vehicle_info(realtime(2, 1700000200)))
            await until(lambda: car.state["realtime"]["elec_percent"] == 80 and doors(car) == [2, 2, 2, 2], "confirmation")

            # a refused command: rolled back
            world.server.script(REMOTE, [world.Reply.code("5005")])
            with pytest.raises(exceptions.BydControlPasswordError):
                await car.lock.unlock()
            assert doors(car) == [2, 2, 2, 2], "rolled back"

            # pushes handed to the car directly, and their validation
            car.handle_mqtt_charging({"elecPercent": 81, "chargingState": 1})
            car.handle_mqtt_energy({"time": 1700000300})
            car.handle_mqtt_realtime(realtime(2, 1700000400, elecPercent=79))
            await until(lambda: car.state["charging"]["soc"] == 81, "the charging push")
            await until(lambda: car.state["realtime"]["elec_percent"] == 79, "the realtime push")
            with pytest.raises(openbyd.ValidationError):
                car.handle_mqtt_charging({"elecPercent": "lots"})
            return car

    car = run(main())
    # The client closed: the car's calls fail as a closed client's do.
    with pytest.raises(exceptions.BydError):
        run(car.update_hvac())
    car.close()


def test_every_car_command_reaches_the_client(world) -> None:
    world.server.reply(REMOTE, world.Reply.data({"controlState": 1, "requestSerial": "C1"}))
    world.server.reply("/control/remoteControlBatteryHeat", world.Reply.data({"controlState": 1, "requestSerial": "C2"}))
    world.server.reply("/control/smartCharge/changeChargeStatue", world.Reply.data({"res": 2, "requestSerial": "S"}))
    world.server.reply("/control/smartCharge/saveOrUpdate", world.Reply.data({"res": 2, "requestSerial": "S"}))
    world.server.reply("/control/smartCharge/homePage", world.Reply.data({"elecPercent": 80}))
    world.server.reply(
        LATEST_CONFIG,
        world.Reply.data({VIN: {"cfFixedList": [{"functionNo": no} for no in [
            "1001", "1005", "1006", "1007", "1008", "1009", "1012", "1020", "1021", "1026", "10300002",
            "10030010", "10030002", "10030001", "10030004",
        ]]}}),
    )

    async def main() -> list[str]:
        async with world.client() as client:
            await client.verify_command_access(VIN)
            car = await client.get_car(VIN, vehicle={"vin": VIN, "energyType": "0", "vehicleFunLearnInfo": {"openWindowLearnInfo": 1}})
            failures = []
            for name, call in [
                ("lock", car.lock.lock()),
                ("unlock", car.lock.unlock()),
                ("hvac.start", car.hvac.start(temperature=22, duration=10)),
                ("hvac.stop", car.hvac.stop()),
                ("hvac.schedule", car.hvac.schedule(booking_id=1, booking_time=1700000000)),
                ("seat.heat", car.seat.heat(openbyd.SeatPosition.DRIVER, openbyd.SeatLevel.HIGH)),
                ("seat.ventilation", car.seat.ventilation("copilot", 1)),
                ("steering", car.steering.heat(on=True)),
                ("battery", car.battery.heat(on=True)),
                ("find", car.finder.find()),
                ("flash", car.finder.flash_lights()),
                ("windows.close", car.windows.close()),
                ("windows.open", car.windows.open()),
                ("trunk.open", car.trunk.open()),
                ("trunk.close", car.trunk.close()),
                ("start_charging", car.start_charging()),
                ("save_schedule", car.save_charging_schedule(start_charge_time="22:00", end_charge_time="06:00", charge_way="e")),
            ]:
                try:
                    await call
                except exceptions.BydEndpointNotSupportedError:
                    failures.append(name)
            with pytest.raises(exceptions.BydEndpointNotSupportedError):
                await car.stop_charging()
            with pytest.raises(TypeError):
                await car.hvac.schedule({"booking_id": 1}, booking_time=1)
            with pytest.raises(ValueError):
                await car.seat.heat("boot", 1)
            with pytest.raises(ValueError):
                await car._native.command("seat", "heat", {"position": "driver", "level": 7})
            with pytest.raises(ValueError):
                await car._native.command("lock", "explode", {})
            with pytest.raises(TypeError):
                await car._native.command("steering", "heat", {})
            with pytest.raises(ValueError):
                car._native.handle_mqtt("weather", {})
            with pytest.raises(ValueError):
                await car._native.update("tyres")
            return failures

    assert run(main()) == [], "every feature the car has goes through"
    assert len(world.requests(REMOTE)) >= 12


def test_a_payload_that_does_not_decrypt_logs_in_again_at_once(world) -> None:
    # Live, BYD sent a push the fresh session could not decrypt. pyBYD logged
    # in again within milliseconds; openbyd did nothing, because its clock
    # started at 0 and the 60 s re-login cooldown (from 0.0) still held.
    logins_before = len(world.requests("/app/account/login"))

    async def main() -> None:
        async with world.client(mqtt_enabled=True) as client:
            await client.login()
            await world.until_subscribed(client)
            topic = "oversea/res/" + client.export_session()["user_id"]
            assert world.broker.publish(topic, b"00" * 48) == 1  # hex, but not sealed with this session's key
            await until(lambda: len(world.requests("/app/account/login")) >= logins_before + 2, "a second login")

    run(main())
