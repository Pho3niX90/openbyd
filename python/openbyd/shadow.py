"""Compare openbyd with pyBYD on live traffic.

An application that still uses pyBYD can check, reply by reply, that
openbyd would have parsed it the same way::

    from openbyd import shadow

    info = await client.get_gps_info(vin)          # a pyBYD model
    for field in shadow.compare(info):
        metrics.increment("openbyd.shadow.mismatch", tags={"field": field})

:func:`compare` returns field *names* only, never values, so its result is
safe to log and count. It never raises, whatever it is given.
"""

from __future__ import annotations

from typing import Any

from openbyd import _native

#: openbyd failed (could not convert or parse the reply) where pyBYD succeeded.
ERROR: str = _native.SHADOW_ERROR
#: openbyd has no model of this class name.
UNKNOWN_MODEL: str = _native.SHADOW_UNKNOWN_MODEL


def compare(model: Any, context: Any = None) -> list[str]:
    """The sorted names of the top-level fields where openbyd differs from pyBYD.

    ``model`` is a pyBYD model instance: its class name picks the openbyd
    model, ``model.raw`` is parsed with ``context`` (pass the one pyBYD was
    given) and the result is compared with ``model.model_dump(mode="json")``.

    An empty list means openbyd gives the same dump. ``raw`` itself is not
    compared: it only echoes the input, and ``model.raw`` is not always the
    input pyBYD was given (a list reply is kept as ``{"list": [...]}``).

    :data:`UNKNOWN_MODEL` is returned for a class openbyd does not know, and
    :data:`ERROR` when openbyd fails, or when ``model`` is not a model with
    ``raw`` and ``model_dump``. Never raises (``KeyboardInterrupt`` and
    ``SystemExit`` still propagate).
    """
    try:
        name = type(model).__name__
        if name not in _KNOWN:
            return [UNKNOWN_MODEL]
        raw = model.raw
        expected = model.model_dump(mode="json")
        return _native.shadow_compare(name, raw, expected, context)
    except (KeyboardInterrupt, SystemExit):
        raise
    except BaseException:  # noqa: BLE001 - must never raise, Rust panics included
        return [ERROR]


_KNOWN = frozenset(_native.model_names())

__all__ = ["ERROR", "UNKNOWN_MODEL", "compare"]
