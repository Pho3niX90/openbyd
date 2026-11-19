"""How the extension runs user callbacks on a client's event loop. Internal.

The Rust side never calls a user callback on its own threads: it posts one
of these functions to the client's event loop with
``loop.call_soon_threadsafe`` (or ``asyncio.run_coroutine_threadsafe``), and
they run the callback there. An exception from a callback is logged on the
``openbyd`` logger and goes no further, as pyBYD does with its callbacks.
"""

from __future__ import annotations

import concurrent.futures
import logging
from collections.abc import Awaitable, Callable
from typing import Any

_logger = logging.getLogger("openbyd")


def invoke(name: str, callback: Callable[..., Any], args: tuple[Any, ...]) -> None:
    """Run a notification callback; log what it raises."""
    try:
        callback(*args)
    except Exception:
        _logger.exception("openbyd: the %s callback raised", name)


def decide(
    future: concurrent.futures.Future[tuple[bool, Any]],
    name: str,
    callback: Callable[..., Any],
    args: tuple[Any, ...],
) -> None:
    """Run a decision callback and hand ``(True, answer)`` to the waiting
    worker, or ``(False, None)`` when it raised (logged)."""
    try:
        answer = callback(*args)
    except Exception:
        _logger.exception("openbyd: the %s callback raised; using the default", name)
        future.set_result((False, None))
        return
    future.set_result((True, answer))


async def run(name: str, callback: Callable[[], Awaitable[Any]]) -> Any:
    """Await a coroutine callback; its exception is the caller's."""
    try:
        return await callback()
    except Exception:
        _logger.debug("openbyd: the %s callback raised", name, exc_info=True)
        raise
