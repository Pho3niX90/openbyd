"""A fake BYD server, an MQTT test broker and a scenario replay, for tests and benchmarks.

It speaks the real wire protocol on ``127.0.0.1``: it checks every
request's envelope, checkcode and signature, accepts logins for the
accounts it is given, and answers token requests with whatever the test
registers, encrypted under the session's key. Any client can talk to it,
pyBYD's included::

    from openbyd.testing import FakeServer, Reply

    with FakeServer([("user@example.com", "secret")]) as server:
        server.reply("/control/getGpsInfo", Reply.data({"requestSerial": "S1"}))
        server.script("/vehicle/vehicleswitch", [Reply.code("1008"), Reply.data({})])
        config = {"username": "user@example.com", "password": "secret", "base_url": server.url}
        ...
        assert server.requests("/control/getGpsInfo")[0]["inner"]["vin"] == "TESTVIN0000000001"

:class:`TestBroker` is a small MQTT v5 broker over TLS on ``127.0.0.1``. A
client built with ``openbyd.Client(..., _test_tls=True)`` trusts it; point
the broker lookup at it and push events to a logged-in client::

    with FakeServer([...]) as server, TestBroker() as broker:
        server.reply("/app/emqAuth/getEmqBrokerIp", Reply.data({"emqBorker": broker.address}))
        async with openbyd.Client(..., base_url=server.url, _test_tls=True) as client:
            await client.login()
            broker.push(client.export_session(), {"event": "vehicleInfo", "vin": VIN, "data": {...}})

Replay
------

:class:`ReplayServer` answers a whole fleet from a scenario file
(``bench/replay/SCENARIO.md``): every account logs in as
``<account>@example.com`` / ``secret`` and reads its cars' state at the replay
clock's time, the recorded errors come back at their moments, and the broker
pushes the recorded events. A manual clock (the default) moves only when
told to, so a test never waits for real time::

    with ReplayServer("bench/replay/samples/demo.jsonl") as replay:
        replay.set_time(3600)          # pushes and 1005s due by then have happened
        config = {"username": "acct-0001@example.com", "password": "secret", "base_url": replay.url}
        ...
        replay.advance(60)
        assert replay.stats()["replay"]["accounts"] == 3

``replay.ca_pem`` is the broker's certificate, for a client that has to be
told to trust it (pyBYD's paho: ``tls_set(ca_certs=...)``).

This module exists only in an extension built with the cargo feature
``testing`` (``python parity/dev_install.py`` builds one). Released wheels
leave it out, and importing it from them raises ``ImportError``.
"""

from __future__ import annotations

from openbyd import _native

if not getattr(_native, "HAS_TESTING", False) or not hasattr(_native, "FakeServer"):
    raise ImportError(
        "openbyd.testing is not in this build: the extension was built without the cargo "
        "feature 'testing' (released wheels leave it out; `python parity/dev_install.py` "
        "builds a development extension with it)"
    )

#: A running fake server; see the module documentation. Stop it with ``stop()`` or ``with``.
FakeServer = _native.FakeServer
#: What the fake server answers to one request.
Reply = _native.Reply
#: A running MQTT test broker; see the module documentation. Stop it with ``stop()`` or ``with``.
TestBroker = _native.TestBroker
#: A running replay of a scenario file; see the module documentation. Stop it with ``stop()`` or ``with``.
ReplayServer = _native.ReplayServer

__all__ = ["FakeServer", "ReplayServer", "Reply", "TestBroker"]
