"""pybyd/client.py: ``BydClient.get_car``, compared by trace.

``get_car`` returns the car it keeps for a VIN, or builds one: it fetches
the vehicle list unless a vehicle is given (a VIN the account does not
have fails), then the capabilities (a failing fetch gives "unknown"
capabilities), and keeps the car until ``async_close``. The result is
``{"cached", "vin", "capabilities", "state"}``: whether the car was kept
before the call, and the new or kept car's VIN, capabilities and snapshot.

The cars' own behaviour is compared in the ``car`` group; MQTT routing to a
kept car needs a running event loop, so it is tested end to end over real
sockets (``crates/openbyd-client/src/car/e2e.rs`` and the Python client tests).
"""

from __future__ import annotations

from parity.api_support import (
    ALL_FUNCTIONS_CONFIG,
    VIN,
    client_case,
    client_oracle,
    error_reply,
    logged_in,
    login_replies,
    mqtt_event,
    ok_reply,
    random_client_case,
    transport_error,
)
from parity.framework import Group
from parity.groups.config import SAMPLE_SESSION

group = Group("client_cars", crate="client")
client_oracle(group, "get_car")

VIN2 = "TESTVIN0000000002"
OWNER = {"vin": VIN, "energyType": "0", "modelName": "SEAL"}
SHARED = {"vin": VIN2, "energyType": "2", "empowerType": -1, "empowerId": 123}
LEARN = {"vin": VIN, "energyType": "0", "vehicleFunLearnInfo": {"openWindowLearnInfo": 1}}
LEARN_BAD = {"vin": VIN, "energyType": "0", "vehicleFunLearnInfo": {"openWindowLearnInfo": "x"}}
WINDOWS = {"cfFixedList": [{"functionNo": no} for no in ["1005", "1026", "1027"]]}
NO_FUNCTIONS = {"cfFixedList": []}


def vehicles(*entries: dict) -> dict:
    return ok_reply(SAMPLE_SESSION, list(entries))


def configs(config: dict, vin: str = VIN) -> dict:
    return ok_reply(SAMPLE_SESSION, {vin: config})


