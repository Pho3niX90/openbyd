"""Single API requests, for checking the stack end to end.

These run pyBYD's login and ``post_token_json`` in Rust, over openbyd's
HTTP client, and are awaited from asyncio::

    from openbyd import lowlevel

    config = {"username": "user@example.com", "password": "secret"}
    session = await lowlevel.login(config)
    data = await lowlevel.post_token_json(config, session, "/app/emqAuth/getEmqBrokerIp", inner)

``config`` holds ``pybyd.BydConfig``'s fields and ``session`` the fields of
``pybyd.session.Session``. Every call opens its own cookie jar; connections
are pooled across calls. Failures raise the classes in
:mod:`openbyd.exceptions`. The full client will be ``openbyd.Client``.
"""

from __future__ import annotations

from typing import Any

from openbyd import _native


async def login(config: dict[str, Any], base_url: str | None = None) -> dict[str, Any]:
    """Log in and return the session's fields.

    The result has ``user_id``, ``sign_token``, ``encry_token``,
    ``created_at`` (``time.monotonic()``) and ``ttl`` (``config``'s
    ``session_ttl``, infinite when not positive), as pyBYD's
    ``BydClient.login`` builds its ``Session``. ``base_url`` overrides the
    config's. A bad config raises :class:`openbyd.ValidationError`, a refused
    login ``BydAuthenticationError``.
    """
    return await _native.login(config, base_url)


async def post_token_json(
    config: dict[str, Any],
    session: dict[str, Any],
    endpoint: str,
    inner: dict[str, Any],
    *,
    vin: str | None = None,
    user_type: str | None = None,
) -> Any:
    """Post ``inner`` to ``endpoint`` in a token envelope; return the decrypted ``respondData``.

    As pyBYD's ``_api._common.post_token_json``: a non-zero code raises the
    error its code maps to (``BydSessionExpiredError`` for an expired
    session, ``BydServiceBusyError`` for 1008, ...), and an empty
    ``respondData`` gives ``{}``. ``user_type`` is sent and signed when given.
    """
    return await _native.post_token_json(config, session, endpoint, inner, vin=vin, user_type=user_type)


__all__ = ["login", "post_token_json"]
