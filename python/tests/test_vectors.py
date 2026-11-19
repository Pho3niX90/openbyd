"""The Python bindings give the answers recorded from pyBYD."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import openbyd

VECTORS = json.loads(
    (Path(__file__).resolve().parents[2] / "crates/openbyd-crypto/tests/vectors/bangcle.json").read_text(
        encoding="utf-8"
    )
)
ERROR_WORDING = {
    "empty_input": "input is empty",
    "missing_prefix": "must start with",
    "invalid_base64": "Invalid base64",
    "empty_ciphertext": "ciphertext is empty",
    "bad_data_length": "not a multiple of 16",
}


@pytest.mark.parametrize("case", VECTORS["envelopes"], ids=lambda case: f"{len(case['plaintext']) // 2}b")
def test_envelopes(case: dict[str, str]) -> None:
    plaintext = bytes.fromhex(case["plaintext"])
    assert openbyd.encode_envelope(plaintext) == case["envelope"]
    assert openbyd.decode_envelope(case["envelope"]) == plaintext


def test_text_is_encoded_as_utf8() -> None:
    text = "Zoë’s Seal 比亚迪"
    assert openbyd.encode_envelope(text) == openbyd.encode_envelope(text.encode("utf-8"))


@pytest.mark.parametrize("case", VECTORS["cbc"], ids=lambda case: f"{len(case['data']) // 32}blocks")
def test_cbc(case: dict[str, str]) -> None:
    data, iv = bytes.fromhex(case["data"]), bytes.fromhex(case["iv"])
    assert openbyd.encrypt_cbc(data, iv).hex() == case["encrypted"]
    assert openbyd.decrypt_cbc(data, iv).hex() == case["decrypted"]


@pytest.mark.parametrize("case", VECTORS["decode_cases"], ids=lambda case: repr(case["input"][:12]))
def test_envelope_clean_up(case: dict[str, str]) -> None:
    if "error" in case:
        with pytest.raises(openbyd.BangcleError, match=ERROR_WORDING[case["error"]]):
            openbyd.decode_envelope(case["input"])
    else:
        assert openbyd.decode_envelope(case["input"]).hex() == case["plaintext"]


def test_errors_are_value_errors() -> None:
    with pytest.raises(ValueError):
        openbyd.encrypt_cbc(b"short", bytes(16))
    with pytest.raises(ValueError):
        openbyd.decrypt_cbc(bytes(16), b"short")


def test_built_in_tables_match_the_recorded_ones() -> None:
    import hashlib

    assert hashlib.sha256(openbyd.embedded_tables()).hexdigest() == VECTORS["tables_sha256"]
