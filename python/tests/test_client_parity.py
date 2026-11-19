"""openbyd.Client answers as pyBYD's BydClient does, over the wire.

Each case runs pyBYD's real ``BydClient`` and ``openbyd.Client`` against the
same ``openbyd.testing.FakeServer`` with the same replies (the realistic
payloads of ``parity/corpus``) and compares the results, pyBYD's dumped with
``model_dump(mode="json")``, through JSON text so key order and ``1``
against ``1.0`` count. Scripted errors must raise the same class with the
same ``code`` (or ``status_code``).
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

import pytest

import openbyd

pybyd = pytest.importorskip("pybyd")
testing = pytest.importorskip(
    "openbyd.testing", reason="extension built without the 'testing' feature", exc_type=ImportError
)
FakeServer, Reply = testing.FakeServer, testing.Reply

from pybyd.models import control  # noqa: E402
from pydantic import BaseModel  # noqa: E402

VIN = "TESTVIN0000000001"
USER = "user@example.com"
PASSWORD = "secret"
PIN = "123456"
CORPUS = Path(__file__).resolve().parents[2] / "parity" / "corpus"

VEHICLES = "/app/account/getAllListByUserId"
LATEST_CONFIG = "/vehicle/vehicleswitch/getLatestConfig"
VERIFY = "/vehicle/vehicleswitch/verifyControlPassword"
REMOTE = "/control/remoteControl"
REMOTE_RESULT = "/control/remoteControlResult"
BATTERY = "/control/remoteControlBatteryHeat"
CHARGE_TOGGLE = "/control/smartCharge/changeChargeStatue"
CHARGE_SAVE = "/control/smartCharge/saveOrUpdate"
CHARGE_RESULT = "/control/smartCharge/changeResult"

VEHICLE = {"vin": VIN, "energyType": "0", "modelName": "SEAL", "outModelType": "SEAL-EU"}
ALL_FUNCTIONS = {
    VIN: {"cfFixedList": [{"functionNo": no} for no in ["1001", "1005", "1006", "1007", "1009", "1012", "1020", "1021", "1026", "1027", "1030", "1033", "10300002"]]}
}
FAST = {"poll_attempts": 1, "poll_interval": 0.0, "mqtt_timeout": 0.0}
ERRORS = [Reply.code("1001"), Reply.code("1008"), Reply.code("6051"), Reply.code("9999"), Reply.code("1005"), Reply.status(502)]


def corpus(model: str) -> list[Any]:
    return [entry["payload"] for entry in json.loads((CORPUS / f"{model}.json").read_text(encoding="utf-8"))]


def plain(value: Any) -> Any:
    """pyBYD's result as plain data."""
    if isinstance(value, BaseModel):
        return value.model_dump(mode="json")
    if isinstance(value, (list, tuple)):
        return [plain(item) for item in value]
    if isinstance(value, dict):
        return {str(int(key)) if not isinstance(key, str) else key: plain(item) for key, item in value.items()}
    return value


def described(error: BaseException) -> dict[str, Any]:
    return {
        "class": type(error).__name__,
        "code": getattr(error, "code", None),
        "status_code": getattr(error, "status_code", None),
    }


async def outcome(call: Awaitable[Any]) -> dict[str, Any]:
    try:
        return {"result": plain(await call)}
    except Exception as error:  # noqa: BLE001 - the class is compared
        return {"error": described(error)}


Setup = Callable[[Any], None]
Call = Callable[[Any, str], Awaitable[Any]]


def compare(setup: Setup, call: Call, *, prepare: Call | None = None) -> dict[str, Any]:
    """Runs ``call`` on both clients against one fake server; returns openbyd's outcome."""

    async def main() -> tuple[dict, dict]:
        with FakeServer([(USER, PASSWORD)]) as server:
            server.reply(VEHICLES, Reply.data([VEHICLE]))
            server.reply(LATEST_CONFIG, Reply.data(ALL_FUNCTIONS))
            server.reply(VERIFY, Reply.data({"ok": True}))
            setup(server)
            config = {"username": USER, "password": PASSWORD, "base_url": server.url, "mqtt_enabled": False, "control_pin": PIN}
            results = []
            for library, client in (
                ("pybyd", pybyd.BydClient(pybyd.BydConfig(**config))),
                ("openbyd", openbyd.Client(**config)),
            ):
                async with client:
                    await client.login()
                    if prepare is not None:
                        await prepare(client, library)
                    results.append(await outcome(call(client, library)))
            return results[0], results[1]

    expected, got = asyncio.run(main())
    assert json.dumps(got) == json.dumps(expected), f"pyBYD: {expected}\nopenbyd: {got}"
    return got


