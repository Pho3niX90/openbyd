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


@group.fuzz
def fuzz(rng):
    while True:
        session = random_session_kwargs(rng, valid=rng.random() < 0.7)
        session.setdefault("created_at", 0.0)
        now = rng.choice([0.0, 100.0, rng.uniform(0, 2e6)])
        yield rng.choice(CALLS), {"session": session, "now": now}
