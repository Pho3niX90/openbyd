"""The surface of the bindings: what every language's package offers.

One description, from which `generate.py` writes the typed layer of the
Node, JVM and .NET packages. The shapes of pyBYD's models are not written
here: they are read from the pyBYD installed (the commit in
`parity/UPSTREAM_COMMIT`), so the types follow upstream.

To add a method: add it to `openbyd-host` (`crates/openbyd-host/src/client.rs`),
describe it here, and run `python bindings/generate.py`.
"""

from __future__ import annotations

import dataclasses
import enum
import importlib
import inspect
import pkgutil
from pathlib import Path
from typing import Any

from gen.ir import (
    ANY,
    BOOL,
    FLOAT,
    INT,
    OBJECT,
    STR,
    Enum,
    ErrorClass,
    Event,
    Feature,
    Field,
    Member,
    Method,
    SchemaReader,
    Struct,
    Surface,
    Type,
    enum_from_class,
    list_of,
    map_of,
    opt,
    ref,
)

ROOT = Path(__file__).resolve().parents[1]

#: pyBYD models that are not part of the surface: bases, internals and Python-only helpers.
SKIPPED_MODELS = {"BydBaseModel", "ControlParams", "AuthToken", "PlanClimate", "PlanSeats"}

#: pyBYD's parameter models: built by the caller, never returned.
INPUT_MODELS = {
    "ClimateStartParams",
    "ClimateScheduleParams",
    "SeatClimateParams",
    "BatteryHeatParams",
    "StandbySchedule",
}

#: pyBYD output fields whose type openbyd widens, as (type, doc): what openbyd
#: answers that pyBYD's annotation does not cover.
OUTPUT_FIELD_TYPES: dict[str, dict[str, tuple[Type, str]]] = {
    "StandbyPlanList": {
        # A save keeps BYD's message as sent (pyBYD reads it from the reply
        # with `.get`), so it can be any JSON value, not only a string.
        "message": (
            opt(ANY),
            "BYD's message about the list: usually text, but after a save the reply's `message` as BYD sent it "
            "(any JSON value). Null when there is none.",
        ),
    },
}

#: Extra fields openbyd adds to pyBYD's parameter models.
EXTRA_INPUT_FIELDS: dict[str, tuple[Field, ...]] = {
    "StandbySchedule": (
        Field(
            "zone",
            opt(STR),
            "IANA time zone name (such as `Europe/Madrid`) for a daily schedule, so the next run follows "
            "that zone's wall clock across daylight-saving changes. Ignored on other schedules; an openbyd addition.",
            required=False,
            default=None,
        ),
    ),
}


def version() -> str:
    """The workspace's version, which every package carries."""
    for line in (ROOT / "Cargo.toml").read_text(encoding="utf-8").splitlines():
        if line.startswith("version = "):
            return line.split('"')[1]
    raise RuntimeError("Cargo.toml has no version")


# ---------------------------------------------------------------------------
# pyBYD's models
# ---------------------------------------------------------------------------


def add_pybyd_models(surface: Surface) -> None:
    """Every public model and enum of ``pybyd.models``, the config, and the seat enums."""
    import pybyd.config
    import pybyd.models
    from pydantic import BaseModel

    reader = SchemaReader()
    for info in pkgutil.iter_modules(pybyd.models.__path__):
        module = importlib.import_module(f"pybyd.models.{info.name}")
        name = info.name.lstrip("_")
        for cls_name, cls in vars(module).items():
            if not inspect.isclass(cls) or cls.__module__ != module.__name__ or cls_name.startswith("_"):
                continue
            if cls_name in SKIPPED_MODELS or cls_name == "BydEnum":
                continue
            if issubclass(cls, BaseModel):
                role = "input" if cls_name in INPUT_MODELS else "output"
                struct = reader.struct(cls, role, name)
                extra = EXTRA_INPUT_FIELDS.get(cls_name)
                if extra:
                    struct = dataclasses.replace(struct, fields=(*struct.fields, *extra))
                widened = OUTPUT_FIELD_TYPES.get(cls_name, {})
                if widened:
                    names = {entry.name for entry in struct.fields}
                    assert set(widened) <= names, f"{cls_name}: no field {sorted(set(widened) - names)}"
                    struct = dataclasses.replace(struct, fields=tuple(
                        dataclasses.replace(entry, type=widened[entry.name][0], doc=widened[entry.name][1])
                        if entry.name in widened else entry
                        for entry in struct.fields
                    ))
                surface.add(struct)
            elif issubclass(cls, enum.Enum):
                surface.add(enum_from_class(cls, name))
    for cls in (pybyd.config.DeviceProfile, pybyd.config.BydConfig):
        surface.add(reader.struct(cls, "input", "config"))
    for generated in reader.enums.values():
        surface.add(generated)
    surface.add(enum_from_class(pybyd.SeatPosition))
    surface.add(enum_from_class(pybyd.SeatLevel))
    missing = reader.referenced - set(surface.structs) - set(surface.enums)
    assert not missing, f"models refer to {sorted(missing)}, which are not exported"


