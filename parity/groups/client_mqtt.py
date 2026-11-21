"""pybyd/client.py: the MQTT side of BydClient, compared by trace.

``client._on_mqtt_event`` runs no method: the MQTT timeline is delivered to
a client in a given state, and the trace shows what the dispatch did (the
callbacks, the pending commands, the re-logins). The cases cover every
event kind, acknowledgements matched, unmatched and expired, decrypt-failure
re-authentication up to and past its limit (upstream
test_mqtt_decrypt_reauth), recovery after refused CONNACKs (upstream
test_mqtt_connack_recovery), and a re-login cancelling a pending wait.

A ``vehicleStatusPush`` fires no callback of its own: what the client makes
of it shows only in what it hands to the car. Cases with the option
``car_pushes`` record that as ``{"car": [method, vin, models...]}`` steps
(``parity/api_support.py``, ``_CarSpies``), for every kind of push
(upstream test_mqtt_status_push).
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
from parity.groups.mqtt import AIR, CHARGE, STATUS_PUSHES, random_status_push

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


CAR_PUSHES = {"car_pushes": True}


def routed(*events: dict, state: dict | None = None, **more) -> dict:
    """``dispatch`` with what goes to the car layer recorded."""
    return dispatch(*events, state=state, options={**CAR_PUSHES, **more.pop("options", {})}, **more)


def status_push(at: float, respond, **more) -> dict:
    return mqtt_event(at, "vehicleStatusPush", {"respondData": respond}, **more)


def status_push_cases():
    call = "client._on_mqtt_event"
    doors = {"doorStatusDto": {"leftFrontDoor": 1, "leftFrontDoorLock": 1}}
    # upstream test_mqtt_status_push: realtime from every section, HVAC from airStatusDto only, no charging;
    # a remoteControlAck is logged and nothing else (no on_command_ack).
    yield call, routed(
        status_push(0.0, STATUS_PUSHES[3]),
        mqtt_event(1.0, "remoteControlAck", {"respondData": {"res": 1, "ackStatus": 1}}, uuid="ser-1"),
    )
    # upstream: a door-only push leaves HVAC and charging alone
    yield call, routed(status_push(0.0, STATUS_PUSHES[4]))
    # every upstream push, for an EV, a hybrid and a car the client has no metadata for
    for vehicles in ([VEHICLE], [{"vin": VIN, "energyType": "2"}], [{"vin": VIN, "energyType": "-1"}], []):
        for payload in STATUS_PUSHES:
            yield call, routed(status_push(0.0, payload), state=logged_in(vehicles=vehicles))
        yield call, routed(
            status_push(0.0, {"can3D903Dto": {"totalEnergy": "6.7kW·h/100km+7.2L/100km", "energyConsumption": "6.1+8.4"}}),
            state=logged_in(vehicles=vehicles),
        )
    # all three models; the envelope's time reaches each of them; a section's own time wins
    full = {**doors, AIR: {"cycleModel": 2, "status": 1, "tempInCar": 21}, CHARGE: {"chargingState": 15, "soc": 70}}
    for extra in ({}, {"time": 1700000000}, {"timestamp": 1700000001}, {"time": 1700000000, "timestamp": "x"},
                  {"time": None}, {AIR: {"time": 1700000500, "cycleModel": 1}, "time": 1700000000}):
        yield call, routed(status_push(0.0, {**full, **extra}))
    # no known section: nothing is routed (the two schedules are not realtime sections)
    for payload in ({}, {"time": 1700000000}, {"unknownDto": {"elecPercent": 5}}, {"smartChargeDto": {"soc": 80}},
                    {"doorStatusDto": None}, {"doorStatusDto": []}, {"doorStatusDto": {}}, {"doorStatusDto": {"a": None}},
                    {"elecPercent": 50}, {"airStatusDTO": {"cycleModel": 2}}):
        yield call, routed(status_push(0.0, payload))
    # sections that are not dicts next to one that is
    yield call, routed(status_push(0.0, {**doors, AIR: "x", CHARGE: [1], "tireStatusDto": 5}))
    # the sections in the data envelope when respondData has none; in the payload when there is no data object
    yield call, routed(mqtt_event(0.0, "vehicleStatusPush", {"respondData": {}, **full, "time": 1700000000}))
    yield call, routed(mqtt_event(0.0, "vehicleStatusPush", {"respondData": {"time": 5, "other": 1}, **doors}, uuid="S1"))
    yield call, routed(mqtt_event(0.0, "vehicleStatusPush", {"respondData": doors, AIR: {"cycleModel": 2}}))
    yield call, routed(mqtt_event(0.0, "vehicleStatusPush", {"code": "6051", **full}))
    yield call, routed(mqtt_event(0.0, "vehicleStatusPush", {"code": "6051", "respondData": "text", **doors}, uuid="S1"))
    for data in ({"respondData": "text", **doors}, {"respondData": None, **doors}, doors, "text", None, [], {}):
        yield call, routed(mqtt_event(0.0, "vehicleStatusPush", data))
    yield call, routed(raw_event(0.0, {"event": "vehicleStatusPush", "vin": VIN, **full, "time": 1700000000},
                                 event="vehicleStatusPush"))
    yield call, routed(raw_event(0.0, {"data": 5, **doors}, event="vehicleStatusPush"))
    yield call, routed(raw_event(0.0, {"data": {"respondData": {}}, **doors}, event="vehicleStatusPush"))
    # a uuid gives respondData a requestSerial, which is not a section
    yield call, routed(status_push(0.0, {"commonDto": {"requestSerial": "INNER"}, **doors}, uuid="S1"))
    # one model that does not validate drops the whole push: realtime, HVAC, charging
    for payload in ({"doorStatusDto": {"leftFrontDoor": [1]}, AIR: {"cycleModel": 2}},
                    {**doors, AIR: {"status": [1]}},
                    {**doors, CHARGE: {"soc": "x"}},
                    {**doors, AIR: {"cycleModel": 2}, CHARGE: {"chargingState": {}}},
                    {"commonDto": {"raw": "x"}, **doors},
                    {"commonDto": {"raw": {"own": 1}}, **doors}):
        yield call, routed(status_push(0.0, payload))
    # sentinels, numbers as text, field names as keys, unknown keys
    yield call, routed(status_push(0.0, {
        "can4A505Dto": {"elecPercent": "82", "elec_percent": 1, "foo": "bar"},
        "doorStatusDto": {"leftFrontDoor": -1, "leftFrontDoorLock": "", "backCover": "1"},
        AIR: {"tempInCar": -129, "status": "--", "airRunState": "1", "mainSettingTempNew": "23.5"},
        CHARGE: {"chargingState": "15", "fullHour": -1, "chargePower": "--", "elecPercent": "70"},
    }))
    # no VIN, an empty VIN, another car; no callbacks; MQTT not running
    yield call, routed(raw_event(0.0, {"data": {"respondData": full}}, event="vehicleStatusPush", vin=None))
    yield call, routed(status_push(0.0, full, vin=""))
    yield call, routed(status_push(0.0, full, vin="OTHERVIN000000001"))
    yield call, routed(status_push(0.0, full), callbacks=False)
    yield call, routed(status_push(0.0, full), state=logged_in(mqtt=False))
    yield call, dispatch(status_push(0.0, full))
    # the other pushes reach the car layer too, after the callback
    yield call, routed(
        mqtt_event(0.0, "vehicleInfo", {"respondData": REALTIME}),
        mqtt_event(1.0, "smartCharge", {"respondData": CHARGING}),
        mqtt_event(2.0, "energyConsumption", {"respondData": ENERGY}),
        mqtt_event(3.0, "vehicleInfo", {"respondData": {"onlineState": [1]}}),
        mqtt_event(4.0, "smartCharge", {"respondData": {"soc": "x"}}),
        mqtt_event(5.0, "somethingElse", {"respondData": full}),
        mqtt_event(6.0, "remoteControl", {"respondData": full}, uuid="S9"),
    )
    # a status push does not answer a wait for another kind of event, even with its serial
    yield "client.get_gps_info", routed(
        status_push(1.0, {**doors, "commonDto": {"latitude": 52.0, "longitude": 4.0}}, uuid="S1"),
        mqtt_event(2.0, "remoteControl", {"respondData": {"latitude": 52.0, "longitude": 4.0}}, uuid="S1"),
        replies=[ok_reply(SAMPLE_SESSION, {"requestSerial": "S1"})],
        vin=VIN,
    )
    # remoteControlAck: the pending command stays pending until the result comes, and counts as nothing
    lock = {"method": "lock", "args": {"vin": VIN, "command_pwd": "123456"}}
    trigger = ok_reply(SAMPLE_SESSION, {"requestSerial": "S1"})
    done = ok_reply(SAMPLE_SESSION, {"controlState": 1, "requestSerial": "S1"})
    ack = {"respondData": {"res": 1, "ackStatus": 1}}
    for events in (
        [mqtt_event(30.0, "remoteControlAck", ack, uuid="S1")],
        [mqtt_event(30.0, "remoteControlAck", ack, uuid="S1"),
         mqtt_event(31.0, "remoteControl", {"respondData": {"res": 4}}, uuid="S1")],
        [mqtt_event(30.0, "remoteControlAck", ack), mqtt_event(31.0, "remoteControlAck", {"respondData": {}}, uuid="S2")],
        [mqtt_event(30.0, "remoteControlAck", {"code": "6051"}, uuid="S1")],
        [mqtt_event(30.0, "remoteControlAck", ack, uuid="S1", vin="")],
    ):
        yield call, routed(*events, state=READY, setup=[lock], replies=[trigger, done])
        yield "client.get_command_ack_diagnostics", routed(*events, state=READY, setup=[lock], replies=[trigger, done])
    # a command waiting for its result is not answered by the ack
    yield "client.lock", routed(
        mqtt_event(1.0, "remoteControlAck", ack, uuid="S1"),
        mqtt_event(2.0, "remoteControl", {"respondData": {"res": 4}}, uuid="S1"),
        state=READY,
        replies=[trigger],
        vin=VIN,
        command_pwd="123456",
    )
    yield "client.lock", routed(
        mqtt_event(1.0, "remoteControlAck", ack, uuid="S1"),
        state=READY,
        replies=[trigger, done],
        vin=VIN,
        command_pwd="123456",
    )


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


def random_push_event(rng, at: float) -> dict:
    """A ``vehicleStatusPush`` in one of the shapes the client reads, or a ``remoteControlAck``."""
    vin = rng.choice([VIN, VIN, VIN, VIN, "OTHERVIN000000001", ""])
    uuid = rng.choice([None, None, "S1", "S2", ""])
    if rng.random() < 0.15:
        respond = rng.choice([{"res": 1, "ackStatus": 1}, {"res": 4}, {}, {"res": "x", "ackStatus": None}])
        return mqtt_event(at, "remoteControlAck", {"respondData": respond}, vin=vin, uuid=uuid)
    payload = random_status_push(rng)
    shape = rng.random()
    if shape < 0.7:
        data = {"respondData": payload}
    elif shape < 0.8:
        # the sections in the data envelope
        data = {"respondData": rng.choice([{}, {"other": 1}, {"time": 5}]), **payload}
    elif shape < 0.87:
        data = {"code": rng.choice(["6051", 6051]), **payload}
    elif shape < 0.93:
        data = {"respondData": rng.choice(["text", None, [payload]]), **payload}
    else:
        body = {"event": "vehicleStatusPush", "vin": vin, **payload}
        return {"at": at, "event": {"event": "vehicleStatusPush", "vin": vin or None, "topic": "t", "payload": body}}
    return mqtt_event(at, "vehicleStatusPush", data, vin=vin, uuid=uuid)


@group.recorded
def recorded_status_push():
    yield from status_push_cases()


@group.fuzz
def fuzz(rng):
    methods = ["_on_mqtt_event", "_on_mqtt_event", "get_command_ack_diagnostics", "get_gps_info", "lock"]
    while True:
        method = rng.choice(methods)
        case = random_client_case(rng, method)
        if method in ("_on_mqtt_event", "get_command_ack_diagnostics") and rng.random() < 0.5:
            case["setup"].append({"method": "lock", "args": {"vin": VIN, "command_pwd": "123456"}})
        if rng.random() < 0.6:
            case["options"]["car_pushes"] = True
        if rng.random() < 0.6:
            times = [rng.choice([0.0, 0.5, 1.0, 2.0, 3.0, 9.0, 40.0]) for _ in range(rng.choice([1, 1, 2, 3, 5]))]
            case["mqtt"] = sorted(
                [*case["mqtt"], *(random_push_event(rng, at) for at in times)], key=lambda entry: entry["at"]
            )
        yield f"client.{method}", case


keep_representable(group)
