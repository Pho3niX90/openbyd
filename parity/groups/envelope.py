"""pybyd/_api/login.py, _api/_envelope.py and the pure halves of _transport.py.

The golden cases run pyBYD's real ``SecureTransport`` and ``post_token_json``
against a fake HTTP session: the request body pyBYD would send and the value
it decodes from a reply built with pyBYD's own encoders must match openbyd's
byte for byte.
"""

from __future__ import annotations

import asyncio
import json
import math
import secrets
import time
from contextlib import contextmanager
from unittest import mock

from pybyd import exceptions
from pybyd._api import _common, _envelope, login
from pybyd._crypto.aes import aes_encrypt_hex
from pybyd._crypto.bangcle import BangcleCodec
from pybyd._crypto.hashing import pwd_login_key
from pybyd._transport import SecureTransport
from pybyd.config import BydConfig
from pybyd.session import Session

from parity.framework import Group, random_hex, random_text
from parity.groups.config import (
    SAMPLE_CONFIG,
    SAMPLE_SESSION,
    awkward_text,
    random_config_kwargs,
    random_session_kwargs,
)
from parity.groups.pyjson import random_json, random_object

group = Group("envelope")

CODEC = BangcleCodec()
_ERRORS = (exceptions.BydError, ValueError, AttributeError)


# ---------------------------------------------------------------------------
# Fakes and patches
# ---------------------------------------------------------------------------


class _Response:
    def __init__(self, status: int, text: str) -> None:
        self.status = status
        self._text = text

    async def __aenter__(self) -> _Response:
        return self

    async def __aexit__(self, *exc) -> bool:
        return False

    async def text(self) -> str:
        return self._text


class FakeHttp:
    """Stands in for ``aiohttp.ClientSession``: records the body, returns a fixed reply."""

    def __init__(self, status: int, text: str) -> None:
        self.status = status
        self.text = text
        self.sent: str | None = None
        self.headers: dict | None = None

    def post(self, url: str, data: str, headers: dict) -> _Response:
        self.sent = data
        self.headers = headers
        return _Response(self.status, self.text)


def transport(status: int = 200, text: str = "") -> tuple[SecureTransport, FakeHttp]:
    http = FakeHttp(status, text)
    return SecureTransport(BydConfig(username="u", password="p"), CODEC, http), http


def wall_clock(ms: int) -> float:
    """A ``time.time()`` value for which ``int(t * 1000) == ms``."""
    t = ms / 1000
    while int(t * 1000) < ms:
        t = math.nextafter(t, math.inf)
    while int(t * 1000) > ms:
        t = math.nextafter(t, -math.inf)
    # Past about 2**42 ms the float steps are wider than a millisecond.
    assert int(t * 1000) == ms, f"no time.time() value gives {ms} ms"
    return t


@contextmanager
def fixed(random: str = "0" * 32, service_time_ms: int = 0):
    with (
        mock.patch.object(secrets, "token_hex", lambda nbytes=None: random.lower()),
        mock.patch.object(time, "time", lambda: wall_clock(service_time_ms)),
    ):
        yield


def reply_text(outer_response) -> str:
    """The HTTP body BYD would send for ``outer_response``, built with pyBYD's codec."""
    return json.dumps({"response": CODEC.encode_envelope(json.dumps(outer_response))})


def login_reply(password: str, token, code="0", **extra) -> str:
    inner = json.dumps({"token": token, **extra}, ensure_ascii=False)
    return reply_text({"code": code, "message": "ok", "respondData": aes_encrypt_hex(inner, pwd_login_key(password))})


def token_reply(content_key: str, data, code="0", message="ok") -> str:
    respond = aes_encrypt_hex(json.dumps(data, ensure_ascii=False), content_key) if data is not None else ""
    return reply_text({"code": code, "message": message, "respondData": respond})


# ---------------------------------------------------------------------------
# Oracles
# ---------------------------------------------------------------------------


@group.oracle("envelope.build_login_request")
def build_login_request(config: dict, now_ms: int, random: str, service_time_ms: int) -> dict:
    with fixed(random, service_time_ms):
        return login.build_login_request(BydConfig(**config), now_ms)