# ---------------------------------------------------------------------------
# openbyd's own shapes
# ---------------------------------------------------------------------------


def output(name: str, doc: str, *fields: Field) -> Struct:
    return Struct(name, doc, fields)


def add_openbyd_shapes(surface: Surface) -> None:
    surface.add(
        Enum(
            "SeatMode",
            "What a seat can do: heat or ventilate.",
            "str",
            (Member("HEAT", "heat"), Member("VENTILATION", "ventilation")),
        )
    )
    surface.add(
        output(
            "SessionInfo",
            "The session a client is using: pyBYD's `Session` fields.",
            Field("user_id", STR, "The account's user ID."),
            Field("sign_token", STR, "The token requests are signed with. A secret."),
            Field("encry_token", STR, "The token the content key derives from. A secret."),
            Field("created_at", FLOAT, "When the session was made, in the client's monotonic seconds."),
            Field("ttl", FLOAT, "Seconds the session lives; infinite for no expiry."),
        )
    )
    surface.add(
        output(
            "SessionTokens",
            "A session as an application stores it, to resume later without a password login. "
            "Give it back unchanged to `import_session`.",
            Field("user_id", STR, "The account's user ID."),
            Field("sign_token", STR, "The token requests are signed with. A secret."),
            Field("encry_token", STR, "The token the content key derives from. A secret."),
            Field(
                "refresh_token",
                STR,
                "The token that buys new tokens when BYD answers 1030; empty when the login gave none. "
                "A secret. Optional on import: older records without it still import.",
                required=False,
                default="",
            ),
            Field("ttl_remaining", opt(FLOAT), "Seconds the session has left; null for no expiry."),
            Field("guest_vins", map_of(BOOL), "Whether the account reaches each VIN as a guest."),
        )
    )
    surface.add(
        output(
            "RequestRecord",
            "One request the client sent, for metrics.",
            Field("endpoint", STR, "The endpoint, such as `/control/getGpsInfo`."),
            Field("outcome", STR, "`reply` (the API answered), `transport` (the HTTP request failed) or `failed`."),
            Field("code", opt(STR), "The reply's code; `0` is success. Null unless the API answered."),
            Field("status", opt(INT), "The HTTP status of a transport failure, when the server answered."),
            Field("error", opt(STR), "The error class of any other failure."),
            Field("duration", FLOAT, "How long the request took, in seconds."),
        )
    )
    surface.add(
        output(
            "ChargingHomepage",
            "Live charging state and the charging schedule, from one call.",
            Field("status", ref("ChargingStatus"), "The live charging state."),
            Field("schedule", ref("SmartChargingSchedule"), "The smart-charging schedule."),
        )
    )
    surface.add(
        output(
            "VehicleSnapshot",
            "A car's merged state, by section, with the projections of commands in flight laid over it. "
            "A section is null until its first data arrived.",
            Field("vehicle", ref("Vehicle"), "The vehicle's metadata."),
            Field("realtime", opt(ref("VehicleRealtimeData")), "Doors, locks, charge, range, tyres."),
            Field("hvac", opt(ref("HvacStatus")), "Climate control."),
            Field("gps", opt(ref("GpsInfo")), "Location."),
            Field("charging", opt(ref("ChargingStatus")), "Live charging state."),
            Field("energy", opt(ref("EnergyConsumption")), "Energy consumption."),
            Field("charging_schedule", opt(ref("SmartChargingSchedule")), "The smart-charging schedule."),
        )
    )
    available = Field("available", BOOL, "Whether the car supports it.")
    surface.add(output("FeatureAvailability", "Whether a car has a feature.", available))
    surface.add(
        output(
            "HvacFeatures",
            "What a car's climate control offers.",
            Field("available", BOOL, "Whether the car supports remote climate control."),
            Field("temp_offset", FLOAT, "°C offset of the car's 1-17 temperature scale."),
            Field("temp_range", list_of(FLOAT), "The settable temperature range in °C: lowest, highest."),
        )
    )
    surface.add(
        output(
            "SeatAvailability",
            "What one seat offers.",
            Field("heat", BOOL, "Whether the seat can be heated."),
            Field("ventilation", BOOL, "Whether the seat can be ventilated."),
        )
    )
    surface.add(
        output(
            "SeatFeatures",
            "What each seat offers.",
            *(Field(seat, ref("SeatAvailability")) for seat in ("driver", "copilot", "rear_left", "rear_right")),
        )
    )
    surface.add(
        output(
            "FinderFeatures",
            "Whether a car can be found and can flash its lights.",
            Field("find_available", BOOL, "Whether the car supports find-my-car."),
            Field("flash_available", BOOL, "Whether the car can flash its lights remotely."),
        )
    )
    surface.add(
        output(
            "WindowsFeatures",
            "Whether a car's windows can be closed and opened remotely.",
            Field("close_available", BOOL, "Whether the car can close its windows remotely."),
            Field("open_available", BOOL, "Whether the car can open its windows remotely (to a vent crack)."),
        )
    )
    surface.add(
        output(
            "TrunkFeatures",
            "Whether a car's trunk can be opened and closed remotely.",
            Field("open_available", BOOL, "Whether the trunk can be opened remotely."),
            Field("close_available", BOOL, "Whether the trunk can be closed remotely."),
        )
    )
    surface.add(
        output(
            "CarFeatures",
            "What each feature of a car offers.",
            Field("lock", ref("FeatureAvailability")),
            Field("hvac", ref("HvacFeatures")),
            Field("seat", ref("SeatFeatures")),
            Field("steering", ref("FeatureAvailability")),
            Field("battery", ref("FeatureAvailability")),
            Field("finder", ref("FinderFeatures")),
            Field("windows", ref("WindowsFeatures")),
            Field("trunk", ref("TrunkFeatures")),
        )
    )


