"""Run pyBYD's envelope cipher in Rust.

pyBYD encrypts every request and decrypts every reply with a table-driven
cipher written in Python. ``install()`` swaps in the Rust version, which
gives the same bytes and releases the GIL while it works::

    from openbyd import pybyd_accel

    pybyd_accel.install()

It changes nothing if pyBYD's tables differ from the ones built in here, or
if the two ciphers disagree on a self-test. A client given its own table file
keeps using pyBYD's own code.

pyBYD has two ciphers of its own: the reference one, and from 0.0.86 a
faster one that works on a "folded" form of the tables and that its codec
prefers. Both are replaced, so a client made before ``install()`` is
accelerated as well, and after ``uninstall()`` every client is back on
pyBYD's fastest cipher.
"""

from __future__ import annotations

import logging
import os
import threading
from typing import Any

from openbyd import _native

_logger = logging.getLogger(__name__)

# What install() replaced, as (owner, attribute, original value).
_originals: list[tuple[Any, str, Any]] | None = None
# Held while pyBYD is being patched or restored: two threads installing at
# once would each take the other's replacements for pyBYD's own functions.
_lock = threading.Lock()


def is_installed() -> bool:
    """Whether pyBYD is currently using the Rust cipher."""
    return _originals is not None


def install() -> bool:
    """Make pyBYD use the Rust cipher. Returns whether it is now in use."""
    with _lock:
        return _install()


def _install() -> bool:
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
        packaged_codec = bangcle.BangcleCodec()
        packaged = packaged_codec._load_tables()
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

    # The folded form of the packaged tables, and pyBYD's cipher for it.
    # All three are missing before pyBYD 0.0.86; the folded tables are also
    # None for a table file pyBYD cannot fold.
    packaged_folded = getattr(packaged_codec, "_folded", None)
    python_encrypt_folded = getattr(bangcle, "encrypt_cbc_folded", None)
    python_decrypt_folded = getattr(bangcle, "decrypt_cbc_folded", None)
    folds = packaged_folded is not None and python_encrypt_folded is not None and python_decrypt_folded is not None

    def encrypt_cbc_folded(folded: Any, data: bytes, iv: bytes) -> bytes:
        if folded is packaged_folded:
            return _native.encrypt_cbc(bytes(data), bytes(iv))
        return python_encrypt_folded(folded, data, iv)

    def decrypt_cbc_folded(folded: Any, data: bytes, iv: bytes) -> bytes:
        if folded is packaged_folded:
            return _native.decrypt_cbc(bytes(data), bytes(iv))
        return python_decrypt_folded(folded, data, iv)

    def load_tables(codec: Any) -> Any:
        # pyBYD loads its 800 KB of tables once per client. Sharing one copy
        # saves that memory and lets the checks above be identity tests. The
        # codec calls this before every message, so a client that loaded its
        # own copy before install() is moved to the shared one here.
        if codec._tables_path is None:
            codec._tables = packaged
            if folds:
                codec._folded = packaged_folded
            return packaged
        return python_load_tables(codec)

    sample, iv = os.urandom(16 * 7), os.urandom(16)
    if encrypt_cbc(packaged, sample, iv) != python_encrypt(packaged, sample, iv) or decrypt_cbc(
        packaged, sample, iv
    ) != python_decrypt(packaged, sample, iv):
        _logger.error("the Rust cipher disagrees with pyBYD; leaving pyBYD's cipher alone")
        return False

    replaced = [
        (bangcle, "encrypt_cbc", encrypt_cbc),
        (bangcle, "decrypt_cbc", decrypt_cbc),
        (bangcle.BangcleCodec, "_load_tables", load_tables),
    ]
    if folds:
        replaced += [
            (bangcle, "encrypt_cbc_folded", encrypt_cbc_folded),
            (bangcle, "decrypt_cbc_folded", decrypt_cbc_folded),
        ]
    originals = []
    for owner, name, replacement in replaced:
        originals.append((owner, name, getattr(owner, name)))
        setattr(owner, name, replacement)
    _originals = originals
    return True


def uninstall() -> None:
    """Put pyBYD's own cipher back."""
    global _originals
    with _lock:
        if _originals is None:
            return
        for owner, name, original in _originals:
            setattr(owner, name, original)
        _originals = None
