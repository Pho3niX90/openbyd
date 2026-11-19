"""openbyd.Client and openbyd.Car have pyBYD's methods with pyBYD's arguments,
and the stub file describes the extension's classes."""

from __future__ import annotations

import ast
import inspect
from pathlib import Path

import pytest

import openbyd
from openbyd import _native

pybyd = pytest.importorskip("pybyd")

STUB = Path(__file__).resolve().parents[1] / "openbyd" / "_native.pyi"


def public_members(cls: type) -> dict[str, object]:
    return {name: value for name, value in vars(cls).items() if not name.startswith("_")}


def parameters(function: object) -> list[tuple[str, inspect._ParameterKind, object]]:
    signature = inspect.signature(function)  # type: ignore[arg-type]
    return [
        (name, parameter.kind, parameter.default)
        for name, parameter in signature.parameters.items()
        if name != "self" and parameter.kind is not inspect.Parameter.VAR_KEYWORD
    ]


@pytest.mark.parametrize(("theirs", "ours"), [(pybyd.BydClient, openbyd.Client), (pybyd.BydCar, openbyd.Car)])
def test_every_pybyd_method_is_here_with_its_arguments(theirs: type, ours: type) -> None:
    for name, member in public_members(theirs).items():
        assert hasattr(ours, name), f"{ours.__name__} lacks {name}"
        mine = getattr(ours, name)
        if isinstance(member, property):
            assert isinstance(inspect.getattr_static(ours, name), property), f"{name} is a property in pyBYD"
            continue
        assert inspect.iscoroutinefunction(member) == inspect.iscoroutinefunction(mine), f"{name}: async differs"
        expected = parameters(member)
        got = parameters(mine)
        if name in ("start_climate", "schedule_climate", "set_seat_climate", "set_battery_heat"):
            # params is also accepted as keyword arguments, so it has a default here.
            expected = [(n, k, None if n == "params" else d) for n, k, d in expected]
        if name == "get_notifications":
            # NotificationCategory.VEHICLE is the int 1.
            expected = [(n, k, int(d)) for n, k, d in expected]
        assert got == expected, f"{ours.__name__}.{name}: {got} != pyBYD's {expected}"


def test_pybyd_capability_namespaces_are_here() -> None:
    car_methods = {
        "lock": ("available", "lock", "unlock"),
        "hvac": ("available", "temp_offset", "temp_range", "start", "stop", "schedule"),
        "seat": ("heat", "ventilation"),
        "steering": ("available", "heat"),
        "battery": ("available", "heat"),
        "finder": ("find_available", "flash_available", "find", "flash_lights"),
        "windows": ("close_available", "open_available", "close", "open"),
        "trunk": ("open_available", "close_available", "open", "close"),
    }
    features = {
        "lock": openbyd.car.LockFeature,
        "hvac": openbyd.car.HvacFeature,
        "seat": openbyd.car.SeatFeature,
        "steering": openbyd.car.SteeringFeature,
        "battery": openbyd.car.BatteryFeature,
        "finder": openbyd.car.FinderFeature,
        "windows": openbyd.car.WindowsFeature,
        "trunk": openbyd.car.TrunkFeature,
    }
    for name, members in car_methods.items():
        for member in members:
            assert hasattr(features[name], member), f"car.{name}.{member}"
    assert [member.value for member in openbyd.SeatPosition] == [member.value for member in pybyd.SeatPosition]
    assert [member.value for member in openbyd.SeatLevel] == [member.value for member in pybyd.SeatLevel]
    start = inspect.signature(openbyd.car.HvacFeature.start).parameters
    theirs = inspect.signature(pybyd._capabilities.hvac.HvacCapability.start).parameters
    assert [(n, p.default) for n, p in start.items()] == [(n, p.default) for n, p in theirs.items()]


def stub_classes() -> dict[str, set[str]]:
    tree = ast.parse(STUB.read_text(encoding="utf-8"))
    return {
        node.name: {item.name for item in node.body if isinstance(item, ast.FunctionDef)}
        for node in tree.body
        if isinstance(node, ast.ClassDef)
    }


@pytest.mark.parametrize("name", ["Client", "Car"])
def test_the_stub_describes_the_extension(name: str) -> None:
    stubbed = stub_classes()[name]
    actual = {member for member in dir(getattr(_native, name)) if not member.startswith("_")}
    assert actual - stubbed == set(), f"{name}: not in _native.pyi"
    assert stubbed - actual - {"__init__"} == set(), f"{name}: in _native.pyi but not in the extension"


def test_exports() -> None:
    for name in ("Client", "Car", "SeatLevel", "SeatPosition"):
        assert name in openbyd.__all__ and hasattr(openbyd, name)
    assert openbyd.Client.__doc__ and openbyd.Car.__doc__
    for cls in (openbyd.Client, openbyd.Car):
        for name, member in public_members(cls).items():
            target = member.fget if isinstance(member, property) else member
            assert getattr(target, "__doc__", None), f"{cls.__name__}.{name} has no docstring"