# ---------------------------------------------------------------------------
# Methods
# ---------------------------------------------------------------------------

VIN = Field("vin", STR, "The vehicle identification number.")


def option(name: str, kind: Type, default: Any, doc: str) -> Field:
    return Field(name, kind, doc, required=False, default=default)


POLL = (
    option("poll_attempts", INT, 10, "HTTP polls after the MQTT wait."),
    option("poll_interval", FLOAT, 1.5, "Seconds slept before each poll."),
    option("mqtt_timeout", opt(FLOAT), None, "Seconds to wait for the MQTT push; the config's `mqtt_timeout` when null."),
    option("signal_retries", INT, 2, '"Data unavailable" answers in a row that end the polling.'),
)
CHARGE_POLL = (
    option("mqtt_timeout", opt(FLOAT), None, "Seconds to wait for the MQTT push; the config's `mqtt_timeout` when null."),
    option("poll_attempts", INT, 10, "HTTP polls after the MQTT wait."),
    option(
        "poll_interval",
        opt(FLOAT),
        None,
        "Seconds slept before each poll; null for the official app's 33 s ladder (1, 0.5, 0.5, 2, 2, 2, 5, 5, 5, 10).",
    ),
)

COMMAND = (
    option(
        "command_pwd",
        opt(STR),
        None,
        "The control PIN for this command; the config's `control_pin` when null.",
    ),
)
OTA_UPGRADE = (
    option("auto_upgrade_time", STR, "2:00", "When the night upgrade runs, `H:mm`."),
    *COMMAND,
)
SCHEDULE = (
    Field("start_charge_time", STR, "When charging starts, `HH:MM`."),
    Field("end_charge_time", STR, "When charging ends, `HH:MM`, or `full`."),
    Field("charge_way", STR, "`s` (single), `e` (every day) or weekday indices."),
)


def read(name: str, returns: Type, doc: str, *, optional: tuple[Field, ...] = (), options_name: str = "") -> Method:
    return Method(name, name, doc, (VIN,), optional, options_name, returns)


def command(name: str, doc: str) -> Method:
    return Method(name, name, doc, (VIN,), COMMAND, "CommandOptions", ref("RemoteControlResult"))


def with_params(name: str, model: str, doc: str) -> Method:
    params = (VIN, Field("params", ref(model), f"The `{model}` fields."))
    return Method(name, name, doc, params, COMMAND, "CommandOptions", ref("RemoteControlResult"))


