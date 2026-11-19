"""The exceptions openbyd raises: pyBYD's hierarchy, with the same names, bases and attributes.

Code written against ``pybyd.exceptions`` can catch these the same way::

    BydError
    ├── BydCryptoError
    │   └── BangcleError             (also a ValueError, see below)
    │       └── BangcleTableLoadError
    ├── BydTransportError            .status_code, .endpoint
    └── BydApiError                  .code, .endpoint
        ├── BydAuthenticationError
        │   └── BydSessionExpiredError
        ├── BydRemoteControlError
        ├── BydControlPasswordError
        ├── BydEndpointNotSupportedError
        ├── BydDataUnavailableError
        ├── BydRateLimitError
        └── BydServiceBusyError

    ValidationError (ValueError)     .error_type, .loc

They are distinct classes from pyBYD's: ``except pybyd.exceptions.BydApiError``
does not catch ``openbyd.exceptions.BydApiError``.

Two additions to pyBYD's hierarchy: ``BangcleError`` also subclasses
``ValueError``, because openbyd's cipher functions have always raised a
``ValueError`` for bad input; and :class:`ValidationError` stands where pyBYD
lets pydantic's ``ValidationError`` through.
"""

from __future__ import annotations


class BydError(Exception):
    """Base exception for all pybyd errors."""


class BydCryptoError(BydError):
    """Encryption or decryption failure."""


class BangcleError(BydCryptoError, ValueError):
    """Bangcle envelope encode/decode failure."""


class BangcleTableLoadError(BangcleError):
    """Could not load Bangcle lookup tables."""


class BydTransportError(BydError):
    """HTTP-level failure (network, non-200, invalid JSON)."""

    def __init__(self, message: str, *, status_code: int | None = None, endpoint: str = "") -> None:
        self.status_code = status_code
        self.endpoint = endpoint
        super().__init__(message)


class BydApiError(BydError):
    """API returned a non-zero code (application-level error)."""

    def __init__(self, message: str, *, code: str = "", endpoint: str = "") -> None:
        self.code = code
        self.endpoint = endpoint
        super().__init__(message)


class BydAuthenticationError(BydApiError):
    """Login failed or session expired."""


class BydSessionExpiredError(BydAuthenticationError):
    """Session token rejected by the server (for example code ``1005``)."""


class BydRemoteControlError(BydApiError):
    """Remote control command failed after being accepted by the cloud."""


class BydControlPasswordError(BydApiError):
    """Remote control command rejected for an invalid or locked operation password (5005, 5006, 5011)."""


class BydEndpointNotSupportedError(BydApiError):
    """Endpoint not supported for this vehicle or region (for example code ``1001``)."""


class BydDataUnavailableError(BydApiError):
    """Vehicle cannot provide the requested data right now (for example GPS code ``6051``)."""


class BydRateLimitError(BydApiError):
    """A previous command is still in progress (code ``6024``)."""


class BydServiceBusyError(BydApiError):
    """Backend busy (code ``1008``); keep the last data and back off."""


class ValidationError(ValueError):
    """A reply did not validate, where pyBYD raises pydantic's ``ValidationError``.

    ``error_type`` is pydantic's error type (such as ``"int_parsing"``) and
    ``loc`` where the bad value was, outermost first. Wording differs from
    pydantic's.
    """

    def __init__(self, message: str, *, error_type: str = "", loc: tuple[str, ...] = ()) -> None:
        self.error_type = error_type
        self.loc = tuple(loc)
        super().__init__(message)


__all__ = [
    "BangcleError",
    "BangcleTableLoadError",
    "BydApiError",
    "BydAuthenticationError",
    "BydControlPasswordError",
    "BydCryptoError",
    "BydDataUnavailableError",
    "BydEndpointNotSupportedError",
    "BydError",
    "BydRateLimitError",
    "BydRemoteControlError",
    "BydServiceBusyError",
    "BydSessionExpiredError",
    "BydTransportError",
    "ValidationError",
]
