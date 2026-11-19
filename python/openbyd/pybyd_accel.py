"""Run pyBYD's envelope cipher in Rust.

pyBYD encrypts every request and decrypts every reply with a table-driven
cipher written in Python. ``install()`` swaps in the Rust version, which
gives the same bytes and releases the GIL while it works::

    from openbyd import pybyd_accel

    pybyd_accel.install()

It changes nothing if pyBYD's tables differ from the ones built in here, or
if the two ciphers disagree on a self-test. A client given its own table file
keeps using pyBYD's own code.
"""

from __future__ import annotations

import logging
import os
from typing import Any

from openbyd import _native

_logger = logging.getLogger(__name__)

_originals: tuple[Any, Any, Any] | None = None


def is_installed() -> bool:
    """Whether pyBYD is currently using the Rust cipher."""
    return _originals is not None


def install() -> bool:
    """Make pyBYD use the Rust cipher. Returns whether it is now in use."""
    global _originals
    if _originals is not None:
        return True

    try:
        from pybyd._crypto import bangcle
    except ImportError:
        _logger.warning("pyBYD is not installed; nothing to accelerate")
        return False

    try:
        python_encrypt = bangcle.encrypt_cbc
        python_decrypt = bangcle.decrypt_cbc
        python_load_tables = bangcle.BangcleCodec._load_tables
        builtin = bangcle._load_tables_from_bin(_native.embedded_tables())
        packaged = bangcle.BangcleCodec()._load_tables()
    except Exception:
        _logger.warning("pyBYD's cipher is not laid out as expected; leaving it alone", exc_info=True)
        return False

    if tuple(packaged) != tuple(builtin):
        _logger.warning("pyBYD ships different cipher tables; leaving its cipher alone")
        return False

    def uses_builtin_tables(tables: Any) -> bool:
        return tables is packaged or tuple(tables) == tuple(builtin)

    def encrypt_cbc(tables: Any, data: bytes, iv: bytes) -> bytes:
        if uses_builtin_tables(tables):
            return _native.encrypt_cbc(bytes(data), bytes(iv))
        return python_encrypt(tables, data, iv)

    def decrypt_cbc(tables: Any, data: bytes, iv: bytes) -> bytes:
        if uses_builtin_tables(tables):
            return _native.decrypt_cbc(bytes(data), bytes(iv))
        return python_decrypt(tables, data, iv)

    def load_tables(codec: Any) -> Any:
        # pyBYD loads its 800 KB of tables once per client. Sharing one copy
        # saves that memory and lets the check above be an identity test.
        if codec._tables_path is None:
            codec._tables = packaged
            return packaged
        return python_load_tables(codec)

    sample, iv = os.urandom(16 * 7), os.urandom(16)
    if encrypt_cbc(packaged, sample, iv) != python_encrypt(packaged, sample, iv) or decrypt_cbc(
        packaged, sample, iv
    ) != python_decrypt(packaged, sample, iv):
        _logger.error("the Rust cipher disagrees with pyBYD; leaving pyBYD's cipher alone")
        return False

    bangcle.encrypt_cbc = encrypt_cbc
    bangcle.decrypt_cbc = decrypt_cbc
    bangcle.BangcleCodec._load_tables = load_tables
    _originals = (python_encrypt, python_decrypt, python_load_tables)
    return True


def uninstall() -> None:
    """Put pyBYD's own cipher back."""
    global _originals
    if _originals is None:
        return
    from pybyd._crypto import bangcle

    bangcle.encrypt_cbc, bangcle.decrypt_cbc, bangcle.BangcleCodec._load_tables = _originals
    _originals = None
