"""One vehicle: pyBYD's ``BydCar`` with a Rust core.

A :class:`Car` comes from :meth:`openbyd.Client.get_car`. It holds the
vehicle's merged state and offers its commands by feature, as pyBYD does::

    car = await client.get_car(vin)
    await car.update_realtime()
    await car.lock.lock()
    await car.hvac.start(temperature=21, duration=20)
    await car.seat.heat(SeatPosition.DRIVER, SeatLevel.HIGH)
    await car.steering.heat(on=True)
    car.state["realtime"]["left_front_door_lock"]    # 2 (locked), projected

A command lays the state it expects over the current state at once (a
projection) and rolls it back when it fails; the next push or update that
agrees confirms it. ``state`` is a dict of sections (``vehicle``,
``realtime``, ``hvac``, ``gps``, ``charging``, ``energy``,
``charging_schedule``), each pyBYD's ``model_dump(mode="json")`` or ``None``.

Each feature tells whether the car supports it (``available``, or
``find_available`` and the like); a command the car does not support raises
``BydEndpointNotSupportedError`` without sending anything.
"""

from __future__ import annotations

import enum
from collections.abc import Mapping
from typing import Any


class SeatPosition(enum.StrEnum):
    """A seat, as pyBYD's ``SeatPosition``."""

    DRIVER = "driver"
    COPILOT = "copilot"
    REAR_LEFT = "rear_left"
    REAR_RIGHT = "rear_right"


class SeatLevel(enum.IntEnum):
    """A seat heating or ventilation level, as pyBYD's ``SeatLevel``."""

    OFF = 0
    LOW = 1
    HIGH = 2


class _Feature:
    def __init__(self, car: Car, name: str) -> None:
        self._car = car
        self._name = name

    def _view(self) -> dict[str, Any]:
        return self._car._native.features()[self._name]

    async def _run(self, action: str, **args: Any) -> None:
        await self._car._native.command(self._name, action, args)

    def __repr__(self) -> str:
        return f"<{type(self).__name__} {self._view()}>"


class LockFeature(_Feature):
    """``car.lock``: lock and unlock the doors."""

    @property
    def available(self) -> bool:
        """Whether the car supports locking."""
        return self._view()["available"]

    async def lock(self) -> None:
        """Lock all doors; the door locks read locked (2) at once."""
        await self._run("lock")

    async def unlock(self) -> None:
        """Unlock all doors; the door locks read unlocked (1) at once."""
        await self._run("unlock")


class HvacFeature(_Feature):
    """``car.hvac``: start, stop and schedule climate control."""

    @property
    def available(self) -> bool:
        """Whether the car supports remote climate control."""
        return self._view()["available"]

    @property
    def temp_offset(self) -> float:
        """°C offset of the car's 1-17 temperature scale, from the last HVAC status."""
        return self._view()["temp_offset"]

    @property
    def temp_range(self) -> tuple[float, float]:
        """The settable temperature range in °C."""
        low, high = self._view()["temp_range"]
        return (low, high)

    async def start(
        self,
        temperature: float = 21.0,
        duration: int = 20,
        cycle_mode: int | None = None,
        fan_level: int | None = None,
    ) -> None:
        """Start climate control, or change a running session's settings.

        ``temperature`` in °C, ``duration`` in minutes (10, 15, 20, 25 or 30),
        ``cycle_mode`` 1 fresh air or 2 recirculate, ``fan_level`` 1-7;
        ``None`` keeps a running session's setting. Without a known HVAC
        status the status is fetched first, as pyBYD does.
        """
        await self._run(
            "start", temperature=temperature, duration=duration, cycle_mode=cycle_mode, fan_level=fan_level
        )

    async def stop(self) -> None:
        """Stop climate control, including seat and steering wheel heating."""
        await self._run("stop")

    async def schedule(self, params: Mapping[str, Any] | Any = None, **fields: Any) -> None:
        """Create, change or remove a climate schedule (``ClimateScheduleParams`` fields)."""
        if params is not None and fields:
            raise TypeError("give the parameters as params or as keyword arguments, not both")
        await self._run("schedule", params=params if params is not None else fields)


