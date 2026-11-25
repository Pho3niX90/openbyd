"""The client: pyBYD's ``BydClient`` with a Rust core.

:class:`Client` has pyBYD's methods, with the same names and arguments, and
returns plain data: every model as the dict pyBYD's
``model_dump(mode="json")`` gives, lists of them as lists of dicts::

    import openbyd

    async with openbyd.Client(username="user@example.com", password="secret", control_pin="123456") as client:
        await client.login()
        for vehicle in await client.get_vehicles():
            realtime = await client.get_vehicle_realtime(vehicle["vin"])
            print(realtime["elec_percent"])
        await client.verify_command_access(vin)
        await client.start_climate(vin, temperature=21.0, time_span=2)

        car = await client.get_car(vin)        # an openbyd.Car
        await car.lock.lock()

Errors are the classes of :mod:`openbyd.exceptions`, with pyBYD's names and
attributes (``code``, ``endpoint``, ``status_code``).

The configuration is given as keyword arguments, named as pyBYD's
``BydConfig`` fields; options openbyd adds in place of monkeypatching are
keyword arguments too (see :class:`Client`). Every client in a process
shares one HTTP connection pool and one Rust runtime, so a client is cheap:
thousands can be created.

Callbacks (``on_vehicle_info``, ``request_observer``, ...) always run on the
event loop the client was started on, never on a Rust thread. One that
raises is logged on the ``openbyd`` logger and does not affect the client.
"""

from __future__ import annotations

import asyncio
import datetime as dt
from collections.abc import Awaitable, Callable, Mapping, Sequence
from typing import TYPE_CHECKING, Any, Literal

from openbyd import _native
from openbyd.car import Car

if TYPE_CHECKING:
    from types import TracebackType

#: The configuration keywords: pyBYD's ``BydConfig`` fields.
CONFIG_FIELDS: tuple[str, ...] = (
    "username",
    "password",
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
    "control_pin",
    "session_ttl",
    "mqtt_enabled",
    "mqtt_keepalive",
    "mqtt_timeout",
    "device",
)

GatePolicy = Literal["strict", "allow_missing_data"] | Callable[[str, dict[str, Any]], bool]
ReloginPolicy = Literal["password", "never"] | Callable[[], Awaitable[dict[str, Any]]]


def _config_from(config: Any) -> dict[str, Any]:
    """A config given as a mapping or as an object with ``model_dump`` (pyBYD's ``BydConfig``)."""
    if config is None:
        return {}
    if isinstance(config, Mapping):
        return dict(config)
    dump = getattr(config, "model_dump", None)
    if callable(dump):
        return dict(dump())
    raise TypeError("config must be a mapping or a pyBYD BydConfig")


def _params(params: Any, fields: dict[str, Any]) -> Any:
    """Command parameters given as ``params`` or as keyword arguments, not both."""
    if params is not None and fields:
        raise TypeError("give the parameters as params= or as keyword arguments, not both")
    return params if params is not None else fields


def _model(what: str, value: Any) -> dict[str, Any]:
    """A model given as a mapping (the raw API dict, or a dict a method here
    returned) or as a pydantic model such as pyBYD's own."""
    dump = getattr(value, "model_dump", None)
    if callable(dump) and not isinstance(value, Mapping):
        return dict(dump(mode="json"))
    if isinstance(value, Mapping):
        return dict(value)
    raise TypeError(f"{what} must be a mapping or a pydantic model")


def _now() -> dt.datetime:
    return dt.datetime.now(dt.timezone.utc)


