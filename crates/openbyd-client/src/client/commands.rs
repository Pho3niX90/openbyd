//! Remote commands: the control PIN, the command gate, `_remote_control`
//! and every command method of pyBYD's `BydClient`, with the charging
//! commands that go through `_trigger_and_poll`.
//!
//! These send commands to real cars. Every check, retry and wait follows
//! pyBYD; the client parity cases in `parity/groups/client_commands.py`
//! hold them to it.

use openbyd_protocol::coerce::py_strip;
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::hashing::md5_hex;
use openbyd_protocol::models::command_gating::{CommandGateVerdict, evaluate_command_gate};
use openbyd_protocol::models::control::{
    BatteryHeatParams, ChargeChangeResult, ClimateScheduleParams, ClimateStartParams, CommandAck,
    RemoteCommand, RemoteControlResult, SeatClimateParams, VerifyControlPasswordResponse,
};
use serde_json::{Map, Value};

use super::{BydClient, ChargePollSettings, ReadFetch, TriggeredRead};
use crate::api::control::{
    BoxFuture, CommandHooks, MqttResultWaiter, PollOptions, RemoteControlOptions, model_error,
    poll_remote_control, truthy, validate, verify_control_password,
};
use crate::api::smart_charging::{
    RESULT_ENDPOINT, SAVE_ENDPOINT, TOGGLE_ENDPOINT, make_change_charge_fetch_fn,
    make_save_charging_schedule_fetch_fn, toggle_smart_charging,
};
use crate::api::vehicle::parse_model;
use crate::env::ClientEnv;

/// Where commands are sent; the endpoint a blocked command reports.
pub const REMOTE_CONTROL_ENDPOINT: &str = "/control/remoteControl";

/// pyBYD's `_resolve_command_pwd`: the `commandPwd` for a given PIN.
///
/// A given value is stripped; 32 hex digits are taken as an MD5 digest and
/// upper-cased, anything else is hashed (uppercase MD5). Without one, the
/// config's `control_pin` is hashed; without that, the result is empty.
pub fn resolve_command_pwd(command_pwd: Option<&str>, config: &BydConfig) -> String {
    if let Some(given) = command_pwd {
        let stripped = py_strip(given);
        if stripped.chars().count() == 32 && stripped.chars().all(|c| c.is_ascii_hexdigit()) {
            return stripped.to_ascii_uppercase();
        }
        return md5_hex(stripped);
    }
    match config.control_pin.as_deref() {
        Some(pin) if !pin.is_empty() => md5_hex(pin),
        _ => String::new(),
    }
}

/// The control parameters of a command: one of pyBYD's `ControlParams`
/// models, or a plain mapping sent as it is.
#[derive(Debug, Clone, Copy)]
pub enum ControlParamsArg<'a> {
    /// A mapping, sent as `dict(control_params)`.
    Map(&'a Map<String, Value>),
    /// Climate start settings.
    ClimateStart(&'a ClimateStartParams),
    /// Climate schedule settings.
    ClimateSchedule(&'a ClimateScheduleParams),
    /// Seat heating and ventilation levels.
    SeatClimate(&'a SeatClimateParams),
    /// Battery heating on or off.
    BatteryHeat(&'a BatteryHeatParams),
}

fn no_pin() -> Error {
    Error::api(
        ErrorKind::BydControlPasswordError,
        "No control PIN configured. Set config.control_pin or pass command_pwd.",
        "no_pin",
        "",
    )
}

fn blocked(what: &str, vin: &str, gate: &CommandGateVerdict, endpoint: &str) -> Error {
    Error::api(
        ErrorKind::BydEndpointNotSupportedError,
        format!(
            "{what} blocked for VIN {vin}: gate={} reason={}",
            gate.gate_id, gate.reason
        ),
        "command_gate_blocked",
        endpoint,
    )
}

/// The client's [`MqttResultWaiter`]: pyBYD's `_mqtt_result_waiter`.
struct ResultWaiter<'a, E: ClientEnv> {
    client: &'a BydClient<E>,
    vin: &'a str,
}

impl<E: ClientEnv> MqttResultWaiter for ResultWaiter<'_, E> {
    fn wait<'a>(
        &'a self,
        serial: &'a Value,
    ) -> BoxFuture<'a, Result<Option<RemoteControlResult>, Error>> {
        Box::pin(async move {
            if serial.is_null() {
                return Ok(None);
            }
            let raw = self
                .client
                .mqtt_wait(self.vin, Some("remoteControl"), Some(serial), None)
                .await?;
            let Some(mut raw) = raw else {
                return Ok(None);
            };
            raw.entry("requestSerial").or_insert_with(|| serial.clone());
            validate(&Value::Object(raw)).map(Some)
        })
    }
}

