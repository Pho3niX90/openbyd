"""openbyd.testing.FakeServer, driven by pyBYD's own client and by openbyd.lowlevel."""

from __future__ import annotations

import asyncio
import importlib
import sys

import pytest

import openbyd
from openbyd import _native, exceptions, lowlevel

testing = pytest.importorskip(
    "openbyd.testing", reason="extension built without the 'testing' feature", exc_type=ImportError
)
FakeServer, Reply = testing.FakeServer, testing.Reply

VIN = "TESTVIN0000000001"
USER = "user@example.com"
PASSWORD = "pa$$ wörd"


@pytest.fixture
def server():
    with FakeServer([(USER, PASSWORD)]) as running:
        yield running
    assert not running.running


def config_for(server, **extra):
    return {"username": USER, "password": PASSWORD, "base_url": server.url, **extra}


def test_pybyd_client_logs_in_and_fetches_gps(server):
    pybyd = pytest.importorskip("pybyd")
    server.reply("/control/getGpsInfo", Reply.data({"requestSerial": "SERIAL1"}))
    server.script("/control/getGpsInfoResult", [Reply.data({"requestSerial": "SERIAL1"})])
    server.reply(
        "/control/getGpsInfoResult",
        Reply.data({"latitude": 52.0, "longitude": 4.0, "speed": 0, "direction": 90, "requestSerial": "SERIAL1"}),
    )

    async def main():
        config = pybyd.BydConfig(username=USER, password=PASSWORD, base_url=server.url, mqtt_enabled=False)
        async with pybyd.BydClient(config) as client:
            await client.login()
            return await client.get_gps_info(VIN, poll_attempts=5, poll_interval=0.01)

    info = asyncio.run(main())
    assert (info.latitude, info.longitude) == (52.0, 4.0)
    login = server.requests("/app/account/login")
    assert login[0]["username"] == USER and login[0]["code"] == "0"
    polls = server.requests("/control/getGpsInfoResult")
    assert len(polls) == 2
    assert polls[0]["inner"]["requestSerial"] == "SERIAL1"
    assert polls[0]["inner"]["vin"] == VIN
    # The model openbyd parses from the same reply is pyBYD's.
    assert openbyd.shadow.compare(info) == []


def test_lowlevel_login_and_post_token_json(server):
    async def main():
        session = await lowlevel.login(config_for(server))
        server.reply("/app/echo", Reply.data({"echo": [1, 2.5, None, "x"]}))
        echoed = await lowlevel.post_token_json(config_for(server), session, "/app/echo", {"vin": VIN, "n": 1})
        empty = await lowlevel.post_token_json(config_for(server), session, "/app/empty", {})
        return session, echoed, empty

    server.reply("/app/empty", Reply.outer({"code": "0", "message": "ok"}))
    session, echoed, empty = asyncio.run(main())
    assert set(session) == {"user_id", "sign_token", "encry_token", "created_at", "ttl"}
    assert session["user_id"] == server.user_id(USER)
    assert echoed == {"echo": [1, 2.5, None, "x"]}
    assert empty == {}
    recorded = server.requests("/app/echo")[0]
    assert recorded["inner"] == {"vin": VIN, "n": 1}
    assert recorded["outer"]["identifier"] == session["user_id"]
    assert ("user-agent" in {name for name, _ in recorded["headers"]})


def test_lowlevel_errors_map_to_pybyd_classes(server):
    async def call(endpoint, session=None, **config):
        session = session or await lowlevel.login(config_for(server))
        return await lowlevel.post_token_json(config_for(server, **config), session, endpoint, {"vin": VIN})

    server.reply("/busy", Reply.code("1008", "busy"))
    server.reply("/broken", Reply.status(500, "oops"))
    server.reply("/garbage", Reply.raw(b"\xff\xfe"))
    server.reply("/gone", Reply.drop())

    with pytest.raises(exceptions.BydServiceBusyError) as caught:
        asyncio.run(call("/busy"))
    assert (caught.value.code, caught.value.endpoint) == ("1008", "/busy")
    with pytest.raises(exceptions.BydTransportError) as caught:
        asyncio.run(call("/broken"))
    assert (caught.value.status_code, caught.value.endpoint) == (500, "/broken")
    with pytest.raises(UnicodeDecodeError):
        asyncio.run(call("/garbage"))
    with pytest.raises(exceptions.BydTransportError):
        asyncio.run(call("/gone"))
    with pytest.raises(exceptions.BydApiError) as caught:
        asyncio.run(call("/unhandled"))
    assert caught.value.code == "9004"


