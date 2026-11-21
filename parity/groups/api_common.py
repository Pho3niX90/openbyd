"""pybyd/_api/_common.py: inner base fields, code mapping, respondData decoding."""

from __future__ import annotations

import json
import secrets
from unittest import mock

from pybyd import exceptions
from pybyd._api import _common
from pybyd._crypto.aes import aes_encrypt_hex
from pybyd.config import BydConfig

from parity.framework import Group, random_hex, random_text
from parity.groups.config import SAMPLE_CONFIG, random_config_kwargs
from parity.groups.pyjson import random_json

group = Group("api_common")

_ERRORS = (exceptions.BydError, ValueError, AttributeError, TypeError)


def describe(error: exceptions.BydApiError) -> dict:
    return {
        "class": type(error).__name__,
        "lineage": [base.__name__ for base in type(error).__mro__ if base not in (Exception, BaseException, object)],
        "message": str(error),
        "code": error.code,
        "endpoint": error.endpoint,
    }


@group.oracle("api.common.constants")
def constants() -> dict:
    return {
        "ENDPOINT_NOT_SUPPORTED_CODES": sorted(_common.ENDPOINT_NOT_SUPPORTED_CODES),
        "SERVICE_BUSY_CODES": sorted(_common.SERVICE_BUSY_CODES),
        "VEHICLE_UNREACHABLE_CODES": sorted(_common.VEHICLE_UNREACHABLE_CODES),
        "USER_TYPE_GUEST": _common.USER_TYPE_GUEST,
        "GUEST_NOT_DECLARED_CODE": _common.GUEST_NOT_DECLARED_CODE,
        "NOT_A_GUEST_CODE": _common.NOT_A_GUEST_CODE,
    }


@group.oracle("api.common.build_inner_base")
def build_inner_base(config: dict, now_ms: int, random: str, vin, request_serial) -> dict:
    with mock.patch.object(secrets, "token_hex", lambda nbytes=None: random.lower()):
        return _common.build_inner_base(BydConfig(**config), now_ms=now_ms, vin=vin, request_serial=request_serial)


@group.oracle("api.common.raise_for_code", errors=(TypeError,))
def raise_for_code(endpoint, code, message, vin, not_supported_codes, extra_code_map) -> dict:
    extra = None
    if extra_code_map is not None:
        extra = {frozenset(codes): getattr(exceptions, name) for codes, name in extra_code_map}
    try:
        _common._raise_for_code(
            endpoint=endpoint,
            code=code,
            message=message,
            vin=vin,
            not_supported_codes=None if not_supported_codes is None else frozenset(not_supported_codes),
            extra_code_map=extra,
        )
    except exceptions.BydApiError as error:
        return describe(error)
    raise AssertionError("_raise_for_code returned")


@group.oracle("api.common.decode_respond_data", errors=_ERRORS)
def decode_respond_data(endpoint: str, response, content_key: str):
    return _common.decode_respond_data(endpoint=endpoint, response=response, content_key=content_key)


CODES = ["0", "", "1001", "1002", "1011", "3001", "3003", "1005", "1008", "1010", "5005", "6002", "6024", "8003", "9999", " 1001"]
API_CLASSES = [
    "BydApiError", "BydAuthenticationError", "BydSessionExpiredError", "BydRemoteControlError",
    "BydControlPasswordError", "BydEndpointNotSupportedError", "BydDataUnavailableError", "BydRateLimitError",
    "BydServiceBusyError",
]
KEY = "6A1E4DB0E2C7A0F5D1C2B3A495867788"


def respond_cases(rng, key: str) -> list:
    def sealed(text: str) -> str:
        return aes_encrypt_hex(text, key)

    return [
        {},
        {"respondData": None},
        {"respondData": ""},
        {"respondData": 5},
        {"respondData": sealed(json.dumps(random_json(rng)))},
        {"respondData": sealed(json.dumps(random_json(rng), ensure_ascii=False))},
        {"respondData": sealed("   \n")},
        {"respondData": sealed("")},
        {"respondData": sealed("{not json")},
        {"respondData": sealed("x" * 200)},
        {"respondData": sealed('{"a": 1}').lower()},
        {"respondData": "ZZ"},
        {"respondData": random_hex(rng, 16)},
        [],
        "text",
    ]


@group.recorded
def recorded():
    yield "api.common.constants", {}
    base = {"now_ms": 1700000000000, "random": "0123456789ABCDEF0123456789abcdef"}
    for vin, serial in [(None, None), ("TESTVIN0000000001", None), ("", ""), (None, "SERIAL-1")]:
        yield "api.common.build_inner_base", {"config": SAMPLE_CONFIG, **base, "vin": vin, "request_serial": serial}
    for code in CODES:
        for not_supported in [None, [], ["1001"], ["1001", "1008", "1005"]]:
            for extra in [None, [], [[["1001", "5005"], "BydControlPasswordError"], [["6024"], "BydRateLimitError"]]]:
                for vin in [None, "", "TESTVIN0000000001"]:
                    yield "api.common.raise_for_code", {
                        "endpoint": "/app/x",
                        "code": code,
                        "message": "msg ü",
                        "vin": vin,
                        "not_supported_codes": not_supported,
                        "extra_code_map": extra,
                    }
    yield "api.common.raise_for_code", {
        "endpoint": "/e", "code": "7", "message": "", "vin": None, "not_supported_codes": None,
        "extra_code_map": [[["7"], "BydTransportError"]],
    }
    import random

    rng = random.Random("api-common-recorded")
    for response in respond_cases(rng, KEY):
        yield "api.common.decode_respond_data", {"endpoint": "/app/x", "response": response, "content_key": KEY}


@group.fuzz
def fuzz(rng):
    while True:
        yield "api.common.build_inner_base", {
            "config": random_config_kwargs(rng, valid=True),
            "now_ms": rng.randint(0, 2**62),
            "random": random_hex(rng, 16, upper=rng.random() < 0.5),
            "vin": rng.choice([None, "", "TESTVIN0000000001", random_text(rng, 17)]),
            "request_serial": rng.choice([None, "", random_text(rng, 10)]),
        }
        extra = None
        if rng.random() < 0.6:
            extra = [
                [rng.sample(CODES, k=rng.randint(0, 3)), rng.choice(API_CLASSES)] for _ in range(rng.randint(0, 3))
            ]
        yield "api.common.raise_for_code", {
            "endpoint": random_text(rng, 20),
            "code": rng.choice(CODES + [random_text(rng, 5)]),
            "message": random_text(rng),
            "vin": rng.choice([None, "", random_text(rng, 17)]),
            "not_supported_codes": rng.choice([None, [], rng.sample(CODES, k=rng.randint(1, 4))]),
            "extra_code_map": extra,
        }
        key = random_hex(rng, 16)
        yield "api.common.decode_respond_data", {
            "endpoint": random_text(rng, 20),
            "response": rng.choice(respond_cases(rng, key)),
            "content_key": key,
        }