@group.oracle("envelope.parse_login_response", errors=_ERRORS)
def parse_login_response(outer_response, password: str) -> dict:
    return login.parse_login_response(outer_response, password).model_dump(mode="json")


@group.oracle("envelope.build_token_outer_envelope")
def build_token_outer_envelope(config, session, now, inner, now_ms, user_type, service_time_ms) -> list:
    with fixed(service_time_ms=service_time_ms):
        outer, key = _envelope.build_token_outer_envelope(
            BydConfig(**config), Session(**session), inner, now_ms, user_type=user_type
        )
    return [outer, key]


@group.oracle("envelope.request_headers")
def request_headers() -> dict:
    secure, http = transport(200, reply_text({}))
    asyncio.run(secure.post_secure("/x", {}))
    return http.headers


@group.oracle("envelope.encode_request_body")
def encode_request_body(outer: dict) -> str:
    secure, http = transport(200, reply_text({}))
    asyncio.run(secure.post_secure("/x", outer))
    return http.sent


@group.oracle("envelope.decode_response_body", errors=_ERRORS)
def decode_response_body(endpoint: str, status: int, text: str):
    secure, _ = transport(status, text)
    return asyncio.run(secure.post_secure(endpoint, {}))


@group.oracle("envelope.golden_login", errors=_ERRORS)
def golden_login(config: dict, now_ms: int, random: str, service_time_ms: int, reply_text: str) -> dict:
    built = BydConfig(**config)
    secure, http = transport(200, reply_text)
    with fixed(random, service_time_ms):
        outer = login.build_login_request(built, now_ms)
    response = asyncio.run(secure.post_secure("/app/account/login", outer))
    token = login.parse_login_response(response, built.password)
    return {"body": http.sent, "token": token.model_dump(mode="json")}


@group.oracle("envelope.golden_token", errors=_ERRORS)
def golden_token(
    config, session, now, inner, now_ms, service_time_ms, user_type, endpoint, reply_text, vin, not_supported_codes
) -> dict:
    secure, http = transport(200, reply_text)
    with fixed(service_time_ms=service_time_ms):
        result = asyncio.run(
            _common.post_token_json(
                endpoint=endpoint,
                config=BydConfig(**config),
                session=Session(**session),
                transport=secure,
                inner=inner,
                now_ms=now_ms,
                vin=vin,
                not_supported_codes=None if not_supported_codes is None else frozenset(not_supported_codes),
                user_type=user_type,
            )
        )
    return {"body": http.sent, "result": result}


# ---------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------

NOW_MS = 1700000000123
SERVICE_MS = 1700000000456
RANDOM = "0123456789ABCDEF0123456789abcdef"
VEHICLE = {"vin": "TESTVIN0000000001", "vehicleName": 'My "Seal" 🚗 \\ ü', "soc": 80.5, "doors": [0, 1]}
TOKEN = {"userId": 1234567, "signToken": "SIGNTOKEN-synthetic", "encryToken": "ENCRYTOKEN-synthetic", "extra": "ü"}


def sample_inner(vin: str | None = "TESTVIN0000000001") -> dict:
    with fixed(RANDOM):
        inner = _common.build_inner_base(BydConfig(**SAMPLE_CONFIG), now_ms=NOW_MS, vin=vin)
    inner["vehicleName"] = 'My "Seal" 🚗'
    inner["note"] = "back\\slash / ctl\x01 ü"
    return inner


def golden_token_case(**changes) -> dict:
    case = {
        "config": SAMPLE_CONFIG,
        "session": SAMPLE_SESSION,
        "now": 0.0,
        "inner": sample_inner(),
        "now_ms": NOW_MS,
        "service_time_ms": SERVICE_MS,
        "user_type": None,
        "endpoint": "/vehicleInfo/vehicle/vehicleRealTimeRequest",
        "reply_text": token_reply(Session(**SAMPLE_SESSION).content_key(), VEHICLE),
        "vin": "TESTVIN0000000001",
        "not_supported_codes": None,
    }
    case.update(changes)
    return case