def client_methods() -> list[Method]:
    return [
        # Sessions
        Method("login", "login", "Log in with the password and start MQTT."),
        Method(
            "ensure_session",
            "ensure_session",
            "The current session, logging in first when there is none or it expired.",
            returns=ref("SessionInfo"),
        ),
        Method(
            "invalidate_session",
            "invalidate_session",
            "Forget the session; the next call logs in again. Command access stays verified.",
            sync=True,
        ),
        Method(
            "export_session",
            "export_session",
            "The session as an application stores it, or null when logged out. Give it to "
            "`import_session` to resume without a password login.",
            returns=opt(ref("SessionTokens")),
            sync=True,
        ),
        Method(
            "import_session",
            "import_session",
            "Use a session from `export_session` as if a login had returned it, and start MQTT.",
            (Field("session", ref("SessionTokens"), "The saved session."),),
        ),
        Method(
            "commands_enabled",
            "commands_enabled",
            "Whether remote commands are available: true once `verify_command_access` succeeded.",
            returns=BOOL,
            sync=True,
            prop=True,
        ),
        Method(
            "mqtt_is_running",
            "mqtt_is_running",
            "Whether the MQTT push channel is connected or connecting.",
            returns=BOOL,
            sync=True,
            prop=True,
        ),
        Method(
            "start_mqtt", "start_mqtt", "Start MQTT for the current session if it is not running, as a login does."
        ),
        Method(
            "get_command_ack_diagnostics",
            "get_command_ack_diagnostics",
            "Counters of command acknowledgements: pending, matched, expired, uncorrelated.",
            returns=ref("CommandAckDiagnostics"),
            sync=True,
        ),
        # Vehicles
        Method("get_vehicles", "get_vehicles", "The account's vehicles.", returns=list_of(ref("Vehicle"))),
        Method(
            "seed_vehicle_metadata",
            "seed_vehicle_metadata",
            "Cache a vehicle's metadata as if the vehicle list had returned it, so realtime, energy "
            "and AC-limit reads need no vehicle list.",
            (VIN, Field("energy_type", ref("EnergyType"), "The powertrain: EV, ICE or hybrid.")),
            (option("model_name", opt(STR), None, "The model's name."),),
            "SeedOptions",
            sync=True,
        ),
        Method(
            "vehicle_metadata",
            "vehicle_metadata",
            "The cached metadata of a vehicle, or null when unknown.",
            (VIN,),
            returns=opt(ref("Vehicle")),
            sync=True,
        ),
        Method(
            "get_latest_configs",
            "get_latest_configs",
            "The latest config of each VIN the cloud knows, by VIN.",
            (Field("vins", list_of(STR), "The vehicles to ask about."),),
            returns=map_of(ref("VehicleLatestConfig")),
        ),
        read(
            "get_latest_config",
            ref("VehicleLatestConfig"),
            "The latest config of a vehicle. Fails with `BydDataUnavailableError` when the reply has none.",
        ),
        read(
            "get_vehicle_capabilities",
            ref("VehicleCapabilities"),
            "What the vehicle supports, cached. A failed fetch gives unknown capabilities.",
            optional=(option("force_refresh", BOOL, False, "Fetch again even when cached."),),
            options_name="CapabilitiesOptions",
        ),
        # Reads
        read(
            "get_vehicle_realtime",
            ref("VehicleRealtimeData"),
            "Realtime data: trigger a refresh, wait for the MQTT push, poll over HTTP if it does not come.",
            optional=POLL,
            options_name="PollOptions",
        ),
        read(
            "get_gps_info",
            ref("GpsInfo"),
            "The vehicle's position: trigger, wait for the push, poll if it does not come.",
            optional=POLL,
            options_name="PollOptions",
        ),
        read("get_hvac_status", ref("HvacStatus"), "Climate status."),
        read("get_charging_status", ref("ChargingStatus"), "Charging status."),
        read("get_charging_schedule", ref("SmartChargingSchedule"), "The smart-charging schedule."),
        read(
            "get_charging_homepage",
            ref("ChargingHomepage"),
            "Live charging state and the schedule, from one call.",
        ),
        read(
            "get_ac_current_limit",
            ref("AcCurrentLimit"),
            "The supported and active AC charging current limits.",
            optional=(option("energy_type", opt(ref("EnergyType")), None, "The powertrain; the vehicle's own when null."),),
            options_name="AcCurrentLimitOptions",
        ),
        read(
            "get_energy_consumption",
            ref("EnergyConsumption"),
            "Energy consumption, asked with the vehicle's energy type and model.",
        ),
        read("get_push_state", ref("PushNotificationState"), "Push notification state."),
        Method(
            "set_push_state",
            "set_push_state",
            "Turn a push notification type on or off.",
            (VIN, Field("enable", BOOL, "On or off.")),
            (option("push_type", INT, 701, "The notification type; 701 is the vehicle status push."),),
            "PushStateOptions",
            ref("CommandAck"),
        ),
        Method(
            "get_notifications",
            "get_notifications",
            "The account's unread inbox messages of one category.",
            (),
            (option("category", ref("NotificationCategory"), 1, "Vehicle or system messages."),),
            "NotificationOptions",
            list_of(ref("VehicleMsgItemInfo")),
        ),
        Method(
            "get_all_notifications",
            "get_all_notifications",
            "The unread inbox messages of every category, by category value.",
            returns=map_of(list_of(ref("VehicleMsgItemInfo")), key="int"),
        ),
        Method(
            "rename_vehicle",
            "rename_vehicle",
            "Rename the vehicle.",
            (VIN, Field("name", STR, "The new name.")),
            returns=ref("CommandAck"),
        ),
        read("get_booking_list", ref("BookingList"), "The scheduled-climate timers."),
        # Firmware and guest grants
        read("get_ota_version", ref("OtaUpgradeInfo"), "The OTA firmware version, release notes and night auto-upgrade settings."),
        Method(
            "set_ota_auto_upgrade",
            "set_ota_auto_upgrade",
            "Turn the night auto-upgrade on or off and set its time. Needs command access and the control PIN.",
            (VIN, Field("enabled", BOOL, "Whether the night auto-upgrade is on.")),
            OTA_UPGRADE,
            "OtaUpgradeOptions",
            ref("CommandAck"),
        ),
        Method(
            "get_empower_detail",
            "get_empower_detail",
            "A shared guest's grant in detail. Fails with `BydEndpointNotSupportedError` (code `not_shared_vehicle`) "
            "when the car has no grant ID.",
            (VIN,),
            (option("empower_id", opt(INT), None, "The grant ID; the cached vehicle's when null."),),
            "EmpowerDetailOptions",
            ref("EmpowerDetail"),
        ),
        # Preconditioning plans
        read("get_standby_plans", ref("StandbyPlanList"), "The preconditioning plans for a vehicle."),
        Method(
            "save_standby_plan",
            "save_standby_plan",
            "Save a preconditioning plan's name, commands and schedule, then return the fresh plans.",
            (VIN, Field("plan", ref("StandbyPlan"), "The plan to save.")),
            (
                option(
                    "schedule",
                    opt(ref("StandbySchedule")),
                    None,
                    "The schedule to save; null keeps the plan's current schedule on save, or leaves a new plan unscheduled.",
                ),
            ),
            "StandbyScheduleOptions",
            ref("StandbyPlanList"),
        ),
        Method(
            "create_standby_plan",
            "create_standby_plan",
            "Create a user preconditioning plan and return it.",
            (
                VIN,
                Field("name", STR, "The plan name."),
                Field("commands", list_of(ref("StandbyPlanCommand")), "The remote commands in the plan; must include `OPENAIR`."),
            ),
            (
                option(
                    "schedule",
                    opt(ref("StandbySchedule")),
                    None,
                    "The schedule to save; null keeps the plan's current schedule on save, or leaves a new plan unscheduled.",
                ),
            ),
            "StandbyScheduleOptions",
            ref("StandbyPlan"),
        ),
        Method(
            "set_default_standby_plan",
            "set_default_standby_plan",
            "Make a preconditioning plan the default, then return the fresh plans.",
            (VIN, Field("plan_id", STR, "The plan ID.")),
            returns=ref("StandbyPlanList"),
        ),
        Method(
            "delete_standby_plan",
            "delete_standby_plan",
            "Delete a user preconditioning plan, then return the fresh plans.",
            (VIN, Field("plan_id", STR, "The plan ID.")),
            returns=ref("StandbyPlanList"),
        ),
        Method(
            "run_standby_plan",
            "run_standby_plan",
            "Run a preconditioning plan now without changing its schedule.",
            (VIN, Field("plan_id", STR, "The plan ID.")),
            (
                option("poll_attempts", INT, 10, "How many times `send-result` is polled."),
                option("poll_interval", FLOAT, 1.5, "Seconds between polls."),
            ),
            "RunStandbyPlanOptions",
            ref("StandbyPlanRunResult"),
        ),
        # Commands
        Method(
            "verify_command_access",
            "verify_command_access",
            "Check the control PIN once and enable remote commands.",
            (VIN,),
            COMMAND,
            "CommandOptions",
            ref("VerifyControlPasswordResponse"),
        ),
        command("lock", "Lock the vehicle."),
        command("unlock", "Unlock the vehicle."),
        command("stop_climate", "Stop climate control."),
        command("flash_lights", "Flash the lights."),
        command("close_windows", "Close all windows."),
        command("open_windows", "Open the windows to a vent crack of about 10 %."),
        command("find_car", "Horn and lights."),
        command("open_trunk", "Release the trunk."),
        command("close_trunk", "Lower the trunk."),
        with_params("start_climate", "ClimateStartParams", "Start climate control."),
        with_params("schedule_climate", "ClimateScheduleParams", "Schedule climate control."),
        with_params("set_seat_climate", "SeatClimateParams", "Set seat heating and ventilation."),
        with_params("set_battery_heat", "BatteryHeatParams", "Turn battery heating on or off."),
        Method(
            "save_charging_schedule",
            "save_charging_schedule",
            "Save a smart-charging schedule and wait for the change to settle.",
            (VIN, *SCHEDULE),
            (option("enabled", BOOL, True, "Whether the schedule is active."), *CHARGE_POLL),
            "SaveScheduleOptions",
            ref("ChargeChangeResult"),
        ),
        Method(
            "toggle_smart_charging",
            "toggle_smart_charging",
            "Turn smart charging on or off.",
            (VIN, Field("enable", BOOL, "On or off.")),
            returns=ref("CommandAck"),
        ),
        read(
            "start_charging",
            ref("ChargeChangeResult"),
            "Start charging now and wait for the change to settle.",
            optional=CHARGE_POLL,
            options_name="ChargePollOptions",
        ),
        read(
            "stop_charging",
            ref("ChargeChangeResult"),
            "Always fails with `BydEndpointNotSupportedError`: the cloud cannot stop a charge (as pyBYD).",
            optional=CHARGE_POLL,
            options_name="ChargePollOptions",
        ),
    ]


