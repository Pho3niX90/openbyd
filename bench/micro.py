"""Micro-benchmarks: one operation, pyBYD against openbyd, in the same Python process.

Every row gets the same input on every side and is checked once for the same answer before it is
timed. A measurement runs the operation for at least ``--min-time`` seconds (0.5 by default) and is
repeated ``--repeats`` times (5); the table gives the median and the spread (max - min) / median.

A row is a :class:`Case` with one :class:`Impl` per library. Adding an implementation (for example
the ``openbyd.Client`` class) is one more ``Impl`` in the cases it applies to.

Run ``python bench/micro.py --help``. ``--quick`` is a smoke test. Results go to ``bench/results/``.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import gc
import importlib
import json
import math
import pkgutil
import random
import sys
import time
from collections.abc import Callable, Iterator
from contextlib import AbstractContextManager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import common  # noqa: E402

USER = "user@example.com"
PASSWORD = "secret"
VIN = "TESTVIN0000000001"
ECHO = "/app/echo"

PYBYD = "pybyd"
PYBYD_ACCEL = "pybyd_accel"
OPENBYD = "openbyd"
LABELS = {
    PYBYD: "pyBYD",
    PYBYD_ACCEL: "pyBYD + openbyd cipher",
    OPENBYD: "openbyd",
}


@dataclass
class Impl:
    """One library's way of doing a row's operation.

    ``open`` is a context manager that sets the implementation up and yields ``unit``, a function
    doing ``Case.ops_per_unit`` operations and returning the answer (compared across the case's
    implementations before timing).
    """

    key: str
    open: Callable[[], AbstractContextManager[Callable[[], Any]]]


@dataclass
class Case:
    group: str
    name: str
    impls: list[Impl]
    ops_per_unit: int = 1
    note: str = ""
    #: Turns an answer into something comparable; the default compares JSON text.
    canon: Callable[[Any], Any] = field(default=lambda value: json.dumps(value, sort_keys=True, default=repr))
    #: When set, the answer (after ``canon``) must also equal this, for rows with a single implementation.
    expected: Any = None
    has_expected: bool = False


def fixed(fn: Callable[[], Any]) -> Callable[[], AbstractContextManager[Callable[[], Any]]]:
    """An ``Impl.open`` for an operation that needs no setup."""

    @contextlib.contextmanager
    def opener() -> Iterator[Callable[[], Any]]:
        yield fn

    return opener


# ---------------------------------------------------------------------------
# Timing
# ---------------------------------------------------------------------------


def measure(unit: Callable[[], Any], ops_per_unit: int, min_time: float, repeats: int) -> dict[str, Any]:
    """Time ``unit``: calibrate a loop count that takes ``min_time``, then repeat it ``repeats`` times."""
    unit()  # warm-up: first calls build caches, connections and tables
    loops = 1
    while True:
        gc.collect()
        started = time.perf_counter()
        for _ in range(loops):
            unit()
        elapsed = time.perf_counter() - started
        if elapsed >= min_time:
            break
        grow = min(100.0, max(2.0, 1.1 * min_time / max(elapsed, 1e-9)))
        loops = max(loops + 1, math.ceil(loops * grow))
    per_op = []
    durations = []
    for _ in range(repeats):
        gc.collect()
        started = time.perf_counter()
        for _ in range(loops):
            unit()
        elapsed = time.perf_counter() - started
        durations.append(elapsed)
        per_op.append(elapsed / (loops * ops_per_unit))
    summary = common.summarize(per_op)
    return {
        "seconds_per_op": summary,
        "ops_per_s": 1 / summary["median"] if summary["median"] else None,
        "loops": loops,
        "ops_per_measurement": loops * ops_per_unit,
        "measurement_s": common.summarize(durations),
        "samples_s_per_op": per_op,
    }


def run_case(case: Case, args: argparse.Namespace) -> dict[str, Any]:
    result: dict[str, Any] = {
        "group": case.group,
        "name": case.name,
        "ops_per_unit": case.ops_per_unit,
        "note": case.note,
        "impls": {},
    }
    answers: dict[str, Any] = {}
    # Pass 1: answers. Pass 2: timings. (An implementation is opened once per pass.)
    for impl in case.impls:
        with impl.open() as unit:
            answers[impl.key] = case.canon(unit())
    reference_key = case.impls[0].key
    reference = answers[reference_key]
    mismatches = [impl.key for impl in case.impls[1:] if answers[impl.key] != reference]
    if case.has_expected and reference != case.canon(case.expected):
        mismatches.insert(0, f"{reference_key} (expected {case.expected!r})")
    result["check"] = {
        "ok": not mismatches,
        "compared": [impl.key for impl in case.impls],
        "mismatch": mismatches,
    }
    if mismatches:
        result["check"]["detail"] = {key: str(answers[key])[:300] for key in [reference_key, *mismatches]}
    for impl in case.impls:
        with impl.open() as unit:
            result["impls"][impl.key] = measure(unit, case.ops_per_unit, args.min_time, args.repeats)
    return result


def print_row(row: dict[str, Any]) -> None:
    parts = []
    for key, data in row["impls"].items():
        median = data["seconds_per_op"]["median"]
        parts.append(f"{LABELS.get(key, key)} {median * 1e6:,.1f} us (+-{data['seconds_per_op']['spread'] * 100:.0f}%)")
    flag = "" if row["check"]["ok"] else "  ANSWERS DIFFER"
    print(f"  {row['group']} / {row['name']}: " + "; ".join(parts) + flag, flush=True)


# ---------------------------------------------------------------------------
# Cases: the envelope cipher
# ---------------------------------------------------------------------------


def sample_text(size: int) -> str:
    """Deterministic ASCII text of ``size`` bytes."""
    rng = random.Random(size)
    return "".join(rng.choices("0123456789abcdef", k=size))


@contextlib.contextmanager
def accelerated() -> Iterator[None]:
    from openbyd import pybyd_accel

    if not pybyd_accel.install():
        raise RuntimeError("pybyd_accel.install() refused: pyBYD's cipher tables differ")
    try:
        yield
    finally:
        pybyd_accel.uninstall()


def cipher_cases(sizes: list[int]) -> list[Case]:
    import openbyd
    from pybyd._crypto.bangcle import BangcleCodec

    cases = []
    for size in sizes:
        text = sample_text(size)

        def pybyd_codec(accel: bool) -> Callable[[], AbstractContextManager[Any]]:
            @contextlib.contextmanager
            def opener() -> Iterator[BangcleCodec]:
                with accelerated() if accel else contextlib.nullcontext():
                    codec = BangcleCodec()
                    codec.encode_envelope("warm")  # loads the tables
                    yield codec

            return opener

        def encode_impls(text: str = text) -> list[Impl]:
            def py(accel: bool) -> Callable[[], AbstractContextManager[Callable[[], Any]]]:
                @contextlib.contextmanager
                def opener() -> Iterator[Callable[[], Any]]:
                    with pybyd_codec(accel)() as codec:
                        yield lambda: codec.encode_envelope(text)

                return opener

            return [
                Impl(PYBYD, py(False)),
                Impl(PYBYD_ACCEL, py(True)),
                Impl(OPENBYD, fixed(lambda: openbyd.encode_envelope(text))),
            ]

        def decode_impls(text: str = text) -> list[Impl]:
            envelope = openbyd.encode_envelope(text)

            def py(accel: bool) -> Callable[[], AbstractContextManager[Callable[[], Any]]]:
                @contextlib.contextmanager
                def opener() -> Iterator[Callable[[], Any]]:
                    with pybyd_codec(accel)() as codec:
                        yield lambda: codec.decode_envelope(envelope)

                return opener

            return [
                Impl(PYBYD, py(False)),
                Impl(PYBYD_ACCEL, py(True)),
                Impl(OPENBYD, fixed(lambda: openbyd.decode_envelope(envelope))),
            ]

        label = f"{size} B" if size < 1024 else f"{size // 1024} KB"
        cases.append(Case("Envelope cipher", f"encode {label}", encode_impls()))
        cases.append(Case("Envelope cipher", f"decode {label}", decode_impls(), canon=lambda value: bytes(value).hex()))
    return cases


# ---------------------------------------------------------------------------
# Cases: reply models
# ---------------------------------------------------------------------------


def pybyd_model_class(name: str) -> type | None:
    import pybyd.models as models

    cls = getattr(models, name, None)
    if cls is not None:
        return cls
    for info in pkgutil.iter_modules(models.__path__):
        module = importlib.import_module(f"pybyd.models.{info.name}")
        cls = getattr(module, name, None)
        if cls is not None:
            return cls
    return None


def model_cases() -> tuple[list[Case], list[str]]:
    from openbyd import models

    cases: list[Case] = []
    skipped: list[str] = []
    headline: Case | None = None
    for path in sorted(common.CORPUS.glob("*.json")):
        name = path.stem
        entries = json.loads(path.read_text(encoding="utf-8"))
        payloads = [entry["payload"] for entry in entries]
        cls = pybyd_model_class(name)
        if cls is None or name not in models.NAMES:
            skipped.append(f"{name}: not a model of both libraries")
            continue

        def pybyd_unit(cls: type = cls, payloads: list[Any] = payloads) -> Callable[[], Any]:
            return lambda: [cls.model_validate(raw).model_dump(mode="json") for raw in payloads]

        def openbyd_unit(name: str = name, payloads: list[Any] = payloads) -> Callable[[], Any]:
            return lambda: [models.parse(name, raw) for raw in payloads]

        note = f"mean over the {len(payloads)} corpus payload(s); pyBYD also validates and builds Python objects"
        cases.append(
            Case("Reply models", name, [Impl(PYBYD, fixed(pybyd_unit())), Impl(OPENBYD, fixed(openbyd_unit()))],
                 ops_per_unit=len(payloads), note=note)
        )  # fmt: skip
        if name == "VehicleRealtimeData":
            full = [entry["payload"] for entry in entries if entry["note"] == "every field present"]
            assert full, "corpus has no 'every field present' realtime payload"
            headline = Case(
                "Reply models",
                "VehicleRealtimeData, every field present (headline)",
                [Impl(PYBYD, fixed(pybyd_unit(cls, full))), Impl(OPENBYD, fixed(openbyd_unit(name, full)))],
                note="the full realtime payload, 3.6 KB of JSON",
            )
    if headline is not None:
        cases.insert(0, headline)
    return cases, skipped


def shadow_cases() -> list[Case]:
    from openbyd import shadow

    cases = []
    for name in ("VehicleRealtimeData", "GpsInfo"):
        entries = json.loads((common.CORPUS / f"{name}.json").read_text(encoding="utf-8"))
        entry = next((e for e in entries if e["note"] == "every field present"), entries[0])
        model = pybyd_model_class(name).model_validate(entry["payload"])  # type: ignore[union-attr]
        cases.append(
            Case(
                "shadow.compare",
                f"{name} ({entry['note']})",
                [Impl(OPENBYD, fixed(lambda model=model: shadow.compare(model)))],
                note="absolute cost of one comparison (parse in openbyd + compare with pyBYD's dump); no pyBYD equivalent. "
                "Answer must be [] (no differing field).",
                expected=[],
                has_expected=True,
            )
        )
    return cases


# ---------------------------------------------------------------------------
# Cases: complete signed requests against the fake server
# ---------------------------------------------------------------------------


def request_cases(server: Any, concurrency: int, batch: int) -> list[Case]:
    import aiohttp
    import pybyd
    from openbyd import lowlevel
    from openbyd.testing import Reply
    from pybyd._api._common import build_inner_base, post_token_json
    from pybyd._crypto.bangcle import BangcleCodec
    from pybyd._transport import SecureTransport

    realtime = next(
        e["payload"]
        for e in json.loads((common.CORPUS / "VehicleRealtimeData.json").read_text(encoding="utf-8"))
        if e["note"] == "every field present"
    )
    server.reply(ECHO, Reply.data(realtime))
    config = pybyd.BydConfig(username=USER, password=PASSWORD, base_url=server.url, mqtt_enabled=False)
    inner = build_inner_base(config, vin=VIN)
    openbyd_config = {"username": USER, "password": PASSWORD, "base_url": server.url}

    @contextlib.contextmanager
    def pybyd_requests(accel: bool, mode: str) -> Iterator[Callable[[], Any]]:
        """pyBYD's ``post_token_json`` over its own ``SecureTransport``, on a loop of its own."""
        loop = asyncio.new_event_loop()
        with accelerated() if accel else contextlib.nullcontext():

            async def start() -> tuple[Any, Any, Any]:
                client = pybyd.BydClient(config)
                await client.async_start()
                await client.login()
                session = await client.ensure_session()
                http = aiohttp.ClientSession(cookie_jar=aiohttp.CookieJar(unsafe=True))
                transport = SecureTransport(config, BangcleCodec(), http)
                return client, http, (session, transport)

            client, http, (session, transport) = loop.run_until_complete(start())

            def one() -> Any:
                return post_token_json(endpoint=ECHO, config=config, session=session, transport=transport, inner=inner)

            try:
                yield _runner(loop, one, mode, concurrency, batch)
            finally:

                async def stop() -> None:
                    await http.close()
                    await client.async_close()

                loop.run_until_complete(stop())
                loop.close()

    @contextlib.contextmanager
    def openbyd_requests(mode: str) -> Iterator[Callable[[], Any]]:
        loop = asyncio.new_event_loop()
        session = loop.run_until_complete(lowlevel.login(openbyd_config))

        def one() -> Any:
            return lowlevel.post_token_json(openbyd_config, session, ECHO, inner)

        try:
            yield _runner(loop, one, mode, concurrency, batch)
        finally:
            loop.close()

    def impls(mode: str) -> list[Impl]:
        return [
            Impl(PYBYD, lambda: pybyd_requests(False, mode)),
            Impl(PYBYD_ACCEL, lambda: pybyd_requests(True, mode)),
            Impl(OPENBYD, lambda: openbyd_requests(mode)),
        ]

    @contextlib.contextmanager
    def pybyd_login() -> Iterator[Callable[[], Any]]:
        loop = asyncio.new_event_loop()
        client = pybyd.BydClient(config)
        loop.run_until_complete(client.async_start())

        async def login() -> Any:
            await client.login()
            return (await client.ensure_session()).user_id

        try:
            yield lambda: loop.run_until_complete(login())
        finally:
            loop.run_until_complete(client.async_close())
            loop.close()

    @contextlib.contextmanager
    def openbyd_login() -> Iterator[Callable[[], Any]]:
        loop = asyncio.new_event_loop()

        async def login() -> Any:
            return (await lowlevel.login(openbyd_config))["user_id"]

        try:
            yield lambda: loop.run_until_complete(login())
        finally:
            loop.close()

    note = (
        "inner payload built once and shared; reply is the 3.6 KB realtime payload; the server runs in a "
        "thread of this process, so it shares the CPU but not the GIL"
    )
    return [
        Case("One signed request, localhost", "sequential (latency)", impls("sequential"), ops_per_unit=batch, note=note),
        Case(
            "One signed request, localhost",
            f"{concurrency} at a time (throughput)",
            impls("concurrent"),
            ops_per_unit=concurrency,
            note=note,
        ),
        Case(
            "Login",
            "one login (sequential)",
            [Impl(PYBYD, pybyd_login), Impl(OPENBYD, openbyd_login)],
            note="pyBYD's BydClient.login() against openbyd.lowlevel.login(); both hash the password, sign and "
            "open the reply; compared on the user id",
        ),
    ]


