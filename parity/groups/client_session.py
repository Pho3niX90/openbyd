"""pybyd/client.py: the session lifecycle, vehicles and caches of BydClient, compared by trace.

Every case runs pyBYD's real ``BydClient`` and openbyd's ``BydClient``
against the same script (``parity/api_support.py``, ``run_client``).
"""

from __future__ import annotations

from parity.api_support import (
    random_client_case,
    LIVE_SESSION,
    LOGIN_TOKEN,
    NOW_MS,
    date_reply,
    http_date,
    OLD_SESSION,
    VIN,
    broker_reply,
    client_case,
    client_oracle,
    error_reply,
    logged_in,
    login_replies,
    login_reply,
    ok_reply,
    raw_reply,
    sealed_reply,
    transport_error,
)
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_CONFIG, SAMPLE_SESSION


group = Group("client_session", crate="client")

METHODS = [
    "login",
    "ensure_session",
    "invalidate_session",
    "async_start",
    "async_close",
    "get_vehicles",
    "get_latest_configs",
    "get_latest_config",
    "get_vehicle_capabilities",
    "commands_enabled",
    "get_command_ack_diagnostics",
]
for name in METHODS:
    client_oracle(group, name)

VIN2 = "TESTVIN0000000002"
NO_MQTT = {**SAMPLE_CONFIG, "mqtt_enabled": False}
NO_TTL = {**SAMPLE_CONFIG, "session_ttl": 0}
SHORT_TTL = {**SAMPLE_CONFIG, "session_ttl": 60}
OWNER = {"vin": VIN, "energyType": "0", "modelName": "SEAL"}
SHARED = {"vin": VIN2, "energyType": "2", "empowerType": -1, "empowerId": 123}
CONFIG = {"cfFixedList": [{"functionNo": no} for no in ["1005", "1006", "1001", "1012", "1020", "1026"]]}


def login_failures() -> list[list[dict]]:
    """Scripts whose login fails: codes, broken payloads, transport errors."""
    return [
        [login_reply(code="1002")],
        [login_reply(code=1005)],
        [login_reply(code="")],
        [login_reply(password="other")],
        [login_reply(token={})],
        [login_reply(token={"userId": "1"})],
        [login_reply(token={**LOGIN_TOKEN, "encryToken": ""})],
        [login_reply(token="text")],
        [raw_reply({"code": "0"})],
        [raw_reply({"code": "0", "respondData": "ZZ"})],
        [raw_reply([])],
        [raw_reply("text")],
        [error_reply("9999")],
        [transport_error(500)],
        [transport_error(None)],
        [],
    ]


def mqtt_starts() -> list[dict]:
    """Login scripts that walk the MQTT start: broker lookup failing in
    every way, the start failing, MQTT disabled."""
    return [
        {"replies": [login_reply(), error_reply("1002")]},
        {"replies": [login_reply(), error_reply("9999")]},
        {"replies": [login_reply(), ok_reply(SAMPLE_SESSION, {})]},
        {"replies": [login_reply(), ok_reply(SAMPLE_SESSION, {"emqBorker": "h:x"})]},
        {"replies": [login_reply(), transport_error(502)]},
        {"replies": [login_reply()]},
        {"replies": login_replies(), "mqtt_starts": [False]},
        {"replies": [login_reply()], "config": NO_MQTT},
    ]