def _schedule(schedule: Any) -> dict[str, Any] | None:
    """A ``StandbySchedule`` given as a mapping of its fields or as pyBYD's
    model, with a ``datetime`` in ``at`` made into text for the extension.

    A daily time that has passed is moved to its next occurrence here, a
    day at a time in its own time zone, as pyBYD moves it: across a change
    of daylight saving time the time of day stays, which the extension
    (it sees a fixed UTC offset) could not do.

    The time may still pass between this call and the request being built
    (a moment, or as long as a new login takes). For that the run of the
    day after goes along as ``_daily_after``, worked out here in the same
    time zone, and the extension sends it instead.
    """
    if schedule is None:
        return None
    dump = getattr(schedule, "model_dump", None)
    if callable(dump) and not isinstance(schedule, Mapping):
        fields = dict(dump())
    elif isinstance(schedule, Mapping):
        fields = dict(schedule)
    else:
        raise TypeError("schedule must be a mapping or a pydantic model")
    # _daily_after is this wrapper's own hint to the extension; never take one from the caller.
    fields.pop("_daily_after", None)
    at = fields.get("at")
    if isinstance(at, dt.datetime):
        if at.utcoffset() is not None:
            if fields.get("repeat") == "daily":
                now = _now()
                while at <= now:
                    at += dt.timedelta(days=1)
                try:
                    fields["_daily_after"] = (at + dt.timedelta(days=1)).astimezone(dt.timezone.utc).isoformat()
                except OverflowError:  # the last day a datetime holds
                    pass
            # The same instant in UTC: the offset itself is never sent.
            # At either end of the calendar the UTC reading may lie outside
            # years 1 to 9999; keep the original offset then.
            try:
                at = at.astimezone(dt.timezone.utc)
            except OverflowError:
                pass
        fields["at"] = at.isoformat()
    days = fields.get("weekly_days")
    if isinstance(days, (tuple, set, frozenset, range)):
        fields["weekly_days"] = list(days)
    return fields


