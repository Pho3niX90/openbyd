"""pybyd/_crypto/aes.py"""

from __future__ import annotations

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
from pybyd._crypto import aes

from parity.framework import Group, random_hex, random_text

group = Group("aes")


@group.oracle("aes.aes_encrypt_hex")
def aes_encrypt_hex(plaintext: str, key_hex: str) -> str:
    return aes.aes_encrypt_hex(plaintext, key_hex)


@group.oracle("aes.aes_decrypt_utf8")
def aes_decrypt_utf8(cipher_hex: str, key_hex: str) -> str:
    return aes.aes_decrypt_utf8(cipher_hex, key_hex)


def raw_encrypt(data: bytes, key: bytes) -> str:
    """AES-CBC with a zero IV and no padding: for crafting bad padding and bad UTF-8."""
    encryptor = Cipher(algorithms.AES(key), modes.CBC(b"\0" * 16)).encryptor()
    return (encryptor.update(data) + encryptor.finalize()).hex().upper()


def pkcs7(data: bytes) -> bytes:
    fill = 16 - len(data) % 16
    return data + bytes([fill]) * fill


KEY16 = "000102030405060708090A0B0C0D0E0F"
KEY24 = KEY16 + "1011121314151617"
KEY32 = KEY16 + "101112131415161718191A1B1C1D1E1F"

BAD_KEYS = [
    "", " ", "0x", "0X" + KEY16, " " + KEY16 + "\n", KEY16.lower(), KEY16[:-1], KEY16 + "00", "zz" * 16,
    " ".join(KEY16[i : i + 2] for i in range(0, 32, 2)) + "  ", KEY16[:2] + " " + KEY16[2:-1], "\x1c" + KEY16,
    KEY16 + "　", "0x0x" + KEY16, "١" * 32, KEY16[:30] + "é0",
]


def cipher_variants(rng, key_hex: str) -> list[str]:
    key = bytes.fromhex(key_hex)
    text = random_text(rng, 50)
    good = aes.aes_encrypt_hex(text, key_hex)
    flipped = bytearray(bytes.fromhex(good))
    flipped[rng.randrange(len(flipped))] ^= 1 << rng.randrange(8)
    return [
        good,
        good.lower(),
        " 0x" + good + " ",
        good[:-2],
        good[:-32] if len(good) > 32 else "",
        bytes(flipped).hex(),
        raw_encrypt(pkcs7(bytes([0xFF, 0xC3, 0x28]) + rng.randbytes(rng.randint(0, 20))), key),
        raw_encrypt(rng.randbytes(16 * rng.randint(1, 3)), key),
        raw_encrypt(b"x" * 15 + b"\x00", key),
        raw_encrypt(b"x" * 15 + b"\x11", key),
        raw_encrypt(b"x" * 14 + b"\x01\x02", key),
        raw_encrypt(b"\x10" * 16, key),
        random_hex(rng, rng.randint(0, 40)),
    ]


@group.recorded
def recorded():
    for key in [KEY16, KEY24, KEY32, *BAD_KEYS]:
        for text in ["", "a", "exactly sixteen!", '{"a":"b"}', "Zoë’s 比亚迪 🚗", "\x00\x7f"]:
            yield "aes.aes_encrypt_hex", {"plaintext": text, "key_hex": key}
    import random

    rng = random.Random("aes-recorded")
    for key in [KEY16, KEY24, KEY32]:
        for cipher_hex in cipher_variants(rng, key):
            yield "aes.aes_decrypt_utf8", {"cipher_hex": cipher_hex, "key_hex": key}
    for key in BAD_KEYS:
        yield "aes.aes_decrypt_utf8", {"cipher_hex": aes.aes_encrypt_hex("x", KEY16), "key_hex": key}


@group.fuzz
def fuzz(rng):
    while True:
        key = random_hex(rng, rng.choice([16, 24, 32]), upper=rng.random() < 0.5)
        if rng.random() < 0.2:
            key = rng.choice(BAD_KEYS + [random_hex(rng, rng.randint(0, 40)), random_text(rng, 64)])
        yield "aes.aes_encrypt_hex", {"plaintext": random_text(rng, 80), "key_hex": key}
        good_key = random_hex(rng, rng.choice([16, 24, 32]))
        yield "aes.aes_decrypt_utf8", {"cipher_hex": rng.choice(cipher_variants(rng, good_key)), "key_hex": good_key}
