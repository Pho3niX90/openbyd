"""MQTT ``vehicleStatusPush`` and ``remoteControlAck`` from Python, through the TLS test broker.

A status push is a partial update: the client lays it over what the car
already knows, tells no ``on_vehicle_info`` callback, and a
``remoteControlAck`` is not a command acknowledgement.
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest

import openbyd

from conftest import VIN, until

REALTIME = "/vehicleInfo/vehicle/vehicleRealTimeRequest"
HOMEPAGE = "/control/smartCharge/homePage"


def run(coroutine: Any) -> Any:
    return asyncio.run(coroutine)


def status_push(**sections: Any) -> dict:
    return {"event": "vehicleStatusPush", "vin": VIN, "data": {"respondData": sections}}


def test_a_status_push_is_laid_over_what_the_car_knows(world) -> None:
    world.server.reply(
        REALTIME,
        world.Reply.data(
            {"onlineState": 1, "time": 1700000000, "elecPercent": 80, "totalMileage": 1234, "tempInCar": 22.5, "leftFrontDoor": 0}
        ),
    )
    world.server.reply(
        HOMEPAGE,
        world.Reply.data({"soc": 70, "chargingState": 1, "connectState": 1, "chargePower": 5.1, "chargePowerUnit": "kW"}),
    )
    events: list[tuple] = []
    infos: list[tuple] = []
    acks: list[Any] = []

    async def main() -> None:
        async with world.client(
            mqtt_enabled=True,
            on_mqtt_event=lambda *args: events.append(args),
            on_vehicle_info=lambda *args: infos.append(args),
            on_command_ack=acks.append,
        ) as client:
            await client.login()
            await world.until_subscribed(client)
            car = await client.get_car(VIN)

            # Before the car knows a section, a partial push shows nothing.
            world.push(client, status_push(can4A505Dto={"elecPercent": 55}))
            await until(lambda: len(events) == 1, "the first push")
            await asyncio.sleep(0.05)
            assert car.state["realtime"] is None

            await car.update_realtime()
            await car.update_hvac()
            await car.update_charging()
            before = car.state
            told = len(infos)
            assert before["hvac"]["temp_in_car"] == 21 and before["charging"]["charge_power"] == 5.1

            world.push(
                client,
                status_push(
                    doorStatusDto={"leftFrontDoor": 1, "leftFrontDoorLock": 2},
                    can4A505Dto={"elecPercent": 91, "tempInCar": -129},
                    airStatusDto={"cycleModel": 2},
                    chargeStatusDto={"chargingState": 15, "connectState": 0},
                    time=1700000500,
                ),
            )
            await until(lambda: car.state["realtime"]["elec_percent"] == 91, "the status push")
            state = car.state
            realtime = state["realtime"]
            assert realtime["left_front_door"] == 1 and realtime["left_front_door_lock"] == 2
            # What the push did not carry is as it was; raw is merged, not replaced.
            assert realtime["total_mileage"] == 1234 and realtime["temp_in_car"] == 22.5
            assert set(before["realtime"]["raw"]) <= set(realtime["raw"])
            assert realtime["raw"]["elecPercent"] == 91
            # HVAC and charging read only their own section.
            assert state["hvac"]["cycle_choice"] == 2 and state["hvac"]["temp_in_car"] == 21
            assert "elecPercent" not in state["hvac"]["raw"]
            charging = state["charging"]
            assert (charging["charging_state"], charging["connect_state"], charging["soc"]) == (15, 0, 70)
            assert charging["charge_power"] is None, "charging stopped"
            assert state["charging_schedule"] == before["charging_schedule"]
            assert len(infos) == told, "on_vehicle_info is for complete payloads"

            # A remoteControlAck reaches the generic callback and is not a command acknowledgement.
            ack = {"event": "remoteControlAck", "vin": VIN, "data": {"uuid": "ser-1", "respondData": {"res": 1, "ackStatus": 1}}}
            world.push(client, ack)
            await until(lambda: events[-1][0] == "remoteControlAck", "the ack")
            await asyncio.sleep(0.05)
            assert acks == []
            assert client.get_command_ack_diagnostics()["uncorrelated"] == 0

            # Partials handed to the car directly, and their validation.
            car.handle_mqtt_status_push({"elecPercent": 40}, hvac={"mainSettingTempNew": 23.0}, charging={"soc": 41})
            await until(lambda: car.state["charging"]["soc"] == 41, "the direct push")
            assert car.state["realtime"]["elec_percent"] == 40
            assert car.state["hvac"]["main_setting_temp_new"] == 23.0
            with pytest.raises(openbyd.ValidationError):
                car.handle_mqtt_status_push({"elecPercent": 39}, charging={"soc": "lots"})
            with pytest.raises(TypeError):
                car._native.handle_mqtt_status_push("x")
            with pytest.raises(TypeError):
                car._native.handle_mqtt_status_push({}, 5)
            car.handle_mqtt_status_push({"foo": 1})
            await asyncio.sleep(0.05)
            assert car.state["realtime"]["elec_percent"] == 40, "nothing of a refused push is merged"

    run(main())
