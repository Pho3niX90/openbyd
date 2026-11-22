"""pybyd/_api/login.py, _api/_envelope.py and the pure halves of _transport.py.

The golden cases run pyBYD's real ``SecureTransport`` and ``post_token_json``
against a fake HTTP session: the request body pyBYD would send and the value
it decodes from a reply built with pyBYD's own encoders must match openbyd's
byte for byte.
"""

from __future__ import annotations

import asyncio
import datetime
import email.utils
import json
import math
import secrets
import time
from contextlib import contextmanager
from unittest import mock

from pybyd import _transport, exceptions
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
    def __init__(self, status: int, text: str, headers: dict | None = None) -> None:
        self.status = status
        self._text = text
        # aiohttp's CIMultiDictProxy; pyBYD reads ``resp.headers.get("Date")``.
        self.headers = dict(headers or {})

    async def __aenter__(self) -> _Response:
        return self

    async def __aexit__(self, *exc) -> bool:
        return False

    async def text(self) -> str:
        return self._text


class FakeHttp:
    """Stands in for ``aiohttp.ClientSession``: records the body, returns a fixed reply."""

    def __init__(self, status: int, text: str, reply_headers: dict | None = None) -> None:
        self.status = status
        self.text = text
        self.reply_headers = reply_headers
        self.sent: str | None = None
        self.headers: dict | None = None

    def post(self, url: str, data: str, headers: dict) -> _Response:
        self.sent = data
        self.headers = headers
        return _Response(self.status, self.text, self.reply_headers)


def transport(
    status: int = 200, text: str = "", reply_headers: dict | None = None
) -> tuple[SecureTransport, FakeHttp]:
    http = FakeHttp(status, text, reply_headers)
    return SecureTransport(BydConfig(username="u", password="p"), CODEC, http), http


def utc_parsedate_to_datetime(data):
    """``email.utils.parsedate_to_datetime`` on a host whose local zone is UTC.

    A date without a zone is a naive ``datetime``, which ``timestamp()`` reads
    in the host's zone. openbyd reads no environment and takes it as UTC, so
    the oracle pins the host's zone the way it pins the clock."""
    parsed = email.utils.parsedate_to_datetime(data)
    return parsed.replace(tzinfo=datetime.UTC) if parsed.tzinfo is None else parsed


@contextmanager
def utc_host():
    with mock.patch.object(_transport, "parsedate_to_datetime", utc_parsedate_to_datetime):
        yield


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


@group.oracle("envelope.sync_clock_from_header", errors=(*_ERRORS, OverflowError))
def sync_clock_from_header(date_header, now_ms: int, offset_ms: int) -> int:
    secure, _ = transport()
    secure.server_time_offset_ms = offset_ms
    with fixed(service_time_ms=now_ms), utc_host():
        secure._sync_clock_from_header(date_header)
    return secure.server_time_offset_ms


@group.oracle("envelope.post_secure_clock", errors=(*_ERRORS, OverflowError))
def post_secure_clock(status: int, text: str, date_header, now_ms: int, offset_ms: int) -> dict:
    """The offset after ``post_secure`` reads a reply: every reply counts, a
    failed one too, before its body is read."""
    secure, _ = transport(status, text, None if date_header is None else {"Date": date_header})
    secure.server_time_offset_ms = offset_ms
    outcome: dict = {}
    with fixed(service_time_ms=now_ms), utc_host():
        try:
            outcome["result"] = asyncio.run(secure.post_secure("/app/x", {}))
        except exceptions.BydError as error:
            outcome["error"] = type(error).__name__
    outcome["offset_ms"] = secure.server_time_offset_ms
    return outcome


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


#: ``Date`` headers: what BYD sends, the other forms Python reads, the ones it
#: refuses and the ones whose numbers overflow.
DATE_MS = 1_795_341_600_000
DATE_HEADERS = [
    "Sat, 22 Nov 2026 10:00:00 GMT",
    "Sat, 22 Nov 2026 10:00:00 +0000",
    "Sat, 22 Nov 2026 12:00:00 +0200",
    "Sat, 22 Nov 2026 05:00:00 EST",
    "Sat, 22 Nov 2026 10:00:00 UT",
    "Sat, 22 Nov 2026 10:00:00 Z",
    "Sat, 22 Nov 2026 10:00:00 -0000",
    "Sat, 22 Nov 2026 10:00:00",
    "Sat, 22 Nov 2026 10:00:00 XYZ",
    "Saturday, 22-Nov-26 10:00:00 GMT",
    "Sat Nov 22 10:00:00 2026",
    "22 Nov 2026 10:00 GMT",
    "22 Nov 2026 10.00.00 GMT",
    "22 Nov 2026 10.00 GMT",
    "22 november 2026 10:00:00+0130",
    "22 Nov 2026 10:00:00-0130",
    "Nov 22, 2026, 10:00:00, GMT",
    "sat,22 Nov 2026 10:00:00 GMT",
    "22 Nov 69 10:00:00 GMT",
    "22 Nov 68 10:00:00 GMT",
    "22 Nov -5 10:00:00 GMT",
    "22 Nov ٢٠٢٦ 10:00:00 GMT",
    "22 Nov 2026 10:00:00 +1_00",
    "22 Nov 2026 10:00:00 +2359",
    "22 Nov 2026 10:00:00 +2400",
    "22 Nov 2026 10:00:00 -9999",
    "29 Feb 2024 10:00:00 GMT",
    "29 Feb 2026 10:00:00 GMT",
    "1 Jan 1 00:00:00 GMT",
    "1 Jan 100 00:00:00 GMT",
    "31 Dec 9999 23:59:59 GMT",
    "1 Jan 10000 00:00:00 GMT",
    "22 Nov 2026 24:00:00 GMT",
    "22 Nov 2026 10:60:00 GMT",
    "22 Nov 2026 10:00:60 GMT",
    "22 Nov 2026 10 GMT",
    "22 Nov 2026 a:b GMT",
    "22 Foo 2026 10:00:00 GMT",
    "22 Nov , 10:00:00 GMT",
    "garbage",
    "   ",
    "",
    "22 Nov 99999999999999999999 10:00:00 GMT",
    "22 Nov 2026 10:00:00 +99999999999999",
    "22 Nov 2026 99999999999:00:00 GMT",
    "22 Nov 2026 10:00:00 +999999999999999999999999999999999999999999",
    "22 Nov 2026 10:00:00 -2147483648",
]