class SeatFeature(_Feature):
    """``car.seat``: seat heating and ventilation."""

    def is_available(self, position: SeatPosition | str, mode: str) -> bool:
        """Whether ``position`` supports ``mode`` (``"heat"`` or ``"ventilation"``)."""
        return self._view()[SeatPosition(position).value][mode]

    async def heat(self, position: SeatPosition | str, level: SeatLevel | int) -> None:
        """Set the heating of one seat."""
        await self._run("heat", position=str(SeatPosition(position).value), level=int(level))

    async def ventilation(self, position: SeatPosition | str, level: SeatLevel | int) -> None:
        """Set the ventilation of one seat."""
        await self._run("ventilation", position=str(SeatPosition(position).value), level=int(level))


class SteeringFeature(_Feature):
    """``car.steering``: steering wheel heating."""

    @property
    def available(self) -> bool:
        """Whether the car has steering wheel heating."""
        return self._view()["available"]

    async def heat(self, *, on: bool) -> None:
        """Turn steering wheel heating on or off."""
        await self._run("heat", on=on)


class BatteryFeature(_Feature):
    """``car.battery``: battery heating."""

    @property
    def available(self) -> bool:
        """Whether the car supports battery heating."""
        return self._view()["available"]

    async def heat(self, *, on: bool) -> None:
        """Turn battery heating on or off."""
        await self._run("heat", on=on)


class FinderFeature(_Feature):
    """``car.finder``: find the car, flash the lights."""

    @property
    def find_available(self) -> bool:
        """Whether the car supports find-my-car."""
        return self._view()["find_available"]

    @property
    def flash_available(self) -> bool:
        """Whether the car can flash its lights remotely."""
        return self._view()["flash_available"]

    async def find(self) -> None:
        """Horn and lights."""
        await self._run("find")

    async def flash_lights(self) -> None:
        """Flash the lights."""
        await self._run("flash_lights")


class WindowsFeature(_Feature):
    """``car.windows``: close the windows or crack them open."""

    @property
    def close_available(self) -> bool:
        """Whether the car can close its windows remotely."""
        return self._view()["close_available"]

    @property
    def open_available(self) -> bool:
        """Whether the car can open its windows remotely (to a vent crack)."""
        return self._view()["open_available"]

    async def close(self) -> None:
        """Close all windows."""
        await self._run("close")

    async def open(self) -> None:
        """Open the windows to a vent crack of about 10 %."""
        await self._run("open")


class TrunkFeature(_Feature):
    """``car.trunk``: open and close the trunk."""

    @property
    def open_available(self) -> bool:
        """Whether the trunk can be opened remotely."""
        return self._view()["open_available"]

    @property
    def close_available(self) -> bool:
        """Whether the trunk can be closed remotely."""
        return self._view()["close_available"]

    async def open(self) -> None:
        """Release the trunk."""
        await self._run("open")

    async def close(self) -> None:
        """Lower the trunk."""
        await self._run("close")


