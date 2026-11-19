"""Shared fixtures of the ``openbyd.Client`` tests: a fake BYD server and an
MQTT test broker wired together, and a way to make clients for them."""

from __future__ import annotations

import asyncio
import time
from collections.abc import Callable, Iterator
from typing import Any

import pytest

VIN = "TESTVIN0000000001"
USER = "user@example.com"
PASSWORD = "secret"
PIN = "123456"

BROKER_LOOKUP = "/app/emqAuth/getEmqBrokerIp"
VEHICLES = "/app/account/getAllListByUserId"
LATEST_CONFIG = "/vehicle/vehicleswitch/getLatestConfig"
VERIFY = "/vehicle/vehicleswitch/verifyControlPassword"
HVAC = "/control/getStatusNow"
LOGIN = "/app/account/login"
REMOTE = "/control/remoteControl"
REMOTE_RESULT = "/control/remoteControlResult"
FUNCTIONS = {VIN: {"cfFixedList": [{"functionNo": no} for no in ["1001", "1005", "1006", "1012", "1020", "1021", "1026", "1027", "1030", "1033"]]}}


class World:
    """A fake server and a broker, and clients pointed at both."""

    def __init__(self, server: Any, broker: Any) -> None:
        import openbyd
        from openbyd.testing import Reply

        self.openbyd = openbyd
        self.Reply = Reply
        self.server = server
        self.broker = broker
        server.reply(BROKER_LOOKUP, Reply.data({"emqBorker": broker.address}))
        server.reply(VEHICLES, Reply.data([{"vin": VIN, "energyType": "0", "modelName": "SEAL"}]))
        server.reply(LATEST_CONFIG, Reply.data(FUNCTIONS))
        server.reply(VERIFY, Reply.data({"ok": True}))
        server.reply(HVAC, Reply.data({"acSwitch": 1, "tempInCar": 21}))

    def config(self, **extra: Any) -> dict[str, Any]:
        return {
            "username": USER,
            "password": PASSWORD,
            "base_url": self.server.url,
            "mqtt_enabled": False,
            "mqtt_timeout": 5.0,
            "control_pin": PIN,
            **extra,
        }

    def client(self, **options: Any) -> Any:
        """A client (not started); config keywords and options mixed, as the constructor takes them."""
        fields = self.config(**{key: options.pop(key) for key in list(options) if key in self.openbyd.client.CONFIG_FIELDS})
        return self.openbyd.Client(**fields, _test_tls=True, **options)

    def requests(self, endpoint: str) -> list[dict[str, Any]]:
        return self.server.requests(endpoint)

    def push(self, client: Any, body: dict[str, Any]) -> None:
        assert self.broker.push(client.export_session(), body) == 1

    async def until_subscribed(self, client: Any) -> None:
        topic = "oversea/res/" + client.export_session()["user_id"]
        await until(lambda: self.broker.subscribers(topic) > 0, "a subscription")


async def until(condition: Callable[[], Any], what: str, timeout: float = 10.0) -> None:
    """Waits until ``condition()`` is true, yielding to the loop."""
    deadline = time.monotonic() + timeout
    while not condition():
        assert time.monotonic() < deadline, f"timed out waiting for {what}"
        await asyncio.sleep(0.005)


@pytest.fixture
def world() -> Iterator[World]:
    testing = pytest.importorskip(
        "openbyd.testing", reason="extension built without the 'testing' feature", exc_type=ImportError
    )
    with testing.FakeServer([(USER, PASSWORD)]) as server, testing.TestBroker() as broker:
        yield World(server, broker)
