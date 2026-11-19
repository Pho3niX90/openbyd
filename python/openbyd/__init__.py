"""openbyd: an unofficial client for the BYD overseas app API, with a Rust core.

The package:

- :class:`Client` (:mod:`openbyd.client`): pyBYD's ``BydClient`` for asyncio,
  returning pyBYD's ``model_dump(mode="json")`` dicts.
- :class:`Car` (:mod:`openbyd.car`): pyBYD's ``BydCar``, from ``Client.get_car``,
  with :class:`SeatPosition` and :class:`SeatLevel`.
- :mod:`openbyd.exceptions`: pyBYD's exception hierarchy (also exported here).
- :mod:`openbyd.models`: pyBYD's reply models, parsed in Rust.
- :mod:`openbyd.shadow`: compare openbyd with pyBYD on live replies.
- :mod:`openbyd.runtime`: the Rust runtime behind openbyd's awaitables.
- :mod:`openbyd.lowlevel`: single API requests from asyncio.
- :mod:`openbyd.pybyd_accel`: run pyBYD's envelope cipher in Rust.
- ``openbyd.testing``: a fake BYD server and an MQTT test broker
  (development builds only).
- The envelope cipher: :func:`encode_envelope`, :func:`decode_envelope`,
  :func:`encrypt_cbc`, :func:`decrypt_cbc`, :func:`embedded_tables`.
"""

from importlib.metadata import PackageNotFoundError, version

# exceptions first: the extension raises its classes.
from openbyd import exceptions
from openbyd._native import (
    decode_envelope,
    decrypt_cbc,
    embedded_tables,
    encode_envelope,
    encrypt_cbc,
)
from openbyd import lowlevel, models, runtime, shadow
from openbyd.car import Car, SeatLevel, SeatPosition
from openbyd.client import Client
from openbyd.exceptions import (
    BangcleError,
    BangcleTableLoadError,
    BydApiError,
    BydAuthenticationError,
    BydControlPasswordError,
    BydCryptoError,
    BydDataUnavailableError,
    BydEndpointNotSupportedError,
    BydError,
    BydRateLimitError,
    BydRemoteControlError,
    BydServiceBusyError,
    BydSessionExpiredError,
    BydTransportError,
    ValidationError,
)

try:
    __version__ = version("openbyd")
except PackageNotFoundError:  # pragma: no cover - only when run from a source tree
    __version__ = "0+local"

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
    "Car",
    "Client",
    "SeatLevel",
    "SeatPosition",
    "ValidationError",
    "__version__",
    "decode_envelope",
    "decrypt_cbc",
    "embedded_tables",
    "encode_envelope",
    "encrypt_cbc",
    "exceptions",
    "lowlevel",
    "models",
    "runtime",
    "shadow",
]
