"""Checks for the sample API payload corpus in parity/corpus/."""

from __future__ import annotations

import importlib
import json
import pkgutil
import re
from pathlib import Path
from typing import Any

import pytest

pytest.importorskip("pybyd")

import pybyd.models as pm  # noqa: E402

CORPUS_DIR = Path(__file__).resolve().parents[2] / "parity" / "corpus"
ENTRY_KEYS = {"source", "note", "payload"}
SYNTHETIC_VINS = {"TESTVIN0000000001", "TESTVIN0000000002"}
VIN_PATTERN = re.compile(r"^[A-HJ-NPR-Z0-9]{17}$")
SYNTHETIC_EMAIL = "user@example.com"


def _model_class(name: str) -> type:
    cls = getattr(pm, name, None)
    if cls is not None:
        return cls
    for info in pkgutil.iter_modules(pm.__path__):
        module = importlib.import_module(f"pybyd.models.{info.name}")
        cls = getattr(module, name, None)
        if cls is not None:
            return cls
    raise LookupError(f"no pybyd model class named {name!r}")


def _load(path: Path) -> list[dict[str, Any]]:
    return json.loads(path.read_text(encoding="utf-8"))


def _corpus_files() -> list[Path]:
    return sorted(CORPUS_DIR.glob("*.json"))


def _entries() -> list[tuple[str, dict[str, Any]]]:
    out: list[tuple[str, dict[str, Any]]] = []
    for path in _corpus_files():
        for i, entry in enumerate(_load(path)):
            out.append((f"{path.stem}[{i}]", entry))
    return out


def _string_values(node: Any) -> list[str]:
    if isinstance(node, str):
        return [node]
    if isinstance(node, dict):
        return [s for v in node.values() for s in _string_values(v)]
    if isinstance(node, list):
        return [s for v in node for s in _string_values(v)]
    return []


def test_corpus_files_exist() -> None:
    assert _corpus_files(), f"no corpus files found in {CORPUS_DIR}"


@pytest.mark.parametrize(("label", "entry"), _entries(), ids=[label for label, _ in _entries()])
def test_entry_has_expected_keys_and_validates(label: str, entry: dict[str, Any]) -> None:
    assert set(entry) == ENTRY_KEYS, f"{label}: keys are {sorted(entry)}"
    model = _model_class(Path(label.split("[")[0]).name)
    model.model_validate(entry["payload"])


def test_no_identifying_values_in_corpus() -> None:
    problems: list[str] = []
    for path in _corpus_files():
        for i, entry in enumerate(_load(path)):
            for value in _string_values(entry):
                if "@" in value and value != SYNTHETIC_EMAIL:
                    problems.append(f"{path.name}[{i}]: e-mail-like value {value!r}")
                if VIN_PATTERN.match(value) and not value.isdigit() and value not in SYNTHETIC_VINS:
                    problems.append(f"{path.name}[{i}]: VIN-like value {value!r}")
    assert problems == []


def test_realtime_corpus_has_full_payload() -> None:
    entries = _load(CORPUS_DIR / "VehicleRealtimeData.json")
    assert any(e["note"] == "every field present" for e in entries)
