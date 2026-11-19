"""Write cipher test vectors using pyBYD as the reference.

Run with the pyBYD commit named in ``parity/UPSTREAM_COMMIT`` installed::

    python parity/gen_bangcle_vectors.py

The output is committed. Both the Rust tests and the Python tests read it,
and CI regenerates it to notice when upstream changes the cipher.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import importlib.resources
import json
import random
from pathlib import Path

from pybyd._crypto._bangcle_block import decrypt_cbc, encrypt_cbc
from pybyd._crypto.bangcle import BangcleCodec
from pybyd.exceptions import BangcleError

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_OUT = ROOT / "crates" / "openbyd-crypto" / "tests" / "vectors" / "bangcle.json"
SEED = 20261119

ENVELOPE_LENGTHS = [*range(0, 66), 127, 128, 129, 255, 256, 1000, 4096]
TEXTS = [
    "",
    "{}",
    '{"countryCode":"ZA","identifier":"12345","language":"en"}',
    '{"name":"Zoë’s Seal","city":"København","note":"比亚迪 海豹"}',
    '{"emoji":"🚗⚡","escaped":"line\\nbreak\\ttab"}',
]
ZERO_IV = bytes(16)


def error_kind(message: str) -> str:
    """Name the failure without depending on Python's exact wording."""
    if message == "Bangcle input is empty":
        return "empty_input"
    if message.startswith("Bangcle envelope must start with"):
        return "missing_prefix"
    if message.startswith("Invalid base64"):
        return "invalid_base64"
    if message == "Bangcle ciphertext is empty":
        return "empty_ciphertext"
    if "is not a multiple of 16" in message:
        return "bad_data_length"
    raise AssertionError(f"unrecognised pyBYD error: {message}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    args = parser.parse_args()

    rng = random.Random(SEED)
    codec = BangcleCodec()
    tables = codec._load_tables()

    envelopes = []
    for length in ENVELOPE_LENGTHS:
        plaintext = rng.randbytes(length)
        envelopes.append({"plaintext": plaintext.hex(), "envelope": codec.encode_envelope(plaintext)})
    for text in TEXTS:
        envelopes.append({"plaintext": text.encode().hex(), "envelope": codec.encode_envelope(text)})
    for item in envelopes:
        assert codec.decode_envelope(item["envelope"]).hex() == item["plaintext"]

    cbc = []
    for blocks in (0, 1, 2, 3, 8, 33):
        data = rng.randbytes(blocks * 16)
        iv = rng.randbytes(16)
        cbc.append(
            {
                "data": data.hex(),
                "iv": iv.hex(),
                "encrypted": encrypt_cbc(tables, data, iv).hex(),
                "decrypted": decrypt_cbc(tables, data, iv).hex(),
            }
        )

    # Inputs that exercise how an envelope is cleaned up before decoding.
    sample = rng.randbytes(40)
    good = codec.encode_envelope(sample)
    body = good[1:]
    url_safe_plain = next(
        plain
        for plain in (rng.randbytes(45) for _ in range(10_000))
        if {"+", "/"} <= set(codec.encode_envelope(plain))
    )
    url_safe = codec.encode_envelope(url_safe_plain).replace("+", "-").replace("/", "_")
    not_padded = rng.randbytes(32)  # ends in no valid padding, so it is returned whole
    raw_block = "F" + base64.b64encode(encrypt_cbc(tables, not_padded, ZERO_IV)).decode()

    decode_inputs = [
        good,
        f"  {good[:20]}\n{good[20:40]}\r\n\t{good[40:]}  ",
        "\x0b\x0c" + good + "\x1c\x1f\x85\xa0",
        good.rstrip("="),
        url_safe,
        "F" + body[:10] + "!*~" + body[10:],
        "F" + body + "trailing-data",
        good + "====",
        raw_block,
        "",
        "   \r\n\t ",
        "F",
        "F====",
        body,
        "f" + body,
        "FA",
        "FAA",
        "FAAA",
        "FAAAA",
        "FZm9v",
        "F" + body[:-4],
        "F" + body + "é",
        "Fé",
        "﻿" + good,
    ]
    decode_cases = []
    for text in decode_inputs:
        try:
            decode_cases.append({"input": text, "plaintext": codec.decode_envelope(text).hex()})
        except BangcleError as exc:
            decode_cases.append({"input": text, "error": error_kind(str(exc))})

    table_file = importlib.resources.files("pybyd").joinpath("data/bangcle_tables.bin").read_bytes()
    document = {
        "source": "pyBYD",
        "upstream_commit": (ROOT / "parity" / "UPSTREAM_COMMIT").read_text().strip(),
        "tables_sha256": hashlib.sha256(table_file).hexdigest(),
        "envelopes": envelopes,
        "cbc": cbc,
        "decode_cases": decode_cases,
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(document, indent=1, ensure_ascii=True) + "\n", encoding="utf-8", newline="\n")
    print(f"wrote {len(envelopes)} envelopes, {len(cbc)} CBC cases, {len(decode_cases)} decode cases to {args.out}")


if __name__ == "__main__":
    main()