@group.recorded
def recorded():
    # login: success, every way it fails, every way MQTT does not start
    yield "client.login", client_case(replies=login_replies())
    yield "client.login", client_case(replies=login_replies(), callbacks=False)
    yield "client.login", client_case(replies=login_replies(), config=NO_TTL)
    for script in login_failures():
        yield "client.login", client_case(replies=script)
    for script in mqtt_starts():
        yield "client.login", client_case(**script)
    # A second login while MQTT runs: the key is updated, MQTT restarted;
    # the guest map and the old session's settled pairs.
    yield "client.login", client_case(
        state=logged_in(guest_vins={VIN: True}, user_type_settled=[(VIN, "/x")]), replies=login_replies()
    )
    yield "client.login", client_case(state=logged_in(mqtt=False), replies=login_replies())
    yield "client.login", client_case(state={"guest_vins": {VIN: True}}, replies=login_replies())
    # a closed client cannot log in; async_start reopens it
    yield "client.login", client_case(setup=[{"method": "async_close"}], replies=login_replies())
    yield "client.login", client_case(
        setup=[{"method": "async_close"}, {"method": "async_start"}], replies=login_replies()
    )

    # ensure_session: valid, missing, expired by TTL (before the call), no TTL
    yield "client.ensure_session", client_case(state=logged_in())
    yield "client.ensure_session", client_case(replies=login_replies())
    yield "client.ensure_session", client_case(state=logged_in(session=OLD_SESSION), replies=login_replies())
    yield "client.ensure_session", client_case(
        state=logged_in(session={**LIVE_SESSION, "ttl": 60.0}),
        setup=[{"advance": 59.0}],
    )
    yield "client.ensure_session", client_case(
        state=logged_in(session={**LIVE_SESSION, "ttl": 60.0}),
        setup=[{"advance": 60.0}],
        replies=login_replies(),
    )
    yield "client.ensure_session", client_case(
        config=SHORT_TTL,
        setup=[{"method": "login"}, {"advance": 61.0}],
        replies=[*login_replies(), *login_replies()],
    )
    yield "client.ensure_session", client_case(
        config=NO_TTL, setup=[{"method": "login"}, {"advance": 1e9}], replies=login_replies()
    )
    yield "client.ensure_session", client_case(
        state=logged_in(session=OLD_SESSION), replies=[login_reply(code="1002")]
    )
    yield "client.ensure_session", client_case(
        setup=[{"method": "invalidate_session"}], state=logged_in(), replies=login_replies()
    )
    yield "client.invalidate_session", client_case(state=logged_in())
    yield "client.async_close", client_case(state=logged_in(capabilities={VIN: CONFIG}))
    yield "client.async_start", client_case(state=logged_in(), setup=[{"method": "async_close"}])

    # get_vehicles: seeds the metadata and the guest map
    vehicle_lists = [
        [OWNER],
        [OWNER, SHARED],
        [{"vin": VIN, "empowerType": 3}],
        [{"vin": VIN, "empowerId": 0, "empowerType": 0}],
        [{"vin": ""}, {"vin": VIN}],
        [OWNER, {**OWNER, "modelName": "DOLPHIN"}],
        [],
    ]
    for vehicles in vehicle_lists:
        yield "client.get_vehicles", client_case(state=logged_in(), replies=[ok_reply(SAMPLE_SESSION, vehicles)])
        yield "client.get_vehicles", client_case(
            state=logged_in(guest_vins={VIN: False, VIN2: False}), replies=[ok_reply(SAMPLE_SESSION, vehicles)]
        )
    # before any login, and the guess surviving a later login
    yield "client.get_vehicles", client_case(replies=[*login_replies(), ok_reply(SAMPLE_SESSION, [SHARED])])
    yield "client.ensure_session", client_case(
        state=logged_in(session=OLD_SESSION),
        setup=[{"method": "get_vehicles"}],
        replies=[*login_replies(), ok_reply(SAMPLE_SESSION, [SHARED]), *login_replies()],
    )
    # session expired mid-call: invalidate, log in again, retry once
    yield "client.get_vehicles", client_case(
        state=logged_in(), replies=[error_reply("1002"), *login_replies(), ok_reply(SAMPLE_SESSION, [OWNER])]
    )
    # every code that means the session is gone logs in again
    for code in ["1011", "3001", "3003"]:
        yield "client.get_vehicles", client_case(
            state=logged_in(), replies=[error_reply(code), *login_replies(), ok_reply(SAMPLE_SESSION, [OWNER])]
        )
        yield "client.get_vehicles", client_case(state=logged_in(), replies=[error_reply(code), *login_replies(), error_reply(code)])
    # 1005 and 1010 no longer do (pyBYD #96): the error goes to the caller and nothing else is sent
    for code in ["1005", "1010"]:
        yield "client.get_vehicles", client_case(
            state=logged_in(), replies=[error_reply(code), *login_replies(), ok_reply(SAMPLE_SESSION, [OWNER])]
        )
    yield "client.get_vehicles", client_case(
        state=logged_in(), replies=[error_reply("1010"), *login_replies(), error_reply("1010")]
    )
    yield "client.get_vehicles", client_case(state=logged_in(), replies=[error_reply("1002"), login_reply(code="1")])
    yield "client.get_vehicles", client_case(
        state=logged_in(), replies=[error_reply("1002"), *login_replies(), transport_error(500)]
    )
    yield "client.get_vehicles", client_case(state=logged_in(), replies=[error_reply("1001")])
    yield "client.get_vehicles", client_case(state=logged_in(), replies=[transport_error(503)])
    yield "client.get_vehicles", client_case(state=logged_in(), replies=[ok_reply(SAMPLE_SESSION, {"a": 1})])
    yield "client.get_vehicles", client_case(state=logged_in(), replies=[sealed_reply(SAMPLE_SESSION, "{")])
    yield "client.get_vehicles", client_case(state=logged_in(), setup=[{"method": "async_close"}])

    # latest configs and capabilities
    configs = {VIN: CONFIG, VIN2: {"cfFixedList": []}}
    yield "client.get_latest_configs", client_case(
        state=logged_in(), replies=[ok_reply(SAMPLE_SESSION, configs)], vins=[VIN, VIN2]
    )
    yield "client.get_latest_configs", client_case(
        state=logged_in(), replies=[error_reply("1002"), *login_replies(), ok_reply(SAMPLE_SESSION, configs)], vins=[VIN]
    )
    yield "client.get_latest_config", client_case(state=logged_in(), replies=[ok_reply(SAMPLE_SESSION, configs)], vin=VIN)
    yield "client.get_latest_config", client_case(state=logged_in(), replies=[ok_reply(SAMPLE_SESSION, {})], vin=VIN)
    yield "client.get_latest_config", client_case(state=logged_in(), replies=[error_reply("1001")], vin=VIN)
    for replies in (
        [ok_reply(SAMPLE_SESSION, configs)],
        [ok_reply(SAMPLE_SESSION, {})],
        [error_reply("1001")],
        [transport_error(500)],
        [],
        [error_reply("1002"), *login_replies(), ok_reply(SAMPLE_SESSION, configs)],
        [error_reply("1002"), login_reply(code="1")],
    ):
        yield "client.get_vehicle_capabilities", client_case(state=logged_in(), replies=replies, vin=VIN)
    yield "client.get_vehicle_capabilities", client_case(state=logged_in(capabilities={VIN: CONFIG}), vin=VIN)
    yield "client.get_vehicle_capabilities", client_case(
        state=logged_in(capabilities={VIN: CONFIG}),
        replies=[ok_reply(SAMPLE_SESSION, {VIN: {"cfFixedList": []}})],
        vin=VIN,
        force_refresh=True,
    )

    # small accessors
    yield "client.commands_enabled", client_case(state=logged_in(commands_enabled=True))
    yield "client.commands_enabled", client_case(state=logged_in())
    yield "client.get_command_ack_diagnostics", client_case(state=logged_in())
    yield "client.login", client_case(now_ms=0, replies=login_replies())
    yield "client.login", client_case(now_ms=NOW_MS, seed=random_text(__import__("random").Random(1), 8), replies=login_replies())
    yield "client.login", client_case(
        replies=[login_reply(), broker_reply(), login_reply(), broker_reply()],
        setup=[{"method": "login"}],
    )
    yield from refresh_and_clock_cases()
    yield from busy_session_cases()