def _runner(loop: asyncio.AbstractEventLoop, one: Callable[[], Any], mode: str, concurrency: int, batch: int) -> Callable[[], Any]:
    if mode == "sequential":

        async def run() -> Any:
            result = None
            for _ in range(batch):
                result = await one()
            return result

    else:

        async def run() -> Any:
            results = await asyncio.gather(*(one() for _ in range(concurrency)))
            return results[-1]

    return lambda: loop.run_until_complete(run())


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------


def build_cases(args: argparse.Namespace, server: Any) -> tuple[list[Case], list[str]]:
    cases = cipher_cases(args.cipher_sizes)
    model, skipped = model_cases()
    cases += model
    cases += shadow_cases()
    cases += request_cases(server, args.concurrency, args.batch)
    return cases, skipped


def run(args: argparse.Namespace) -> dict[str, Any]:
    from openbyd.testing import FakeServer

    rows = []
    background = common.background_cpu(1.0 if args.quick else 3.0)
    with FakeServer([(USER, PASSWORD)]) as server:
        cases, skipped = build_cases(args, server)
        print(f"micro: {len(cases)} rows, >= {args.min_time} s per measurement, {args.repeats} repeats", flush=True)
        for case in cases:
            if args.only and not any(token.lower() in f"{case.group} {case.name}".lower() for token in args.only):
                continue
            row = run_case(case, args)
            print_row(row)
            rows.append(row)
    return {
        "settings": {
            "min_time_s": args.min_time,
            "repeats": args.repeats,
            "cipher_sizes": args.cipher_sizes,
            "concurrency": args.concurrency,
            "sequential_batch": args.batch,
            "model_context": None,
            "labels": LABELS,
        },
        "background_cpu_percent_before": background,
        "skipped": skipped,
        "rows": rows,
    }