def data(endpoint: str, payload: Any, **more: Any) -> Setup:
    def setup(server: Any) -> None:
        server.reply(endpoint, Reply.data(payload))
        for other, reply in more.items():
            server.reply(other.replace("__", "/"), reply)

    return setup


def failing(endpoint: str, reply: Any) -> Setup:
    def setup(server: Any) -> None:
        server.reply(endpoint, reply)

    return setup


# ---------------------------------------------------------------------------
# Reads
# ---------------------------------------------------------------------------

SIMPLE_READS = [
    ("get_hvac_status", "/control/getStatusNow", "HvacStatus"),
    ("get_charging_status", "/control/smartCharge/homePage", "ChargingStatus"),
    ("get_charging_schedule", "/control/smartCharge/homePage", "ChargingStatus"),
    ("get_charging_homepage", "/control/smartCharge/homePage", "ChargingStatus"),
    ("get_energy_consumption", "/vehicleInfo/vehicle/getEnergyConsumption", "EnergyConsumption"),
    ("get_push_state", "/vehicle/vehicleswitch/getPushSwitchState", "PushNotificationState"),
    ("get_booking_list", "/control/getBookingList", "BookingList"),
    ("get_ac_current_limit", "/control/smartCharge/getAcCurrentLimit", "AcCurrentLimit"),
]
READ_CASES = [(method, endpoint, payload) for method, endpoint, model in SIMPLE_READS for payload in corpus(model)]


@pytest.mark.parametrize(("method", "endpoint", "payload"), READ_CASES, ids=[f"{m}-{i}" for i, (m, _, _) in enumerate(READ_CASES)])
def test_simple_reads(method: str, endpoint: str, payload: Any) -> None:
    got = compare(data(endpoint, payload), lambda client, _: getattr(client, method)(VIN))
    assert "result" in got


@pytest.mark.parametrize("method", [method for method, _, _ in SIMPLE_READS])
@pytest.mark.parametrize("reply", range(len(ERRORS)))
def test_simple_read_errors(method: str, reply: int) -> None:
    endpoint = dict((m, e) for m, e, _ in SIMPLE_READS)[method]
    got = compare(failing(endpoint, ERRORS[reply]), lambda client, _: getattr(client, method)(VIN))
    assert "error" in got


@pytest.mark.parametrize("payload", corpus("Vehicle"))
def test_get_vehicles(payload: Any) -> None:
    compare(data(VEHICLES, [payload, VEHICLE]), lambda client, _: client.get_vehicles())


@pytest.mark.parametrize("payload", corpus("VehicleLatestConfig"))
def test_latest_config_and_capabilities(payload: Any) -> None:
    setup = data(LATEST_CONFIG, {VIN: payload})
    compare(setup, lambda client, _: client.get_latest_configs([VIN]))
    compare(setup, lambda client, _: client.get_latest_config(VIN))
    compare(setup, lambda client, _: client.get_vehicle_capabilities(VIN))
    compare(setup, lambda client, _: client.get_vehicle_capabilities(VIN, force_refresh=True))
    compare(data(LATEST_CONFIG, {}), lambda client, _: client.get_latest_config(VIN))


@pytest.mark.parametrize("payload", corpus("VehicleRealtimeData"))
def test_realtime(payload: Any) -> None:
    ready = {**payload, "requestSerial": "S1", "time": payload.get("time", 1700000000)}
    setup = data(
        "/vehicleInfo/vehicle/vehicleRealTimeRequest",
        ready,
        __vehicleInfo__vehicle__vehicleRealTimeResult=Reply.data(ready),
    )
    compare(setup, lambda client, _: client.get_vehicle_realtime(VIN, **FAST))
    # Not ready: one poll of the result endpoint.
    pending = data(
        "/vehicleInfo/vehicle/vehicleRealTimeRequest",
        {"requestSerial": "S1"},
        __vehicleInfo__vehicle__vehicleRealTimeResult=Reply.data(ready),
    )
    compare(pending, lambda client, _: client.get_vehicle_realtime(VIN, **FAST))


@pytest.mark.parametrize("payload", corpus("GpsInfo"))
def test_gps(payload: Any) -> None:
    setup = data("/control/getGpsInfo", {"requestSerial": "S1"}, __control__getGpsInfoResult=Reply.data(payload))
    compare(setup, lambda client, _: client.get_gps_info(VIN, **FAST))
    compare(data("/control/getGpsInfo", payload), lambda client, _: client.get_gps_info(VIN, **FAST))


@pytest.mark.parametrize("reply", range(len(ERRORS)))
def test_polled_read_errors(reply: int) -> None:
    for endpoint, method in (
        ("/control/getGpsInfo", "get_gps_info"),
        ("/vehicleInfo/vehicle/vehicleRealTimeRequest", "get_vehicle_realtime"),
    ):
        compare(failing(endpoint, ERRORS[reply]), lambda client, _, method=method: getattr(client, method)(VIN, **FAST))