class Car:
    """One vehicle: its merged state, its features and its updates (pyBYD's ``BydCar``).

    Get one from :meth:`openbyd.Client.get_car`; the client keeps it and
    merges the MQTT pushes for its VIN into it. A car whose client is gone
    or closed fails its calls with ``BydError``.
    """

    def __init__(self, native: Any) -> None:
        self._native = native
        #: Lock and unlock.
        self.lock = LockFeature(self, "lock")
        #: Climate control.
        self.hvac = HvacFeature(self, "hvac")
        #: Seat heating and ventilation.
        self.seat = SeatFeature(self, "seat")
        #: Steering wheel heating.
        self.steering = SteeringFeature(self, "steering")
        #: Battery heating.
        self.battery = BatteryFeature(self, "battery")
        #: Find the car and flash the lights.
        self.finder = FinderFeature(self, "finder")
        #: Close the windows or crack them open.
        self.windows = WindowsFeature(self, "windows")
        #: Open and close the trunk.
        self.trunk = TrunkFeature(self, "trunk")

    def __repr__(self) -> str:
        return f"<openbyd.Car vin={self.vin!r}>"

    @property
    def vin(self) -> str:
        """The vehicle identification number."""
        return self._native.vin

    @property
    def state(self) -> dict[str, Any]:
        """The current state with active projections laid over it, by section."""
        return self._native.state()

    @property
    def capabilities(self) -> dict[str, Any]:
        """The car's capability flags (pyBYD's ``VehicleCapabilities`` dump)."""
        return self._native.capabilities()

    @property
    def features(self) -> dict[str, Any]:
        """Every feature's availability (and the climate scale), by feature name."""
        return self._native.features()

    async def update_realtime(self) -> dict[str, Any]:
        """Fetch realtime data and merge it; returns the data as fetched."""
        return await self._native.update("realtime")

    async def update_hvac(self) -> dict[str, Any]:
        """Fetch the HVAC status and merge it."""
        return await self._native.update("hvac")

    async def update_gps(self) -> dict[str, Any]:
        """Fetch the location and merge it."""
        return await self._native.update("gps")

    async def update_charging(self) -> dict[str, Any]:
        """Fetch the charging state and the schedule in one call and merge both; returns the charging state."""
        return await self._native.update("charging")

    async def update_energy(self) -> dict[str, Any]:
        """Fetch energy consumption and merge it."""
        return await self._native.update("energy")

    async def start_charging(self) -> dict[str, Any]:
        """Start charging now and wait for the change to settle."""
        return await self._native.start_charging()

    async def stop_charging(self) -> dict[str, Any]:
        """Always raises ``BydEndpointNotSupportedError``: the cloud cannot stop a charge."""
        return await self._native.stop_charging()

    async def get_ac_current_limit(self) -> dict[str, Any]:
        """The supported and active AC charging current limits."""
        return await self._native.get_ac_current_limit()

    async def save_charging_schedule(
        self, *, start_charge_time: str, end_charge_time: str, charge_way: str, enabled: bool = True
    ) -> dict[str, Any]:
        """Save a smart-charging schedule, wait for it to settle, then refresh the charging state."""
        return await self._native.save_charging_schedule(
            start_charge_time=start_charge_time, end_charge_time=end_charge_time, charge_way=charge_way, enabled=enabled
        )

    def handle_mqtt_realtime(self, data: Mapping[str, Any]) -> None:
        """Merge a ``vehicleInfo`` push (its raw ``respondData``), after any command in flight."""
        self._native.handle_mqtt("realtime", dict(data))

    def handle_mqtt_status_push(
        self,
        data: Mapping[str, Any],
        *,
        hvac: Mapping[str, Any] | None = None,
        charging: Mapping[str, Any] | None = None,
    ) -> None:
        """Merge a ``vehicleStatusPush``: a partial update, laid over what the car already knows.

        ``data`` holds the fields of the push's realtime sections in one
        dict, ``hvac`` those of its ``airStatusDto`` and ``charging`` those of
        its ``chargeStatusDto`` (leave either out when the push has no such
        section). Only fields that carry a reading are merged; a section the
        car has not had a full payload for yet is skipped. Nothing is merged
        when one of the three does not validate.
        """
        self._native.handle_mqtt_status_push(
            dict(data), None if hvac is None else dict(hvac), None if charging is None else dict(charging)
        )

    def handle_mqtt_charging(self, data: Mapping[str, Any]) -> None:
        """Merge a ``smartCharge`` push (live charging state only)."""
        self._native.handle_mqtt("charging", dict(data))

    def handle_mqtt_energy(self, data: Mapping[str, Any]) -> None:
        """Merge an ``energyConsumption`` push."""
        self._native.handle_mqtt("energy", dict(data))

    def close(self) -> None:
        """Release the car's resources (nothing yet; pyBYD's lifecycle hook)."""
        self._native.close()


__all__ = [
    "BatteryFeature",
    "Car",
    "FinderFeature",
    "HvacFeature",
    "LockFeature",
    "SeatFeature",
    "SeatLevel",
    "SeatPosition",
    "SteeringFeature",
    "TrunkFeature",
    "WindowsFeature",
]
