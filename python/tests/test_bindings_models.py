"""openbyd.models answers what pyBYD's models answer, through the direct Python conversion."""

from __future__ import annotations

import importlib
import json
import os
import pkgutil
import random
import threading
import time
from pathlib import Path
from typing import Any

import pytest

pytest.importorskip("pybyd")

import pybyd.models as pybyd_models  # noqa: E402
from pydantic import BaseModel  # noqa: E402

import openbyd  # noqa: E402
from openbyd import models  # noqa: E402
from parity.framework import Group, load_groups  # noqa: E402
from parity.models_support import jsonable  # noqa: E402

CORPUS = Path(__file__).resolve().parents[2] / "parity" / "corpus"
PER_MODEL = int(os.environ.get("OPENBYD_MODEL_CASES", "300"))
GROUPS = load_groups()


def _model_calls() -> dict[str, tuple[Group, str]]:
    """Every reply model the parity groups register, by class name: calls such as
    ``models.<file>.<Class>`` and ``mqtt.<Class>`` whose cases are ``{"raw", "context"}``.
    (Request-parameter models such as ``ClimateStartParams`` are called differently.)"""
    calls = {}
    for group in GROUPS.values():
        raw_calls = {call for call, args in group.recorded_cases() if "raw" in args}
        for call in group.oracles:
            if call not in raw_calls:
                continue
            parts = call.split(".")
            if parts[0] == "mqtt" and parts[1].startswith("Mqtt") or parts[0] == "models" and len(parts) == 3:
                name = parts[-1]
                if name[0].isupper() and _is_model(name):
                    calls[name] = (group, call)
    return calls


def _pybyd_class(name: str) -> type | None:
    if name.startswith("Mqtt"):
        from pybyd import _mqtt

        return getattr(_mqtt, name, None)
    for info in pkgutil.iter_modules(pybyd_models.__path__):
        cls = getattr(importlib.import_module(f"pybyd.models.{info.name}"), name, None)
        if cls is not None:
            return cls
    return None


def _is_model(name: str) -> bool:
    cls = _pybyd_class(name)
    return isinstance(cls, type) and issubclass(cls, BaseModel)


MODEL_CALLS = _model_calls()


def test_names_cover_every_pybyd_model_with_a_parity_entry():
    assert set(MODEL_CALLS) == set(models.NAMES)
    assert openbyd.models is models


def _openbyd_answer(name: str, raw: Any, context: Any) -> dict[str, Any]:
    try:
        dump = models.parse(name, raw, context)
        properties = models.properties(name, raw, context)
    except openbyd.ValidationError:
        return {"error": "ValidationError"}
    except (ValueError, TypeError, AttributeError, OverflowError, OSError) as error:
        return {"error": type(error).__name__}
    return {"ok": {"dump": jsonable(dump), "properties": jsonable(properties)}}


def _cases(group: Group, call: str) -> list[dict[str, Any]]:
    recorded = [args for case_call, args in group.recorded_cases() if case_call == call]
    rng = random.Random(f"bindings:{call}")
    fuzz: list[dict[str, Any]] = []
    sources = [iter(source(rng)) for source in group._fuzz]
    attempts = 0
    while len(recorded) + len(fuzz) < PER_MODEL and sources and attempts < PER_MODEL * 50:
        for source in list(sources):
            attempts += 1
            try:
                case_call, args = next(source)
            except StopIteration:
                sources.remove(source)
                continue
            if case_call == call:
                fuzz.append(args)
    return (recorded + fuzz)[: max(PER_MODEL, len(recorded))]


@pytest.mark.parametrize("name", sorted(MODEL_CALLS))
def test_generated_payloads_agree_with_pybyd(name):
    group, call = MODEL_CALLS[name]
    cases = _cases(group, call)
    assert len(cases) >= min(PER_MODEL, 50), f"only {len(cases)} cases for {name}"
    problems = []
    for args in cases:
        expected = group.answer(call, args)
        if "error" not in expected and not _fits_i64(expected["ok"]):
            continue  # openbyd's integers are 64-bit; see parity/models_support.py
        got = _openbyd_answer(name, args["raw"], args.get("context"))
        if got.get("error") == "OverflowError" and "error" not in expected:
            continue  # a 64-bit limit openbyd reports instead of pyBYD's unbounded int
        if json.dumps(got, sort_keys=True) != json.dumps(expected, sort_keys=True):
            problems.append(f"raw={json.dumps(args, ensure_ascii=True)[:300]}\n  pyBYD:   {expected}\n  openbyd: {got}")
    assert not problems, f"{len(problems)} of {len(cases)} differ:\n" + "\n".join(problems[:5])


