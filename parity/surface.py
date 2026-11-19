"""The public surface of the installed pyBYD, as parity call names.

``expected_calls()`` walks the installed ``pybyd`` package and lists every
public symbol that needs a parity call, mapped to where it lives::

    {"hashing.md5_hex": "pybyd._crypto.hashing:md5_hex", ...}

``io_functions()`` lists the public coroutine functions. They are ported in
``openbyd-client`` and need a parity call too: one that runs the flow against
a script and compares its trace (``parity/api_support.py``).

``pybyd.client`` and ``pybyd.car`` are audited member by member, as
``Session`` is: every public method and property of ``BydClient`` needs a
``client.<name>`` call and every one of ``BydCar`` a ``car.<name>`` call.
Anything else public that those two modules define (a type variable, a
helper class) needs a ``client.<name>`` or ``car.<name>`` call too, or an
entry in ``parity/surface_exceptions.py``.

The rules that decide which module maps to which prefix are in ``_EXACT``,
``_SKIP`` and ``prefix_for``. A module that matches no rule raises
``RuntimeError``, so a new upstream module cannot be missed silently.

Run it to print the report::

    python parity/surface.py
"""

from __future__ import annotations

import enum
import importlib
import inspect
import pkgutil
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

# Module name to prefix, for modules whose prefix is not derived from the name.
_EXACT: dict[str, str] = {
    "pybyd._crypto.hashing": "hashing",
    "pybyd._crypto.aes": "aes",
    "pybyd._crypto.signing": "signing",
    "pybyd._constants": "constants",
    "pybyd._redact": "redact",
    "pybyd._mqtt": "mqtt",
    "pybyd.config": "config",
    "pybyd.session": "session",
    "pybyd._api.login": "envelope",
    "pybyd._api._envelope": "envelope",
    "pybyd._transport": "envelope",
    "pybyd._api._common": "api.common",
    "pybyd.models._base": "models.base",
    "pybyd._state_engine": "state",
    "pybyd._validators": "state",
    "pybyd.client": "client",
    "pybyd.car": "car",
}

# Classes audited member by member: every public method and property needs
# ``<prefix>.<member>``, and the class itself needs no call of its own.
_EXPANDED: dict[str, str] = {
    "pybyd.session": "Session",
    "pybyd.client": "BydClient",
    "pybyd.car": "BydCar",
}

# Modules where every public name they define is surface, not only
# functions, models and enums.
_EVERYTHING: frozenset[str] = frozenset({"pybyd.client", "pybyd.car"})

# Modules with nothing to port here. ``pybyd._crypto`` is listed by prefix
# below: the cipher is tested separately, so everything under it is skipped.
_SKIP: frozenset[str] = frozenset(
    {
        "pybyd",
        "pybyd.models",
        "pybyd._api",
        "pybyd._capabilities",
        "pybyd._version",
        "pybyd.exceptions",
    }
)

# Public members of ``Session`` that are not part of openbyd's surface.
# ``guest_vins`` and ``user_type_settled`` are skipped as the spec asks.
# ``model_post_init`` is pydantic's construction hook, not an API method.
_MEMBER_SKIP: dict[str, frozenset[str]] = {
    "Session": frozenset({"guest_vins", "user_type_settled", "model_post_init"}),
}


def prefix_for(module_name: str) -> str | None:
    """Return the call-name prefix for a pyBYD module, or None to skip it.

    Raises ``RuntimeError`` for a module that no rule covers.
    """
    if module_name in _EXACT:
        return _EXACT[module_name]
    if module_name in _SKIP or module_name == "pybyd._crypto" or module_name.startswith("pybyd._crypto."):
        return None
    for package, prefix in (
        ("pybyd._api.", "api."),
        ("pybyd.models.", "models."),
        ("pybyd._capabilities.", "capabilities."),
    ):
        if module_name.startswith(package):
            rest = module_name[len(package) :]
            if rest and "." not in rest:
                return prefix + rest
    raise RuntimeError(
        f"no parity prefix for pyBYD module {module_name!r}; add a rule for it in parity/surface.py"
    )


def _fail_on_import_error(name: str) -> None:
    # pkgutil ignores ImportError while walking packages by default. A pyBYD
    # module that fails to import must stop the audit, not drop out of it.
    raise RuntimeError(f"pyBYD package {name!r} failed to import")


def _add(table: dict[str, str], call: str, symbol: str) -> None:
    if call in table:
        raise RuntimeError(f"two pyBYD symbols map to {call!r}: {table[call]} and {symbol}")
    table[call] = symbol


def _collect() -> tuple[dict[str, str], dict[str, str]]:
    import pybyd
    from pydantic import BaseModel

    calls: dict[str, str] = {}
    io: dict[str, str] = {}

    for info in pkgutil.walk_packages(pybyd.__path__, prefix="pybyd.", onerror=_fail_on_import_error):
        prefix = prefix_for(info.name)
        if prefix is None:
            continue
        module = importlib.import_module(info.name)
        for name, obj in vars(module).items():
            if name.startswith("_") or getattr(obj, "__module__", None) != module.__name__:
                continue
            symbol = f"{module.__name__}:{name}"
            call = f"{prefix}.{name}"

            if inspect.isclass(obj) and _EXPANDED.get(module.__name__) == name:
                # Expanded to its members rather than listed as one class.
                skip = _MEMBER_SKIP.get(name, frozenset())
                for member, value in vars(obj).items():
                    if member.startswith("_") or member in skip:
                        continue
                    if inspect.iscoroutinefunction(value):
                        _add(io, f"{prefix}.{member}", f"{symbol}.{member}")
                    elif isinstance(value, property) or inspect.isfunction(value):
                        _add(calls, f"{prefix}.{member}", f"{symbol}.{member}")
                continue
            if module.__name__ in _EVERYTHING:
                _add(calls, call, symbol)
                continue
            if inspect.iscoroutinefunction(obj):
                _add(io, call, symbol)
            elif inspect.isfunction(obj):
                _add(calls, call, symbol)
            elif inspect.isclass(obj) and issubclass(obj, (BaseModel, enum.Enum)):
                _add(calls, call, symbol)
    return calls, io


def expected_calls() -> dict[str, str]:
    """Every pyBYD symbol that needs a parity call, mapped to ``module:Symbol``."""
    return _collect()[0]


def io_functions() -> dict[str, str]:
    """Every public pyBYD coroutine function, mapped to ``module:Symbol``.

    These are ported in ``openbyd-client`` and compared by trace.
    """
    return _collect()[1]


def main() -> None:
    calls = expected_calls()
    io = io_functions()

    print(f"{len(calls)} expected parity calls")
    groups: dict[str, list[str]] = {}
    for name in sorted(calls):
        groups.setdefault(name.rsplit(".", 1)[0], []).append(name)
    for prefix, names in groups.items():
        print(f"\n[{prefix}] {len(names)}")
        for name in names:
            print(f"  {name:<40} {calls[name]}")

    print(f"\n{len(io)} I/O functions (ported in openbyd-client, compared by trace)")
    for name in sorted(io):
        print(f"  {name:<40} {io[name]}")


if __name__ == "__main__":
    main()
