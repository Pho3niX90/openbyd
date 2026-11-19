"""The Rust runtime behind openbyd's awaitables.

openbyd's ``async`` functions run on a tokio runtime inside the extension,
shared by every event loop and thread in the process. It starts on first
use with :data:`DEFAULT_WORKER_THREADS` worker threads; change that once,
before the first awaitable, with :func:`configure`::

    import openbyd.runtime

    openbyd.runtime.configure(worker_threads=4)

openbyd's ``async`` functions are coroutines, as pyBYD's are: nothing runs
until they are awaited. Inside, each awaits an ``asyncio.Future`` of the
running event loop that the Rust side completes. The GIL is not held
while Rust runs, cancelling the awaiting task drops the Rust work, and
results arrive on the loop that awaited them. At interpreter exit the runtime shuts down and drops whatever is
still pending.
"""

from __future__ import annotations

from typing import Any, TypeVar

from openbyd import _native

T = TypeVar("T")

#: Worker threads the runtime starts with unless :func:`configure` says otherwise.
DEFAULT_WORKER_THREADS: int = _native.DEFAULT_WORKER_THREADS


def configure(*, worker_threads: int | None = None) -> None:
    """Set the runtime's number of worker threads (1 to 256).

    Raises ``RuntimeError`` once the runtime has started, that is after the
    first openbyd awaitable was created.
    """
    _native.configure(worker_threads=worker_threads)


def stats() -> dict[str, Any]:
    """Counters for monitoring and tests.

    ``started``, ``completed`` (ran to the end) and ``dropped`` (cancelled
    before the end) Rust futures, ``pending`` ones, the event-loop
    ``wakeups`` that delivered results (one wake-up delivers every result
    that finished meanwhile), ``worker_threads`` and whether the runtime is
    ``running``.
    """
    return dict(_native.stats())


async def sleep(seconds: float, result: T = None) -> T:  # type: ignore[assignment]
    """Wait ``seconds`` on the Rust runtime, then return ``result``.

    Like ``asyncio.sleep``, but the waiting happens in Rust: useful to check
    the bridge or measure its overhead.
    """
    return await _native.sleep(seconds, result)


__all__ = ["DEFAULT_WORKER_THREADS", "configure", "sleep", "stats"]