def test_notifications() -> None:
    messages = corpus("VehicleMsgListResponse")[0]
    setup = data("/app/messageCenter/getUnReadMessageList", messages)
    compare(setup, lambda client, _: client.get_notifications())
    compare(setup, lambda client, _: client.get_notifications(2))
    compare(setup, lambda client, _: client.get_all_notifications())
    compare(failing("/app/messageCenter/getUnReadMessageList", Reply.code("1001")), lambda client, _: client.get_all_notifications())


def test_settings_and_push_switches() -> None:
    compare(data("/control/vehicle/modifyAutoAlias", {}), lambda client, _: client.rename_vehicle(VIN, name="Seal"))
    compare(data("/control/vehicle/modifyAutoAlias", {"result": 1}), lambda client, _: client.rename_vehicle(VIN, name="比亚迪"))
    for push_type in (701, 702):
        compare(
            data("/vehicle/vehicleswitch/setPushSwitchState", {}),
            lambda client, _, push_type=push_type: client.set_push_state(VIN, enable=True, push_type=push_type),
        )
    compare(failing("/vehicle/vehicleswitch/setPushSwitchState", Reply.code("1001")), lambda client, _: client.set_push_state(VIN, enable=False))
    compare(data("/control/smartCharge/changeChargeStatue", {}), lambda client, _: client.toggle_smart_charging(VIN, enable=True))
    for energy_type in (None, 0, 2, "1"):
        compare(
            data("/control/smartCharge/getAcCurrentLimit", corpus("AcCurrentLimit")[0]),
            lambda client, _, energy_type=energy_type: client.get_ac_current_limit(VIN, energy_type=energy_type),
        )


def test_sessions() -> None:
    async def ensure(client: Any, library: str) -> Any:
        session = await client.ensure_session()
        fields = session.model_dump() if library == "pybyd" else session
        # The tokens differ per login; their presence and the rest compare.
        return {key: fields[key] if key in ("user_id", "ttl") else bool(fields[key]) for key in ("user_id", "sign_token", "encry_token", "ttl")}

    compare(lambda server: None, ensure)

    async def invalidated(client: Any, _: str) -> Any:
        client.invalidate_session()
        return await client.get_vehicles()

    compare(lambda server: None, invalidated)

    async def expired(client: Any, _: str) -> Any:
        return client.commands_enabled, await client.get_vehicles()

    compare(lambda server: server.expire_sessions(USER), expired)
    compare(lambda server: None, lambda client, _: asyncio.sleep(0, client.get_command_ack_diagnostics()))


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------

SIMPLE_COMMANDS = ["lock", "unlock", "stop_climate", "flash_lights", "close_windows", "open_windows", "find_car", "open_trunk", "close_trunk"]
PARAM_COMMANDS = {
    "start_climate": (control.ClimateStartParams, [{}, {"temperature": 21.0}, {"temperature": 25.5, "time_span": 2}]),
    "schedule_climate": (control.ClimateScheduleParams, [{"booking_id": 1, "booking_time": 1700000000}]),
    "set_seat_climate": (control.SeatClimateParams, [{"chair_type": "1", "main_heat": 3}, {"chair_type": "3", "lr_seat_heat_state": 2}]),
    "set_battery_heat": (control.BatteryHeatParams, [{"on": True}, {"on": False}]),
}
DONE = {"controlState": 1, "requestSerial": "S1", "res": 2}
COMMAND_REPLIES = [
    Reply.data(DONE),
    Reply.data({"controlState": 2, "requestSerial": "S1", "message": "refused"}),
    Reply.data({"requestSerial": ""}),
    Reply.code("5005"),
    Reply.code("5006"),
    Reply.code("1009"),
    Reply.code("1001"),
    Reply.status(500),
]


async def verified(client: Any, _: str) -> None:
    await client.verify_command_access(VIN)


def command_setup(reply: Any) -> Setup:
    def setup(server: Any) -> None:
        for endpoint in (REMOTE, BATTERY):
            server.reply(endpoint, reply)
        for endpoint in (REMOTE_RESULT, BATTERY + "Result"):
            server.reply(endpoint, Reply.data(DONE))

    return setup


# Refusals that pyBYD retries (command_retries=3, with real pauses) take
# seconds each, so every command meets one of them and lock meets them all.
COMMAND_MATRIX = [(method, reply) for method in SIMPLE_COMMANDS for reply in (0, 1, 3)] + [
    ("lock", reply) for reply in range(len(COMMAND_REPLIES)) if reply not in (0, 1, 3)
]


