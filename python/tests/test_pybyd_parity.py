"""openbyd against a live pyBYD install. Skipped when pyBYD is absent."""

from __future__ import annotations

import os
import random
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

import openbyd
from openbyd import pybyd_accel

pytest.importorskip("pybyd")

from pybyd._crypto import _bangcle_block, bangcle  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture
def python_codec():
    """A pyBYD codec that is certain to be running pyBYD's own cipher."""
    pybyd_accel.uninstall()
    yield bangcle.BangcleCodec()
    pybyd_accel.uninstall()


def test_random_inputs_agree(python_codec) -> None:
    rng = random.Random(os.environ.get("OPENBYD_PARITY_SEED", "openbyd"))
    for _ in range(150):
        plaintext = rng.randbytes(rng.randrange(0, 700))
        envelope = python_codec.encode_envelope(plaintext)
        assert openbyd.encode_envelope(plaintext) == envelope
        assert openbyd.decode_envelope(envelope) == python_codec.decode_envelope(envelope) == plaintext


def test_recorded_vectors_are_current(tmp_path) -> None:
    """Fails when upstream changes the cipher and the vectors need a refresh."""
    fresh = tmp_path / "bangcle.json"
    subprocess.run(
        [sys.executable, str(ROOT / "parity/gen_bangcle_vectors.py"), "--out", str(fresh)],
        check=True,
        capture_output=True,
    )
    recorded = ROOT / "crates/openbyd-crypto/tests/vectors/bangcle.json"
    assert fresh.read_bytes() == recorded.read_bytes()


def test_accelerator_swaps_the_cipher_and_back(python_codec) -> None:
    plaintext = b'{"vin":"TESTVIN0000000001","speed":0}' * 9
    expected = python_codec.encode_envelope(plaintext)

    assert pybyd_accel.install() is True
    assert pybyd_accel.is_installed()
    assert pybyd_accel.install() is True  # a second call is harmless
    assert bangcle.encrypt_cbc is not _bangcle_block.encrypt_cbc

    fast_codec = bangcle.BangcleCodec()
    assert fast_codec.encode_envelope(plaintext) == expected
    assert fast_codec.decode_envelope(expected) == plaintext
    # A codec made before install() keeps working too.
    assert python_codec.encode_envelope(plaintext) == expected

    pybyd_accel.uninstall()
    assert not pybyd_accel.is_installed()
    assert bangcle.encrypt_cbc is _bangcle_block.encrypt_cbc
    assert bangcle.BangcleCodec().encode_envelope(plaintext) == expected


def _count_native(monkeypatch) -> dict[str, int]:
    """Count the calls that reach the Rust cipher."""
    calls = {"encrypt": 0, "decrypt": 0}
    encrypt, decrypt = openbyd._native.encrypt_cbc, openbyd._native.decrypt_cbc

    def counted_encrypt(data: bytes, iv: bytes) -> bytes:
        calls["encrypt"] += 1
        return encrypt(data, iv)

    def counted_decrypt(data: bytes, iv: bytes) -> bytes:
        calls["decrypt"] += 1
        return decrypt(data, iv)

    monkeypatch.setattr(openbyd._native, "encrypt_cbc", counted_encrypt)
    monkeypatch.setattr(openbyd._native, "decrypt_cbc", counted_decrypt)
    return calls


def test_accelerator_reaches_every_codec(python_codec, monkeypatch) -> None:
    """pyBYD 0.0.86 prefers its own folded cipher; the accelerator must still be the one that runs."""
    plaintext = b'{"vin":"TESTVIN0000000001"}' * 40
    expected = python_codec.encode_envelope(plaintext)  # loads (and folds) the tables before install()
    calls = _count_native(monkeypatch)

    assert pybyd_accel.install() is True
    calls.update(encrypt=0, decrypt=0)  # the self-test used it
    late = bangcle.BangcleCodec()
    for codec in (late, python_codec):
        assert codec.encode_envelope(plaintext) == expected
        assert codec.decode_envelope(expected) == plaintext
    assert calls == {"encrypt": 2, "decrypt": 2}

    # After uninstall() both are back on pyBYD's own cipher, and on its fastest one.
    pybyd_accel.uninstall()
    for codec in (late, python_codec):
        assert codec.encode_envelope(plaintext) == expected
        assert codec.decode_envelope(expected) == plaintext
    assert calls == {"encrypt": 2, "decrypt": 2}
    fresh = bangcle.BangcleCodec()
    fresh.encode_envelope(b"")
    if hasattr(fresh, "_folded"):
        assert fresh._folded is not None
        assert late._folded is not None and python_codec._folded is not None