impl<E: ClientEnv> BydClient<E> {
    fn pin(&self, command_pwd: Option<&str>) -> String {
        match &self.inner.options.pin_resolver {
            Some(resolver) => resolver(command_pwd, &self.inner.config),
            None => resolve_command_pwd(command_pwd, &self.inner.config),
        }
    }

    /// Whether remote commands are available: `true` once
    /// [`verify_command_access`](Self::verify_command_access) succeeded, for
    /// the client's lifetime (pyBYD's `commands_enabled`).
    pub fn commands_enabled(&self) -> bool {
        self.state().commands_enabled
    }

    /// Checks the control PIN once and enables remote commands (pyBYD's
    /// `verify_command_access`). A wrong PIN fails with
    /// `BydControlPasswordError` and leaves commands disabled.
    pub async fn verify_command_access(
        &self,
        vin: &str,
        command_pwd: Option<&str>,
    ) -> Result<VerifyControlPasswordResponse, Error> {
        let resolved = self.pin(command_pwd);
        if resolved.is_empty() {
            return Err(no_pin());
        }
        let result = authed!(self, |cx| verify_control_password(cx, vin, &resolved))?;
        self.state().commands_enabled = true;
        log::info!("Command access verified");
        Ok(result)
    }

    /// pyBYD's `_require_command_pwd`.
    fn require_command_pwd(&self, command_pwd: Option<&str>) -> Result<String, Error> {
        if !self.commands_enabled() {
            return Err(Error::api(
                ErrorKind::BydControlPasswordError,
                "Command access not available. Call verify_command_access() during setup, \
                 or check that the control PIN is correct.",
                "commands_disabled",
                "",
            ));
        }
        let resolved = self.pin(command_pwd);
        if resolved.is_empty() {
            return Err(no_pin());
        }
        Ok(resolved)
    }

    /// Whether the gate lets the command through, as the gate policy says.
    fn gate_allows(&self, vin: &str, gate: &CommandGateVerdict) -> bool {
        match &self.inner.options.gate_policy {
            Some(policy) => policy(vin, gate),
            None => gate.supported,
        }
    }

