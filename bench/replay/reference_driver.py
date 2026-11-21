"""Guarded replay protocol example for tests. Its toy detections do not implement the backend's rules, and this driver is not a benchmark variant."""

from __future__ import annotations

import asyncio
from datetime import datetime, timedelta, timezone
import json
import os
import secrets
import sys
from typing import Any

from openbyd import pybyd_accel
from paho.mqtt import client as mqtt
import pybyd
from pybyd import _mqtt, client as byd_client, session
from pybyd._api import _common, _envelope, login, realtime
from pybyd.exceptions import BydApiError
from pybyd.models.charging import ChargingStatus
from pybyd.models.realtime import VehicleRealtimeData


class Clock:
    """Scenario time for detection and pyBYD, independent of the event loop's clock."""

    def __init__(self, epoch_ms: int) -> None:
        self.epoch = datetime.fromtimestamp(epoch_ms / 1000, timezone.utc)
        self.t = 0

    def time(self) -> float:
        """The scenario's wall time in Unix seconds."""
        return self.epoch.timestamp() + self.t

    def monotonic(self) -> int:
        """Seconds since the start of this account's replay."""
        return self.t

    def stamp(self) -> str:
        """UTC timestamp for a detection boundary."""
        return (self.epoch + timedelta(seconds=self.t)).isoformat().replace("+00:00", "Z")


class Reference:
    """Read realtime samples and close a drive when odometer growth stops, or a charge when charging stops."""

    def __init__(self, clock: Clock, vins: list[str]) -> None:
        self.clock = clock
        self.vins = vins
        self.counters = {"polls": 0, "pushes_applied": 0, "logins": 0}
        self.detections: dict[str, list[dict]] = {"drives": [], "charges": []}
        self.open: dict[str, dict[str, Any]] = {vin: {"drive": None, "charge": None} for vin in vins}
        self.last_odo: dict[str, tuple[float, str]] = {}
        self.inbox: asyncio.Queue = asyncio.Queue()
        self.event: tuple[str, str, dict] | None = None

    def receive(self, event: str, vin: str, raw: dict) -> None:
        """Keep the decoded push until the next step applies it at its virtual time."""
        self.event = event, vin, raw

    def delivered(self) -> None:
        """Queue a wire receipt after pyBYD dispatched its callback, including discarded pushes."""
        self.inbox.put_nowait(self.event)
        self.event = None

    def sample(self, vin: str, data: VehicleRealtimeData) -> None:
        """Use one realtime sample for both toy detectors."""
        odo = data.total_mileage_v2 if data.total_mileage_v2 is not None else data.total_mileage
        at = self.clock.stamp()
        last = self.last_odo.get(vin)
        if odo is not None:
            drive = self.open[vin]["drive"]
            if last is not None and odo > last[0]:
                if drive is None:
                    drive = self.open[vin]["drive"] = {"start": last[1], "start_odo": last[0]}
                drive.update(end=at, end_odo=odo)
            elif drive is not None:
                self.detections["drives"].append({"vin": vin, **drive, "km": drive["end_odo"] - drive["start_odo"]})
                self.open[vin]["drive"] = None
            self.last_odo[vin] = (odo, at)
        if data.charge_state is not None:
            self.charge(vin, data.is_charging, data.power_battery)

    def charge(self, vin: str, charging: bool, soc: int | float | None) -> None:
        """Start or finish a toy charging session."""
        at = self.clock.stamp()
        charge = self.open[vin]["charge"]
        if charging and charge is None:
            self.open[vin]["charge"] = {"start": at, "start_soc": soc}
        elif not charging and charge is not None:
            self.detections["charges"].append({"vin": vin, **charge, "end": at, "end_soc": soc})
            self.open[vin]["charge"] = None

    async def step(self, client: pybyd.BydClient, message: dict) -> None:
        """Consume the exact number of delivered MQTT messages before polling."""
        self.clock.t = message["t"]
        for _ in range(message["pushes"]):
            item = await self.inbox.get()
            if item is None:
                continue
            event, vin, raw = item
            if event == "vehicleInfo":
                self.sample(vin, VehicleRealtimeData.model_validate(raw))
            elif event == "smartCharge":
                data = ChargingStatus.model_validate(raw)
                self.charge(vin, data.is_charging, data.soc)
            self.counters["pushes_applied"] += 1
        if message["poll"]:
            self.counters["polls"] += 1
            for vin in self.vins:
                try:
                    data = await client.get_vehicle_realtime(
                        vin, mqtt_timeout=0, poll_attempts=2, poll_interval=0, signal_retries=0,
                    )
                except BydApiError:
                    continue
                self.sample(vin, data)

    def finish(self, t: int) -> dict:
        """Return detections and optionally exercise the harness's failure paths."""
        self.clock.t = t
        if self.vins and self.vins[0] == "TESTVIN0000000001" and self.detections["drives"]:
            item = self.detections["drives"][0]
            if os.environ.get("OPENBYD_TEST_REPLAY_DIFFERENCE"):
                item["end_odo"] += 0.125
            if os.environ.get("OPENBYD_TEST_REPLAY_RANDOM"):
                item["end_odo"] = secrets.randbits(64)
        return {"open": self.open, "counters": self.counters, "detections": self.detections}


