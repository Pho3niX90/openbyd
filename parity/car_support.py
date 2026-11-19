"""Shared pieces of the ``capabilities`` and ``car`` parity groups.

Scripted errors and their trace form, and a way to run a coroutine that
never waits on anything outside the script.

A scripted error is ``{"class": "BydRemoteControlError", "code": "timeout",
"endpoint": "/control/remoteControl"}``: ``class`` names a pyBYD exception or
``ValueError``, ``TypeError`` or ``AttributeError``; ``code`` and
``endpoint`` (API errors) and ``status_code`` and ``endpoint`` (transport
errors) are optional and default as the constructors do.
"""

from __future__ import annotations

import builtins
from typing import Any

from pybyd import exceptions

#: Every error class a script may raise, with the attributes it takes.
ERROR_CLASSES = [
    "BydError", "BydApiError", "BydRemoteControlError", "BydControlPasswordError", "BydEndpointNotSupportedError",
    "BydDataUnavailableError", "BydRateLimitError", "BydServiceBusyError", "BydAuthenticationError",
    "BydSessionExpiredError", "BydTransportError", "BydCryptoError", "ValueError", "TypeError", "AttributeError",
]

#: Typical failures of a remote command, in pyBYD's own words.
COMMAND_ERRORS = [
    {"class": "BydRemoteControlError", "code": "2", "endpoint": "/control/remoteControl"},
    {"class": "BydRemoteControlError", "code": "timeout", "endpoint": "/control/remoteControlResult"},
    {"class": "BydRemoteControlError", "code": "1009", "endpoint": "/control/remoteControl"},
    {"class": "BydControlPasswordError", "code": "5005", "endpoint": "/control/remoteControl"},
    {"class": "BydControlPasswordError", "code": "5011", "endpoint": "/control/remoteControl"},
    {"class": "BydEndpointNotSupportedError", "code": "1001", "endpoint": "/control/remoteControl"},
    {"class": "BydRateLimitError", "code": "6024", "endpoint": "/control/remoteControl"},
    {"class": "BydServiceBusyError", "code": "1008", "endpoint": "/control/remoteControl"},
    {"class": "BydSessionExpiredError", "code": "1005", "endpoint": "/control/remoteControl"},
    {"class": "BydApiError", "code": "9999", "endpoint": "/control/remoteControl"},
    {"class": "BydTransportError", "status_code": 502, "endpoint": "/control/remoteControl"},
    {"class": "BydTransportError"},
    {"class": "BydError"},
    {"class": "ValueError"},
    {"class": "TypeError"},
]


class ScriptExhausted(Exception):
    """The car called the client more often than the script has replies for."""


def scripted_error(description: dict[str, Any]) -> BaseException:
    """The exception a script asks for, built as pyBYD builds it."""
    name = description["class"]
    cls = getattr(exceptions, name, None) or getattr(builtins, name)
    message = f"scripted {name}"
    if isinstance(cls, type) and issubclass(cls, exceptions.BydApiError):
        return cls(message, code=description.get("code") or "", endpoint=description.get("endpoint") or "")
    if isinstance(cls, type) and issubclass(cls, exceptions.BydTransportError):
        return cls(message, status_code=description.get("status_code"), endpoint=description.get("endpoint") or "")
    return cls(message)


def describe(error: BaseException) -> dict[str, Any]:
    """The trace form of an exception: its class and the attributes pyBYD's exceptions carry."""
    api = isinstance(error, exceptions.BydApiError)
    transport = isinstance(error, exceptions.BydTransportError)
    return {
        "class": type(error).__name__,
        "code": error.code if api else None,
        "endpoint": error.endpoint if api or transport else None,
        "status_code": error.status_code if transport else None,
    }


def drive(coroutine: Any) -> Any:
    """Run a coroutine that never suspends; fail if it does."""
    try:
        coroutine.send(None)
    except StopIteration as stop:
        return stop.value
    coroutine.close()
    raise RuntimeError("the coroutine waited on something")