def login_responses(rng, password: str) -> list:
    key = pwd_login_key(password)

    def sealed(value) -> str:
        return aes_encrypt_hex(value if isinstance(value, str) else json.dumps(value), key)

    return [
        {"code": "0", "respondData": sealed({"token": TOKEN})},
        {"code": 0, "respondData": sealed({"token": TOKEN})},
        {"code": 0.0, "respondData": sealed({"token": TOKEN})},
        {"code": "1005", "message": "expired"},
        {"code": None},
        {},
        {"code": "0"},
        {"code": "0", "respondData": ""},
        {"code": "0", "respondData": 5},
        {"code": "0", "respondData": [1]},
        {"code": "0", "respondData": "ZZ"},
        {"code": "0", "respondData": sealed("not json")},
        {"code": "0", "respondData": sealed([1, 2])},
        {"code": "0", "respondData": sealed({"token": None})},
        {"code": "0", "respondData": sealed({"token": "abc"})},
        {"code": "0", "respondData": sealed({"token": [1]})},
        {"code": "0", "respondData": sealed({"token": {**TOKEN, "userId": 0}})},
        {"code": "0", "respondData": sealed({"token": {**TOKEN, "signToken": ""}})},
        {"code": "0", "respondData": sealed({"token": {**TOKEN, "userId": True, "signToken": 1.5}})},
        {"code": "0", "respondData": sealed({"token": {**TOKEN, "userId": [1], "encryToken": {"a": None}}})},
        {"code": "0", "respondData": sealed({"token": {key_: random_json(rng) for key_ in TOKEN}})},
        [],
    ]


def body_texts(rng) -> list[tuple[int, str]]:
    data = random_json(rng)
    return [
        (200, reply_text(data)),
        (200, reply_text({"code": "0", "x": "ü 🚗"})),
        (200, json.dumps({"response": CODEC.encode_envelope("F" + json.dumps({"a": 1}))})),
        (200, json.dumps({"response": CODEC.encode_envelope("F[1]")})),
        (200, json.dumps({"response": CODEC.encode_envelope(" \n" + json.dumps(data) + "\t")})),
        (200, json.dumps({"response": CODEC.encode_envelope("Fx")})),
        (200, json.dumps({"response": CODEC.encode_envelope(b"\xff\xfe")})),
        (200, json.dumps({"response": CODEC.encode_envelope("")})),
        (200, json.dumps({"response": " "})),
        (200, json.dumps({"response": 5})),
        (200, json.dumps({"response": "Gabc"})),
        (200, json.dumps({"response": "F" + random_text(rng, 10)})),
        (200, json.dumps({"other": 1})),
        (200, "[]"),
        (200, "not json"),
        (200, "\ufeff{}"),
        (500, "Server error " * 30),
        (404, ""),
        (200, json.dumps({"response": CODEC.encode_envelope(json.dumps(data))[:-3]})),
    ]