REFRESHABLE = {**LIVE_SESSION, "refresh_token": "REFRESH-synthetic"}
NEW_TOKENS = {"signToken": "SIGN-2", "encryToken": "ENCRY-2", "refreshToken": "REFRESH-2"}
VEHICLES = ok_reply(SAMPLE_SESSION, [OWNER])
CONFIGS = {VIN: CONFIG}


def refresh_and_clock_cases():
    """pyBYD #102 (test_token_refresh_clock_sync): 1030 refreshes the
    tokens, 1003 is tried again with BYD's clock."""
    # The login reply's refresh token: in the token, or newer in newTokens.
    for token, extra in [({**LOGIN_TOKEN, "refreshToken": "R1"}, {}), (LOGIN_TOKEN, {"newTokens": {"refreshToken": "R2"}}),
                         ({**LOGIN_TOKEN, "refreshToken": "R1"}, {"newTokens": {"refreshToken": ""}})]:
        yield "client.login", client_case(replies=[login_reply(token, **extra), broker_reply()])
    # 1003 on login: tried once more with the clock of the reply's Date header.
    drifted = http_date(NOW_MS + 120_000)
    yield "client.login", client_case(replies=[date_reply(login_reply(code="1003"), drifted), *login_replies()])
    yield "client.login", client_case(replies=[login_reply(code="1003"), login_reply(code="1003")])
    yield "client.login", client_case(replies=[login_reply(code="1003"), login_reply(code="1002")])
    yield "client.login", client_case(replies=[date_reply(login_reply(code="1003"), drifted), login_reply(code="1003")])
    # 1030 with a refresh token: refreshed, the MQTT key follows, the call runs again.
    for mqtt in [True, False]:
        refresh = ok_reply(SAMPLE_SESSION, NEW_TOKENS)
        yield "client.get_vehicles", client_case(
            state=logged_in(mqtt=mqtt, session=REFRESHABLE, guest_vins={VIN: True}, user_type_settled=[(VIN, "/x")]),
            replies=[error_reply("1030"), refresh, ok_reply({**SAMPLE_SESSION, "encry_token": "ENCRY-2"}, [OWNER])],
        )
    yield "client.get_vehicles", client_case(
        state=logged_in(session=REFRESHABLE),
        replies=[error_reply("1030"), ok_reply(SAMPLE_SESSION, {"newTokens": {**NEW_TOKENS, "userId": "42"}}),
                 error_reply("1030")],
    )
    # The refresh fails (a pyBYD error): a full login instead.
    for failure in [error_reply("9999"), error_reply("1030"), ok_reply(SAMPLE_SESSION, {"signToken": "S"}),
                    transport_error(500), ok_reply(SAMPLE_SESSION, [])]:
        yield "client.get_vehicles", client_case(
            state=logged_in(session=REFRESHABLE), replies=[error_reply("1030"), failure, *login_replies(), VEHICLES],
        )
    yield "client.get_vehicles", client_case(state=logged_in(session=REFRESHABLE), replies=[error_reply("1030")])
    # Without a refresh token: a full login.
    yield "client.get_vehicles", client_case(state=logged_in(), replies=[error_reply("1030"), *login_replies(), VEHICLES])
    yield "client.get_vehicles", client_case(
        state=logged_in(), replies=[error_reply("1030"), login_reply(code="1002")]
    )
    # 1003 mid-call: the same call again, no new session; the Date header's clock stamps it.
    yield "client.get_vehicles", client_case(state=logged_in(), replies=[error_reply("1003"), VEHICLES])
    yield "client.get_vehicles", client_case(
        state=logged_in(), replies=[date_reply(error_reply("1003"), drifted), VEHICLES]
    )
    yield "client.get_vehicles", client_case(state=logged_in(), replies=[error_reply("1003"), error_reply("1003")])
    yield "client.get_vehicles", client_case(state=logged_in(), replies=[error_reply("1003"), error_reply("1002")])
    # A request after a drifted Date header carries BYD's clock.
    yield "client.get_latest_configs", client_case(
        state=logged_in(), setup=[{"method": "get_vehicles"}],
        replies=[date_reply(VEHICLES, http_date(NOW_MS - 3_600_000)), ok_reply(SAMPLE_SESSION, CONFIGS)], vins=[VIN],
    )


