"""pybyd/config.py

Also holds the case helpers other groups share: random configs and sessions.
"""

from __future__ import annotations

import math
import os
from typing import Any
from unittest import mock

from pybyd.config import BydConfig, DeviceProfile

from parity.framework import Group, random_text

group = Group("config")

DEVICE_FIELDS = list(DeviceProfile.model_fields)
STR_FIELDS = [
    "base_url",
    "country_code",
    "language",
    "time_zone",
    "app_version",
    "app_inner_version",
    "soft_type",
    "tbox_version",
    "is_auto",
    "identifier_type",
]


def finite(value: Any) -> Any:
    """JSON cannot hold inf or NaN: write non-finite floats as "inf", "-inf", "nan"."""
    if isinstance(value, float) and not math.isfinite(value):
        return repr(value)
    if isinstance(value, dict):
        return {key: finite(item) for key, item in value.items()}
    if isinstance(value, list):
        return [finite(item) for item in value]
    return value


@group.oracle("config.DeviceProfile")
def device_profile(kwargs: dict) -> dict:
    return DeviceProfile(**kwargs).model_dump(mode="json")


@group.oracle("config.BydConfig")
def byd_config(kwargs: dict) -> dict:
    return finite(BydConfig(**kwargs).model_dump(mode="json"))


@group.oracle("config.BydConfig.from_env")
def from_env(env: dict, overrides: dict) -> dict:
    with mock.patch.object(os, "environ", dict(env)):
        return finite(BydConfig.from_env(**overrides).model_dump(mode="json"))


# ---------------------------------------------------------------------------
# Shared case helpers
# ---------------------------------------------------------------------------

AWKWARD_STRINGS = [
    "",
    " ",
    " padded ",
    "\t tab\n",
    "\x1cfs\x1f",
    "\x85nel\xa0",
    "　wide ",
    "​zero-width",
    "user@example.com",
    "name+byd@example.com",
    'My "Seal" 🚗',
    "Zoë’s 比亚迪",
    "back\\slash",
    "00000000000000000000000000000000",
    "000",
]

FLOAT_STRINGS = [
    "1", "1.5", " 2 ", "1e3", "1E-2", "inf", "-Infinity", "nan", "NaN", "+.5", "5.", ".", "1_000", "1__0",
    "_1", "1_", "1_.5", " 1_0", "1_0 ", "0x10", "", " ", "1,5", "--1", "1e400", "-0", "١", "\x1c1", "1\x85",
    "i_nf", "1e_5", "1._", "12abc",
]
INT_STRINGS = [
    "1", " 2 ", "1.0", "1.00", "1.5", "1.", ".0", "1_000", "1__0", "_1", "+3", "-0", "-0.0", "00012",
    "1e3", "9223372036854775807", "-9223372036854775808", "0x10", "", "+", "1_0.0", "1.0_0", "\x1c1", "1\x85",
]
BOOL_STRINGS = ["yes", "YES", "On", "off", "t", "f", "T", "true ", " on", "0", "1", "2", "", "y", "N", "nO", "1.0"]


def awkward_text(rng) -> str:
    if rng.random() < 0.4:
        return rng.choice(AWKWARD_STRINGS)
    return random_text(rng)


def random_float_value(rng) -> Any:
    roll = rng.random()
    if roll < 0.3:
        return rng.choice(FLOAT_STRINGS)
    if roll < 0.5:
        return rng.choice([0, 1, -1, 2**53 + 1, 2**63 - 1, 2**64 - 1, True, False])
    if roll < 0.6:
        return rng.choice([None, [], {}, "x"])
    return rng.choice([0.0, -0.0, 1e300, -1.5, 3600.0]) if rng.random() < 0.3 else rng.uniform(-1e6, 1e6)


def random_int_value(rng) -> Any:
    roll = rng.random()
    if roll < 0.35:
        return rng.choice(INT_STRINGS)
    if roll < 0.55:
        return rng.choice([0.0, 1.0, -0.0, 1.5, 1e18, 9.2e18, 2.0**63, -(2.0**63), 1e20, True, False])
    if roll < 0.65:
        return rng.choice([None, [], {}])
    return rng.randint(-(2**63), 2**63 - 1) if rng.random() < 0.3 else rng.randint(-1000, 1000)