    /// Sends a remote command and waits for its result (pyBYD's
    /// `_remote_control`): the command gate, then `poll_remote_control` with
    /// the MQTT waiter and the pending-command registry, retried once after
    /// a fresh session when the session expired.
    ///
    /// For the car layer and other callers that build their own commands;
    /// the named command methods call it. It does not check
    /// [`commands_enabled`](Self::commands_enabled): `command_pwd` is sent
    /// as given.
    pub async fn remote_control(
        &self,
        vin: &str,
        command: RemoteCommand,
        control_params: Option<ControlParamsArg<'_>>,
        command_pwd: Option<&str>,
        poll_attempts: i64,
        poll_interval: f64,
    ) -> Result<RemoteControlResult, Error> {
        let capabilities = self.get_vehicle_capabilities(vin, false).await;
        let params = control_params.map(|params| match params {
            ControlParamsArg::BatteryHeat(battery)
                if capabilities
                    .function_nos
                    .iter()
                    .any(|no| no == "10100001" || no == "10100002")
                    && !capabilities.function_nos.iter().any(|no| no == "10300002") =>
            {
                battery.to_dedicated_control_params_map()
            }
            ControlParamsArg::BatteryHeat(battery) => battery.to_control_params_map(),
            ControlParamsArg::ClimateStart(climate) => climate.to_control_params_map(),
            ControlParamsArg::ClimateSchedule(schedule) => schedule.to_control_params_map(),
            ControlParamsArg::SeatClimate(seat) => seat.to_control_params_map(),
            ControlParamsArg::Map(map) => map.clone(),
        });
        let learn_info = self.vehicle_fun_learn_info_for(vin).await;
        let gate =
            evaluate_command_gate(command, &capabilities, params.as_ref(), learn_info.as_ref())
                .map_err(model_error)?;
        if !self.gate_allows(vin, &gate) {
            let what = format!("Remote command {}", command.as_str());
            return Err(blocked(&what, vin, &gate, REMOTE_CONTROL_ENDPOINT));
        }
        let options = RemoteControlOptions {
            poll: PollOptions {
                poll_attempts,
                poll_interval,
                ..PollOptions::default()
            },
            command_retries: self.inner.options.command_retries,
            ..RemoteControlOptions::default()
        };
        with_reauth!(
            self,
            async {
                let session = self.ensure_session().await?;
                self.require_transport()?;
                let waiter = ResultWaiter { client: self, vin };
                let dispatched = |serial: &Value| {
                    if truthy(serial) {
                        self.register_pending_command(vin, serial, command.as_str());
                    }
                };
                let hooks = CommandHooks {
                    mqtt_result_waiter: Some(&waiter),
                    on_trigger_dispatched: Some(&dispatched),
                };
                poll_remote_control(
                    self.context(&session),
                    vin,
                    command,
                    params.as_ref(),
                    command_pwd,
                    options,
                    hooks,
                )
                .await
            }
            .await
        )
    }

    async fn command(
        &self,
        vin: &str,
        command: RemoteCommand,
        params: Option<ControlParamsArg<'_>>,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        let pwd = self.require_command_pwd(command_pwd)?;
        self.remote_control(vin, command, params, Some(&pwd), 10, 1.5)
            .await
    }

    /// Locks the vehicle (pyBYD's `lock`).
    pub async fn lock(
        &self,
        vin: &str,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        self.command(vin, RemoteCommand::Lock, None, command_pwd)
            .await
    }

    /// Unlocks the vehicle (pyBYD's `unlock`).
    pub async fn unlock(
        &self,
        vin: &str,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        self.command(vin, RemoteCommand::Unlock, None, command_pwd)
            .await
    }

    /// Starts climate control (pyBYD's `start_climate`).
    pub async fn start_climate(
        &self,
        vin: &str,
        params: &ClimateStartParams,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        let params = ControlParamsArg::ClimateStart(params);
        self.command(vin, RemoteCommand::StartClimate, Some(params), command_pwd)
            .await
    }

    /// Stops climate control (pyBYD's `stop_climate`).
    pub async fn stop_climate(
        &self,
        vin: &str,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        self.command(vin, RemoteCommand::StopClimate, None, command_pwd)
            .await
    }

    /// Flashes the lights (pyBYD's `flash_lights`).
    pub async fn flash_lights(
        &self,
        vin: &str,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        self.command(vin, RemoteCommand::FlashLights, None, command_pwd)
            .await
    }

    /// Closes all windows (pyBYD's `close_windows`).
    pub async fn close_windows(
        &self,
        vin: &str,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        self.command(vin, RemoteCommand::CloseWindows, None, command_pwd)
            .await
    }

    /// Opens the windows to a vent crack of about 10 % (pyBYD's `open_windows`).
    pub async fn open_windows(
        &self,
        vin: &str,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        self.command(vin, RemoteCommand::OpenWindows, None, command_pwd)
            .await
    }

