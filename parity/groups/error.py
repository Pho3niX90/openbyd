"""pybyd/exceptions.py: the class hierarchy openbyd's ErrorKind mirrors."""

from __future__ import annotations

import json

import pydantic
from pybyd import exceptions

from parity.framework import Group

group = Group("error")

PYBYD_CLASSES = [
    name
    for name, value in vars(exceptions).items()
    if isinstance(value, type) and issubclass(value, exceptions.BydError)
]
BUILTIN_CLASSES = {
    "ValueError": ValueError,
    "JSONDecodeError": json.JSONDecodeError,
    "UnicodeDecodeError": UnicodeDecodeError,
    "ValidationError": pydantic.ValidationError,
    "TypeError": TypeError,
    "AttributeError": AttributeError,
}


def resolve(name: str) -> type:
    return BUILTIN_CLASSES.get(name) or getattr(exceptions, name)


@group.oracle("error.issubclass")
def is_subclass(child: str, parent: str) -> bool:
    return issubclass(resolve(child), resolve(parent))


@group.oracle("error.lineage")
def lineage(**args) -> list[str]:
    cls = getattr(exceptions, args["class"])
    return [base.__name__ for base in cls.__mro__ if base not in (Exception, BaseException, object)]


@group.recorded
def recorded():
    names = PYBYD_CLASSES + list(BUILTIN_CLASSES)
    for child in names:
        for parent in names:
            yield "error.issubclass", {"child": child, "parent": parent}
    for name in PYBYD_CLASSES:
        yield "error.lineage", {"class": name}