def car_methods() -> list[Method]:
    def update(section: str, returns: str, doc: str) -> Method:
        return Method(f"update_{section}", f"car.update_{section}", doc, returns=ref(returns))

    def push(kind: str, event: str, what: str = "") -> Method:
        return Method(
            f"handle_mqtt_{kind}",
            f"car.handle_mqtt_{kind}",
            f"Merge a `{event}` push (its raw `respondData`){what}.",
            (Field("data", OBJECT, "The push's `respondData`."),),
            sync=True,
        )

    return [
        Method(
            "state",
            "car.state",
            "The current state with active projections laid over it, by section.",
            returns=ref("VehicleSnapshot"),
            sync=True,
            prop=True,
        ),
        Method(
            "capabilities",
            "car.capabilities",
            "The car's capability flags.",
            returns=ref("VehicleCapabilities"),
            sync=True,
            prop=True,
        ),
        Method(
            "features",
            "car.features",
            "Every feature's availability, and the climate scale.",
            returns=ref("CarFeatures"),
            sync=True,
            prop=True,
        ),
        update("realtime", "VehicleRealtimeData", "Fetch realtime data and merge it; returns the data as fetched."),
        update("hvac", "HvacStatus", "Fetch the HVAC status and merge it."),
        update("gps", "GpsInfo", "Fetch the location and merge it."),
        update(
            "charging",
            "ChargingStatus",
            "Fetch the charging state and the schedule in one call and merge both; returns the charging state.",
        ),
        update("energy", "EnergyConsumption", "Fetch energy consumption and merge it."),
        Method(
            "start_charging",
            "car.start_charging",
            "Start charging now and wait for the change to settle.",
            returns=ref("ChargeChangeResult"),
        ),
        Method(
            "stop_charging",
            "car.stop_charging",
            "Always fails with `BydEndpointNotSupportedError`: the cloud cannot stop a charge.",
            returns=ref("ChargeChangeResult"),
        ),
        Method(
            "get_ac_current_limit",
            "car.get_ac_current_limit",
            "The supported and active AC charging current limits.",
            returns=ref("AcCurrentLimit"),
        ),
        Method(
            "save_charging_schedule",
            "car.save_charging_schedule",
            "Save a smart-charging schedule, wait for it to settle, then refresh the charging state.",
            SCHEDULE,
            (option("enabled", BOOL, True, "Whether the schedule is active."),),
            "CarScheduleOptions",
            ref("ChargeChangeResult"),
        ),
        Method(
            "get_ota_version",
            "car.get_ota_version",
            "The OTA firmware version and night auto-upgrade settings.",
            returns=ref("OtaUpgradeInfo"),
        ),
        Method(
            "set_ota_auto_upgrade",
            "car.set_ota_auto_upgrade",
            "Turn the night auto-upgrade on or off and set its time.",
            (Field("enabled", BOOL, "Whether the night auto-upgrade is on."),),
            OTA_UPGRADE,
            "OtaUpgradeOptions",
        ),
        push("realtime", "vehicleInfo", ", after any command in flight"),
        Method(
            "handle_mqtt_status_push",
            "car.handle_mqtt_status_push",
            "Merge a partial `vehicleStatusPush` (its raw `respondData`), optionally with HVAC and charging sections.",
            (Field("data", OBJECT, "The push's `respondData`."),),
            (
                option("hvac", opt(OBJECT), None, "The push's raw HVAC section, when present."),
                option("charging", opt(OBJECT), None, "The push's raw charging section, when present."),
            ),
            "StatusPushOptions",
            sync=True,
        ),
        push("charging", "smartCharge", " (live charging state only)"),
        push("energy", "energyConsumption"),
        Method("close", "car.close", "Release the car's resources (nothing yet; pyBYD's lifecycle hook).", sync=True),
    ]