    /// Horn and lights (pyBYD's `find_car`).
    pub async fn find_car(
        &self,
        vin: &str,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        self.command(vin, RemoteCommand::FindCar, None, command_pwd)
            .await
    }

    /// Releases the trunk (pyBYD's `open_trunk`).
    pub async fn open_trunk(
        &self,
        vin: &str,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        self.command(vin, RemoteCommand::OpenTrunk, None, command_pwd)
            .await
    }

    /// Lowers the trunk (pyBYD's `close_trunk`).
    pub async fn close_trunk(
        &self,
        vin: &str,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        self.command(vin, RemoteCommand::CloseTrunk, None, command_pwd)
            .await
    }

    /// Schedules climate control (pyBYD's `schedule_climate`).
    pub async fn schedule_climate(
        &self,
        vin: &str,
        params: &ClimateScheduleParams,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        let params = ControlParamsArg::ClimateSchedule(params);
        self.command(
            vin,
            RemoteCommand::ScheduleClimate,
            Some(params),
            command_pwd,
        )
        .await
    }

    /// Sets seat heating and ventilation (pyBYD's `set_seat_climate`).
    pub async fn set_seat_climate(
        &self,
        vin: &str,
        params: &SeatClimateParams,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        let params = ControlParamsArg::SeatClimate(params);
        self.command(vin, RemoteCommand::SeatClimate, Some(params), command_pwd)
            .await
    }

    /// Turns battery heating on or off (pyBYD's `set_battery_heat`).
    pub async fn set_battery_heat(
        &self,
        vin: &str,
        params: &BatteryHeatParams,
        command_pwd: Option<&str>,
    ) -> Result<RemoteControlResult, Error> {
        let params = ControlParamsArg::BatteryHeat(params);
        self.command(vin, RemoteCommand::BatteryHeat, Some(params), command_pwd)
            .await
    }

    /// The gate of the charging commands (`START_CHARGE`'s).
    async fn charge_gate(&self, vin: &str, what: &str, endpoint: &str) -> Result<(), Error> {
        let capabilities = self.get_vehicle_capabilities(vin, false).await;
        let learn_info = self.vehicle_fun_learn_info_for(vin).await;
        let gate = evaluate_command_gate(
            RemoteCommand::StartCharge,
            &capabilities,
            None,
            learn_info.as_ref(),
        )
        .map_err(model_error)?;
        if !self.gate_allows(vin, &gate) {
            return Err(blocked(what, vin, &gate, endpoint));
        }
        Ok(())
    }

    /// Saves a smart charging schedule and waits for the change to settle
    /// (pyBYD's `save_charging_schedule`). Times are `"HH:MM"` (the end may
    /// be `"full"`); `charge_way` is `"s"`, `"e"` or weekday indices.
    #[allow(clippy::too_many_arguments)]
    pub async fn save_charging_schedule(
        &self,
        vin: &str,
        start_charge_time: &str,
        end_charge_time: &str,
        charge_way: &str,
        enabled: bool,
        settings: ChargePollSettings,
    ) -> Result<ChargeChangeResult, Error> {
        self.charge_gate(vin, "save_charging_schedule", SAVE_ENDPOINT)
            .await?;
        let fetch = make_save_charging_schedule_fetch_fn(
            start_charge_time,
            end_charge_time,
            charge_way,
            enabled,
        );
        with_reauth!(
            self,
            async {
                let raw = self
                    .trigger_and_poll(TriggeredRead {
                        vin,
                        trigger_endpoint: SAVE_ENDPOINT,
                        poll_endpoint: RESULT_ENDPOINT,
                        fetch: ReadFetch::Save(&fetch),
                        label: "SaveChargingSchedule",
                        mqtt_event_type: Some("smartCharge"),
                        mqtt_timeout: settings.mqtt_timeout,
                        poll_attempts: settings.poll_attempts,
                        poll_interval: settings.poll_interval,
                        signal_retries: 2,
                    })
                    .await?;
                let result: ChargeChangeResult = parse_model(&raw)?;
                match result.res {
                    Some(2) => Ok(result),
                    None => Err(Error::api(
                        ErrorKind::BydRemoteControlError,
                        "smartCharge saveOrUpdate timed out without a terminal result",
                        "timeout",
                        RESULT_ENDPOINT,
                    )),
                    Some(res) => Err(Error::api(
                        ErrorKind::BydRemoteControlError,
                        format!(
                            "smartCharge saveOrUpdate rejected by cloud (res={res}, message={:?})",
                            result.message
                        ),
                        res.to_string(),
                        RESULT_ENDPOINT,
                    )),
                }
            }
            .await
        )
    }

