"""openbyd against a live pyBYD install. Skipped when pyBYD is absent."""

from __future__ import annotations

import os
import random
import subprocess
import sys
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