def check(results: dict[str, Any]) -> list[str]:
    return [
        f"{row['group']} / {row['name']}: openbyd's answer differs from pyBYD's ({row['check']['mismatch']})"
        for row in results["rows"]
        if not row["check"]["ok"]
    ]


def add_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--quick", action="store_true", help="short measurements, for a smoke test")
    parser.add_argument("--min-time", type=float, help="seconds per measurement (default 0.5)")
    parser.add_argument("--repeats", type=int, help="measurements per row (default 5)")
    parser.add_argument("--concurrency", type=int, help="requests at a time (default 200)")
    parser.add_argument("--only", nargs="+", help="run only rows whose group or name contains one of these words")
    parser.add_argument("--skip-build", action="store_true", help="accepted for symmetry with the other scripts (nothing to build)")


def settle_defaults(args: argparse.Namespace) -> None:
    quick = args.quick
    args.min_time = args.min_time if args.min_time is not None else (0.03 if quick else 0.5)
    args.repeats = args.repeats if args.repeats is not None else (2 if quick else 5)
    args.concurrency = args.concurrency if args.concurrency is not None else (10 if quick else 200)
    args.batch = 3 if quick else 20
    args.cipher_sizes = [512, 2048] if quick else [512, 2048, 8192]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    add_arguments(parser)
    args = parser.parse_args(argv)
    settle_defaults(args)
    results = run(args)
    path = common.write_results("micro", args.quick, results)
    print(f"wrote {path}")
    problems = check(results)
    for problem in problems:
        print("PROBLEM:", problem)
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