@group.recorded
def recorded():
    import random

    rng = random.Random("envelope-recorded")
    yield "envelope.build_login_request", {
        "config": SAMPLE_CONFIG, "now_ms": NOW_MS, "random": RANDOM, "service_time_ms": SERVICE_MS,
    }
    yield "envelope.build_login_request", {
        "config": {"username": "user@example.com", "password": "secret"}, "now_ms": 0, "random": "F" * 32,
        "service_time_ms": 1,
    }
    for response in login_responses(rng, "secret"):
        yield "envelope.parse_login_response", {"outer_response": response, "password": "secret"}
    yield "envelope.parse_login_response", {"outer_response": login_responses(rng, "secret")[0], "password": "other"}
    for user_type in [None, "1", ""]:
        yield "envelope.build_token_outer_envelope", {
            "config": SAMPLE_CONFIG, "session": SAMPLE_SESSION, "now": 0.0, "inner": sample_inner(),
            "now_ms": NOW_MS, "user_type": user_type, "service_time_ms": SERVICE_MS,
        }
    yield "envelope.request_headers", {}
    yield "envelope.encode_request_body", {"outer": {"a": 'My "Seal" 🚗', "b": [1, 1.5, None]}}
    for status, text in body_texts(rng):
        yield "envelope.decode_response_body", {"endpoint": "/app/x", "status": status, "text": text}

    # Golden end-to-end vectors.
    password = SAMPLE_CONFIG["password"]
    login_case = {"config": SAMPLE_CONFIG, "now_ms": NOW_MS, "random": RANDOM, "service_time_ms": SERVICE_MS}
    yield "envelope.golden_login", {**login_case, "reply_text": login_reply(password, TOKEN)}
    yield "envelope.golden_login", {**login_case, "reply_text": login_reply(password, TOKEN, code="1005")}
    yield "envelope.golden_login", {**login_case, "reply_text": login_reply("wrong", TOKEN)}
    key = Session(**SAMPLE_SESSION).content_key()
    yield "envelope.golden_token", golden_token_case()
    yield "envelope.golden_token", golden_token_case(user_type="1")
    yield "envelope.golden_token", golden_token_case(inner=sample_inner(None), vin=None)
    yield "envelope.golden_token", golden_token_case(reply_text=token_reply(key, [VEHICLE]))
    yield "envelope.golden_token", golden_token_case(reply_text=token_reply(key, None))
    for code in ["1001", "1005", "1008", "6002", "1007", 1001]:
        for not_supported in [None, ["1001"]]:
            yield "envelope.golden_token", golden_token_case(
                reply_text=token_reply(key, None, code=code, message="Fehler ü"), not_supported_codes=not_supported
            )
    yield "envelope.golden_token", golden_token_case(reply_text=reply_text([1, 2]))
    yield "envelope.golden_token", golden_token_case(reply_text=reply_text({"respondData": "00"}))


@group.fuzz
def fuzz(rng):
    while True:
        config = random_config_kwargs(rng, valid=True)
        session = random_session_kwargs(rng)
        password = BydConfig(**config).password
        yield "envelope.build_login_request", {
            "config": config,
            "now_ms": rng.randint(0, 2**53),
            "random": random_hex(rng, 16, upper=rng.random() < 0.5),
            "service_time_ms": rng.randint(0, 2**42),
        }
        yield "envelope.parse_login_response", {
            "outer_response": rng.choice(login_responses(rng, password)),
            "password": password,
        }
        inner = random_object(rng) if rng.random() < 0.5 else {random_text(rng, 8): awkward_text(rng) for _ in range(5)}
        envelope_args = {
            "config": config,
            "session": session,
            "now": 0.0,
            "inner": inner,
            "now_ms": rng.randint(0, 2**53),
            "user_type": rng.choice([None, "1", random_text(rng, 3)]),
            "service_time_ms": rng.randint(0, 2**42),
        }
        yield "envelope.build_token_outer_envelope", envelope_args
        yield "envelope.encode_request_body", {"outer": random_object(rng)}
        status, text = rng.choice(body_texts(rng))
        yield "envelope.decode_response_body", {"endpoint": random_text(rng, 20), "status": status, "text": text}
        key = Session(**session).content_key()
        reply = rng.choice(
            [
                token_reply(key, random_json(rng)),
                token_reply(key, random_object(rng)),
                token_reply(key, None, code=rng.choice(["1001", "1002", "1008", "6002", "9"]), message=random_text(rng)),
                token_reply(random_hex(rng, 16), {"a": 1}),
            ]
        )
        yield "envelope.golden_token", {
            **envelope_args,
            "endpoint": random_text(rng, 20),
            "reply_text": reply,
            "vin": rng.choice([None, "", "TESTVIN0000000001"]),
            "not_supported_codes": rng.choice([None, ["1001"], []]),
        }
        token = {**TOKEN, "userId": rng.choice([TOKEN["userId"], random_text(rng, 8)])}
        yield "envelope.golden_login", {
            "config": config,
            "now_ms": rng.randint(0, 2**53),
            "random": random_hex(rng, 16),
            "service_time_ms": rng.randint(0, 2**42),
            "reply_text": login_reply(password, token, code=rng.choice(["0", "0", "1005"])),
        }