def random_bool_value(rng) -> Any:
    roll = rng.random()
    if roll < 0.4:
        return rng.choice(BOOL_STRINGS)
    if roll < 0.7:
        return rng.choice([0, 1, 2, -1, 0.0, 1.0, -0.0, 0.5])
    if roll < 0.8:
        return rng.choice([None, [], {}])
    return rng.random() < 0.5


def random_str_value(rng) -> Any:
    if rng.random() < 0.1:
        return rng.choice([None, 1, 1.5, True, [], {}])
    return awkward_text(rng)


def random_device_kwargs(rng, *, valid: bool = False) -> dict:
    fields = rng.sample(DEVICE_FIELDS, k=rng.randint(0, len(DEVICE_FIELDS)))
    kwargs = {name: (awkward_text(rng) if valid else random_str_value(rng)) for name in fields}
    if not valid and rng.random() < 0.05:
        kwargs["unknown"] = "x"
    return kwargs


def random_config_kwargs(rng, *, valid: bool = False) -> dict:
    kwargs: dict[str, Any] = {}
    if valid or rng.random() > 0.05:
        kwargs["username"] = awkward_text(rng) if valid else random_str_value(rng)
    if valid or rng.random() > 0.05:
        kwargs["password"] = awkward_text(rng) if valid else random_str_value(rng)
    for name in rng.sample(STR_FIELDS, k=rng.randint(0, len(STR_FIELDS))):
        kwargs[name] = awkward_text(rng) if valid else random_str_value(rng)
    if rng.random() < 0.3:
        kwargs["control_pin"] = rng.choice([None, "123456", " 1234 "]) if valid else random_str_value(rng)
    if not valid:
        if rng.random() < 0.3:
            kwargs["session_ttl"] = random_float_value(rng)
        if rng.random() < 0.3:
            kwargs["mqtt_timeout"] = random_float_value(rng)
        if rng.random() < 0.3:
            kwargs["mqtt_enabled"] = random_bool_value(rng)
        if rng.random() < 0.3:
            kwargs["mqtt_keepalive"] = random_int_value(rng)
        if rng.random() < 0.03:
            kwargs["unknown"] = 1
    if rng.random() < 0.6:
        kwargs["device"] = random_device_kwargs(rng, valid=valid)
        if not valid and rng.random() < 0.05:
            kwargs["device"] = rng.choice([None, "x", [], 1])
    # Shuffle the key order: pydantic does not care, the port must not either.
    items = list(kwargs.items())
    rng.shuffle(items)
    return dict(items)


def random_session_kwargs(rng, *, valid: bool = True) -> dict:
    kwargs: dict[str, Any] = {
        "user_id": awkward_text(rng),
        "sign_token": awkward_text(rng),
        "encry_token": awkward_text(rng),
        "created_at": rng.choice([0.0, 12.5, rng.uniform(0, 1e6)]),
    }
    if rng.random() < 0.5:
        kwargs["ttl"] = rng.choice([0, 0.0, 43200.0, rng.uniform(0, 1e5)])
    if not valid:
        roll = rng.random()
        if roll < 0.2:
            kwargs["created_at"] = random_float_value(rng)
        elif roll < 0.4:
            kwargs["ttl"] = random_float_value(rng)
        elif roll < 0.5:
            kwargs[rng.choice(["user_id", "sign_token", "encry_token"])] = random_str_value(rng)
        elif roll < 0.55:
            del kwargs[rng.choice(["user_id", "sign_token", "encry_token"])]
        elif roll < 0.6:
            kwargs["extra"] = "x"
    return kwargs


SAMPLE_CONFIG = {
    "username": "name+byd@example.com",
    "password": 'p@ss "wörd" \\ 🚗 &=',
    "country_code": "NL",
    "device": {"mobile_model": 'Pixel "9" 🚀', "imei": "BANGCLE01234"},
}
SAMPLE_SESSION = {
    "user_id": "1234567",
    "sign_token": "SIGNTOKEN-synthetic",
    "encry_token": "ENCRYTOKEN-synthetic",
    "created_at": 100.0,
    "ttl": 43200.0,
}


# ---------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------