async def _run() -> None:
    def reply(message: dict, **answer: Any) -> None:
        print("DRIVER " + json.dumps({"seq": message["seq"], **answer}, allow_nan=False), flush=True)

    first = await asyncio.to_thread(sys.stdin.readline)
    if not first:
        return
    message = json.loads(first)
    if message["op"] != "open" or message.get("protocol") != 1:
        reply(message, ok=False, error="expected protocol 1 open")
        return
    clock = Clock(message["epoch_unix_ms"])
    for module in (byd_client, _mqtt, session, _common, _envelope, login, realtime):
        module.time = clock
    make_session = byd_client.Session
    byd_client.Session = lambda **values: make_session(created_at=clock.t, **values)
    reference = Reference(clock, message["vins"])
    loop = asyncio.get_running_loop()
    subscriptions: asyncio.Queue[bool] = asyncio.Queue()

    class Subscriber(mqtt.Client):
        def __init__(self, *args: Any, **kwargs: Any) -> None:
            super().__init__(*args, **kwargs)
            self.on_subscribe = self.ack

        def ack(self, _client: Any, _userdata: Any, _mid: int, reasons: list, _properties: Any) -> None:
            loop.call_soon_threadsafe(subscriptions.put_nowait, bool(reasons) and all(not code.is_failure for code in reasons))

        def tls_set(self, *args: Any, **kwargs: Any) -> None:
            super().tls_set(ca_certs=os.environ["SSL_CERT_FILE"])

        def _handle_on_message(self, message: mqtt.MQTTMessage) -> None:
            super()._handle_on_message(message)
            # pyBYD queued its decoded event on this loop before this receipt.
            loop.call_soon_threadsafe(reference.delivered)

    _mqtt.mqtt.Client = Subscriber

    class Client(pybyd.BydClient):
        async def login(self) -> None:
            await super().login()
            if self._mqtt_runtime is None or not await subscriptions.get():
                raise RuntimeError("broker did not acknowledge an MQTT subscription")
            reference.counters["logins"] += 1

    config = pybyd.BydConfig(
        username=f"{message['account']}@example.com", password="secret", base_url=os.environ["BYD_BASE_URL_OVERRIDE"],
        mqtt_enabled=True, mqtt_timeout=0, session_ttl=0,
    )
    try:
        if not pybyd_accel.install():
            raise RuntimeError("could not install the reference driver's native envelope cipher")
        async with Client(config, on_mqtt_event=reference.receive) as client:
            await client.login()
            reply(message, ok=True, driver="reference", settings={"native_cipher": pybyd_accel.is_installed()}, mqtt=True)
            while line := await asyncio.to_thread(sys.stdin.readline):
                message = json.loads(line)
                if message["op"] == "step":
                    await reference.step(client, message)
                    reply(message, ok=True, mqtt=True)
                elif message["op"] == "finish":
                    reply(message, ok=True, **reference.finish(message["t"]))
                    if await asyncio.to_thread(sys.stdin.readline):
                        raise ValueError("expected EOF after finish")
                    return
                else:
                    raise ValueError(f"unexpected operation {message['op']}")
    except Exception as exc:
        reply(message, ok=False, error=f"{type(exc).__name__}: {exc}")


def main() -> None:
    """Serve requests on stdin using only the launcher's guarded sockets."""
    if not os.environ.get("BYD_BASE_URL_OVERRIDE", "").startswith("http://127.0.0.1:"):
        raise SystemExit("reference driver requires the loopback replay URL")
    asyncio.run(_run())


if __name__ == "__main__":
    main()