class Client:
    """The async client for the BYD API: pyBYD's ``BydClient``.

    Use it as ``async with Client(...) as client:``, or call :meth:`start`
    and :meth:`close`. Calls on a client that is not started raise
    :class:`~openbyd.exceptions.BydError`, as pyBYD's do.

    Configuration (pyBYD's ``BydConfig`` fields, with its defaults):
    ``username``, ``password``, ``base_url``, ``country_code``,
    ``language``, ``time_zone``, ``app_version``, ``app_inner_version``,
    ``soft_type``, ``tbox_version``, ``is_auto``, ``identifier_type``,
    ``control_pin``, ``session_ttl``, ``mqtt_enabled``, ``mqtt_keepalive``,
    ``mqtt_timeout`` and ``device`` (a dict of ``DeviceProfile`` fields).
    They may also come as a mapping or a pyBYD ``BydConfig`` in ``config``;
    keyword arguments win. A bad value raises
    :class:`~openbyd.exceptions.ValidationError`.

    pyBYD's callbacks, run on the client's event loop:

    - ``on_vehicle_info(vin, realtime)``: every MQTT ``vehicleInfo`` push, parsed.
    - ``on_mqtt_event(event, vin, respond_data)``: every MQTT event naming a VIN.
    - ``on_command_ack(ack)``: every remote-control acknowledgement.
    - ``on_command_lifecycle(event)``: registered, matched, expired, uncorrelated.
    - ``on_mqtt_connect()``: every accepted MQTT connection.
    - ``on_mqtt_disconnect(reason)``: every dropped MQTT connection, with
      the reason as text (it reconnects by itself). openbyd's addition.
    - ``on_mqtt_connack_refused(consecutive)``: every refused MQTT
      connection, with how many in a row. openbyd's addition.
    - ``command_ack_ttl_seconds``: how long a command waits for its acknowledgement.

    openbyd's options:

    - ``gate_policy``: ``"strict"`` (the default, pyBYD's: a command is sent
      only when the capability gate says supported), ``"allow_missing_data"``
      (also send when the car's capability data is incomplete: the reasons
      ``function_no_missing`` and ``learn_info_missing``), or a callable
      ``(vin, verdict) -> bool`` deciding from the gate's verdict dict.
    - ``command_retries``: attempts per remote command (pyBYD's 1). More
      attempts retry a command the cloud could not confirm; one the car
      refused is never sent again.
    - ``pin_resolver``: a callable ``(command_pwd, control_pin) -> str``
      returning the ``commandPwd`` to send (empty for none), in place of
      pyBYD's rule (32 hex digits are a digest, anything else is hashed).
    - ``relogin``: what to do when a new session is needed: ``"password"``
      (log in again, pyBYD's), ``"never"`` (fail with
      ``BydSessionExpiredError`` with BYD's own expiry code, or ``relogin_disabled``
      when there was no session) or an async
      callable returning a session dict (as :meth:`export_session` gives).
    - ``request_observer``: a callable given a dict per request sent:
      ``endpoint``, ``outcome`` (``"reply"``, ``"transport"``, ``"failed"``),
      ``code``, ``status``, ``error`` and ``duration`` in seconds.
    - ``session``: a session dict to restore at :meth:`start`, so no
      password login is needed (see :meth:`from_session`).
    - ``commands_enabled``: start with remote commands enabled, without
      :meth:`verify_command_access` (see :meth:`enable_commands`).
    """

    def __init__(
        self,
        config: Mapping[str, Any] | Any = None,
        *,
        on_vehicle_info: Callable[[str, dict[str, Any]], None] | None = None,
        on_mqtt_event: Callable[[str, str, dict[str, Any]], None] | None = None,
        on_command_ack: Callable[[dict[str, Any]], None] | None = None,
        on_command_lifecycle: Callable[[dict[str, Any]], None] | None = None,
        on_mqtt_connect: Callable[[], None] | None = None,
        on_mqtt_disconnect: Callable[[str], None] | None = None,
        on_mqtt_connack_refused: Callable[[int], None] | None = None,
        command_ack_ttl_seconds: float = 300.0,
        gate_policy: GatePolicy | None = None,
        command_retries: int = 1,
        pin_resolver: Callable[[str | None, str | None], str] | None = None,
        relogin: ReloginPolicy | None = None,
        request_observer: Callable[[dict[str, Any]], None] | None = None,
        session: Mapping[str, Any] | None = None,
        commands_enabled: bool = False,
        _test_tls: bool = False,
        **config_fields: Any,
    ) -> None:
        unknown = sorted(set(config_fields) - set(CONFIG_FIELDS))
        if unknown:
            raise TypeError(f"unexpected keyword arguments: {', '.join(unknown)}")
        fields = _config_from(config)
        fields.update(config_fields)
        self._native = _native.Client(
            fields,
            on_vehicle_info=on_vehicle_info,
            on_mqtt_event=on_mqtt_event,
            on_command_ack=on_command_ack,
            on_command_lifecycle=on_command_lifecycle,
            on_mqtt_connect=on_mqtt_connect,
            on_mqtt_disconnect=on_mqtt_disconnect,
            on_mqtt_connack_refused=on_mqtt_connack_refused,
            command_ack_ttl_seconds=command_ack_ttl_seconds,
            gate_policy=gate_policy,
            command_retries=command_retries,
            pin_resolver=pin_resolver,
            relogin=relogin,
            request_observer=request_observer,
            commands_enabled=commands_enabled,
            test_tls=_test_tls,
        )
        self._restore = dict(session) if session is not None else None
        self._cars: dict[str, Car] = {}

    @classmethod
    def from_session(cls, session: Mapping[str, Any], *, password: str = "", **kwargs: Any) -> Client:
        """A client that resumes ``session`` (from :meth:`export_session`) at :meth:`start`.

        Without a ``password`` it never logs in by itself (``relogin``
        defaults to ``"never"``): when the session expires, calls raise
        ``BydSessionExpiredError`` until the application imports a new one.
        """
        if not password:
            kwargs.setdefault("relogin", "never")
        return cls(session=session, password=password, **kwargs)

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def __aenter__(self) -> Client:
        await self.start()
        return self

    async def __aexit__(
        self,
        kind: type[BaseException] | None,
        value: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        await self.close()

    async def start(self) -> None:
        """Open the client on the running event loop (pyBYD's ``async_start``).

        Callbacks run on this loop from now on. A ``session`` given to the
        constructor is restored here, starting MQTT as a login would.
        """
        self._native.start(asyncio.get_running_loop())
        if self._restore is not None:
            restore, self._restore = self._restore, None
            await self._native.import_session(restore)

    async def close(self) -> None:
        """Close the cars, stop MQTT and make later calls fail (pyBYD's ``async_close``).

        Calls already in flight finish; one waiting for an MQTT push stops
        waiting and polls over HTTP instead, as when the wait times out
        (pyBYD #98).
        """
        cars, self._cars = self._cars, {}
        for car in cars.values():
            car.close()
        self._native.close()

    async def async_start(self) -> None:
        """pyBYD's name for :meth:`start`."""
        await self.start()

    async def async_close(self) -> None:
        """pyBYD's name for :meth:`close`."""
        await self.close()

    # ------------------------------------------------------------------
    # Sessions
    # ------------------------------------------------------------------

    async def login(self) -> None:
        """Log in with the password and start MQTT."""
        await self._native.login()

    async def ensure_session(self) -> dict[str, Any]:
        """The current session, logging in first when there is none or it expired.

        A dict of pyBYD's ``Session`` fields: ``user_id``, ``sign_token``,
        ``encry_token``, ``created_at`` (monotonic seconds) and ``ttl``.
        """
        return await self._native.ensure_session()

    def invalidate_session(self) -> None:
        """Forget the session; the next call logs in again. Command access stays verified."""
        self._native.invalidate_session()

    def export_session(self) -> dict[str, Any] | None:
        """The session as an application stores it, or ``None`` when logged out.

        ``user_id``, ``sign_token``, ``encry_token``, ``refresh_token``
        (secrets; the last may be empty, and a record without it still
        imports), ``ttl_remaining`` (seconds, ``None`` for no expiry) and
        ``guest_vins`` (``{vin: bool}``). Give it to :meth:`import_session`,
        :meth:`from_session` or ``Client(session=...)`` to resume without a
        password login.
        """
        return self._native.export_session()

    async def import_session(self, session: Mapping[str, Any]) -> None:
        """Use ``session`` (from :meth:`export_session`) as if a login had returned it, and start MQTT."""
        await self._native.import_session(dict(session))

    @property
    def commands_enabled(self) -> bool:
        """Whether remote commands are available: ``True`` once :meth:`verify_command_access` succeeded."""
        return self._native.commands_enabled

    def enable_commands(self) -> None:
        """Enable remote commands without checking the PIN with BYD first (openbyd's addition).

        For an application that verified the PIN elsewhere and builds a new
        client per request (pyBYD users set ``_commands_enabled``). Every
        command still sends the PIN, and BYD still refuses a wrong one.
        """
        self._native.enable_commands()

    @property
    def guest_vins(self) -> dict[str, bool]:
        """Whether the account reaches each VIN as a guest, as the client knows it now.

        Unlike :meth:`export_session` it survives :meth:`invalidate_session`,
        so guest flips learnt during a refused call can still be saved.
        """
        return self._native.guest_vins

    def stop_mqtt(self) -> None:
        """Stop MQTT (pyBYD's ``_stop_mqtt``); :meth:`start_mqtt` starts it again.

        A call waiting for an MQTT push stops waiting and polls over HTTP
        instead, as when the wait times out (pyBYD #98).
        """
        self._native.stop_mqtt()

    @property
    def mqtt_is_running(self) -> bool:
        """Whether the MQTT push channel is connected or connecting."""
        return self._native.mqtt_is_running

    async def start_mqtt(self) -> None:
        """Start MQTT for the current session if it is not running, as a login does."""
        await self._native.start_mqtt()

    def get_command_ack_diagnostics(self) -> dict[str, Any]:
        """Counters of command acknowledgements: pending, matched, expired, uncorrelated, pending_by_vin."""
        return self._native.get_command_ack_diagnostics()

    # ------------------------------------------------------------------
    # Vehicles
    # ------------------------------------------------------------------

    async def get_vehicles(self) -> list[dict[str, Any]]:
        """The account's vehicles."""
        return await self._native.get_vehicles()

    def seed_vehicle_metadata(self, vin: str, energy_type: int, model_name: str | None = None) -> None:
        """Cache ``vin``'s metadata as if the vehicle list had returned it.

        Realtime, energy and AC-limit reads then need no vehicle list.
        ``energy_type`` is BYD's value (0 EV, 1 ICE, 2 hybrid).
        """
        self._native.seed_vehicle_metadata(vin, int(energy_type), model_name)

    def vehicle_metadata(self, vin: str) -> dict[str, Any] | None:
        """The cached metadata of ``vin`` (a ``Vehicle`` dict), if known."""
        return self._native.vehicle_metadata(vin)

    async def get_car(
        self,
        vin: str,
        *,
        vehicle: Mapping[str, Any] | None = None,
        on_state_changed: Callable[[str, dict[str, Any]], None] | None = None,
        projection_ttl: float = 30.0,
    ) -> Car:
        """The :class:`~openbyd.Car` of ``vin``, built once and kept.

        As pyBYD: a kept car is returned as it is. Otherwise the vehicle
        list is fetched unless ``vehicle`` (a raw ``Vehicle`` payload) is
        given, then the capabilities. ``on_state_changed(vin, state)`` runs
        on the client's loop after every change of the car's state.
        """
        native = await self._native.get_car(
            vin, vehicle=None if vehicle is None else dict(vehicle), on_state_changed=on_state_changed,
            projection_ttl=projection_ttl,
        )
        known = self._cars.get(vin)
        if known is not None and known._native.same_car(native):
            return known
        car = Car(native)
        self._cars[vin] = car
        return car

    async def get_latest_configs(self, vins: list[str]) -> dict[str, dict[str, Any]]:
        """The latest config of each VIN the cloud knows, by VIN."""
        return await self._native.get_latest_configs(list(vins))

    async def get_latest_config(self, vin: str) -> dict[str, Any]:
        """The latest config of ``vin``; ``BydDataUnavailableError`` when the reply has none."""
        return await self._native.get_latest_config(vin)

    async def get_vehicle_capabilities(self, vin: str, *, force_refresh: bool = False) -> dict[str, Any]:
        """What ``vin`` supports, cached; a failed fetch gives unknown capabilities."""
        return await self._native.get_vehicle_capabilities(vin, force_refresh=force_refresh)

    # ------------------------------------------------------------------
    # Reads
    # ------------------------------------------------------------------

    async def get_vehicle_realtime(
        self,
        vin: str,
        *,
        poll_attempts: int = 10,
        poll_interval: float = 1.5,
        mqtt_timeout: float | None = None,
        signal_retries: int = 2,
    ) -> dict[str, Any]:
        """Realtime data: trigger, wait for the MQTT push, poll over HTTP if it does not come."""
        return await self._native.get_vehicle_realtime(
            vin,
            poll_attempts=poll_attempts,
            poll_interval=poll_interval,
            mqtt_timeout=mqtt_timeout,
            signal_retries=signal_retries,
        )

    async def get_gps_info(
        self,
        vin: str,
        *,
        poll_attempts: int = 10,
        poll_interval: float = 1.5,
        mqtt_timeout: float | None = None,
        signal_retries: int = 2,
    ) -> dict[str, Any]:
        """The vehicle's position: trigger, wait for the push, poll if it does not come."""
        return await self._native.get_gps_info(
            vin,
            poll_attempts=poll_attempts,
            poll_interval=poll_interval,
            mqtt_timeout=mqtt_timeout,
            signal_retries=signal_retries,
        )

    async def get_hvac_status(self, vin: str) -> dict[str, Any]:
        """Climate status."""
        return await self._native.get_hvac_status(vin)

    async def get_charging_status(self, vin: str) -> dict[str, Any]:
        """Charging status."""
        return await self._native.get_charging_status(vin)

    async def get_charging_schedule(self, vin: str) -> dict[str, Any]:
        """The smart-charging schedule."""
        return await self._native.get_charging_schedule(vin)

    async def get_charging_homepage(self, vin: str) -> tuple[dict[str, Any], dict[str, Any]]:
        """Live charging state and schedule from one call, as ``(status, schedule)``."""
        return await self._native.get_charging_homepage(vin)

    async def get_ac_current_limit(self, vin: str, *, energy_type: int | str | None = None) -> dict[str, Any]:
        """Supported and active AC charging current limits; ``energy_type`` defaults to the vehicle's."""
        return await self._native.get_ac_current_limit(vin, energy_type=energy_type)

    async def get_energy_consumption(self, vin: str) -> dict[str, Any]:
        """Energy consumption, asked with the vehicle's energy type and model."""
        return await self._native.get_energy_consumption(vin)

    async def get_push_state(self, vin: str) -> dict[str, Any]:
        """Push notification state."""
        return await self._native.get_push_state(vin)

    async def set_push_state(self, vin: str, *, enable: bool, push_type: int = 701) -> dict[str, Any]:
        """Turn a push notification type on or off (701 is the vehicle status push)."""
        return await self._native.set_push_state(vin, enable=enable, push_type=push_type)

    async def get_notifications(self, category: int = 1) -> list[dict[str, Any]]:
        """The account's unread inbox messages of one category (1 vehicle, 2 system)."""
        return await self._native.get_notifications(int(category))

    async def get_all_notifications(self) -> dict[int, list[dict[str, Any]]]:
        """The unread inbox messages of every category, keyed by category value."""
        return await self._native.get_all_notifications()

    async def rename_vehicle(self, vin: str, *, name: str) -> dict[str, Any]:
        """Rename the vehicle."""
        return await self._native.rename_vehicle(vin, name=name)

    async def get_booking_list(self, vin: str) -> dict[str, Any]:
        """The scheduled-climate timers."""
        return await self._native.get_booking_list(vin)

    async def get_maintenance_history(self, vin: str) -> dict[str, Any]:
        """Past workshop visits (service history)."""
        return await self._native.get_maintenance_history(vin)

    async def get_ota_version(self, vin: str) -> dict[str, Any]:
        """The OTA firmware version, release notes and night auto-upgrade settings."""
        return await self._native.get_ota_version(vin)

    async def set_ota_auto_upgrade(
        self, vin: str, *, enabled: bool, auto_upgrade_time: str = "2:00", command_pwd: str | None = None
    ) -> dict[str, Any]:
        """Turn the night auto-upgrade on or off and set its time (``H:mm``).

        Needs command access (:meth:`verify_command_access`) and the control
        PIN, as a command does; ``command_pwd`` defaults to the config's
        ``control_pin``.
        """
        return await self._native.set_ota_auto_upgrade(
            vin, enabled=enabled, auto_upgrade_time=auto_upgrade_time, command_pwd=command_pwd
        )

    async def get_empower_detail(self, vin: str, *, empower_id: int | None = None) -> dict[str, Any]:
        """A shared guest's grant in detail; ``empower_id`` defaults to the cached vehicle's."""
        return await self._native.get_empower_detail(vin, empower_id=empower_id)

    # ------------------------------------------------------------------
    # Preconditioning plans
    # ------------------------------------------------------------------

    async def get_standby_plans(self, vin: str) -> dict[str, Any]:
        """The preconditioning plans app 3.6.0 schedules climate with: ``{"vin": ..., "plans": [...]}``."""
        return await self._native.get_standby_plans(vin)

    async def save_standby_plan(self, vin: str, plan: Any, *, schedule: Any = None) -> dict[str, Any]:
        """Save a plan's name, commands and schedule, then return the fresh plans.

        ``plan`` is a plan as :meth:`get_standby_plans` returns it (or the
        raw API dict, or pyBYD's ``StandbyPlan``); change its ``commands``
        (for example the ``OPENAIR`` ``control_params_map``) to change what
        it does. ``schedule`` is a mapping of pyBYD's ``StandbySchedule``
        fields (or that model): ``{"repeat": "off"}`` unschedules,
        ``{"repeat": "once", "at": <aware datetime>}`` (or ``"daily"``),
        ``{"repeat": "weekly", "start_time": "HH:MM", "weekly_days": [0, 4]}``
        (0 is Monday). ``None`` keeps the plan's current schedule. Saving
        with ``default_plan`` 1 makes it the default.
        """
        return await self._native.save_standby_plan(vin, _model("plan", plan), schedule=_schedule(schedule))

    async def create_standby_plan(
        self, vin: str, name: str, commands: Sequence[Any], *, schedule: Any = None
    ) -> dict[str, Any]:
        """Create a user plan and return it.

        ``commands`` are what the plan does, each as a command of a plan
        :meth:`get_standby_plans` returned (or the raw API dict, or pyBYD's
        ``StandbyPlanCommand``), for example copied from the preset plan.
        They must include the A/C (``OPENAIR``, else ``ValueError``); seats
        and steering wheel only go on top of it. ``schedule`` is as in
        :meth:`save_standby_plan`; ``None`` is not scheduled. The new plan
        is picked out of the plan list read afterwards; ``BydError`` when it
        cannot be told apart there.
        """
        given = [_model("a command", command) for command in commands]
        return await self._native.create_standby_plan(vin, name, given, schedule=_schedule(schedule))

    async def set_default_standby_plan(self, vin: str, plan_id: str) -> dict[str, Any]:
        """Make a plan the default, then return the fresh plans."""
        return await self._native.set_default_standby_plan(vin, plan_id)

    async def delete_standby_plan(self, vin: str, plan_id: str) -> dict[str, Any]:
        """Delete a user plan, then return the fresh plans. The preset plan cannot be deleted; unschedule it instead."""
        return await self._native.delete_standby_plan(vin, plan_id)

    async def run_standby_plan(
        self, vin: str, plan_id: str, *, poll_attempts: int = 10, poll_interval: float = 1.5
    ) -> dict[str, Any]:
        """Run a plan now, keeping its schedule.

        The result's ``res`` is 2 when done, or still 1 when polling gave
        up (the app stops there too); any other value raises
        ``BydRemoteControlError``.
        """
        return await self._native.run_standby_plan(
            vin, plan_id, poll_attempts=poll_attempts, poll_interval=poll_interval
        )

    # ------------------------------------------------------------------
    # Commands
    # ------------------------------------------------------------------

    async def verify_command_access(self, vin: str, *, command_pwd: str | None = None) -> dict[str, Any]:
        """Check the control PIN once and enable remote commands."""
        return await self._native.verify_command_access(vin, command_pwd=command_pwd)

    async def lock(self, vin: str, *, command_pwd: str | None = None) -> dict[str, Any]:
        """Lock the vehicle."""
        return await self._native.lock(vin, command_pwd=command_pwd)

    async def unlock(self, vin: str, *, command_pwd: str | None = None) -> dict[str, Any]:
        """Unlock the vehicle."""
        return await self._native.unlock(vin, command_pwd=command_pwd)

    async def stop_climate(self, vin: str, *, command_pwd: str | None = None) -> dict[str, Any]:
        """Stop climate control."""
        return await self._native.stop_climate(vin, command_pwd=command_pwd)

    async def flash_lights(self, vin: str, *, command_pwd: str | None = None) -> dict[str, Any]:
        """Flash the lights."""
        return await self._native.flash_lights(vin, command_pwd=command_pwd)

    async def close_windows(self, vin: str, *, command_pwd: str | None = None) -> dict[str, Any]:
        """Close all windows."""
        return await self._native.close_windows(vin, command_pwd=command_pwd)

    async def open_windows(self, vin: str, *, command_pwd: str | None = None) -> dict[str, Any]:
        """Open the windows to a vent crack of about 10 %."""
        return await self._native.open_windows(vin, command_pwd=command_pwd)

    async def find_car(self, vin: str, *, command_pwd: str | None = None) -> dict[str, Any]:
        """Horn and lights."""
        return await self._native.find_car(vin, command_pwd=command_pwd)

    async def open_trunk(self, vin: str, *, command_pwd: str | None = None) -> dict[str, Any]:
        """Release the trunk."""
        return await self._native.open_trunk(vin, command_pwd=command_pwd)

    async def close_trunk(self, vin: str, *, command_pwd: str | None = None) -> dict[str, Any]:
        """Lower the trunk."""
        return await self._native.close_trunk(vin, command_pwd=command_pwd)

    async def start_climate(
        self, vin: str, *, params: Any = None, command_pwd: str | None = None, **fields: Any
    ) -> dict[str, Any]:
        """Start climate control with ``ClimateStartParams`` fields, as ``params`` or keywords."""
        return await self._native.start_climate(vin, params=_params(params, fields), command_pwd=command_pwd)

    async def schedule_climate(
        self, vin: str, *, params: Any = None, command_pwd: str | None = None, **fields: Any
    ) -> dict[str, Any]:
        """Schedule climate control with ``ClimateScheduleParams`` fields."""
        return await self._native.schedule_climate(vin, params=_params(params, fields), command_pwd=command_pwd)

    async def set_seat_climate(
        self, vin: str, *, params: Any = None, command_pwd: str | None = None, **fields: Any
    ) -> dict[str, Any]:
        """Set seat heating and ventilation with ``SeatClimateParams`` fields."""
        return await self._native.set_seat_climate(vin, params=_params(params, fields), command_pwd=command_pwd)

    async def set_battery_heat(
        self, vin: str, *, params: Any = None, command_pwd: str | None = None, **fields: Any
    ) -> dict[str, Any]:
        """Turn battery heating on or off with ``BatteryHeatParams`` fields (``on``)."""
        return await self._native.set_battery_heat(vin, params=_params(params, fields), command_pwd=command_pwd)

    async def save_charging_schedule(
        self,
        vin: str,
        *,
        start_charge_time: str,
        end_charge_time: str,
        charge_way: str,
        enabled: bool = True,
        mqtt_timeout: float | None = None,
        poll_attempts: int = 10,
        poll_interval: float | None = None,
    ) -> dict[str, Any]:
        """Save a smart-charging schedule and wait for the change to settle."""
        return await self._native.save_charging_schedule(
            vin,
            start_charge_time=start_charge_time,
            end_charge_time=end_charge_time,
            charge_way=charge_way,
            enabled=enabled,
            mqtt_timeout=mqtt_timeout,
            poll_attempts=poll_attempts,
            poll_interval=poll_interval,
        )

    async def toggle_smart_charging(self, vin: str, *, enable: bool) -> dict[str, Any]:
        """Turn smart charging on or off."""
        return await self._native.toggle_smart_charging(vin, enable=enable)

    async def start_charging(
        self,
        vin: str,
        *,
        mqtt_timeout: float | None = None,
        poll_attempts: int = 10,
        poll_interval: float | None = None,
    ) -> dict[str, Any]:
        """Start charging now and wait for the change to settle."""
        return await self._native.start_charging(
            vin, mqtt_timeout=mqtt_timeout, poll_attempts=poll_attempts, poll_interval=poll_interval
        )

    async def stop_charging(
        self,
        vin: str,
        *,
        mqtt_timeout: float | None = None,
        poll_attempts: int = 10,
        poll_interval: float | None = None,
    ) -> dict[str, Any]:
        """Always raises ``BydEndpointNotSupportedError``: the cloud cannot stop a charge (as pyBYD)."""
        return await self._native.stop_charging(
            vin, mqtt_timeout=mqtt_timeout, poll_attempts=poll_attempts, poll_interval=poll_interval
        )


__all__ = ["CONFIG_FIELDS", "Client", "GatePolicy", "ReloginPolicy"]