@group.recorded
def recorded():
    # built from the vehicle list and the latest config
    for config in (ALL_FUNCTIONS_CONFIG, WINDOWS, NO_FUNCTIONS):
        yield "client.get_car", client_case(
            state=logged_in(), replies=[vehicles(OWNER, SHARED), configs(config)], vin=VIN
        )
    yield "client.get_car", client_case(
        state=logged_in(), replies=[vehicles(OWNER, SHARED), configs(ALL_FUNCTIONS_CONFIG, VIN2)], vin=VIN2
    )
    # the window gate reads the vehicle's learn info
    for vehicle in (LEARN, LEARN_BAD, {**LEARN, "vehicleFunLearnInfo": {"openWindowLearnInfo": 0}}):
        yield "client.get_car", client_case(state=logged_in(), replies=[vehicles(vehicle), configs(WINDOWS)], vin=VIN)
        yield "client.get_car", client_case(state=logged_in(), replies=[configs(WINDOWS)], vin=VIN, vehicle=vehicle)
    # a VIN the account does not have, an empty list, a failing list
    yield "client.get_car", client_case(state=logged_in(), replies=[vehicles(SHARED)], vin=VIN)
    yield "client.get_car", client_case(state=logged_in(), replies=[vehicles()], vin=VIN)
    yield "client.get_car", client_case(state=logged_in(), replies=[error_reply("1001")], vin=VIN)
    yield "client.get_car", client_case(state=logged_in(), replies=[transport_error(502)], vin=VIN)
    yield "client.get_car", client_case(
        state=logged_in(), replies=[error_reply("1002"), *login_replies(), vehicles(OWNER), configs(WINDOWS)], vin=VIN
    )
    # the capabilities fail: "unknown" capabilities, and the car is built anyway
    for failure in (error_reply("1001"), transport_error(500), ok_reply(SAMPLE_SESSION, {}), error_reply("1008")):
        yield "client.get_car", client_case(state=logged_in(), replies=[vehicles(OWNER), failure], vin=VIN)
    yield "client.get_car", client_case(state=logged_in(), replies=[vehicles(OWNER)], vin=VIN)
    # capabilities already cached; a vehicle given; a projection TTL
    yield "client.get_car", client_case(state=logged_in(capabilities={VIN: WINDOWS}), vin=VIN, vehicle=OWNER)
    yield "client.get_car", client_case(
        state=logged_in(capabilities={VIN: WINDOWS}), vin=VIN, vehicle=OWNER, projection_ttl=5.0
    )
    yield "client.get_car", client_case(
        state=logged_in(capabilities={VIN: WINDOWS}, vehicles=[OWNER]), replies=[vehicles(OWNER)], vin=VIN
    )
    # a bad vehicle given
    yield "client.get_car", client_case(state=logged_in(), vin=VIN, vehicle={"vin": VIN, "energyType": []})
    # kept: a second call returns the same car without a request, whatever it is given
    yield "client.get_car", client_case(
        state=logged_in(),
        replies=[vehicles(OWNER), configs(WINDOWS)],
        setup=[{"method": "get_car", "args": {"vin": VIN}}],
        vin=VIN,
        vehicle=SHARED,
    )
    yield "client.get_car", client_case(
        state=logged_in(),
        replies=[vehicles(OWNER, SHARED), configs(WINDOWS), configs(NO_FUNCTIONS, VIN2)],
        setup=[{"method": "get_car", "args": {"vin": VIN}}],
        vin=VIN2,
    )
    # async_close forgets the cars; after async_start the car is built again
    yield "client.get_car", client_case(
        state=logged_in(),
        replies=[vehicles(OWNER), configs(WINDOWS), vehicles(OWNER), configs(NO_FUNCTIONS)],
        setup=[
            {"method": "get_car", "args": {"vin": VIN}},
            {"method": "async_close"},
            {"method": "async_start"},
        ],
        vin=VIN,
    )
    # closed: the vehicle list fails, a given vehicle still makes a car
    yield "client.get_car", client_case(state=logged_in(), setup=[{"method": "async_close"}], vin=VIN)
    yield "client.get_car", client_case(state=logged_in(), setup=[{"method": "async_close"}], vin=VIN, vehicle=OWNER)
    # not logged in: the vehicle list logs in first
    yield "client.get_car", client_case(replies=[*login_replies(), vehicles(OWNER), configs(WINDOWS)], vin=VIN)
    # pushes for a kept car while the client waits (merged only with a running loop)
    yield "client.get_car", client_case(
        state=logged_in(),
        replies=[vehicles(OWNER), configs(WINDOWS)],
        setup=[{"method": "get_car", "args": {"vin": VIN}}],
        mqtt=[mqtt_event(0.0, "vehicleInfo", {"respondData": {"elecPercent": 50, "time": 1700000000}})],
        vin=VIN,
    )


def _random_case(rng) -> dict:
    case = random_client_case(rng, "get_car")
    roll = rng.random()
    vehicle_list = [rng.choice([OWNER, SHARED, LEARN, LEARN_BAD])]
    if roll < 0.5:
        case["replies"][:0] = [vehicles(*vehicle_list), configs(rng.choice([WINDOWS, NO_FUNCTIONS, ALL_FUNCTIONS_CONFIG]))]
    elif roll < 0.7:
        case["args"]["vehicle"] = rng.choice([OWNER, LEARN, LEARN_BAD, SHARED])
    if rng.random() < 0.3:
        case["args"]["projection_ttl"] = rng.choice([0.0, 5.0, 30.0])
    if rng.random() < 0.2:
        case["setup"].append({"method": "get_car", "args": {"vin": case["args"]["vin"], "vehicle": OWNER}})
    return case


@group.fuzz
def fuzz(rng):
    while True:
        yield "client.get_car", _random_case(rng)
