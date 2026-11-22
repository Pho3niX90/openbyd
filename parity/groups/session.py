"""pybyd/session.py"""

from __future__ import annotations

import time
from unittest import mock

from pybyd.session import Session

from parity.framework import Group
from parity.groups.config import SAMPLE_SESSION, finite, random_session_kwargs

group = Group("session")


def make_session(session: dict, now: float) -> Session:
    """``Session(**session)``. Its ``created_at`` default is bound to the real
    ``time.monotonic`` at import, so cases always give ``created_at``."""
    assert "created_at" in session, "cases must give created_at"
    return Session(**session)


@group.oracle("session.Session")
def session_dump(session: dict, now: float) -> dict:
    return finite(make_session(session, now).model_dump(mode="json"))


@group.oracle("session.content_key")
def content_key(session: dict, now: float) -> str:
    return make_session(session, now).content_key()


@group.oracle("session.sign_key")
def sign_key(session: dict, now: float) -> str:
    return make_session(session, now).sign_key()


@group.oracle("session.is_expired")
def is_expired(session: dict, now: float) -> bool:
    built = make_session(session, now)
    with mock.patch.object(time, "monotonic", lambda: now):
        return built.is_expired


@group.oracle("session.age")
def age(session: dict, now: float):
    built = make_session(session, now)
    with mock.patch.object(time, "monotonic", lambda: now):
        return finite(built.age)


@group.oracle("session.with_refreshed_tokens")
def with_refreshed_tokens(session: dict, now: float, guest_vins: dict, user_type_settled: list, **tokens) -> dict:
    built = make_session(session, now)
    built.guest_vins.update(guest_vins)
    built.user_type_settled.update((vin, endpoint) for vin, endpoint in user_type_settled)
    # The new Session's created_at default is the real time.monotonic bound
    # at import (see make_session); pyBYD means "now", so read it at now.
    refreshed = built.with_refreshed_tokens(**tokens).model_copy(update={"created_at": now}, deep=True)
    return {
        "dump": finite(refreshed.model_dump(mode="json")),
        "guest_vins": dict(sorted(refreshed.guest_vins.items())),
        "user_type_settled": [list(pair) for pair in sorted(refreshed.user_type_settled)],
    }


def refresh_case(rng, session: dict, now: float) -> dict:
    tokens = {"sign_token": rng.choice(["S2", " padded\t", "", "ü 🚗"]), "encry_token": rng.choice(["E2", "  ", "ü"])}
    if rng.random() < 0.6:
        tokens["refresh_token"] = rng.choice([None, "R2", " R3 ", ""])
    return {
        "session": session,
        "now": now,
        "guest_vins": rng.choice([{}, {"TESTVIN0000000001": True}, {"TESTVIN0000000001": False, "B": True}]),
        "user_type_settled": rng.choice([[], [["TESTVIN0000000001", "/control/getGpsInfo"]]]),
        **tokens,
    }


CALLS = ["session.Session", "session.content_key", "session.sign_key", "session.is_expired", "session.age"]


@group.recorded
def recorded():
    for call in CALLS:
        yield call, {"session": SAMPLE_SESSION, "now": 100.0}
        yield call, {"session": SAMPLE_SESSION, "now": 43300.0}
        yield call, {"session": {**SAMPLE_SESSION, "sign_token": " padded\t", "ttl": "60"}, "now": 159.5}
        yield call, {"session": {**SAMPLE_SESSION, "ttl": 0}, "now": 50.0}
        yield call, {"session": {**SAMPLE_SESSION, "ttl": "inf"}, "now": 1e9}
        yield call, {"session": {**SAMPLE_SESSION, "created_at": "nan"}, "now": 1.0}
        yield call, {"session": {**SAMPLE_SESSION, "user_id": 5}, "now": 1.0}
        yield call, {"session": {"user_id": "1", "created_at": 0.0}, "now": 1.0}
        yield call, {"session": {**SAMPLE_SESSION, "refresh_token": " REFRESH-synthetic "}, "now": 1.0}
        yield call, {"session": {**SAMPLE_SESSION, "refresh_token": 5}, "now": 1.0}
    import random

    rng = random.Random("session-recorded")
    for session in [SAMPLE_SESSION, {**SAMPLE_SESSION, "refresh_token": "REFRESH-synthetic", "ttl": 60.0}]:
        for _ in range(6):
            yield "session.with_refreshed_tokens", refresh_case(rng, session, rng.choice([100.0, 5e5]))


@group.fuzz
def fuzz(rng):
    while True:
        session = random_session_kwargs(rng, valid=rng.random() < 0.7)
        session.setdefault("created_at", 0.0)
        now = rng.choice([0.0, 100.0, rng.uniform(0, 2e6)])
        yield rng.choice(CALLS), {"session": session, "now": now}
        if "created_at" in session and rng.random() < 0.3:
            yield "session.with_refreshed_tokens", refresh_case(rng, session, now)
