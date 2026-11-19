"""pybyd/models/latest_config.py: getLatestConfig nodes and the capability flags built from them."""

from __future__ import annotations

import dataclasses
import random
from collections.abc import Iterator
from typing import Any

from pybyd.models.latest_config import (
    FEATURE_REGISTRY,
    LatestConfigFunction,
    VehicleCapabilities,
    VehicleLatestConfig,
    _normalize_code,
    registered_latest_config_function_nos,
)

from parity.framework import Case, Group, random_text
from parity.groups.models_command_gating import ATTO3_EVO, HAN_REAR_SEATS, OVERSEAS
from parity.models_support import SENTINELS, json_safe, jsonable, register_model

group = Group("models_latest_config")
VIN = "TESTVIN0000000001"
KNOWN = sorted(registered_latest_config_function_nos())

NODE = {"code": "Electric A/C", "functionName": "电动空调", "functionNo": "1015", "sortNum": 15}
NESTED = {"code": "door/window", "functionName": "门窗", "functionNo": "1002", "sortNum": 2,
          "cfFixedSecondLevelList": [{"code": "Sunroof", "functionNo": "10020001", "sortNum": 1},
                                     {"code": "Trunk", "functionNo": "10020003",
                                      "cfFixedSecondLevelList": [{"code": "deep", "functionNo": "99999999"}]}]}


def latest(function_nos: list[str], **extra: Any) -> dict[str, Any]:
    """A latest-config reply listing these functionNos (as the upstream tests build it)."""
    return {"cfFixedList": [{"functionNo": function_no} for function_no in function_nos], **extra}


LATEST_SAMPLES = [
    {"widgetConfigId": "W1", "configVersion": 3, "appConfigVersion": "4", "styleId": 1, "terminalType": 2,
     "cfFixedList": [NODE, NESTED]},
    latest(OVERSEAS),
    latest(ATTO3_EVO),
    latest(HAN_REAR_SEATS),
    latest([*OVERSEAS, "99999999"]),
    latest([]),
    {"cfFixedList": [NODE]},
]

FUNCTION_CASES = register_model(group, "models.latest_config.LatestConfigFunction", LatestConfigFunction,
                                samples=[NODE, NESTED, {"functionNo": "1005"}, {"code": "--", "sortNum": "--"}])
LATEST_CASES = register_model(group, "models.latest_config.VehicleLatestConfig", VehicleLatestConfig,
                              samples=LATEST_SAMPLES)
CAPS_CASES = register_model(
    group,
    "models.latest_config.VehicleCapabilities",
    VehicleCapabilities,
    samples=[
        {"vin": VIN, "source": "test_fixture", "functionNos": ["1026"], "openWindows": True, "closeWindows": True},
        {"vin": VIN, "source": "unknown:unavailable"},
        {"vin": VIN, "lock": True, "unlock": False, "nfcKey3c": True, "nfc_key_3c": False, "functionNos": ["1005"],
         "codes": ["LOCK"], "unknownFunctionNos": ["99999999"]},
    ],
)


@group.oracle("models.latest_config.registered_latest_config_function_nos")
def registered() -> list[str]:
    return sorted(registered_latest_config_function_nos())


@group.oracle("models.latest_config.FEATURE_REGISTRY")
def feature_registry() -> list[dict[str, Any]]:
    return [jsonable(dataclasses.asdict(spec)) for spec in FEATURE_REGISTRY]


@group.oracle("models.latest_config.normalize_code")
def normalize_code(code: str) -> str:
    return _normalize_code(code)


@group.oracle("models.latest_config.LatestConfigFunction.iter_flat")
def iter_flat(raw: Any, context: Any = None) -> list[Any]:
    node = LatestConfigFunction.model_validate(raw, context=context)
    return [jsonable(item.model_dump(mode="json")) for item in node.iter_flat()]


@group.oracle("models.latest_config.VehicleLatestConfig.iter_functions")
def iter_functions(raw: Any, context: Any = None) -> list[Any]:
    config = VehicleLatestConfig.model_validate(raw, context=context)
    return [jsonable(item.model_dump(mode="json")) for item in config.iter_functions()]


def _report(capabilities: VehicleCapabilities) -> dict[str, Any]:
    return {"feature_report": jsonable(capabilities.feature_report()),
            "supported_summary": list(capabilities.supported_summary())}


@group.oracle("models.latest_config.VehicleCapabilities.from_latest_config")
def from_latest_config(vin: str, latest: Any) -> dict[str, Any]:
    capabilities = VehicleCapabilities.from_latest_config(vin, VehicleLatestConfig.model_validate(latest))
    return {"dump": jsonable(capabilities.model_dump(mode="json")), **_report(capabilities)}


@group.oracle("models.latest_config.VehicleCapabilities.unknown")
def unknown(vin: str, reason: str) -> Any:
    return jsonable(VehicleCapabilities.unknown(vin, reason=reason).model_dump(mode="json"))


@group.oracle("models.latest_config.VehicleCapabilities.feature_report")
def feature_report(raw: Any, context: Any = None) -> dict[str, Any]:
    return _report(VehicleCapabilities.model_validate(raw, context=context))


