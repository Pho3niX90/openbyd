"""pybyd/client.py: the MQTT side of BydClient, compared by trace.

``client._on_mqtt_event`` runs no method: the MQTT timeline is delivered to
a client in a given state, and the trace shows what the dispatch did (the
callbacks, the pending commands, the re-logins). The cases cover every
event kind, acknowledgements matched, unmatched and expired, decrypt-failure
re-authentication up to and past its limit (upstream
test_mqtt_decrypt_reauth), recovery after refused CONNACKs (upstream
test_mqtt_connack_recovery), and a re-login cancelling a pending wait.
"""

from __future__ import annotations

from parity.api_support import (
    ALL_FUNCTIONS_CONFIG,
    OLD_SESSION,
    VIN,
    client_case,
    client_oracle,
    error_reply,
    logged_in,
    login_replies,
    login_reply,
    mqtt_event,
    ok_reply,
    random_client_case,
)
from parity.flow_cases import corpus, keep_representable
from parity.framework import Group
from parity.groups.config import SAMPLE_SESSION

group = Group("client_mqtt", crate="client")

for name in ["_on_mqtt_event", "get_command_ack_diagnostics", "get_gps_info", "lock"]:
    client_oracle(group, name)

VEHICLE = {"vin": VIN, "energyType": "0", "modelName": "SEAL"}
REALTIME = next(payload for payload in corpus("VehicleRealtimeData") if isinstance(payload, dict))
CHARGING = next(payload for payload in corpus("ChargingStatus") if isinstance(payload, dict))
ENERGY = next(payload for payload in corpus("EnergyConsumption") if isinstance(payload, dict))
STATE = logged_in(vehicles=[VEHICLE])
READY = logged_in(vehicles=[VEHICLE], capabilities={VIN: ALL_FUNCTIONS_CONFIG}, commands_enabled=True)


def raw_event(at: float, payload: dict, *, event: str = "remoteControl", vin: str | None = VIN) -> dict:
    return {"at": at, "event": {"event": event, "vin": vin, "topic": "t", "payload": payload}}


def dispatch(*events: dict, state: dict | None = None, **more) -> dict:
    return client_case(state=STATE if state is None else state, mqtt=list(events), **more)


