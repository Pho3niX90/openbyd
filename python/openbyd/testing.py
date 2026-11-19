"""A fake BYD server for tests and benchmarks.

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

__all__ = ["FakeServer", "Reply"]