def random_date_header(rng) -> str:
    """A header near a real one: a known form, a random instant in one of the
    forms, or a known form with a part changed."""
    roll = rng.random()
    if roll < 0.3:
        return rng.choice(DATE_HEADERS)
    moment = datetime.datetime(1, 1, 1, tzinfo=datetime.UTC) + datetime.timedelta(seconds=rng.randint(0, 315_537_897_599))
    if roll < 0.6:
        return email.utils.format_datetime(moment.astimezone(datetime.timezone(datetime.timedelta(minutes=rng.randint(-1439, 1439)))))
    parts = rng.choice(DATE_HEADERS).split(" ")
    index = rng.randrange(len(parts)) if parts else 0
    parts[index] = rng.choice(["", "-0000", "+0100", "GMT", "Feb", "31", "-1", "99", "12:00", "1.2.3", random_text(rng, 4), str(rng.randint(-10**6, 10**6))])
    return rng.choice([" ", "  ", "\t"]).join(parts)


def login_responses(rng, password: str) -> list:
    key = pwd_login_key(password)

    def sealed(value) -> str:
        return aes_encrypt_hex(value if isinstance(value, str) else json.dumps(value), key)

    return [
        {"code": "0", "respondData": sealed({"token": TOKEN})},
        {"code": 0, "respondData": sealed({"token": TOKEN})},
        {"code": 0.0, "respondData": sealed({"token": TOKEN})},
        {"code": "1005", "message": "expired"},
        {"code": "1003", "message": "time"},
        {"code": 1003, "message": None},
        {"code": " 1003"},
        {"code": "1030"},
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
        {"code": "0", "respondData": sealed({"token": {**TOKEN, "refreshToken": "REFRESH-synthetic"}})},
        {"code": "0", "respondData": sealed({"token": {**TOKEN, "refreshToken": 77}})},
        {"code": "0", "respondData": sealed({"token": {**TOKEN, "refreshToken": ""}})},
        {"code": "0", "respondData": sealed({"token": TOKEN, "newTokens": {"refreshToken": "NEW-R"}})},
        {"code": "0", "respondData": sealed({"token": {**TOKEN, "refreshToken": "OLD"}, "newTokens": {"refreshToken": ""}})},
        {"code": "0", "respondData": sealed({"token": {**TOKEN, "refreshToken": "OLD"}, "newTokens": ["x"]})},
        {"code": "0", "respondData": sealed({"token": TOKEN, "newTokens": {"refreshToken": [1]}})},
        {"code": "0", "respondData": sealed({"token": TOKEN, "newTokens": random_json(rng)})},
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
    for header in DATE_HEADERS:
        for now_ms, offset_ms in [(DATE_MS, 0), (DATE_MS - 60_000, 0), (DATE_MS + 30_001, 9), (DATE_MS + 30_000, 9)]:
            yield "envelope.sync_clock_from_header", {"date_header": header, "now_ms": now_ms, "offset_ms": offset_ms}
    for status, text in [(200, reply_text({"code": "0"})), (500, "down"), (200, "not json")]:
        for header in [None, "", DATE_HEADERS[0], "nonsense"]:
            yield "envelope.post_secure_clock", {
                "status": status, "text": text, "date_header": header, "now_ms": DATE_MS - 120_000, "offset_ms": 5,
            }
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
    for code in ["1001", "1005", "1008", "6002", "1007", 1001, "1030", "1003", 1030]:
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
                token_reply(
                    key, None, code=rng.choice(["1001", "1002", "1008", "6002", "9", "1030", "1003"]), message=random_text(rng)
                ),
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
        now_ms = rng.choice([DATE_MS, rng.randint(0, 2**48)])
        yield "envelope.sync_clock_from_header", {
            "date_header": rng.choice([random_date_header(rng), None]),
            "now_ms": now_ms,
            "offset_ms": rng.choice([0, 0, rng.randint(-10**9, 10**9)]),
        }
        status, text = rng.choice(body_texts(rng))
        yield "envelope.post_secure_clock", {
            "status": status,
            "text": text,
            "date_header": rng.choice([random_date_header(rng), None, ""]),
            "now_ms": now_ms,
            "offset_ms": rng.choice([0, rng.randint(-10**6, 10**6)]),
        }
        yield "envelope.golden_login", {
            "config": config,
            "now_ms": rng.randint(0, 2**53),
            "random": random_hex(rng, 16),
            "service_time_ms": rng.randint(0, 2**42),
            "reply_text": login_reply(password, token, code=rng.choice(["0", "0", "1005"])),
        }