@pytest.mark.parametrize(("method", "reply"), COMMAND_MATRIX)
def test_simple_commands(method: str, reply: int) -> None:
    compare(command_setup(COMMAND_REPLIES[reply]), lambda client, _: getattr(client, method)(VIN), prepare=verified)


@pytest.mark.parametrize("method", sorted(PARAM_COMMANDS))
def test_commands_with_params(method: str) -> None:
    model, cases = PARAM_COMMANDS[method]
    for fields in cases:
        for reply in (COMMAND_REPLIES[0], COMMAND_REPLIES[3]):

            def call(client: Any, library: str, fields: dict = fields) -> Awaitable[Any]:
                if library == "pybyd":
                    return getattr(client, method)(VIN, params=model(**fields))
                return getattr(client, method)(VIN, **fields)

            compare(command_setup(reply), call, prepare=verified)
            # A dict given as params works the same.
            if fields and reply is COMMAND_REPLIES[0]:
                compare(
                    command_setup(reply),
                    lambda client, library, fields=fields: getattr(client, method)(VIN, params=model(**fields) if library == "pybyd" else dict(fields)),
                    prepare=verified,
                )


@pytest.mark.parametrize(
    ("method", "fields"),
    [
        ("start_climate", {"temperature": "hot"}),
        ("start_climate", {"temperature": 99}),
        ("start_climate", {"time_span": 9}),
        ("start_climate", {"unknown": 1}),
        ("set_seat_climate", {"chair_type": "x"}),
        ("set_seat_climate", {"main_heat": 9}),
        ("set_battery_heat", {}),
        ("set_battery_heat", {"on": "maybe"}),
        ("schedule_climate", {"booking_id": "x"}),
    ],
)
def test_command_params_are_validated_as_pybyd_does(method: str, fields: dict) -> None:
    model = PARAM_COMMANDS[method][0]

    def call(client: Any, library: str) -> Awaitable[Any]:
        if library == "pybyd":
            try:
                params = model(**fields)
            except Exception as error:  # noqa: BLE001 - compared as an outcome
                failure = error

                async def raised() -> Any:
                    raise failure

                return raised()
            return getattr(client, method)(VIN, params=params)
        return getattr(client, method)(VIN, **fields)

    got = compare(command_setup(COMMAND_REPLIES[0]), call, prepare=verified)
    if "error" in got and got["error"]["class"] == "ValidationError":
        # openbyd raises its own ValidationError, a ValueError like pydantic's.
        assert issubclass(openbyd.ValidationError, ValueError)


def test_verify_and_pins() -> None:
    compare(command_setup(COMMAND_REPLIES[0]), lambda client, _: client.verify_command_access(VIN))
    compare(command_setup(COMMAND_REPLIES[0]), lambda client, _: client.verify_command_access(VIN, command_pwd="0123456789abcdef0123456789ABCDEF"))
    compare(lambda server: server.reply(VERIFY, Reply.code("5005")), lambda client, _: client.verify_command_access(VIN))
    # Commands before verification are refused without a request.
    compare(command_setup(COMMAND_REPLIES[0]), lambda client, _: client.lock(VIN))
    compare(command_setup(COMMAND_REPLIES[0]), lambda client, _: client.lock(VIN, command_pwd=" 9999 "), prepare=verified)
    # The gate: a car without the function refuses without sending.
    compare(
        lambda server: server.reply(LATEST_CONFIG, Reply.data({VIN: {"cfFixedList": []}})),
        lambda client, _: client.lock(VIN),
        prepare=verified,
    )


CHARGE_REPLIES = [
    Reply.data({"res": 2, "requestSerial": "S1"}),
    Reply.data({"res": 3, "requestSerial": "S1", "message": "already"}),
    Reply.data({"res": 4, "requestSerial": "S1"}),
    Reply.code("1001"),
]


@pytest.mark.parametrize("reply", range(len(CHARGE_REPLIES)))
def test_charging_commands(reply: int) -> None:
    def setup(server: Any) -> None:
        for endpoint in (CHARGE_TOGGLE, CHARGE_SAVE):
            server.reply(endpoint, CHARGE_REPLIES[reply])
        server.reply(CHARGE_RESULT, Reply.data({"requestSerial": "S1"}))

    compare(setup, lambda client, _: client.start_charging(VIN, **FAST))
    compare(setup, lambda client, _: client.stop_charging(VIN))
    compare(
        setup,
        lambda client, _: client.save_charging_schedule(
            VIN, start_charge_time="22:00", end_charge_time="full", charge_way="0,1,2", **FAST
        ),
    )
    compare(
        setup,
        lambda client, _: client.save_charging_schedule(
            VIN, start_charge_time="00:30", end_charge_time="06:00", charge_way="e", enabled=False, **FAST
        ),
    )
