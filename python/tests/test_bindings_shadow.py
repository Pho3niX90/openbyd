"""openbyd.shadow.compare: names only, never raises, cheap."""

from __future__ import annotations

import json
import random
import time
from pathlib import Path
from typing import Any

import pytest

pytest.importorskip("pybyd")

from pybyd.models import GpsInfo, VehicleRealtimeData  # noqa: E402
from pydantic import BaseModel  # noqa: E402

import openbyd  # noqa: E402
from openbyd import shadow  # noqa: E402
from parity.models_support import random_json  # noqa: E402

CORPUS = Path(__file__).resolve().parents[2] / "parity" / "corpus"
REALTIME = json.loads((CORPUS / "VehicleRealtimeData.json").read_text("utf-8"))[-1]["payload"]


def _corpus_models():
    import pybyd.models as pm

    for path in sorted(CORPUS.glob("*.json")):
        cls = getattr(pm, path.stem, None)
        if cls is None:
            for module in ("message_center", "latest_config", "energy", "smart_charging", "booking"):
                cls = getattr(__import__(f"pybyd.models.{module}", fromlist=["x"]), path.stem, None) or cls
        for entry in json.loads(path.read_text("utf-8")):
            yield cls.model_validate(entry["payload"])


def test_corpus_is_identical():
    checked = 0
    for model in _corpus_models():
        assert shadow.compare(model) == [], type(model).__name__
        checked += 1
    assert checked >= 14


def test_reports_the_names_of_changed_fields_only():
    model = VehicleRealtimeData.model_validate(REALTIME)
    secret = 123.456789
    changed = model.model_copy(update={"elec_percent": secret, "speed": secret})
    result = shadow.compare(changed)
    assert result == ["elec_percent", "speed"]
    assert all(isinstance(name, str) for name in result)
    assert str(secret) not in repr(result)


def _dump_without(model: Any, *names: str) -> _Evil:
    """``model`` as a pyBYD model of the same class whose dump lacks ``names``."""
    dump = {key: value for key, value in model.model_dump(mode="json").items() if key not in names}
    return _named(type(model).__name__, raw=model.raw, dump=dump)


def test_a_field_only_openbyd_has_is_not_reported():
    # An older pyBYD (0.0.85) has no charge_power fields: not a difference.
    model = VehicleRealtimeData.model_validate(REALTIME)
    older = _dump_without(model, "charge_power", "charge_power_unit")
    assert shadow.compare(older) == []


def test_a_field_only_pybyd_has_is_reported():
    model = VehicleRealtimeData.model_validate(REALTIME)
    dump = {**model.model_dump(mode="json"), "brand_new_field": 1}
    assert shadow.compare(_named("VehicleRealtimeData", raw=model.raw, dump=dump)) == ["brand_new_field"]


def test_a_field_both_have_with_another_value_is_reported():
    model = VehicleRealtimeData.model_validate(REALTIME)
    dump = model.model_dump(mode="json")
    dump["speed"] = None if dump["speed"] is not None else 1.0
    dump["elec_percent"] = int(dump["elec_percent"]) if isinstance(dump["elec_percent"], float) else 1.5
    assert shadow.compare(_named("VehicleRealtimeData", raw=model.raw, dump=dump)) == ["elec_percent", "speed"]


def test_markers():
    class Unknown(BaseModel):
        raw: dict = {}

    assert shadow.compare(Unknown()) == [shadow.UNKNOWN_MODEL]
    assert shadow.compare(None) == [shadow.UNKNOWN_MODEL]
    # pyBYD accepts an integer openbyd cannot hold: openbyd fails, pyBYD did not.
    big = GpsInfo.model_validate({"latitude": 52.0, "longitude": 4.0, "extra": 2**80})
    assert shadow.compare(big) == [shadow.ERROR]
    assert shadow.ERROR == "<error>" and shadow.UNKNOWN_MODEL == "<unknown model>"


def test_context_is_used():
    model = GpsInfo.model_validate({"latitude": 52.0}, context={"x": 1})
    assert shadow.compare(model, context={"x": 1}) == []
    assert shadow.compare(model, context=object()) == [shadow.ERROR]


class _Evil:
    """Looks like a model and fails in every way it can."""

    def __init__(self, raw: Any = None, dump: Any = None, raise_on: str = "") -> None:
        self._raw, self._dump, self._raise_on = raw, dump, raise_on

    @property
    def raw(self) -> Any:
        if self._raise_on == "raw":
            raise RuntimeError("raw")
        return self._raw

    def model_dump(self, mode: str = "python") -> Any:
        if self._raise_on == "dump":
            raise MemoryError("dump")
        return self._dump


def _named(name: str, **kwargs: Any) -> Any:
    return type(name, (_Evil,), {})(**kwargs)


def test_never_raises_whatever_it_is_given():
    rng = random.Random("shadow")
    cyclic: list = []
    cyclic.append(cyclic)
    odd_values = [None, 1, "x", [], {}, cyclic, {1: 2}, {"a": float("nan")}, 2**200, object(), b"x", REALTIME]
    subjects: list[Any] = [None, 0, "GpsInfo", object(), GpsInfo, BaseModel, type, Exception()]
    for name in ["GpsInfo", "VehicleRealtimeData", "AuthToken", "MqttEvent", "Nope"]:
        for raise_on in ["", "raw", "dump"]:
            for raw in odd_values:
                subjects.append(_named(name, raw=raw, dump=rng.choice(odd_values), raise_on=raise_on))
    for _ in range(500):
        subjects.append(_named(rng.choice(["GpsInfo", "VehicleRealtimeData", "ChargingStatus"]),
                               raw=random_json(rng), dump=random_json(rng)))
    for subject in subjects:
        for context in (None, {}, cyclic, object()):
            result = shadow.compare(subject, context)
            assert isinstance(result, list) and all(isinstance(item, str) for item in result)


def test_unconvertible_expected_still_compares():
    model = _named("GpsInfo", raw={"latitude": 52.0}, dump={"latitude": float("nan"), "raw": {"latitude": 52.0}})
    result = shadow.compare(model)
    assert "latitude" in result


def test_cheap_on_a_full_realtime_payload(capsys):
    model = VehicleRealtimeData.model_validate(REALTIME)
    rounds = 300
    shadow.compare(model)
    started = time.perf_counter()
    for _ in range(rounds):
        shadow.compare(model)
    cost = (time.perf_counter() - started) / rounds
    started = time.perf_counter()
    for _ in range(rounds):
        VehicleRealtimeData.model_validate(REALTIME)
    pybyd_cost = (time.perf_counter() - started) / rounds
    with capsys.disabled():
        print(f"\nshadow.compare on a full realtime payload: {cost * 1e6:.0f} us (pyBYD's own validate: {pybyd_cost * 1e6:.0f} us)")
    assert cost < 0.005  # generous; it is about a tenth of a millisecond


def test_exported():
    assert openbyd.shadow is shadow
