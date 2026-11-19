class BangcleError(ValueError):
    """An envelope or cipher input could not be processed."""

def encode_envelope(plaintext: str | bytes) -> str:
    """Wrap plaintext in a Bangcle envelope."""

def decode_envelope(envelope: str) -> bytes:
    """Open a Bangcle envelope and return the plaintext bytes."""

def encrypt_cbc(data: bytes, iv: bytes) -> bytes:
    """Encrypt whole 16-byte blocks in CBC mode with the built-in tables."""

def decrypt_cbc(data: bytes, iv: bytes) -> bytes:
    """Decrypt whole 16-byte blocks in CBC mode with the built-in tables."""

def embedded_tables() -> bytes:
    """The built-in table file, in pyBYD's ``bangcle_tables.bin`` format."""

def parity_call(name: str, args_json: str) -> str:
    """Run one parity call. For the test suite only; not part of the public API."""
