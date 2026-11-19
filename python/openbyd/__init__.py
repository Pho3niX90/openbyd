"""openbyd: an unofficial client for the BYD overseas app API, with a Rust core.

So far this package holds the envelope cipher. The rest of the client is
being ported from pyBYD.
"""

from importlib.metadata import PackageNotFoundError, version

from openbyd._native import (
    BangcleError,
    decode_envelope,
    decrypt_cbc,
    embedded_tables,
    encode_envelope,
    encrypt_cbc,
)

try:
    __version__ = version("openbyd")
except PackageNotFoundError:
    __version__ = "0+local"

__all__ = [
    "BangcleError",
    "__version__",
    "decode_envelope",
    "decrypt_cbc",
    "embedded_tables",
    "encode_envelope",
    "encrypt_cbc",
]