def test_expired_sessions_raise_session_expired(server):
    async def main():
        session = await lowlevel.login(config_for(server))
        server.reply("/x", Reply.data({}))
        assert await lowlevel.post_token_json(config_for(server), session, "/x", {}) == {}
        server.expire_sessions(USER)
        with pytest.raises(exceptions.BydSessionExpiredError):
            await lowlevel.post_token_json(config_for(server), session, "/x", {})
        fresh = await lowlevel.login(config_for(server))
        return await lowlevel.post_token_json(config_for(server), fresh, "/x", {})

    assert asyncio.run(main()) == {}


def test_bad_logins_and_arguments(server):
    with pytest.raises(exceptions.BydAuthenticationError):
        asyncio.run(lowlevel.login(config_for(server, password="wrong")))
    with pytest.raises(openbyd.ValidationError):
        asyncio.run(lowlevel.login({"username": USER}))
    with pytest.raises(openbyd.ValidationError):
        asyncio.run(lowlevel.login({**config_for(server), "nonsense": 1}))
    with pytest.raises(TypeError, match="config must be a dict"):
        asyncio.run(lowlevel.login([]))
    with pytest.raises(openbyd.ValidationError):
        asyncio.run(lowlevel.post_token_json(config_for(server), {"user_id": "1"}, "/x", {}))
    tokens = {"user_id": "1", "sign_token": "s", "encry_token": "e"}
    with pytest.raises(TypeError, match="inner must be a dict"):
        asyncio.run(lowlevel.post_token_json(config_for(server), tokens, "/x", None))
    # base_url overrides the config's.
    session = asyncio.run(lowlevel.login({**config_for(server), "base_url": "http://127.0.0.1:1"}, base_url=server.url))
    assert session["user_id"]


def test_ttl_follows_session_ttl(server):
    assert asyncio.run(lowlevel.login(config_for(server, session_ttl=0)))["ttl"] == float("inf")
    assert asyncio.run(lowlevel.login(config_for(server, session_ttl=60)))["ttl"] == 60.0


def test_concurrent_requests_through_the_fake_server(server):
    server.reply("/n", Reply.data({"ok": True}))

    async def main():
        session = await lowlevel.login(config_for(server))
        replies = await asyncio.gather(
            *(lowlevel.post_token_json(config_for(server), session, "/n", {"i": i}) for i in range(200))
        )
        return replies

    assert asyncio.run(main()) == [{"ok": True}] * 200
    assert sorted(request["inner"]["i"] for request in server.requests("/n")) == list(range(200))


def test_cancelling_a_slow_request(server):
    server.reply("/slow", Reply.data({}).delayed(30))

    async def main():
        session = await lowlevel.login(config_for(server))
        dropped = openbyd.runtime.stats()["dropped"]
        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(lowlevel.post_token_json(config_for(server), session, "/slow", {}), 0.2)
        for _ in range(200):
            if openbyd.runtime.stats()["dropped"] > dropped:
                return True
            await asyncio.sleep(0.01)
        return False

    assert asyncio.run(main())


def test_server_management():
    server = FakeServer([{"username": USER, "password": PASSWORD, "user_id": "42", "guest_vins": [VIN]}])
    try:
        assert server.url.startswith("http://127.0.0.1:")
        assert server.user_id(USER) == "42"
        server.add_account("other@example.com", "x", user_id="7", guest_vins=[VIN], user_type_rule=False)
        assert server.user_id("other@example.com") == "7"
        assert server.user_id("nobody@example.com") is None
        server.set_cookie("a=b; Path=/")
        asyncio.run(lowlevel.login(config_for(server)))
        assert server.connections >= 1
        assert len(server.requests()) == 1
        server.clear_requests()
        assert server.requests() == []
        reply = Reply.default().with_cookie("c=d").delayed(0)
        assert "Delay" in repr(reply)
        server.script("/app/account/login", [Reply.default()])
    finally:
        server.stop()
        server.stop()  # twice is fine
    with pytest.raises(RuntimeError, match="stopped"):
        server.requests()
    with pytest.raises(ValueError):
        Reply.data({}).delayed(-1)
    with pytest.raises(TypeError):
        FakeServer([42])
    with pytest.raises(ValueError, match="needs password"):
        FakeServer([{"username": USER}])


def test_import_fails_clearly_without_the_feature(monkeypatch):
    monkeypatch.setattr(_native, "HAS_TESTING", False)
    monkeypatch.delitem(sys.modules, "openbyd.testing")
    with pytest.raises(ImportError, match="cargo feature 'testing'"):
        importlib.import_module("openbyd.testing")
    monkeypatch.undo()
    assert importlib.import_module("openbyd.testing").FakeServer is FakeServer
