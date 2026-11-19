"""The parity test framework.

Every function ported from pyBYD has a *call name* such as
``"hashing.md5_hex"``. On the Rust side the name is answered by
``openbyd_protocol::parity::call``, or for the ``_api`` flows ported in the
client crate by ``openbyd_client::parity::call``. Here, a *group* module in
``parity/groups/`` says what pyBYD answers for it (the oracle) and which
arguments to try:

- recorded cases: a fixed list. ``parity/generate.py`` runs pyBYD on them
  and writes ``crates/<crate>/tests/vectors/<group>.json`` (``crate`` is
  ``openbyd-protocol`` by default, ``openbyd-client`` for
  ``Group(name, crate="client")``), which the Rust tests replay without
  needing Python.
- fuzz cases: random arguments, compared live in ``python/tests``.

A group module looks like this::

    from parity.framework import Group

    group = Group("hashing")

    @group.oracle("hashing.md5_hex")
    def md5_hex(value: str) -> str:
        from pybyd._crypto.hashing import md5_hex
        return md5_hex(value)

    @group.recorded
    def recorded():
        yield "hashing.md5_hex", {"value": ""}

    @group.fuzz
    def fuzz(rng):
        yield "hashing.md5_hex", {"value": random_text(rng)}

An oracle takes the call's arguments as keywords and returns something
``json.dumps`` accepts. An exception it raises is the expected answer too:
the Rust side must fail with an error whose kind is the exception's class
name. Only pyBYD's own errors and ``ValueError`` (which covers pydantic's
``ValidationError``) count; anything else is a bug in the oracle and stops
the run. Pass ``errors=`` to ``oracle`` to allow more.

The flows of pyBYD's ``_api`` package (``async def``s that post requests)
are compared as traces; ``parity/api_support.py`` has the runner and the
case builders.
"""

from __future__ import annotations

import importlib
import json
import pkgutil
import random
from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
#: Where each crate's recorded vectors live.
VECTOR_DIRS = {
    "protocol": ROOT / "crates" / "openbyd-protocol" / "tests" / "vectors",
    "client": ROOT / "crates" / "openbyd-client" / "tests" / "vectors",
}
VECTOR_DIR = VECTOR_DIRS["protocol"]
UPSTREAM_COMMIT = (ROOT / "parity" / "UPSTREAM_COMMIT").read_text(encoding="utf-8").strip()

Case = tuple[str, dict[str, Any]]


def _default_errors() -> tuple[type[BaseException], ...]:
    from pybyd.exceptions import BydError

    return (BydError, ValueError)