CODES = ["Electric A/C", "lock-door", "ß", "straße", "ﬁ", "ı", "ſ", "ǰ", "Ａ", "１", "١", "a_b c.d", "", "--", "  ",
         "比亚迪", "🚗x", "ŉ", "ﬃ9", "ͅ", "ᾳ"]
VINS = [VIN, "", " ", "--", "nan", "NaN", " -- ", "VIN_PLACEHOLDER", "比亚迪"]


@group.recorded
def recorded() -> Iterator[Case]:
    yield "models.latest_config.registered_latest_config_function_nos", {}
    yield "models.latest_config.FEATURE_REGISTRY", {}
    for code in CODES:
        yield "models.latest_config.normalize_code", {"code": code}
    for raw in [NODE, NESTED, {}, {"cfFixedSecondLevelList": [NESTED, NODE]}, {"cfFixedSecondLevelList": [5]}, 5]:
        yield "models.latest_config.LatestConfigFunction.iter_flat", {"raw": raw, "context": None}
    for raw in [*LATEST_SAMPLES, {}, {"cfFixedList": [NESTED, NESTED]}, {"cfFixedList": "x"}, None]:
        yield "models.latest_config.VehicleLatestConfig.iter_functions", {"raw": raw, "context": None}
    # Upstream tests/test_latest_config.py and the capability halves of test_battery_heat.py,
    # test_rear_seat_climate.py: real-shaped dumps, single functionNos, unknown values.
    for vin in VINS:
        yield "models.latest_config.VehicleCapabilities.from_latest_config", {"vin": vin, "latest": LATEST_SAMPLES[0]}
    for sample in LATEST_SAMPLES[1:]:
        yield "models.latest_config.VehicleCapabilities.from_latest_config", {"vin": "VIN_PLACEHOLDER",
                                                                               "latest": sample}
    for function_no in KNOWN + ["99999999", "", " 1005", "1005.0"]:
        yield "models.latest_config.VehicleCapabilities.from_latest_config", {"vin": VIN,
                                                                               "latest": latest([function_no])}
    for code_list in [CODES, ["", "x"], ["--"]]:
        yield "models.latest_config.VehicleCapabilities.from_latest_config", {
            "vin": VIN, "latest": {"cfFixedList": [{"code": code, "functionNo": "1005"} for code in code_list]}}
    yield "models.latest_config.VehicleCapabilities.from_latest_config", {
        "vin": VIN, "latest": {"cfFixedList": [NESTED, NODE, NESTED], "raw": {"a": 1}}}
    yield "models.latest_config.VehicleCapabilities.from_latest_config", {"vin": VIN, "latest": {"cfFixedList": 5}}
    for vin in VINS:
        for reason in ["unavailable", "", "--", "timeout", "比亚迪"]:
            yield "models.latest_config.VehicleCapabilities.unknown", {"vin": vin, "reason": reason}
    for raw in [{}, {"lock": True, "unlock": False}, {key: True for key in [spec.key for spec in FEATURE_REGISTRY]},
                {"lock": "yes", "unlock": 0, "carOn": "--"}, {"lock": 2}]:
        yield "models.latest_config.VehicleCapabilities.feature_report", {"raw": raw, "context": None}


def _random_latest(rng: random.Random) -> Any:
    payload = LATEST_CASES.fuzz(rng)
    if rng.random() < 0.6 and isinstance(payload, dict):
        nodes = []
        for _ in range(rng.randint(0, 12)):
            node: dict[str, Any] = {"functionNo": rng.choice(KNOWN + ["99999999", "", random_text(rng, 6)])}
            if rng.random() < 0.4:
                node["code"] = rng.choice(CODES + [random_text(rng, 10)])
            if rng.random() < 0.15:
                node["cfFixedSecondLevelList"] = [{"functionNo": rng.choice(KNOWN)} for _ in range(rng.randint(0, 3))]
            nodes.append(node)
        payload["cfFixedList"] = nodes
    return payload


@group.fuzz
def fuzz(rng: random.Random) -> Iterator[Case]:
    while True:
        roll = rng.random()
        if roll < 0.4:
            args = {"vin": rng.choice(VINS + [random_text(rng, 17)]), "latest": _random_latest(rng)}
            call = "models.latest_config.VehicleCapabilities.from_latest_config"
        elif roll < 0.55:
            call, args = "models.latest_config.normalize_code", {"code": random_text(rng, 20)}
        elif roll < 0.65:
            call = "models.latest_config.VehicleCapabilities.unknown"
            args = {"vin": rng.choice(VINS + [random_text(rng, 17)]), "reason": rng.choice([random_text(rng, 8), *SENTINELS[:4]])}
        elif roll < 0.8:
            call, args = "models.latest_config.VehicleCapabilities.feature_report", {"raw": CAPS_CASES.fuzz(rng),
                                                                                     "context": None}
        elif roll < 0.9:
            call, args = "models.latest_config.LatestConfigFunction.iter_flat", {"raw": FUNCTION_CASES.fuzz(rng),
                                                                                 "context": None}
        else:
            call, args = "models.latest_config.VehicleLatestConfig.iter_functions", {"raw": _random_latest(rng),
                                                                                    "context": None}
        if json_safe(args) and (call != "models.latest_config.VehicleCapabilities.unknown" or
                                isinstance(args["reason"], str)):
            yield call, args


