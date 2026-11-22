"""The asyncio bridge: Rust futures awaited from Python."""

from __future__ import annotations

import asyncio
import re
import subprocess
import sys
import textwrap
import threading
import time
from pathlib import Path

import pytest

import openbyd
from openbyd import _native, exceptions, runtime


def run(coroutine, *, debug: bool = True):
    """Run on a fresh loop. Debug mode makes asyncio raise if a result is set from another thread."""
    return asyncio.run(coroutine, debug=debug)


async def wait_for(predicate, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while not predicate():
        assert time.monotonic() < deadline, "timed out"
        await asyncio.sleep(0.005)


def test_sleep_returns_its_result():
    async def main():
        assert await runtime.sleep(0, "now") == "now"
        assert await runtime.sleep(0.01) is None
        assert await runtime.sleep(-1, 3) == 3
        marker = object()
        assert await runtime.sleep(0.001, marker) is marker

    run(main())


def test_native_awaitable_is_a_future_of_the_running_loop():
    async def main():
        future = _native.sleep(0, 1)
        assert isinstance(future, asyncio.Future)
        assert future.get_loop() is asyncio.get_running_loop()
        assert await future == 1

    run(main())


def test_public_functions_are_coroutines():
    coroutine = runtime.sleep(0, 1)
    assert asyncio.iscoroutine(coroutine)
    assert asyncio.run(coroutine) == 1


def test_native_awaitable_needs_a_running_loop():
    with pytest.raises(RuntimeError, match="no running event loop"):
        _native.sleep(0)


def test_rejects_nan_seconds():
    async def main():
        with pytest.raises(ValueError):
            await runtime.sleep(float("nan"))

    run(main())


def test_cancelling_drops_the_rust_future():
    async def main():
        before = runtime.stats()
        task = asyncio.ensure_future(runtime.sleep(30, "never"))
        await asyncio.sleep(0.01)
        assert runtime.stats()["pending"] >= 1
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        await wait_for(lambda: runtime.stats()["dropped"] > before["dropped"], timeout=2.0)

    started = time.monotonic()
    run(main())
    assert time.monotonic() - started < 3


def test_wait_for_timeout_drops_the_rust_future():
    async def main():
        dropped = runtime.stats()["dropped"]
        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(runtime.sleep(30), 0.05)
        await wait_for(lambda: runtime.stats()["dropped"] > dropped, timeout=2.0)

    run(main())


def test_cancel_after_completion_is_harmless():
    async def main():
        future = _native.sleep(0, 5)
        await asyncio.sleep(0.05)
        future.cancel()  # the result may already be on its way: it is then discarded
        await asyncio.sleep(0.05)
        assert future.cancelled() or future.result() == 5

    run(main())


ERROR_KINDS = [
    "BydError", "BydCryptoError", "BangcleError", "BangcleTableLoadError", "BydTransportError",
    "BydApiError", "BydAuthenticationError", "BydSessionExpiredError", "BydTokenRefreshError",
    "BydServerTimeError", "BydRemoteControlError",
    "BydControlPasswordError", "BydEndpointNotSupportedError", "BydDataUnavailableError",
    "BydRateLimitError", "BydServiceBusyError", "BydStandbyScheduleError", "ValueError", "JSONDecodeError",
    "UnicodeDecodeError", "ValidationError", "TypeError", "AttributeError", "AssertionError",
    "ScriptExhausted", "CancelledError",
]
BUILTIN = {
    "CancelledError": asyncio.CancelledError,
    "ValueError": ValueError,
    "TypeError": TypeError,
    "AttributeError": AttributeError,
    "AssertionError": AssertionError,
    "UnicodeDecodeError": UnicodeDecodeError,
    "ScriptExhausted": RuntimeError,
}


@pytest.mark.parametrize("kind", ERROR_KINDS)
def test_every_error_kind_maps_to_its_class(kind):
    async def main():
        with pytest.raises(BaseException) as caught:
            await _native._fail(kind, "it failed", code="1008", endpoint="/x", status_code=503, delay=0.001)
        return caught.value

    error = run(main())
    if kind in BUILTIN:
        assert type(error) is BUILTIN[kind]
    elif kind == "JSONDecodeError":
        import json

        assert type(error) is json.JSONDecodeError
    else:
        assert type(error) is getattr(exceptions, kind)
        assert type(error).__module__ == "openbyd.exceptions"
    assert "it failed" in str(error)
    if isinstance(error, exceptions.BydApiError):
        assert (error.code, error.endpoint) == ("1008", "/x")
    if isinstance(error, exceptions.BydTransportError):
        assert (error.status_code, error.endpoint) == (503, "/x")


def test_every_rust_error_kind_is_tested():
    source = (Path(__file__).resolve().parents[2] / "crates/openbyd-protocol/src/error.rs").read_text("utf-8")
    block = source[source.index("pub const ALL") : source.index("];", source.index("pub const ALL"))]
    variants = re.findall(r"ErrorKind::(\w+),", block)
    names = {"JsonDecodeError": "JSONDecodeError"}
    assert sorted(names.get(v, v) for v in variants) == sorted(ERROR_KINDS)
    with pytest.raises(ValueError, match="unknown error class"):
        _native._fail("NoSuchError")


def test_a_panic_becomes_runtime_error():
    async def main():
        with pytest.raises(RuntimeError, match="panicked: boom"):
            await _native._panic("boom")
        assert await _native._panic("") is None

    run(main())


def test_ten_thousand_concurrent_awaits():
    async def main():
        before = runtime.stats()
        results = await asyncio.gather(*(runtime.sleep(0.05 * (i % 3) / 2, i) for i in range(10_000)))
        assert results == list(range(10_000))
        after = runtime.stats()
        assert after["completed"] - before["completed"] == 10_000
        # Batching: far fewer loop wake-ups than results.
        assert after["wakeups"] - before["wakeups"] < 10_000

    run(main(), debug=False)


def test_event_loop_lag_stays_low_with_a_thousand_pending():
    async def main():
        pending = [_native.sleep(1.0 + i / 10_000, i) for i in range(1000)]
        lags = []
        end = time.monotonic() + 1.3
        while time.monotonic() < end:
            asked = time.monotonic()
            await asyncio.sleep(0.01)
            lags.append(time.monotonic() - asked - 0.01)
        assert await asyncio.gather(*pending) == list(range(1000))
        return max(lags)

    worst = run(main(), debug=False)
    # The loop never waits on Rust; generous bound for slow CI machines.
    assert worst < 0.25, f"worst event-loop lag {worst * 1000:.1f} ms"


def test_two_event_loops_in_two_threads():
    results: dict[str, object] = {}

    def worker(name: str, offset: int) -> None:
        async def main():
            loop = asyncio.get_running_loop()
            values = await asyncio.gather(*(runtime.sleep(0.001 * (i % 5), offset + i) for i in range(500)))
            # Exceptions too arrive on this loop.
            with pytest.raises(exceptions.BydServiceBusyError):
                await _native._fail("BydServiceBusyError", delay=0.01)
            assert asyncio.get_running_loop() is loop
            return values

        try:
            results[name] = run(main())  # debug mode: setting a result off-thread raises
        except BaseException as error:  # pragma: no cover - reported below
            results[name] = error

    threads = [threading.Thread(target=worker, args=(f"t{n}", n * 1000)) for n in range(2)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(30)
    assert results["t0"] == list(range(500))
    assert results["t1"] == list(range(1000, 1500))


def test_many_loops_one_after_another():
    for index in range(20):
        assert asyncio.run(runtime.sleep(0, index)) == index


def test_results_for_a_closed_loop_are_dropped():
    loop = asyncio.new_event_loop()
    future = loop.run_until_complete(_make_future_on(loop))
    loop.close()
    time.sleep(0.1)  # the future finishes on a closed loop: nothing must crash
    assert not future.done()
    assert asyncio.run(runtime.sleep(0, "still fine")) == "still fine"


async def _make_future_on(loop):
    return _native.sleep(0.02, "late")


def test_stats_shape():
    stats = runtime.stats()
    assert set(stats) == {"started", "completed", "dropped", "pending", "wakeups", "worker_threads", "running"}
    assert stats["worker_threads"] >= 1


def test_configure_after_start_raises():
    asyncio.run(runtime.sleep(0))
    with pytest.raises(RuntimeError, match="already started"):
        runtime.configure(worker_threads=3)


def _python(code: str, timeout: float = 30) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-c", textwrap.dedent(code)],
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def test_configure_before_start():
    done = _python(
        """
        import asyncio
        from openbyd import runtime
        assert runtime.stats()["running"] is False
        for bad in (0, 257):
            try:
                runtime.configure(worker_threads=bad)
            except ValueError:
                pass
            else:
                raise SystemExit("accepted " + str(bad))
        runtime.configure()
        runtime.configure(worker_threads=5)
        asyncio.run(runtime.sleep(0))
        stats = runtime.stats()
        assert stats["worker_threads"] == 5 and stats["running"], stats
        print("ok")
        """
    )
    assert done.returncode == 0, done.stderr
    assert done.stdout.strip() == "ok"


def test_exit_with_pending_futures_neither_crashes_nor_hangs():
    started = time.monotonic()
    done = _python(
        """
        import asyncio, threading
        from openbyd import _native, runtime

        async def leave_pending():
            for _ in range(2000):
                _native.sleep(60)
            await runtime.sleep(0.01)

        asyncio.run(leave_pending())

        # A loop still running in a daemon thread, with results arriving, at exit.
        def spin():
            async def busy():
                while True:
                    await asyncio.gather(*(runtime.sleep(0.001, i) for i in range(200)))
            asyncio.run(busy())

        threading.Thread(target=spin, daemon=True).start()
        import time; time.sleep(0.2)
        print("exiting")
        """
    )
    assert done.returncode == 0, done.stderr
    assert "exiting" in done.stdout
    assert time.monotonic() - started < 20


def test_after_shutdown_new_awaitables_raise():
    done = _python(
        """
        import asyncio
        from openbyd import _native, runtime
        _native.shutdown()
        async def main():
            try:
                await runtime.sleep(0)
            except RuntimeError as error:
                print("refused:", "shut down" in str(error))
        asyncio.run(main())
        """
    )
    assert done.returncode == 0, done.stderr
    assert "refused: True" in done.stdout


def test_works_with_the_default_policy_loop():
    # On Windows this is the proactor loop, on Linux the selector loop.
    loop = asyncio.new_event_loop()
    try:
        assert loop.run_until_complete(_await_value(7)) == 7
    finally:
        loop.close()


async def _await_value(value):
    return await runtime.sleep(0.001, value)


def test_works_with_the_selector_loop():
    loop = asyncio.SelectorEventLoop()
    try:
        assert loop.run_until_complete(_await_value(8)) == 8
    finally:
        loop.close()


def test_works_with_uvloop():
    uvloop = pytest.importorskip("uvloop")
    loop = uvloop.new_event_loop()
    try:
        async def main():
            values = await asyncio.gather(*(runtime.sleep(0.001 * (i % 3), i) for i in range(2000)))
            task = asyncio.ensure_future(runtime.sleep(30))
            await asyncio.sleep(0.01)
            dropped = runtime.stats()["dropped"]
            task.cancel()
            await wait_for(lambda: runtime.stats()["dropped"] > dropped)
            return values

        assert loop.run_until_complete(main()) == list(range(2000))
    finally:
        loop.close()


def test_package_exports_the_runtime():
    assert openbyd.runtime is runtime
    assert runtime.DEFAULT_WORKER_THREADS == _native.DEFAULT_WORKER_THREADS