@group.recorded
def recorded():  # noqa: C901
    call = "client._on_mqtt_event"
    # vehicleInfo: parsed with the cached energy type, or not parsed at all
    for vehicles in ([VEHICLE], [{"vin": VIN, "energyType": "2"}], [{"vin": VIN, "energyType": "-1"}], []):
        yield call, dispatch(
            mqtt_event(0.0, "vehicleInfo", {"respondData": REALTIME}), state=logged_in(vehicles=vehicles)
        )
    for data in (
        {"respondData": {"elecPercent": "x" * 3}},
        {"respondData": {"onlineState": [1]}},
        {"respondData": "text"},
        {"respondData": None},
        {"code": "6051"},
        {"code": "6051", "uuid": "S1"},
        {"other": 1},
        "text",
        None,
        [],
    ):
        yield call, dispatch(mqtt_event(0.0, "vehicleInfo", data))
    # the payload itself is the respondData when there is no data object
    yield call, dispatch(raw_event(0.0, {"event": "vehicleInfo", "vin": VIN, "elecPercent": 50}, event="vehicleInfo"))
    yield call, dispatch(raw_event(0.0, {"data": 5, "res": 2, "requestSerial": "S1"}))
    # no VIN, an empty VIN, another car
    yield call, dispatch(raw_event(0.0, {"data": {"respondData": {"res": 2}, "uuid": "S1"}}, vin=None))
    yield call, dispatch(mqtt_event(0.0, "remoteControl", {"respondData": {"res": 2}}, vin=""))
    yield call, dispatch(mqtt_event(0.0, "vehicleInfo", {"respondData": REALTIME}, vin="OTHERVIN000000001"))
    # other event kinds go to the car layer only
    yield call, dispatch(mqtt_event(0.0, "smartCharge", {"respondData": CHARGING}))
    yield call, dispatch(mqtt_event(0.0, "energyConsumption", {"respondData": ENERGY}))
    yield call, dispatch(mqtt_event(0.0, "somethingElse", {"respondData": {"a": 1}}, uuid="U"))
    # remote-control acknowledgements: no serial, unknown serial, every result field
    for respond in (
        {"res": 2},
        {"res": 3, "message": "no"},
        {"controlState": 1, "time": 1700000000123},
        {"controlState": 2, "timestamp": "1700000000123", "msg": 5},
        {"res": 2, "time": "x", "result": {"a": 1}},
        {"res": 2, "time": None, "timestamp": 7, "result": None, "message": 1.5},
        {"res": 2, "time": [1], "result": True},
        {"res": "abc"},
        {"controlState": [1]},
        {"res": 2, "requestSerial": "INNER"},
        {},
    ):
        yield call, dispatch(mqtt_event(0.0, "remoteControl", {"respondData": respond}))
        yield call, dispatch(mqtt_event(0.0, "remoteControl", {"respondData": respond}, uuid="S7"))
    yield call, dispatch(mqtt_event(0.0, "remoteControl", {"respondData": {"res": 2}}, uuid=""))
    yield call, dispatch(mqtt_event(0.0, "remoteControl", {"respondData": {"res": 2}}, uuid=5))
    yield call, dispatch(mqtt_event(0.0, "remoteControl", {"respondData": {"res": 2}}, uuid="S1"), callbacks=False)
    # MQTT not running: deliveries are lost
    yield call, dispatch(mqtt_event(0.0, "vehicleInfo", {"respondData": REALTIME}), state=logged_in(mqtt=False))
    yield call, dispatch(mqtt_event(0.0, "vehicleInfo", {"respondData": REALTIME}), state={})

    # pending commands: matched by a late acknowledgement, never matched, expired
    lock = {"method": "lock", "args": {"vin": VIN, "command_pwd": "123456"}}
    trigger = ok_reply(SAMPLE_SESSION, {"requestSerial": "S1"})
    done = ok_reply(SAMPLE_SESSION, {"controlState": 1, "requestSerial": "S1"})
    for events, options in (
        ([mqtt_event(30.0, "remoteControl", {"respondData": {"res": 2}}, uuid="S1")], {}),
        ([mqtt_event(30.0, "remoteControl", {"respondData": {"res": 2}}, uuid="S2")], {}),
        ([mqtt_event(400.0, "remoteControl", {"respondData": {"res": 2}}, uuid="S1")], {}),
        ([mqtt_event(30.0, "remoteControl", {"respondData": {"res": 2}}, uuid="S1")], {"command_ack_ttl_seconds": 5.0}),
        ([mqtt_event(30.0, "remoteControl", {"respondData": {"res": 2}}, uuid="S1")], {"command_ack_ttl_seconds": 0}),
        ([], {}),
    ):
        yield call, dispatch(*events, state=READY, setup=[lock], replies=[trigger, done], options=options)
    yield "client.get_command_ack_diagnostics", dispatch(state=READY, setup=[lock], replies=[trigger, done])
    yield "client.get_command_ack_diagnostics", dispatch(
        state=READY, setup=[lock, {"advance": 301.0}], replies=[trigger, done]
    )
    yield "client.get_command_ack_diagnostics", dispatch(
        mqtt_event(1.0, "remoteControl", {"respondData": {"res": 2}}, uuid="S1"),
        mqtt_event(2.0, "remoteControl", {"respondData": {"res": 2}}),
        state=READY,
        setup=[lock, lock],
        replies=[trigger, done, ok_reply(SAMPLE_SESSION, {"requestSerial": "S2"}), done],
    )
    # serials that are not strings, or cannot be dict keys
    for serial in (5, True, 1.0, {"a": 1}, ["x"]):
        yield call, dispatch(
            state=READY,
            setup=[lock],
            replies=[ok_reply(SAMPLE_SESSION, {"requestSerial": serial}), done],
        )

    # a data poll's answer is not a command acknowledgement
    gps_trigger = ok_reply(SAMPLE_SESSION, {"requestSerial": "S1"})
    yield "client.get_gps_info", dispatch(
        mqtt_event(1.0, "remoteControl", {"respondData": {"latitude": 52.0, "longitude": 4.0}}, uuid="S1"),
        replies=[gps_trigger],
        vin=VIN,
    )
    yield "client.get_gps_info", dispatch(
        mqtt_event(1.0, "remoteControl", {"respondData": {"res": 2}}, uuid="S2"),
        mqtt_event(1.5, "remoteControl", {"respondData": {"latitude": 52.0}}, uuid="S1"),
        replies=[gps_trigger],
        vin=VIN,
    )
    # several waits are matched by one event
    yield "client.lock", dispatch(
        mqtt_event(1.0, "remoteControl", {"respondData": {"res": 2}}, uuid="S1"),
        state=READY,
        replies=[trigger],
        vin=VIN,
        command_pwd="123456",
    )

    # decrypt failures: one re-login, then none until a payload decrypts again
    yield call, dispatch({"at": 0.0, "decrypt_error": True}, replies=login_replies())
    yield call, dispatch({"at": 0.0, "decrypt_error": True}, {"at": 1.0, "decrypt_error": True}, replies=login_replies())
    yield call, dispatch(
        {"at": 0.0, "decrypt_error": True},
        {"at": 61.0, "decrypt_error": True},
        {"at": 200.0, "decrypt_error": True},
        {"at": 300.0, "decrypt_error": True},
        replies=[*login_replies(), *login_replies()],
    )
    yield call, dispatch(
        {"at": 0.0, "decrypt_error": True},
        mqtt_event(10.0, "vehicleInfo", {"respondData": REALTIME}),
        {"at": 61.0, "decrypt_error": True},
        {"at": 62.0, "decrypt_error": True},
        replies=[*login_replies(), *login_replies()],
    )
    yield call, dispatch({"at": 0.0, "decrypt_error": True}, replies=[login_reply(code="1002")])
    yield call, dispatch({"at": 0.0, "decrypt_error": True}, replies=[])
    yield call, dispatch({"at": 0.0, "decrypt_error": True}, replies=[login_reply()], mqtt_starts=[False])
    yield call, dispatch({"at": 0.0, "decrypt_error": True}, now_ms=30_000, replies=login_replies())
    # a re-login while a read waits for its push cancels the wait (pyBYD's
    # stop of the old runtime cancels the waiting future)
    yield "client.get_gps_info", dispatch(
        {"at": 1.0, "decrypt_error": True},
        replies=[gps_trigger, *login_replies()],
        vin=VIN,
    )
    yield "client.lock", dispatch(
        {"at": 1.0, "decrypt_error": True},
        state=READY,
        replies=[trigger, *login_replies()],
        vin=VIN,
        command_pwd="123456",
    )

    # refused CONNACKs: below the threshold, at it, inside the backoff, past the ceiling
    for counts in ([1, 2], [3], [3, 4], [3, 4, 5, 6, 7, 8, 9, 10]):
        events = [{"at": float(index), "connack_refused": count} for index, count in enumerate(counts)]
        yield call, dispatch(*events, replies=[*login_replies()] * len(counts))
    yield call, dispatch(
        {"at": 0.0, "connack_refused": 3},
        {"at": 100.0, "connack_refused": 3},
        {"at": 1000.0, "connack_refused": 3},
        {"at": 5000.0, "connack_refused": 3},
        replies=[*login_replies()] * 4,
    )
    yield call, dispatch(
        {"at": 0.0, "connack_refused": 3},
        {"at": 1.0, "connected": True},
        {"at": 2.0, "connack_refused": 3},
        replies=[*login_replies()] * 2,
    )
    # the ceiling: six attempts, then MQTT stops until the next login
    events = []
    for attempt in range(8):
        events.append({"at": 4000.0 * attempt, "connack_refused": 3})
    yield call, dispatch(*events, replies=[*login_replies()] * 8)
    yield call, dispatch({"at": 0.0, "connack_refused": 3}, replies=[error_reply("1002")])
    yield call, dispatch({"at": 0.0, "connack_refused": 3}, now_ms=10_000, replies=login_replies())
    yield call, dispatch({"at": 0.0, "connected": True}, {"at": 1.0, "connected": True})
    yield call, dispatch({"at": 0.0, "connected": True}, callbacks=False)
    # expired session and MQTT events together
    yield call, dispatch(
        {"at": 0.0, "decrypt_error": True},
        state=logged_in(session=OLD_SESSION, vehicles=[VEHICLE]),
        replies=login_replies(),
    )


@group.fuzz
def fuzz(rng):
    methods = ["_on_mqtt_event", "_on_mqtt_event", "get_command_ack_diagnostics", "get_gps_info", "lock"]
    while True:
        method = rng.choice(methods)
        case = random_client_case(rng, method)
        if method in ("_on_mqtt_event", "get_command_ack_diagnostics") and rng.random() < 0.5:
            case["setup"].append({"method": "lock", "args": {"vin": VIN, "command_pwd": "123456"}})
        yield f"client.{method}", case


keep_representable(group)
