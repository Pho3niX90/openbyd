"""openbyd answers every parity call the way pyBYD does.

Needs pyBYD at the commit in ``parity/UPSTREAM_COMMIT``. Without pyBYD the
whole module is skipped; the recorded vectors are still replayed by the Rust
tests.

Environment:
    OPENBYD_FUZZ          random cases per group (default 300)
    OPENBYD_PARITY_SEED   seed for the random cases (default "openbyd")
"""

from __future__ import annotations

import json
import os
import random

import pytest

pytest.importorskip("pybyd")

from parity.framework import Group, load_groups, openbyd_answer  # noqa: E402

GROUPS = load_groups()
FUZZ_COUNT = int(os.environ.get("OPENBYD_FUZZ", "300"))
SEED = os.environ.get("OPENBYD_PARITY_SEED", "openbyd")


def _describe(call: str, args: dict, expected: dict, got: dict) -> str:
    return (
        f"{call} differs from pyBYD\n"
        f"  args:    {json.dumps(args, ensure_ascii=True)}\n"
        f"  pyBYD:   {json.dumps(expected, ensure_ascii=True)}\n"
        f"  openbyd: {json.dumps(got, ensure_ascii=True)}"
    )


def _differences(group: Group, cases) -> list[str]:
    problems = []
    for call, args in cases:
        expected = group.answer(call, args)
        got = openbyd_answer(call, args)
        comparable = {key: value for key, value in got.items() if key != "message"}
        # Compare through JSON text so 1 and 1.0, or True and 1, are told apart.
        if json.dumps(expected, sort_keys=True) != json.dumps(comparable, sort_keys=True):
            problems.append(_describe(call, args, expected, got))
    return problems


def _report(problems: list[str], total: int) -> None:
    if problems:
        shown = "\n\n".join(problems[:10])
        pytest.fail(f"{len(problems)} of {total} cases differ.\n\n{shown}", pytrace=False)


@pytest.mark.parametrize("group", GROUPS.values(), ids=list(GROUPS))
def test_recorded_vectors_are_current(group: Group) -> None:
    """Fails when pyBYD's answers changed and the vector file needs regenerating."""
    if not group.recorded_cases():
        pytest.skip("group has no recorded cases")
    path = group.vector_path()
    assert path.exists(), f"{path.name} is missing. Run python parity/generate.py"
    assert path.read_text(encoding="utf-8") == group.vector_document(), (
        f"{path.name} is out of date. Run python parity/generate.py"
    )


@pytest.mark.parametrize("group", GROUPS.values(), ids=list(GROUPS))
def test_recorded_cases_agree(group: Group) -> None:
    cases = group.recorded_cases()
    if not cases:
        pytest.skip("group has no recorded cases")
    _report(_differences(group, cases), len(cases))


@pytest.mark.parametrize("group", GROUPS.values(), ids=list(GROUPS))
def test_random_cases_agree(group: Group) -> None:
    cases = list(group.fuzz_cases(random.Random(f"{SEED}:{group.name}"), FUZZ_COUNT))
    if not cases:
        pytest.skip("group has no fuzz cases")
    _report(_differences(group, cases), len(cases))


def test_every_oracle_has_a_case() -> None:
    """An oracle nobody calls is a ported function with no test."""
    unused = []
    for group in GROUPS.values():
        called = {call for call, _ in group.recorded_cases()}
        called |= {call for call, _ in group.fuzz_cases(random.Random(0), 2000)}
        unused += sorted(set(group.oracles) - called)
    assert not unused, f"oracles with no recorded or fuzz case: {unused}"


def test_unknown_calls_are_reported() -> None:
    assert openbyd_answer("nothing.here", {})["error"] == "UnknownCall"
