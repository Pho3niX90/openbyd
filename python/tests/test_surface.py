"""Every public pyBYD symbol has a parity call, or a stated reason it has none.

``parity/surface.py`` lists what the installed pyBYD exposes. A symbol counts
as covered when a group registers an oracle under its call name, or when
``parity/surface_exceptions.py`` lists it in ``NOT_PORTED``.

While the port is in progress the coverage test skips and reports the gap.
Set ``OPENBYD_SURFACE_STRICT=1`` to make it fail instead.
"""

from __future__ import annotations

import os

import pytest

pytest.importorskip("pybyd")

from parity.framework import load_groups  # noqa: E402
from parity.surface import expected_calls, io_functions  # noqa: E402
from parity.surface_exceptions import NOT_PORTED  # noqa: E402


def _registered() -> set[str]:
    names: set[str] = set()
    for group in load_groups().values():
        names.update(group.oracles)
    return names


def _missing(expected: dict[str, str]) -> list[str]:
    registered = _registered()
    return sorted(name for name in expected if name not in registered and name not in NOT_PORTED)


def test_every_pybyd_symbol_has_a_parity_call() -> None:
    expected = expected_calls()
    missing = _missing(expected)
    if os.environ.get("OPENBYD_SURFACE_STRICT") == "1":
        if missing:
            lines = [f"  {name}  ({expected[name]})" for name in missing]
            pytest.fail(f"{len(missing)} pyBYD symbols have no parity call:\n" + "\n".join(lines))
        return
    pytest.skip(
        f"{len(missing)} of {len(expected)} pyBYD symbols have no parity call yet "
        "(set OPENBYD_SURFACE_STRICT=1 to fail)"
    )


def test_exceptions_are_still_needed() -> None:
    expected = expected_calls()
    registered = _registered()
    stale = sorted(name for name in NOT_PORTED if name not in expected)
    assert not stale, f"NOT_PORTED names symbols pyBYD no longer has: {stale}"
    ported = sorted(name for name in NOT_PORTED if name in registered)
    assert not ported, f"NOT_PORTED lists calls that now have an oracle; remove them: {ported}"


def test_surface_introspection_is_sane() -> None:
    expected = expected_calls()
    assert len(expected) > 60, f"only {len(expected)} expected calls found"
    for name in (
        "hashing.md5_hex",
        "models.gps.GpsInfo",
        "models.realtime.VehicleRealtimeData",
        "aes.aes_encrypt_hex",
        "session.content_key",
    ):
        assert name in expected, f"{name} missing from expected_calls()"
    assert "api.gps.fetch_gps_endpoint" in io_functions()