def busy_session_cases():
    """pyBYD #109 (test_busy_session_recovery): a 1008 from a read-only
    vehicle endpoint asks the vehicle list whether the session is dead."""
    call = "client.get_latest_configs"
    ok_configs = ok_reply(SAMPLE_SESSION, CONFIGS)
    # The probe succeeds: the session is fine, the 1008 stands.
    yield call, client_case(state=logged_in(), replies=[error_reply("1008"), VEHICLES], vins=[VIN])
    # The probe says the session is gone: a new login, then the call once more.
    for code in ["1002", "1011", "3001", "3003"]:
        yield call, client_case(state=logged_in(), replies=[error_reply("1008"), error_reply(code), *login_replies(),
                                                            ok_configs], vins=[VIN])
    # The probe says 1030: the tokens are refreshed (or, without a refresh token, a login).
    yield call, client_case(
        state=logged_in(session=REFRESHABLE),
        replies=[error_reply("1008"), error_reply("1030"), ok_reply(SAMPLE_SESSION, NEW_TOKENS),
                 ok_reply({**SAMPLE_SESSION, "encry_token": "ENCRY-2"}, CONFIGS)], vins=[VIN],
    )
    yield call, client_case(state=logged_in(), replies=[error_reply("1008"), error_reply("1030"), *login_replies(),
                                                        ok_configs], vins=[VIN])
    # The probe fails another way, or runs out of replies: the 1008 stands.
    for failure in [[error_reply("9999")], [error_reply("1008")], [transport_error(502)], [error_reply("1003")],
                    [ok_reply(SAMPLE_SESSION, {"a": 1})], []]:
        yield call, client_case(state=logged_in(), replies=[error_reply("1008"), *failure], vins=[VIN])
    # The recovery fails: the 1008 stands; the retry fails again: no loop.
    yield call, client_case(state=logged_in(), replies=[error_reply("1008"), error_reply("1002"),
                                                        login_reply(code="1002")], vins=[VIN])
    yield call, client_case(state=logged_in(session=REFRESHABLE),
                            replies=[error_reply("1008"), error_reply("1030"), error_reply("9999"),
                                     login_reply(code="1002")], vins=[VIN])
    yield call, client_case(state=logged_in(), replies=[error_reply("1008"), error_reply("1002"), *login_replies(),
                                                        error_reply("1008")], vins=[VIN])
    # Cooldowns: a probe at most every 60 s, none within 600 s of a recovery.
    healthy = [error_reply("1008"), VEHICLES]
    for gap, more in [(10.0, [error_reply("1008")]), (61.0, healthy)]:
        yield call, client_case(state=logged_in(), setup=[{"method": "get_latest_configs", "args": {"vins": [VIN]}},
                                                          {"advance": gap}],
                                replies=[*healthy, *more], vins=[VIN])
    recovered = [error_reply("1008"), error_reply("1002"), *login_replies(), ok_configs]
    for gap, more in [(30.0, [error_reply("1008")]), (100.0, [error_reply("1008")]), (601.0, recovered)]:
        yield call, client_case(state=logged_in(), setup=[{"method": "get_latest_configs", "args": {"vins": [VIN]}},
                                                          {"advance": gap}],
                                replies=[*recovered, *more], vins=[VIN])
    # The vehicle list itself never probes; nor does a closed client.
    yield "client.get_vehicles", client_case(state=logged_in(), replies=[error_reply("1008")])
    yield call, client_case(state=logged_in(), setup=[{"method": "async_close"}], vins=[VIN])
    yield call, client_case(replies=[*login_replies(), error_reply("1008"), VEHICLES], vins=[VIN])


@group.fuzz
def fuzz(rng):
    while True:
        method = rng.choice(METHODS)
        yield f"client.{method}", random_client_case(rng, method)
