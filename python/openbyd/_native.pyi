"""The compiled extension. Use the documented modules of :mod:`openbyd` instead."""

import asyncio
from collections.abc import Iterable, Sequence
from types import TracebackType
from typing import Any, Final, TypeVar

from openbyd.exceptions import BangcleError as BangcleError

_T = TypeVar("_T")

# ---- envelope cipher ----

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

# ---- runtime and asyncio bridge ----

DEFAULT_WORKER_THREADS: Final[int]

def configure(*, worker_threads: int | None = None) -> None:
    """Set the shared runtime's worker threads; only before it starts."""

def stats() -> dict[str, Any]:
    """Counters of the bridge: started, completed, dropped, pending, wakeups, worker_threads, running."""

def sleep(seconds: float, result: _T = ...) -> asyncio.Future[_T]:
    """An ``asyncio.Future`` of the running loop, completed with ``result`` after ``seconds`` in Rust."""

def shutdown() -> None:
    """Stop the runtime, dropping pending futures. Registered with ``atexit``."""

def _fail(
    kind: str,
    message: str = "",
    code: str = "",
    endpoint: str = "",
    status_code: int | None = None,
    delay: float = 0.0,
) -> asyncio.Future[None]:
    """A future failing with the openbyd error of class ``kind``. For tests."""

def _panic(message: str) -> asyncio.Future[None]:
    """A future whose Rust side panics (or returns ``None`` for an empty message). For tests."""

class Dispatcher:
    """Delivers finished Rust futures on one event loop's thread. Internal."""

    def __call__(self) -> None: ...

class _Value:
    """A JSON value held in Rust, for measuring conversions. Internal."""

    def __init__(self, value: Any) -> None: ...
    @staticmethod
    def from_json(text: str) -> _Value: ...
    def to_python(self) -> Any: ...
    def to_json(self) -> str: ...

# ---- models and shadow comparison ----

SHADOW_ERROR: Final[str]
SHADOW_UNKNOWN_MODEL: Final[str]

def model_names() -> list[str]:
    """The pyBYD class names openbyd parses, sorted."""

def parse_model(name: str, raw: Any, context: Any = None) -> dict[str, Any]:
    """``model_validate(raw, context=context).model_dump(mode="json")`` of pyBYD's class ``name``."""

def model_properties(name: str, raw: Any, context: Any = None) -> dict[str, Any]:
    """The ``@property`` values of pyBYD's class ``name`` for ``raw``."""

def shadow_compare(name: str, raw: Any, expected: Any, context: Any = None) -> list[str]:
    """Names of top-level fields where openbyd's dump differs from ``expected``, or a marker."""

# ---- low-level requests ----

def login(config: dict[str, Any], base_url: str | None = None) -> asyncio.Future[dict[str, Any]]:
    """Log in; the future gives the session's fields."""

def post_token_json(
    config: dict[str, Any],
    session: dict[str, Any],
    endpoint: str,
    inner: dict[str, Any],
    *,
    vin: str | None = None,
    user_type: str | None = None,
) -> asyncio.Future[Any]:
    """Post a token request; the future gives the decrypted ``respondData``."""

# ---- fake server (only with the cargo feature "testing") ----

HAS_TESTING: Final[bool]

class Reply:
    """What the fake server answers to one request."""

    @staticmethod
    def data(data: Any) -> Reply: ...
    @staticmethod
    def code(code: str, message: str = "") -> Reply: ...
    @staticmethod
    def outer(outer: Any) -> Reply: ...
    @staticmethod
    def status(status: int, body: str = "") -> Reply: ...
    @staticmethod
    def raw(body: bytes) -> Reply: ...
    @staticmethod
    def drop() -> Reply: ...
    @staticmethod
    def default() -> Reply: ...
    def delayed(self, seconds: float) -> Reply: ...
    def with_cookie(self, header: str) -> Reply: ...

class FakeServer:
    """A fake BYD server on 127.0.0.1, on a thread of its own."""

    def __init__(self, accounts: Iterable[tuple[str, str] | dict[str, Any]] | None = None) -> None: ...
    @property
    def url(self) -> str: ...
    @property
    def running(self) -> bool: ...
    @property
    def connections(self) -> int: ...
    def add_account(
        self,
        username: str,
        password: str,
        *,
        user_id: str | None = None,
        guest_vins: Sequence[str] | None = None,
        user_type_rule: bool = True,
    ) -> None: ...
    def user_id(self, username: str) -> str | None: ...
    def reply(self, endpoint: str, reply: Reply) -> None: ...
    def script(self, endpoint: str, replies: Sequence[Reply]) -> None: ...
    def set_cookie(self, header: str) -> None: ...
    def expire_sessions(self, username: str) -> None: ...
    def requests(self, endpoint: str | None = None) -> list[dict[str, Any]]: ...
    def clear_requests(self) -> None: ...
    def stop(self) -> None: ...
    def __enter__(self) -> FakeServer: ...
    def __exit__(
        self,
        kind: type[BaseException] | None,
        value: BaseException | None,
        traceback: TracebackType | None,
    ) -> bool: ...