    /// Turns smart charging on or off (pyBYD's `toggle_smart_charging`).
    pub async fn toggle_smart_charging(
        &self,
        vin: &str,
        enable: bool,
    ) -> Result<CommandAck, Error> {
        authed!(self, |cx| toggle_smart_charging(cx, vin, enable))
    }

    /// Starts charging now and waits for the change to settle (pyBYD's
    /// `start_charging`). `res` 3 usually means the car is already charging
    /// or full.
    pub async fn start_charging(
        &self,
        vin: &str,
        settings: ChargePollSettings,
    ) -> Result<ChargeChangeResult, Error> {
        self.charge_gate(vin, "start_charging", TOGGLE_ENDPOINT)
            .await?;
        let fetch = make_change_charge_fetch_fn(true);
        with_reauth!(
                    self,
                    async {
            let raw = self
                .trigger_and_poll(TriggeredRead {
                    vin,
                    trigger_endpoint: TOGGLE_ENDPOINT,
                    poll_endpoint: RESULT_ENDPOINT,
                    fetch: ReadFetch::Charge(&fetch),
                    label: "ChargeStart",
                    mqtt_event_type: Some("smartCharge"),
                    mqtt_timeout: settings.mqtt_timeout,
                    poll_attempts: settings.poll_attempts,
                    poll_interval: settings.poll_interval,
                    signal_retries: 2,
                })
                .await?;
            let result: ChargeChangeResult = parse_model(&raw)?;
            let failed = |message: String, code: String| {
                Error::api(
                    ErrorKind::BydRemoteControlError,
                    message,
                    code,
                    RESULT_ENDPOINT,
                )
            };
            match result.res {
                Some(2) => Ok(result),
                None => Err(failed(
                    "smartCharge change_charge_status timed out without a terminal result".into(),
                    "timeout".into(),
                )),
                // Only seen when the car is already in the target state.
                Some(3) => Err(failed(
                    format!(
                        "smartCharge change_charge_status rejected by cloud (res=3, message={:?}) \
                         - vehicle is likely already in the target state (already charging, at \
                         maximum SoC, or a duplicate start request)",
                        result.message
                    ),
                    "3".into(),
                )),
                Some(res) => Err(failed(
                    format!(
                        "smartCharge change_charge_status unexpected res={res} message={:?}",
                        result.message
                    ),
                    res.to_string(),
                )),
            }
            }
            .await
        )
    }

    /// Not supported: BYD's stop-charge request answers success but the car
    /// keeps charging, so this always fails with
    /// `BydEndpointNotSupportedError` (code `"cloud_stop_unsupported"`)
    /// without sending anything (pyBYD's `stop_charging`). Stop a charge
    /// with the key fob or door handle double-unlock.
    pub async fn stop_charging(
        &self,
        _vin: &str,
        _settings: ChargePollSettings,
    ) -> Result<ChargeChangeResult, Error> {
        Err(Error::api(
            ErrorKind::BydEndpointNotSupportedError,
            "stop_charging is not supported: the BYD cloud changeChargeStatue status='0' is a \
             verified no-op (returns success but does not stop an active charge). Stop an \
             active charge with the key-fob/door-handle double-unlock instead.",
            "cloud_stop_unsupported",
            TOGGLE_ENDPOINT,
        ))
    }
}