def features() -> list[Feature]:
    def action(
        feature: str, name: str, doc: str, *params: Field, optional: tuple[Field, ...] = (), options_name: str = ""
    ) -> Method:
        return Method(name, f"car.{feature}.{name}", doc, params, optional, options_name)

    position = Field("position", ref("SeatPosition"), "The seat.")
    level = Field("level", ref("SeatLevel"), "Off, low or high.")
    on = Field("on", BOOL, "On or off.")
    return [
        Feature(
            "lock",
            "LockFeature",
            "Lock and unlock the doors.",
            (Field("available", BOOL, "Whether the car supports locking."),),
            (
                action("lock", "lock", "Lock all doors; the door locks read locked at once."),
                action("lock", "unlock", "Unlock all doors; the door locks read unlocked at once."),
            ),
        ),
        Feature(
            "hvac",
            "HvacFeature",
            "Start, stop and schedule climate control.",
            (
                Field("available", BOOL, "Whether the car supports remote climate control."),
                Field("temp_offset", FLOAT, "°C offset of the car's 1-17 temperature scale, from the last HVAC status."),
                Field("temp_range", list_of(FLOAT), "The settable temperature range in °C: lowest, highest."),
            ),
            (
                action(
                    "hvac",
                    "start",
                    "Start climate control, or change a running session's settings. Without a known "
                    "HVAC status the status is fetched first, as pyBYD does.",
                    optional=(
                        option("temperature", FLOAT, 21.0, "Target temperature in °C."),
                        option("duration", INT, 20, "Run time in minutes: 10, 15, 20, 25 or 30."),
                        option("cycle_mode", opt(INT), None, "1 fresh air, 2 recirculate; null keeps a running session's setting."),
                        option("fan_level", opt(INT), None, "Fan speed 1-7; null keeps a running session's speed."),
                    ),
                    options_name="ClimateOptions",
                ),
                action("hvac", "stop", "Stop climate control, including seat and steering wheel heating."),
                action(
                    "hvac",
                    "schedule",
                    "Create, change or remove a climate schedule.",
                    Field("params", ref("ClimateScheduleParams"), "The `ClimateScheduleParams` fields."),
                ),
            ),
        ),
        Feature(
            "seat",
            "SeatFeature",
            "Seat heating and ventilation.",
            (),
            (
                action("seat", "heat", "Set the heating of one seat.", position, level),
                action("seat", "ventilation", "Set the ventilation of one seat.", position, level),
            ),
        ),
        Feature(
            "steering",
            "SteeringFeature",
            "Steering wheel heating.",
            (Field("available", BOOL, "Whether the car has steering wheel heating."),),
            (action("steering", "heat", "Turn steering wheel heating on or off.", on),),
        ),
        Feature(
            "battery",
            "BatteryFeature",
            "Battery heating.",
            (Field("available", BOOL, "Whether the car supports battery heating."),),
            (action("battery", "heat", "Turn battery heating on or off.", on),),
        ),
        Feature(
            "finder",
            "FinderFeature",
            "Find the car, flash the lights.",
            (
                Field("find_available", BOOL, "Whether the car supports find-my-car."),
                Field("flash_available", BOOL, "Whether the car can flash its lights remotely."),
            ),
            (
                action("finder", "find", "Horn and lights."),
                action("finder", "flash_lights", "Flash the lights."),
            ),
        ),
        Feature(
            "windows",
            "WindowsFeature",
            "Close the windows or crack them open.",
            (
                Field("close_available", BOOL, "Whether the car can close its windows remotely."),
                Field("open_available", BOOL, "Whether the car can open its windows remotely (to a vent crack)."),
            ),
            (
                action("windows", "close", "Close all windows."),
                action("windows", "open", "Open the windows to a vent crack of about 10 %."),
            ),
        ),
        Feature(
            "trunk",
            "TrunkFeature",
            "Open and close the trunk.",
            (
                Field("open_available", BOOL, "Whether the trunk can be opened remotely."),
                Field("close_available", BOOL, "Whether the trunk can be closed remotely."),
            ),
            (
                action("trunk", "open", "Release the trunk."),
                action("trunk", "close", "Lower the trunk."),
            ),
        ),
    ]