def test_accelerator_works_with_a_pybyd_without_folded_tables(python_codec, monkeypatch) -> None:
    """pyBYD before 0.0.86 has only the reference cipher."""
    plaintext = bytes(range(200))
    expected = python_codec.encode_envelope(plaintext)
    if hasattr(bangcle, "encrypt_cbc_folded"):
        monkeypatch.delattr(bangcle, "encrypt_cbc_folded")
        monkeypatch.delattr(bangcle, "decrypt_cbc_folded")
        monkeypatch.setattr(bangcle, "fold_tables", lambda tables: None)
    calls = _count_native(monkeypatch)

    assert pybyd_accel.install() is True
    calls.update(encrypt=0, decrypt=0)
    codec = bangcle.BangcleCodec()
    assert codec.encode_envelope(plaintext) == expected
    assert codec.decode_envelope(expected) == plaintext
    assert calls == {"encrypt": 1, "decrypt": 1}
    assert not hasattr(bangcle, "encrypt_cbc_folded")
    pybyd_accel.uninstall()
    assert not hasattr(bangcle, "encrypt_cbc_folded")


def test_accelerator_installs_once_from_several_threads(python_codec, monkeypatch) -> None:
    """Threads that install at the same moment must not take each other's replacements for pyBYD's own."""
    names = [name for name in ("encrypt_cbc", "decrypt_cbc", "encrypt_cbc_folded", "decrypt_cbc_folded") if hasattr(bangcle, name)]
    originals = {name: getattr(bangcle, name) for name in names}
    load_tables = bangcle.BangcleCodec._load_tables
    read_tables = bangcle._load_tables_from_bin

    def slow_read(data: bytes):
        time.sleep(0.05)  # long enough for every thread to be inside install() together
        return read_tables(data)

    monkeypatch.setattr(bangcle, "_load_tables_from_bin", slow_read)
    start = threading.Barrier(4)
    results: list[bool] = []

    def run() -> None:
        start.wait()
        results.append(pybyd_accel.install())

    threads = [threading.Thread(target=run) for _ in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    assert results == [True] * 4
    assert bangcle.encrypt_cbc is not originals["encrypt_cbc"]

    pybyd_accel.uninstall()
    assert not pybyd_accel.is_installed()
    for name, original in originals.items():
        assert getattr(bangcle, name) is original
    assert bangcle.BangcleCodec._load_tables is load_tables


def test_accelerated_clients_share_one_copy_of_the_tables(python_codec) -> None:
    assert pybyd_accel.install() is True
    first, second = bangcle.BangcleCodec(), bangcle.BangcleCodec()
    assert first._load_tables() is second._load_tables()


def test_accelerator_leaves_custom_tables_to_pybyd(python_codec, tmp_path) -> None:
    # Scramble the last-round encryption table, which sits just before the
    # two 8-byte permutation tables at the end of the file.
    table_file = bytearray(openbyd.embedded_tables())
    for at in range(len(table_file) - 16 - 0x1000, len(table_file) - 16):
        table_file[at] ^= 0xFF
    path = tmp_path / "tables.bin"
    path.write_bytes(table_file)

    plaintext = bytes(range(64))
    expected = bangcle.BangcleCodec(tables_path=path).encode_envelope(plaintext)
    assert expected != python_codec.encode_envelope(plaintext)

    assert pybyd_accel.install() is True
    assert bangcle.BangcleCodec(tables_path=path).encode_envelope(plaintext) == expected


def test_accelerator_refuses_when_the_ciphers_disagree(python_codec, monkeypatch) -> None:
    monkeypatch.setattr(openbyd._native, "encrypt_cbc", lambda data, iv: bytes(len(data)))
    assert pybyd_accel.install() is False
    assert not pybyd_accel.is_installed()
    assert bangcle.encrypt_cbc is _bangcle_block.encrypt_cbc


def test_accelerator_does_nothing_without_pybyd(python_codec, monkeypatch) -> None:
    import pybyd._crypto

    monkeypatch.delattr(pybyd._crypto, "bangcle")
    monkeypatch.setitem(sys.modules, "pybyd._crypto.bangcle", None)  # makes the import fail
    assert pybyd_accel.install() is False
    assert not pybyd_accel.is_installed()


def test_accelerator_leaves_an_unfamiliar_pybyd_alone(python_codec, monkeypatch) -> None:
    monkeypatch.delattr(bangcle, "_load_tables_from_bin")
    assert pybyd_accel.install() is False
    assert bangcle.encrypt_cbc is _bangcle_block.encrypt_cbc


def test_accelerator_refuses_when_the_tables_differ(python_codec, monkeypatch) -> None:
    other = bytearray(openbyd.embedded_tables())
    other[-20] ^= 0xFF
    monkeypatch.setattr(openbyd._native, "embedded_tables", lambda: bytes(other))
    assert pybyd_accel.install() is False
    assert bangcle.encrypt_cbc is _bangcle_block.encrypt_cbc


def test_custom_tables_decode_through_pybyd_too(python_codec, tmp_path) -> None:
    table_file = bytearray(openbyd.embedded_tables())
    for at in range(len(table_file) - 16 - 0x1000, len(table_file) - 16):
        table_file[at] ^= 0xFF
    path = tmp_path / "tables.bin"
    path.write_bytes(table_file)
    sealed = bytes(range(32))
    expected = _bangcle_block.decrypt_cbc(bangcle.BangcleCodec(tables_path=path)._load_tables(), sealed, bytes(16))

    assert pybyd_accel.install() is True
    custom = bangcle.BangcleCodec(tables_path=path)._load_tables()
    assert bangcle.decrypt_cbc(custom, sealed, bytes(16)) == expected
