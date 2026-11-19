"""pyBYD's reply models, parsed in Rust.

:func:`parse` gives exactly what pyBYD's
``Model.model_validate(raw, context=context).model_dump(mode="json")``
gives for the model class of the same name, and :func:`properties` the
values of the class's ``@property`` attributes::

    from openbyd import models

    dump = models.parse("GpsInfo", {"latitude": 52.0, "longitude": 4.0})
    assert dump["latitude"] == 52.0

The GIL is released while a model is parsed. Input is converted directly
from Python objects (no JSON text): ``dict`` keys must be ``str``,
integers must fit in 64 bits (``OverflowError`` otherwise) and floats must
be finite (``ValueError`` otherwise).

Where pyBYD raises, these raise the same class: pydantic's
``ValidationError`` becomes :class:`openbyd.ValidationError` (a
``ValueError``), and the few exceptions that escape pydantic in pyBYD
(``TypeError``, ``OverflowError``, ...) are raised as themselves. A value
pyBYD accepts but openbyd cannot hold (an integer result outside 64 bits)
raises ``OverflowError``.
"""

from __future__ import annotations

from typing import Any

from openbyd import _native

#: The pyBYD class names openbyd parses.
NAMES: frozenset[str] = frozenset(_native.model_names())


def parse(name: str, raw: Any, context: Any = None) -> dict[str, Any]:
    """``model_validate(raw, context=context).model_dump(mode="json")`` of pyBYD's class ``name``.

    Raises ``KeyError`` for a name not in :data:`NAMES`.
    """
    return _native.parse_model(name, raw, context)


def properties(name: str, raw: Any, context: Any = None) -> dict[str, Any]:
    """The value of every ``@property`` of pyBYD's class ``name`` for ``raw``, by name.

    Values are in JSON form, as in :func:`parse` (enums as their values,
    datetimes as ISO text). Raises ``KeyError`` for a name not in :data:`NAMES`.
    """
    return _native.model_properties(name, raw, context)


__all__ = ["NAMES", "parse", "properties"]