def _fits_i64(value: Any) -> bool:
    if isinstance(value, bool):
        return True
    if isinstance(value, int):
        return -(2**63) <= value < 2**63
    if isinstance(value, dict):
        return all(_fits_i64(item) for key, item in value.items() if key != "raw")
    if isinstance(value, list):
        return all(_fits_i64(item) for item in value)
    return True


def _corpus() -> list[tuple[str, int, dict[str, Any]]]:
    entries = []
    for path in sorted(CORPUS.glob("*.json")):
        for index, entry in enumerate(json.loads(path.read_text("utf-8"))):
            entries.append((path.stem, index, entry["payload"]))
    return entries


@pytest.mark.parametrize(("name", "index", "payload"), _corpus(), ids=lambda value: str(value)[:30])
def test_corpus_matches_model_dump_exactly(name, index, payload):
    model = _pybyd_class(name).model_validate(payload)
    expected = model.model_dump(mode="json")
    got = models.parse(name, payload)
    assert json.dumps(jsonable(got), sort_keys=False) == json.dumps(jsonable(expected), sort_keys=False)
    assert list(got) == list(expected)  # field order too
    properties = models.properties(name, payload)
    for key, value in properties.items():
        assert jsonable(value) == jsonable(getattr(model, key)), key


def test_non_finite_floats_come_back_as_floats_except_in_raw():
    raw = {"latitude": "nan", "longitude": "inf", "speed": "-inf", "direction": 1}
    expected = _pybyd_class("GpsInfo").model_validate(raw).model_dump(mode="json")
    got = models.parse("GpsInfo", raw)
    assert repr(got) == repr(expected)
    assert got["raw"] == raw


def test_errors():
    with pytest.raises(KeyError, match="no model named"):
        models.parse("NoSuchModel", {})
    with pytest.raises(KeyError):
        models.properties("NoSuchModel", {})
    with pytest.raises(openbyd.ValidationError) as caught:
        models.parse("GpsInfo", [])
    assert caught.value.error_type
    with pytest.raises(OverflowError):
        models.parse("GpsInfo", {"latitude": 2**70})
    with pytest.raises(ValueError):
        models.parse("GpsInfo", {"latitude": float("nan")})
    with pytest.raises(openbyd.ValidationError):
        models.parse("AuthToken", {"user_id": 1})


def test_context_is_passed():
    raw = json.loads((CORPUS / "VehicleRealtimeData.json").read_text("utf-8"))[0]["payload"]
    assert models.parse("VehicleRealtimeData", raw, None) == models.parse("VehicleRealtimeData", raw, {})


def test_parsing_releases_the_gil():
    raw = json.loads((CORPUS / "VehicleRealtimeData.json").read_text("utf-8"))[-1]["payload"]
    ticks = 0
    stop = False

    def count():
        nonlocal ticks
        while not stop:
            ticks += 1

    thread = threading.Thread(target=count)
    thread.start()
    try:
        end = time.monotonic() + 0.3
        while time.monotonic() < end:
            models.parse("VehicleRealtimeData", raw)
    finally:
        stop = True
        thread.join()
    assert ticks > 0


def test_parse_cost(capsys):
    raw = json.loads((CORPUS / "VehicleRealtimeData.json").read_text("utf-8"))[-1]["payload"]
    cls = _pybyd_class("VehicleRealtimeData")
    rounds = 500
    started = time.perf_counter()
    for _ in range(rounds):
        models.parse("VehicleRealtimeData", raw)
    ours = (time.perf_counter() - started) / rounds
    started = time.perf_counter()
    for _ in range(rounds):
        cls.model_validate(raw).model_dump(mode="json")
    theirs = (time.perf_counter() - started) / rounds
    with capsys.disabled():
        print(f"\nfull realtime payload: openbyd parse {ours * 1e6:.0f} us, pyBYD validate+dump {theirs * 1e6:.0f} us")