@dataclass
class Group:
    """The oracles and cases of one area of pyBYD.

    ``crate`` says which Rust crate answers the group's calls and replays its
    vectors: ``"protocol"`` (the default) or ``"client"``.
    """

    name: str
    crate: str = "protocol"
    oracles: dict[str, Callable[..., Any]] = field(default_factory=dict)
    expected_errors: dict[str, tuple[type[BaseException], ...]] = field(default_factory=dict)
    _recorded: list[Callable[[], Iterable[Case]]] = field(default_factory=list)
    _fuzz: list[Callable[[random.Random], Iterable[Case]]] = field(default_factory=list)

    def __post_init__(self) -> None:
        if self.crate not in VECTOR_DIRS:
            raise ValueError(f"group {self.name}: crate must be one of {sorted(VECTOR_DIRS)}")

    def oracle(
        self, call: str, *, errors: tuple[type[BaseException], ...] | None = None
    ) -> Callable[[Callable[..., Any]], Callable[..., Any]]:
        """Register what pyBYD answers for ``call``."""

        def register(function: Callable[..., Any]) -> Callable[..., Any]:
            if call in self.oracles:
                raise ValueError(f"{call} is registered twice in group {self.name}")
            self.oracles[call] = function
            if errors is not None:
                self.expected_errors[call] = errors
            return function

        return register

    def recorded(self, function: Callable[[], Iterable[Case]]) -> Callable[[], Iterable[Case]]:
        """Register a source of fixed cases."""
        self._recorded.append(function)
        return function

    def fuzz(self, function: Callable[[random.Random], Iterable[Case]]) -> Callable[[random.Random], Iterable[Case]]:
        """Register a source of random cases. It may yield forever."""
        self._fuzz.append(function)
        return function

    def recorded_cases(self) -> list[Case]:
        cases = [case for source in self._recorded for case in source()]
        for call, _ in cases:
            self._require(call)
        return cases

    def fuzz_cases(self, rng: random.Random, count: int) -> Iterator[Case]:
        """Up to ``count`` random cases, taken in turn from each source."""
        sources = [iter(source(rng)) for source in self._fuzz]
        produced = 0
        while sources and produced < count:
            for source in list(sources):
                try:
                    call, args = next(source)
                except StopIteration:
                    sources.remove(source)
                    continue
                self._require(call)
                yield call, args
                produced += 1
                if produced >= count:
                    return

    def answer(self, call: str, args: dict[str, Any]) -> dict[str, Any]:
        """What pyBYD says: ``{"ok": value}`` or ``{"error": "ClassName"}``."""
        allowed = self.expected_errors.get(call) or _default_errors()
        try:
            value = self._require(call)(**args)
        except allowed as exc:
            return {"error": type(exc).__name__}
        # Round-trip so tuples, enums and the like take their JSON shape.
        return {"ok": json.loads(json.dumps(value, allow_nan=False))}

    def _require(self, call: str) -> Callable[..., Any]:
        if call not in self.oracles:
            raise KeyError(f"group {self.name} has a case for {call!r} but no oracle")
        return self.oracles[call]

    def vector_path(self) -> Path:
        return VECTOR_DIRS[self.crate] / f"{self.name}.json"

    def vector_document(self) -> str:
        """The text of this group's vector file, from running pyBYD now."""
        cases = [{"call": call, "args": args, **self.answer(call, args)} for call, args in self.recorded_cases()]
        document = {"group": self.name, "upstream_commit": UPSTREAM_COMMIT, "cases": cases}
        return json.dumps(document, indent=1, ensure_ascii=True) + "\n"


def load_groups() -> dict[str, Group]:
    """Import every module in ``parity/groups`` and return its ``group``."""
    from parity import groups as package

    found: dict[str, Group] = {}
    for info in sorted(pkgutil.iter_modules(package.__path__), key=lambda info: info.name):
        module = importlib.import_module(f"parity.groups.{info.name}")
        group = getattr(module, "group", None)
        if not isinstance(group, Group):
            raise TypeError(f"parity/groups/{info.name}.py must define `group = Group(...)`")
        if group.name in found:
            raise ValueError(f"two group modules are named {group.name}")
        found[group.name] = group
    return found


def openbyd_answer(call: str, args: dict[str, Any]) -> dict[str, Any]:
    """What openbyd says, in the same shape as :meth:`Group.answer`."""
    from openbyd import _native

    reply = json.loads(_native.parity_call(call, json.dumps(args, ensure_ascii=False, allow_nan=False)))
    if "error" in reply:
        return {"error": reply["error"]["kind"], "message": reply["error"]["message"]}
    return {"ok": reply["ok"]}


# ---------------------------------------------------------------------------
# Helpers for writing cases
# ---------------------------------------------------------------------------

_TEXT_ALPHABETS = (
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
    " \t\n\r\"\\/&=+-_.,:;{}[]<>!?@#$%^*()|~`'",
    "éüñçÅøßŁ",
    "比亚迪海豹充电",
    "  ​\x00\x01\x1f\x7f",
    "🚗⚡😀",
)


def random_text(rng: random.Random, max_length: int = 40) -> str:
    """Text that leans on the awkward cases: quotes, control characters, non-ASCII."""
    alphabets = rng.sample(_TEXT_ALPHABETS, k=rng.randint(1, len(_TEXT_ALPHABETS)))
    return "".join(rng.choice(rng.choice(alphabets)) for _ in range(rng.randint(0, max_length)))


def random_hex(rng: random.Random, nbytes: int, *, upper: bool = True) -> str:
    text = rng.randbytes(nbytes).hex()
    return text.upper() if upper else text