# ---------------------------------------------------------------------------
# Errors
# ---------------------------------------------------------------------------


def errors() -> list[ErrorClass]:
    code = Field("code", STR, "The API's code, such as `1005`.")
    endpoint = Field("endpoint", STR, "The endpoint that failed.")

    def api(name: str, doc: str, parent: str = "BydApiError") -> ErrorClass:
        return ErrorClass(name, parent, doc, (name,))

    return [
        ErrorClass(
            "OpenBydError",
            "",
            "Base of every error openbyd raises. `kind` is the name of the Python exception class pyBYD raises "
            "for the same failure.",
            ("ScriptExhausted",),
            (Field("kind", STR, "The name of the Python exception class pyBYD raises."),),
        ),
        ErrorClass("BydError", "OpenBydError", "Base of pyBYD's errors.", ("BydError",)),
        ErrorClass("BydCryptoError", "BydError", "Encryption or decryption failed.", ("BydCryptoError",)),
        ErrorClass(
            "BangcleError", "BydCryptoError", "A Bangcle envelope could not be encoded or decoded.", ("BangcleError",)
        ),
        ErrorClass(
            "BangcleTableLoadError",
            "BangcleError",
            "The Bangcle lookup tables could not be loaded.",
            ("BangcleTableLoadError",),
        ),
        ErrorClass(
            "BydTransportError",
            "BydError",
            "HTTP-level failure: the network, a status other than 200, a body that is not JSON.",
            ("BydTransportError",),
            (Field("status_code", opt(INT), "The HTTP status, when the server answered."), endpoint),
        ),
        ErrorClass(
            "BydApiError",
            "BydError",
            "The API answered with a code other than 0.",
            ("BydApiError",),
            (code, endpoint),
        ),
        api("BydAuthenticationError", "Login failed or the session expired."),
        api(
            "BydSessionExpiredError",
            "The server rejected the session token (for example codes `1002`, `1011`).",
            "BydAuthenticationError",
        ),
        api(
            "BydTokenRefreshError",
            "The session token needs a refresh or a new login (code `1030`).",
            "BydSessionExpiredError",
        ),
        api("BydServerTimeError", "The client's clock is too far from BYD's (code `1003`)."),
        api("BydRemoteControlError", "A remote control command failed after the cloud accepted it."),
        api(
            "BydControlPasswordError",
            "A remote control command was rejected for an invalid or locked operation password "
            "(codes `5005`, `5006`, `5011`).",
        ),
        api(
            "BydEndpointNotSupportedError",
            "The endpoint is not supported or not permitted for this vehicle or account (for example code `1001`, "
            "GPS code `8002`, or `guest_permission_denied` for a shared guest).",
        ),
        api(
            "BydDataUnavailableError",
            "The vehicle cannot provide the data right now (for example GPS code `6051`).",
        ),
        api("BydRateLimitError", "A previous command is still in progress (code `6024`)."),
        api("BydServiceBusyError", "The backend is busy (code `1008`); keep the last data and back off."),
        api(
            "BydStandbyScheduleError",
            "A preconditioning plan's run time was refused (code `6111`): a one-off run must be at least 20 minutes "
            "ahead and within 30 days.",
        ),
        ErrorClass(
            "DataError",
            "OpenBydError",
            "A value could not be used: an argument, or a reply that is not what it should be. `kind` names the "
            "Python built-in exception pyBYD lets through (`ValueError`, `TypeError`, `JSONDecodeError`, ...).",
            (
                "ValueError",
                "TypeError",
                "AttributeError",
                "AssertionError",
                "JSONDecodeError",
                "UnicodeDecodeError",
                "KeyError",
                "LookupError",
            ),
        ),
        ErrorClass(
            "ValidationError",
            "DataError",
            "A reply or a parameter did not validate, where pyBYD raises pydantic's `ValidationError`.",
            ("ValidationError",),
            (
                Field("error_type", STR, "pydantic's error type, such as `int_parsing`."),
                Field("loc", list_of(STR), "Where the bad value was, outermost first."),
            ),
        ),
        ErrorClass(
            "CancelledError",
            "OpenBydError",
            "A wait for an MQTT push was cut short because the client stopped its MQTT channel, "
            "for example when it was closed.",
            ("CancelledError",),
        ),
    ]