ENV_SAMPLES = [
    {},
    {"BYD_USERNAME": "user@example.com", "BYD_PASSWORD": "secret"},
    {
        "BYD_USERNAME": " user@example.com ",
        "BYD_PASSWORD": "secret",
        "BYD_SESSION_TTL": " 3600 ",
        "BYD_MQTT_ENABLED": " Off ",
        "BYD_MQTT_KEEPALIVE": "1_000",
        "BYD_MQTT_TIMEOUT": "",
        "BYD_MQTT_COMMAND_TIMEOUT": "2.5",
        "BYD_IMEI": " 123 ",
        "BYD_CONTROL_PIN": "123456",
    },
    {"BYD_USERNAME": "u", "BYD_PASSWORD": "p", "BYD_SESSION_TTL": "abc"},
    {"BYD_USERNAME": "u", "BYD_PASSWORD": "p", "BYD_MQTT_KEEPALIVE": "1.0"},
    {"BYD_USERNAME": "u", "BYD_PASSWORD": "p", "BYD_MQTT_TIMEOUT": "x", "BYD_MQTT_COMMAND_TIMEOUT": "1"},
    {"BYD_USERNAME": "u", "BYD_PASSWORD": "p", "BYD_SESSION_TTL": "inf", "BYD_MQTT_TIMEOUT": "nan"},
    {"BYD_USERNAME": "u", "BYD_PASSWORD": "p", "BYD_MQTT_ENABLED": "maybe"},
]
OVERRIDE_SAMPLES = [
    {},
    {"username": "over@example.com", "password": "p"},
    {"session_ttl": 5, "mqtt_enabled": False, "mqtt_keepalive": 1, "mqtt_timeout": 1},
    {"device": {"imei": 5, "sdk": None, "mod": True, "model": 1.0}},
    {"device": "ignored"},
    {"device": {"unknown": "x"}},
    {"language": 5},
]


@group.recorded
def recorded():
    yield "config.DeviceProfile", {"kwargs": {}}
    yield "config.DeviceProfile", {"kwargs": {"imei": " 1 ", "mod": "\x1cx\x1c"}}
    yield "config.DeviceProfile", {"kwargs": {"imei": 1}}
    yield "config.DeviceProfile", {"kwargs": {"nope": "x"}}
    yield "config.BydConfig", {"kwargs": SAMPLE_CONFIG}
    yield "config.BydConfig", {"kwargs": {"username": "u"}}
    yield "config.BydConfig", {"kwargs": {"username": " u ", "password": "p", "device": {"imei_md5": " " + "0" * 32}}}
    for value in FLOAT_STRINGS:
        yield "config.BydConfig", {"kwargs": {"username": "u", "password": "p", "session_ttl": value}}
    for value in INT_STRINGS:
        yield "config.BydConfig", {"kwargs": {"username": "u", "password": "p", "mqtt_keepalive": value}}
    for value in BOOL_STRINGS:
        yield "config.BydConfig", {"kwargs": {"username": "u", "password": "p", "mqtt_enabled": value}}
    for env in ENV_SAMPLES:
        for overrides in OVERRIDE_SAMPLES:
            yield "config.BydConfig.from_env", {"env": env, "overrides": overrides}


@group.fuzz
def fuzz(rng):
    while True:
        yield "config.DeviceProfile", {"kwargs": random_device_kwargs(rng)}
        yield "config.BydConfig", {"kwargs": random_config_kwargs(rng)}
        env = {}
        for name in rng.sample(
            [
                "BYD_USERNAME", "BYD_PASSWORD", "BYD_IMEI", "BYD_IMEI_MD5", "BYD_LANGUAGE", "BYD_CONTROL_PIN",
                "BYD_SESSION_TTL", "BYD_MQTT_ENABLED", "BYD_MQTT_KEEPALIVE", "BYD_MQTT_TIMEOUT",
                "BYD_MQTT_COMMAND_TIMEOUT", "BYD_OS_VERSION",
            ],
            k=rng.randint(0, 12),
        ):
            pools = {
                "BYD_SESSION_TTL": FLOAT_STRINGS,
                "BYD_MQTT_TIMEOUT": FLOAT_STRINGS,
                "BYD_MQTT_COMMAND_TIMEOUT": FLOAT_STRINGS,
                "BYD_MQTT_KEEPALIVE": INT_STRINGS,
                "BYD_MQTT_ENABLED": BOOL_STRINGS,
            }
            env[name] = rng.choice(pools[name]) if name in pools and rng.random() < 0.8 else awkward_text(rng)
        overrides = {}
        if rng.random() < 0.5:
            overrides = random_config_kwargs(rng)
            if "device" in overrides and isinstance(overrides["device"], dict):
                overrides["device"] = {
                    key: rng.choice([value, 1, None, True, 2.5]) for key, value in overrides["device"].items()
                }
        yield "config.BydConfig.from_env", {"env": env, "overrides": overrides}