# ---------------------------------------------------------------------------
# Events and client options
# ---------------------------------------------------------------------------


def events() -> list[Event]:
    return [
        Event(
            "on_vehicle_info",
            "vehicle_info",
            "Every MQTT `vehicleInfo` push, parsed.",
            (VIN, Field("data", ref("VehicleRealtimeData"), "The realtime data.")),
        ),
        Event(
            "on_mqtt_event",
            "mqtt_event",
            "Every MQTT event that names a VIN.",
            (
                Field("event", STR, "The event's name, such as `vehicleInfo`."),
                VIN,
                Field("data", OBJECT, "The event's `respondData`."),
            ),
        ),
        Event(
            "on_command_ack",
            "command_ack",
            "Every remote-control acknowledgement.",
            (Field("ack", ref("CommandAckEvent"), "The acknowledgement."),),
        ),
        Event(
            "on_command_lifecycle",
            "command_lifecycle",
            "A command was registered, matched, expired or acknowledged without a match.",
            (Field("event", ref("CommandLifecycleEvent"), "What happened."),),
        ),
        Event("on_mqtt_connect", "mqtt_connect", "Every accepted MQTT connection."),
        Event(
            "request_observer",
            "request",
            "Every request the client sends, with its outcome and duration.",
            (Field("record", ref("RequestRecord"), "The request."),),
        ),
        Event(
            "gate_policy",
            "gate_policy",
            "Decides whether a command is sent, from the command gate's verdict. A callback that fails "
            "counts as the verdict itself.",
            (VIN, Field("verdict", ref("CommandGateVerdict"), "What the gate found.")),
            BOOL,
        ),
        Event(
            "pin_resolver",
            "pin_resolver",
            "Returns the `commandPwd` to send (empty for none), in place of pyBYD's rule: 32 hex digits "
            "are a digest, anything else is hashed.",
            (
                Field("command_pwd", opt(STR), "The PIN the call was given."),
                Field("control_pin", opt(STR), "The config's `control_pin`."),
            ),
            STR,
        ),
        Event(
            "relogin",
            "relogin",
            "Produces a new session when the current one has expired; its failure is the call's failure.",
            (),
            ref("SessionTokens"),
        ),
        Event(
            "on_state_changed",
            "state_changed",
            "A car's state changed.",
            (VIN, Field("state", ref("VehicleSnapshot"), "The new state.")),
        ),
    ]


CLIENT_OPTIONS = (
    option(
        "command_ack_ttl_seconds", FLOAT, 300.0, "How long a command waits for its acknowledgement, in seconds."
    ),
    option(
        "command_retries",
        INT,
        1,
        "Attempts per remote command. More retry a command the cloud could not confirm; one the car refused "
        "is never sent again.",
    ),
)

GET_CAR_OPTIONS = (
    option("vehicle", opt(OBJECT), None, "The vehicle's raw payload; the vehicle list is fetched when null."),
    option("projection_ttl", FLOAT, 30.0, "How long a command's projections stay active, in seconds."),
)

# ---------------------------------------------------------------------------


def build() -> Surface:
    """The whole surface, read from this file and from the pyBYD installed."""
    surface = Surface(version(), (ROOT / "parity" / "UPSTREAM_COMMIT").read_text(encoding="utf-8").strip())
    add_pybyd_models(surface)
    add_openbyd_shapes(surface)
    surface.client = client_methods()
    surface.car = car_methods()
    surface.features = features()
    surface.errors = errors()
    surface.events = events()
    surface.client_options = CLIENT_OPTIONS
    surface.check()
    return surface


__all__ = ["ANY", "CLIENT_OPTIONS", "GET_CAR_OPTIONS", "build"]
