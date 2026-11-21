//! Remote control endpoints (`pybyd/_api/control.py`).
//!
//! - `/control/remoteControl` (trigger) and `/control/remoteControlResult` (poll)
//! - `/control/remoteControlBatteryHeat` and `/control/remoteControlBatteryHeatResult`
//!   (the dedicated battery heat pair)
//! - `/vehicle/vehicleswitch/verifyControlPassword`
//!
//! The inner payload carries `commandPwd` (the MD5 of the control PIN set
//! in the BYD app) and never `instructionCode`.
//!
//! These flows send commands to real cars and decide when a command counts
//! as done, failed or worth sending again. Every branch, sleep and retry
//! count follows pyBYD; the trace parity cases in
//! `parity/groups/api_control.py` hold them to it.
//!
//! Serials are kept as the JSON value BYD sent (normally a string), because
//! pyBYD passes whatever `requestSerial` held on to the next request, the
//! callback and the MQTT waiter, and turns it into text with `str()` only
//! when it builds a request.

use std::future::Future;
use std::pin::Pin;

use openbyd_protocol::coerce::ValidationError;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::control::{
    ControlState, RemoteCommand, RemoteControlResult, VerifyControlPasswordResponse,
};
use openbyd_protocol::parity::{Args, Outcome, ParityError};
use openbyd_protocol::pyjson::{
    DumpOptions, Separators, dumps, py_float_from_str, py_int_from_str, py_str,
};
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_token_json, post_vehicle_json,
};
use crate::env::Env;
use crate::scripted::{Scenario, ScriptedEnv};

/// Codes meaning the control password (PIN) was refused or is not set.
/// 5011 means "not set on this account", not "wrong PIN".
pub const CONTROL_PASSWORD_ERROR_CODES: [&str; 3] = ["5005", "5006", "5011"];
/// Remote control service errors.
pub const REMOTE_CONTROL_SERVICE_ERROR_CODES: [&str; 1] = ["1009"];
/// Generic refusal of a command. On data endpoints the same code means
/// "not supported", so remote control endpoints do not use the
/// not-supported codes.
pub const REMOTE_CONTROL_GENERIC_ERROR_CODES: [&str; 1] = ["1001"];
/// The endpoints that reach the car, posted with the guest `userType` retry.
pub const REMOTE_CONTROL_ENDPOINTS: [&str; 4] = [
    "/control/remoteControl",
    "/control/remoteControlResult",
    "/control/remoteControlBatteryHeat",
    "/control/remoteControlBatteryHeatResult",
];
/// Where the control password is checked.
pub const VERIFY_CONTROL_PASSWORD_ENDPOINT: &str = "/vehicle/vehicleswitch/verifyControlPassword";

/// pyBYD's `_CONTROL_EXTRA_CODES`.
pub const CONTROL_EXTRA_CODES: [(&[&str], ErrorKind); 1] = [(
    &CONTROL_PASSWORD_ERROR_CODES,
    ErrorKind::BydControlPasswordError,
)];
/// pyBYD's `_REMOTE_CONTROL_EXTRA_CODES`.
pub const REMOTE_CONTROL_EXTRA_CODES: [(&[&str], ErrorKind); 3] = [
    (
        &CONTROL_PASSWORD_ERROR_CODES,
        ErrorKind::BydControlPasswordError,
    ),
    (
        &REMOTE_CONTROL_SERVICE_ERROR_CODES,
        ErrorKind::BydRemoteControlError,
    ),
    (
        &REMOTE_CONTROL_GENERIC_ERROR_CODES,
        ErrorKind::BydRemoteControlError,
    ),
];

/// The API code meaning "a previous command is still in progress".
pub const PREVIOUS_COMMAND_IN_PROGRESS_CODE: &str = "6024";

// ---------------------------------------------------------------------------
// Python semantics the flows need
// ---------------------------------------------------------------------------

/// Python's `bool(value)` for JSON.
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|float| float != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(entries) => !entries.is_empty(),
    }
}

/// Python's `int(value)` for JSON, saturated to `i128` (enough to compare
/// with 0 and 2). Text that is not an integer is a `ValueError`; null, a
/// list or an object a `TypeError`.
pub(crate) fn python_int(value: &Value) -> Result<i128, Error> {
    match value {
        Value::Bool(flag) => Ok(i128::from(*flag)),
        Value::Number(number) => Ok(match (number.as_i64(), number.as_u64()) {
            (Some(int), _) => i128::from(int),
            (None, Some(int)) => i128::from(int),
            // `as` truncates toward zero and saturates, as `int()` truncates.
            _ => number.as_f64().unwrap_or(0.0) as i128,
        }),
        Value::String(text) => {
            if let Some(int) = py_int_from_str(text) {
                return Ok(int);
            }
            // A valid literal too long for i128: the float reading of the
            // same digits gives its sign and size.
            let integral = !text.contains(['.', 'e', 'E', 'i', 'I', 'n', 'N']);
            match py_float_from_str(text) {
                Some(float) if integral && float.is_finite() => Ok(float as i128),
                _ => Err(Error::new(
                    ErrorKind::ValueError,
                    format!("invalid literal for int() with base 10: {text:?}"),
                )),
            }
        }
        Value::Null | Value::Array(_) | Value::Object(_) => Err(Error::new(
            ErrorKind::TypeError,
            "int() argument must be a string, a bytes-like object or a real number",
        )),
    }
}

/// The error a failed `model_validate` raises: pydantic's `ValidationError`,
/// or the exception a validator let escape. A value pydantic accepts but
/// `i64` cannot hold ("Unsupported") is reported as a `ValueError`.
pub(crate) fn model_error(error: ValidationError) -> Error {
    let kind = ErrorKind::from_class_name(error.exception).unwrap_or(ErrorKind::ValueError);
    Error::new(kind, error.to_string())
}

/// `Model.model_validate(raw)`, failing as pyBYD does.
pub(crate) fn validate<M: BydModel>(raw: &Value) -> Result<M, Error> {
    M::parse(raw, &Value::Null).map_err(model_error)
}

/// [`validate`] of a value not needed afterwards ([`BydModel::parse_owned`]).
pub(crate) fn validate_owned<M: BydModel>(raw: Value) -> Result<M, Error> {
    M::parse_owned(raw, &Value::Null).map_err(model_error)
}

/// `{"vin": vin, **raw}`, optionally followed by `"raw": raw`.
pub(crate) fn with_vin(vin: &str, raw: &Map<String, Value>, keep_raw: bool) -> Value {
    let mut merged = Map::new();
    merged.insert("vin".into(), json!(vin));
    for (key, value) in raw {
        merged.insert(key.clone(), value.clone());
    }
    if keep_raw {
        merged.insert("raw".into(), Value::Object(raw.clone()));
    }
    Value::Object(merged)
}

/// `build_inner_base(config, vin=vin, request_serial=request_serial)` with
/// a serial of any JSON type: it is sent as `str(serial)` when truthy.
pub(crate) fn inner_base<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    request_serial: Option<&Value>,
) -> Map<String, Value> {
    let serial = request_serial.filter(|serial| truthy(serial)).map(py_str);
    build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        serial.as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Password verification
// ---------------------------------------------------------------------------

/// Checks the control password (PIN) for a vehicle at
/// `/vehicle/vehicleswitch/verifyControlPassword` and returns the reply.
///
/// 5005, 5006 and 5011 fail with `BydControlPasswordError`, 1001 with
/// `BydEndpointNotSupportedError`.
pub async fn verify_control_password<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    command_pwd: &str,
) -> Result<VerifyControlPasswordResponse, Error> {
    // _build_verify_control_password_inner
    let mut inner = inner_base(cx, vin, None);
    inner.insert("commandPwd".into(), json!(command_pwd));
    inner.insert("functionType".into(), json!("remoteControl"));
    let codes = ErrorCodes {
        not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
        extra_code_map: &CONTROL_EXTRA_CODES,
    };
    let data = post_token_json(
        cx,
        VERIFY_CONTROL_PASSWORD_ENDPOINT,
        &inner,
        None,
        Some(vin),
        codes,
        None,
    )
    .await?;
    let raw = data.as_object().cloned().unwrap_or_default();
    validate_owned(with_vin(vin, &raw, true))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Whether a remote control reply is final (pyBYD's `_is_remote_control_ready`).
///
/// It is when `controlState` is set and not 0 (pending), when a `res` of 2
/// or more is present, or when there is a `result` field. `res: 1` means
/// "received, in progress": keep polling. A `controlState` or `res` that
/// `int()` cannot read fails with `ValueError` or `TypeError`, as in pyBYD.
pub fn is_remote_control_ready(data: &Map<String, Value>) -> Result<bool, Error> {
    if data.is_empty() {
        return Ok(false);
    }
    if let Some(state) = data.get("controlState").filter(|state| !state.is_null())
        && python_int(state)? != 0
    {
        return Ok(true);
    }
    if let Some(res) = data.get("res").filter(|res| !res.is_null()) {
        return Ok(python_int(res)? >= 2);
    }
    Ok(data.contains_key("result"))
}

/// Parses a raw remote control result into the typed model.
pub fn parse_remote_control_result_data(data: &Value) -> Result<RemoteControlResult, Error> {
    validate(data)
}

/// pyBYD's `_build_control_inner`: the base fields, `commandPwd` (empty
/// when there is none, left out when `include_command_pwd` is false),
/// `commandType` and, when given, the parameters as `controlParamsMap`
/// (compact JSON with sorted keys).
fn build_control_inner<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    command: RemoteCommand,
    control_params: Option<&Map<String, Value>>,
    command_pwd: Option<&str>,
    request_serial: Option<&Value>,
    include_command_pwd: bool,
) -> Map<String, Value> {
    let mut inner = inner_base(cx, vin, request_serial);
    if include_command_pwd {
        inner.insert("commandPwd".into(), json!(command_pwd.unwrap_or("")));
    }
    inner.insert("commandType".into(), json!(command.as_str()));
    if let Some(params) = control_params {
        let options = DumpOptions {
            ensure_ascii: true,
            separators: Separators::Compact,
            sort_keys: true,
        };
        inner.insert(
            "controlParamsMap".into(),
            json!(dumps(&Value::Object(params.clone()), options)),
        );
    }
    inner
}

/// Posts one control endpoint and returns the reply's data with the serial
/// to use next (pyBYD's `_fetch_control_endpoint`).
///
/// The serial is the reply's `requestSerial` when truthy, else
/// `request_serial` (JSON null for none). When the reply is an object and
/// the serial truthy, the object gets it as `requestSerial` unless it has
/// that key already.
///
/// The [`REMOTE_CONTROL_ENDPOINTS`] are posted with the guest `userType`
/// retry and [`REMOTE_CONTROL_EXTRA_CODES`] (1001 is a refused command
/// there); any other endpoint as a plain token request with the
/// not-supported codes and [`CONTROL_EXTRA_CODES`].
pub async fn fetch_control_endpoint<E: Env>(
    cx: Context<'_, E>,
    endpoint: &str,
    vin: &str,
    command: RemoteCommand,
    control_params: Option<&Map<String, Value>>,
    command_pwd: Option<&str>,
    request_serial: Option<&Value>,
) -> Result<(Value, Value), Error> {
    let inner = build_control_inner(
        cx,
        vin,
        command,
        control_params,
        command_pwd,
        request_serial,
        // The dedicated battery-heat endpoints are sent without a PIN.
        !endpoint.starts_with("/control/remoteControlBatteryHeat"),
    );
    let mut result = if REMOTE_CONTROL_ENDPOINTS.contains(&endpoint) {
        let codes = ErrorCodes {
            not_supported_codes: None,
            extra_code_map: &REMOTE_CONTROL_EXTRA_CODES,
        };
        post_vehicle_json(cx, endpoint, &inner, vin, None, codes).await?
    } else {
        let codes = ErrorCodes {
            not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
            extra_code_map: &CONTROL_EXTRA_CODES,
        };
        post_token_json(cx, endpoint, &inner, None, Some(vin), codes, None).await?
    };
    let next_serial = match result.get("requestSerial") {
        Some(serial) if result.is_object() && truthy(serial) => serial.clone(),
        _ => request_serial.cloned().unwrap_or(Value::Null),
    };
    if let Value::Object(entries) = &mut result
        && truthy(&next_serial)
    {
        entries
            .entry("requestSerial")
            .or_insert_with(|| next_serial.clone());
    }
    Ok((result, next_serial))
}

/// A boxed future, for the object-safe hooks below.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Waits for a command's result to arrive over MQTT: pyBYD's
/// `mqtt_result_waiter` argument of `poll_remote_control`.
///
/// It is called once per attempt with the trigger's serial. `Ok(None)`
/// (nothing arrived), an error, or a result that is still pending makes the
/// flow fall back to HTTP polling.
pub trait MqttResultWaiter: Sync {
    /// Waits for the result of the command with this serial.
    fn wait<'a>(
        &'a self,
        serial: &'a Value,
    ) -> BoxFuture<'a, Result<Option<RemoteControlResult>, Error>>;
}

/// Called with the trigger's serial (JSON null when the reply had none) as
/// soon as the trigger was accepted: pyBYD's `on_trigger_dispatched`.
/// pyBYD ignores anything it raises, so it returns nothing.
pub type TriggerDispatched<'a> = dyn Fn(&Value) + Send + Sync + 'a;

/// The optional hooks of [`poll_remote_control`].
#[derive(Clone, Copy, Default)]
pub struct CommandHooks<'a> {
    /// Waits for the result over MQTT before polling.
    pub mqtt_result_waiter: Option<&'a dyn MqttResultWaiter>,
    /// Told the serial once the trigger is accepted.
    pub on_trigger_dispatched: Option<&'a TriggerDispatched<'a>>,
}

/// How one attempt at a command triggers and polls (pyBYD's defaults).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PollOptions {
    /// Result polls after the trigger (default 10).
    pub poll_attempts: i64,
    /// Seconds slept before each poll; none when not above 0 (default 1.5).
    pub poll_interval: f64,
    /// Triggers sent while the cloud answers 6024 (default 3).
    pub rate_limit_retries: i64,
    /// Seconds slept after each 6024, the last one included (default 5.0).
    pub rate_limit_delay: f64,
}

impl Default for PollOptions {
    fn default() -> Self {
        Self {
            poll_attempts: 10,
            poll_interval: 1.5,
            rate_limit_retries: 3,
            rate_limit_delay: 5.0,
        }
    }
}

/// The tunables of [`poll_remote_control`], with pyBYD's defaults.
///
/// `RemoteControlOptions { command_retries: 1, ..Default::default() }`
/// sends a command once and never again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RemoteControlOptions {
    /// How each attempt triggers and polls.
    pub poll: PollOptions,
    /// Attempts in all when an attempt fails with `BydRemoteControlError`
    /// (default 3). 1 means no retry; 0 or less fails with `AssertionError`
    /// without sending anything, as in pyBYD.
    pub command_retries: i64,
    /// Seconds slept between attempts (default 3.0).
    pub command_retry_delay: f64,
}

impl Default for RemoteControlOptions {
    fn default() -> Self {
        Self {
            poll: PollOptions::default(),
            command_retries: 3,
            command_retry_delay: 3.0,
        }
    }
}

/// The error of a command the car or cloud reported as failed.
fn command_failed(command: RemoteCommand, message: String, code: &str, endpoint: &str) -> Error {
    Error::api(
        ErrorKind::BydRemoteControlError,
        format!("Remote control {} {message}", command.name()),
        code,
        endpoint,
    )
}

/// `data.get("message") or data.get("msg") or "controlState=2"`, as text.
fn failure_message(data: &Value) -> String {
    ["message", "msg"]
        .iter()
        .filter_map(|key| data.get(key))
        .find(|value| truthy(value))
        .map_or_else(|| "controlState=2".to_owned(), py_str)
}

/// Sends a command once and polls until it settles (pyBYD's
/// `_poll_remote_control_once`). Fails on anything but success.
///
/// 1. Trigger, up to `rate_limit_retries` times while the cloud answers
///    6024, sleeping `rate_limit_delay` after each 6024; then
///    `BydRateLimitError`. Any other error ends the attempt. The callback
///    hears the serial once the trigger is accepted.
/// 2. A final trigger reply is the answer (`controlState` 2 fails with
///    `BydRemoteControlError` code `"2"`). With no serial to poll with, the
///    command cannot be confirmed: code `"no_serial"`.
/// 3. The MQTT waiter, if any: a successful result is the answer. Anything
///    else falls back to polling, including a rejection: pyBYD raises it
///    inside the `try` that guards the waiter, so it is swallowed.
/// 4. Up to `poll_attempts` polls, each after `poll_interval` when above 0.
///    API errors are skipped, except an expired session; others end the
///    attempt. The last reply decides: `controlState` 2 is code `"2"`, a
///    reply that never became final is code `"timeout"`.
///
/// The battery heat command with a `batteryHeat` parameter uses the
/// dedicated endpoints and sends its parameters and PIN on every poll.
pub async fn poll_remote_control_once<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    command: RemoteCommand,
    control_params: Option<&Map<String, Value>>,
    command_pwd: Option<&str>,
    options: PollOptions,
    hooks: CommandHooks<'_>,
) -> Result<RemoteControlResult, Error> {
    let is_battery_heat = command == RemoteCommand::BatteryHeat
        && control_params.is_some_and(|params| params.contains_key("batteryHeat"));
    let (trigger_endpoint, poll_endpoint) = if is_battery_heat {
        (
            "/control/remoteControlBatteryHeat",
            "/control/remoteControlBatteryHeatResult",
        )
    } else {
        ("/control/remoteControl", "/control/remoteControlResult")
    };

    // Phase 1: trigger, retrying on 6024.
    let mut triggered = None;
    for _ in 0..options.rate_limit_retries.max(0) {
        match fetch_control_endpoint(
            cx,
            trigger_endpoint,
            vin,
            command,
            control_params,
            command_pwd,
            None,
        )
        .await
        {
            Ok(reply) => {
                if let Some(dispatched) = hooks.on_trigger_dispatched {
                    dispatched(&reply.1);
                }
                triggered = Some(reply);
                break;
            }
            Err(error)
                if error.is_a(ErrorKind::BydApiError)
                    && error.code() == PREVIOUS_COMMAND_IN_PROGRESS_CODE =>
            {
                cx.env.sleep(options.rate_limit_delay).await;
            }
            Err(error) => return Err(error),
        }
    }
    let Some((result, mut serial)) = triggered else {
        return Err(Error::api(
            ErrorKind::BydRateLimitError,
            format!(
                "Remote control {} rate-limited after {} retries (code 6024)",
                command.name(),
                options.rate_limit_retries
            ),
            PREVIOUS_COMMAND_IN_PROGRESS_CODE,
            trigger_endpoint,
        ));
    };

    if let Value::Object(data) = &result
        && is_remote_control_ready(data)?
    {
        let parsed = parse_remote_control_result_data(&result)?;
        if parsed.control_state == ControlState::Failure {
            let message = format!("failed: {}", failure_message(&result));
            return Err(command_failed(command, message, "2", trigger_endpoint));
        }
        return Ok(parsed);
    }

    if !truthy(&serial) {
        // Nothing to correlate an answer with: the command cannot be confirmed.
        return Err(command_failed(
            command,
            "could not be confirmed (vehicle returned no request serial)".into(),
            "no_serial",
            trigger_endpoint,
        ));
    }

    let waited = match hooks.mqtt_result_waiter {
        Some(waiter) => Some(waiter.wait(&serial).await),
        None => None,
    };
    // pyBYD's `except Exception` around the waiter does not catch
    // `asyncio.CancelledError`: a wait cut short by a re-login ends the command.
    if let Some(Err(error)) = &waited
        && error.kind() == ErrorKind::CancelledError
    {
        return Err(error.clone());
    }
    if let Some(Ok(Some(mqtt_result))) = waited
        // Terminal is `controlState != 0` on the model's dump.
        && mqtt_result.control_state != ControlState::Pending
        // pyBYD raises a rejection inside the `try` around the waiter and
        // swallows it, so a rejected command is polled like an unanswered one.
        && mqtt_result.control_state != ControlState::Failure
    {
        return Ok(mqtt_result);
    }

    // Phase 2: poll.
    let mut latest = result;
    let mut reached_terminal = false;
    for _ in 0..options.poll_attempts.max(0) {
        if options.poll_interval > 0.0 {
            cx.env.sleep(options.poll_interval).await;
        }
        match fetch_control_endpoint(
            cx,
            poll_endpoint,
            vin,
            command,
            control_params.filter(|_| is_battery_heat),
            command_pwd.filter(|_| is_battery_heat),
            Some(&serial),
        )
        .await
        {
            Ok((reply, next_serial)) => {
                latest = reply;
                serial = next_serial;
                if let Value::Object(data) = &latest
                    && is_remote_control_ready(data)?
                {
                    reached_terminal = true;
                    break;
                }
            }
            Err(error) if error.is_a(ErrorKind::BydSessionExpiredError) => return Err(error),
            Err(error) if error.is_a(ErrorKind::BydApiError) => {}
            Err(error) => return Err(error),
        }
    }

    let latest = if latest.is_object() {
        latest
    } else {
        json!({})
    };
    let parsed = parse_remote_control_result_data(&latest)?;
    if parsed.control_state == ControlState::Failure {
        let message = format!("failed: {}", failure_message(&latest));
        return Err(command_failed(command, message, "2", poll_endpoint));
    }
    if !reached_terminal {
        // Typically a deeply asleep or offline car: not a quiet success.
        return Err(command_failed(
            command,
            "not confirmed within timeout (vehicle may be asleep or offline)".into(),
            "timeout",
            poll_endpoint,
        ));
    }
    Ok(parsed)
}

/// Sends a remote command and polls until it completes (pyBYD's
/// `poll_remote_control`).
///
/// Each attempt is [`poll_remote_control_once`]. An attempt that fails with
/// `BydRemoteControlError` (refused, failed, unconfirmed, timed out, or the
/// cloud's 1001/1009) is made again after `command_retry_delay`, up to
/// `command_retries` attempts in all; then the last such error is returned.
/// Any other error is returned at once.
pub async fn poll_remote_control<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    command: RemoteCommand,
    control_params: Option<&Map<String, Value>>,
    command_pwd: Option<&str>,
    options: RemoteControlOptions,
    hooks: CommandHooks<'_>,
) -> Result<RemoteControlResult, Error> {
    let mut last_error = None;
    let retries = options.command_retries;
    for attempt in 1..=retries {
        match poll_remote_control_once(
            cx,
            vin,
            command,
            control_params,
            command_pwd,
            options.poll,
            hooks,
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(error) if error.is_a(ErrorKind::BydRemoteControlError) => {
                last_error = Some(error);
                if attempt < retries {
                    cx.env.sleep(options.command_retry_delay).await;
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        Error::new(
            ErrorKind::AssertionError,
            "poll_remote_control ran no attempt (command_retries < 1)",
        )
    }))
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

/// An MQTT waiter that follows a script: `{"sleep": seconds, "raises":
/// bool, "result": raw}`. It records `{"mqtt_wait": serial}`, sleeps when
/// told, fails when told, and otherwise returns the parsed `result` (none
/// for null).
struct ScriptedWaiter<'a> {
    env: &'a ScriptedEnv,
    script: &'a Value,
}

impl MqttResultWaiter for ScriptedWaiter<'_> {
    fn wait<'a>(
        &'a self,
        serial: &'a Value,
    ) -> BoxFuture<'a, Result<Option<RemoteControlResult>, Error>> {
        Box::pin(async move {
            self.env.record(json!({ "mqtt_wait": serial }));
            if let Some(seconds) = self.script.get("sleep").and_then(Value::as_f64) {
                self.env.sleep(seconds).await;
            }
            if self.script.get("raises").is_some_and(truthy) {
                return Err(Error::new(ErrorKind::BydError, "scripted MQTT failure"));
            }
            match self.script.get("result").unwrap_or(&Value::Null) {
                Value::Null => Ok(None),
                raw => validate(raw).map(Some),
            }
        })
    }
}

pub(crate) fn command_argument(args: &Args<'_>) -> Result<RemoteCommand, ParityError> {
    let value = args.str("command")?;
    RemoteCommand::from_value(value)
        .ok_or_else(|| ParityError::bad_arguments(format!("unknown command {value:?}")))
}

fn params_argument<'a>(args: &Args<'a>) -> Result<Option<&'a Map<String, Value>>, ParityError> {
    match args.value("control_params") {
        Value::Null => Ok(None),
        Value::Object(params) => Ok(Some(params)),
        _ => Err(ParityError::bad_arguments(
            "control_params must be an object or null",
        )),
    }
}

fn int_or(args: &Args<'_>, key: &str, default: i64) -> Result<i64, ParityError> {
    match args.value(key) {
        Value::Null => Ok(default),
        _ => args.i64(key),
    }
}

fn float_or(args: &Args<'_>, key: &str, default: f64) -> Result<f64, ParityError> {
    match args.value(key) {
        Value::Null => Ok(default),
        value => value
            .as_f64()
            .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be a number"))),
    }
}

fn poll_options(args: &Args<'_>) -> Result<PollOptions, ParityError> {
    let defaults = PollOptions::default();
    Ok(PollOptions {
        poll_attempts: int_or(args, "poll_attempts", defaults.poll_attempts)?,
        poll_interval: float_or(args, "poll_interval", defaults.poll_interval)?,
        rate_limit_retries: int_or(args, "rate_limit_retries", defaults.rate_limit_retries)?,
        rate_limit_delay: float_or(args, "rate_limit_delay", defaults.rate_limit_delay)?,
    })
}

/// Runs `poll_remote_control` (`retries`) or `_poll_remote_control_once`.
fn command_call(scenario: &Scenario, retries: bool) -> Outcome {
    let args = scenario.args();
    let vin = args.str("vin")?;
    let command = command_argument(&args)?;
    let control_params = params_argument(&args)?;
    let command_pwd = args.opt_str("command_pwd")?;
    let poll = poll_options(&args)?;
    let defaults = RemoteControlOptions::default();
    let options = RemoteControlOptions {
        poll,
        command_retries: int_or(&args, "command_retries", defaults.command_retries)?,
        command_retry_delay: float_or(&args, "command_retry_delay", defaults.command_retry_delay)?,
    };
    let cx = scenario.context();
    let env = cx.env;
    let waiter_script = args.value("mqtt_result_waiter");
    let waiter = ScriptedWaiter {
        env,
        script: waiter_script,
    };
    let dispatched = |serial: &Value| env.record(json!({ "dispatched": serial }));
    let hooks = CommandHooks {
        mqtt_result_waiter: (!waiter_script.is_null()).then_some(&waiter as &dyn MqttResultWaiter),
        // `true`, or `"raises"` for a callback that raises after recording
        // (pyBYD swallows it; a Rust callback cannot fail).
        on_trigger_dispatched: truthy(args.value("on_trigger_dispatched"))
            .then_some(&dispatched as &TriggerDispatched<'_>),
    };
    let dump = |result: RemoteControlResult| result.dump();
    if retries {
        scenario.run(
            poll_remote_control(
                cx,
                vin,
                command,
                control_params,
                command_pwd,
                options,
                hooks,
            ),
            dump,
        )
    } else {
        scenario.run(
            poll_remote_control_once(cx, vin, command, control_params, command_pwd, poll, hooks),
            dump,
        )
    }
}

fn fetch_call(scenario: &Scenario) -> Outcome {
    let args = scenario.args();
    let request_serial = match args.value("request_serial") {
        Value::Null => None,
        serial => Some(serial),
    };
    scenario.run(
        fetch_control_endpoint(
            scenario.context(),
            args.str("endpoint")?,
            args.str("vin")?,
            command_argument(&args)?,
            params_argument(&args)?,
            args.opt_str("command_pwd")?,
            request_serial,
        ),
        |(result, serial)| json!([result, serial]),
    )
}

fn constants() -> Value {
    let sorted = |codes: &[&str]| {
        let mut codes = codes.to_vec();
        codes.sort_unstable();
        json!(codes)
    };
    let map = |entries: &[(&[&str], ErrorKind)]| {
        Value::Array(
            entries
                .iter()
                .map(|(codes, kind)| json!([sorted(codes), kind.class_name()]))
                .collect(),
        )
    };
    json!({
        "_CONTROL_PASSWORD_ERROR_CODES": sorted(&CONTROL_PASSWORD_ERROR_CODES),
        "_REMOTE_CONTROL_SERVICE_ERROR_CODES": sorted(&REMOTE_CONTROL_SERVICE_ERROR_CODES),
        "_REMOTE_CONTROL_GENERIC_ERROR_CODES": sorted(&REMOTE_CONTROL_GENERIC_ERROR_CODES),
        "_REMOTE_CONTROL_ENDPOINTS": sorted(&REMOTE_CONTROL_ENDPOINTS),
        "_VERIFY_CONTROL_PASSWORD_ENDPOINT": VERIFY_CONTROL_PASSWORD_ENDPOINT,
        "_CONTROL_EXTRA_CODES": map(&CONTROL_EXTRA_CODES),
        "_REMOTE_CONTROL_EXTRA_CODES": map(&REMOTE_CONTROL_EXTRA_CODES),
    })
}

fn ready_call(args: &Args<'_>) -> Outcome {
    match args.value("data") {
        Value::Object(data) => Ok(json!(is_remote_control_ready(data)?)),
        // `if not data: return False`; anything else truthy has no `get`.
        other if !truthy(other) => Ok(json!(false)),
        _ => Err(ParityError::new(
            "AttributeError",
            "object has no attribute 'get'",
        )),
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let flow = |run: fn(&Scenario) -> Outcome| Scenario::from_args(args).and_then(|s| run(&s));
    Some(match name {
        "api.control.constants" => Ok(constants()),
        "api.control._is_remote_control_ready" => ready_call(&Args(args)),
        "api.control.parse_remote_control_result_data" => {
            parse_remote_control_result_data(Args(args).value("data"))
                .map(|result| result.dump())
                .map_err(ParityError::from)
        }
        "api.control.verify_control_password" => flow(|scenario| {
            let args = scenario.args();
            scenario.run(
                verify_control_password(
                    scenario.context(),
                    args.str("vin")?,
                    args.str("command_pwd")?,
                ),
                |response| response.dump(),
            )
        }),
        "api.control._fetch_control_endpoint" => flow(fetch_call),
        "api.control._poll_remote_control_once" => flow(|scenario| command_call(scenario, false)),
        "api.control.poll_remote_control" => flow(|scenario| command_call(scenario, true)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use openbyd_protocol::config::BydConfig;
    use openbyd_protocol::session::Session;

    use super::*;
    use crate::scripted::{Reply, block_on};

    #[test]
    fn python_int_follows_python() {
        let int = |value: Value| python_int(&value).map_err(|error| error.kind());
        assert_eq!(int(json!(true)), Ok(1));
        assert_eq!(int(json!(-3)), Ok(-3));
        assert_eq!(int(json!(u64::MAX)), Ok(i128::from(u64::MAX)));
        assert_eq!(int(json!(2.9)), Ok(2));
        assert_eq!(int(json!(-1e300)), Ok(i128::MIN));
        assert_eq!(int(json!(" ２ ")), Ok(2));
        assert_eq!(int(json!("9".repeat(50))), Ok(i128::MAX));
        assert_eq!(int(json!(format!("-{}", "9".repeat(50)))), Ok(i128::MIN));
        for bad in ["1.5", "1e3", "inf", "nan", "", "x"] {
            assert_eq!(int(json!(bad)), Err(ErrorKind::ValueError), "{bad}");
        }
        for bad in [json!(null), json!([]), json!({})] {
            assert_eq!(int(bad), Err(ErrorKind::TypeError));
        }
    }

    #[test]
    fn readiness() {
        let ready = |value: Value| ready_call(&Args(&json!({ "data": value })));
        assert_eq!(ready(json!({})).unwrap(), json!(false));
        assert_eq!(ready(json!(null)).unwrap(), json!(false));
        assert_eq!(ready(json!({"controlState": 0})).unwrap(), json!(false));
        assert_eq!(
            ready(json!({"controlState": 0, "res": 2})).unwrap(),
            json!(true)
        );
        assert_eq!(ready(json!({"res": 1})).unwrap(), json!(false));
        assert_eq!(ready(json!({"result": null})).unwrap(), json!(true));
        assert_eq!(ready(json!([1])).unwrap_err().kind, "AttributeError");
        assert_eq!(
            ready(json!({"controlState": "x"})).unwrap_err().kind,
            "ValueError"
        );
    }

    #[test]
    fn messages_and_errors() {
        assert_eq!(failure_message(&json!({"message": "", "msg": 5})), "5");
        assert_eq!(failure_message(&json!({})), "controlState=2");
        let error = model_error(ValidationError::unsupported_int());
        assert_eq!(error.kind(), ErrorKind::ValueError);
        assert!(constants()["_REMOTE_CONTROL_EXTRA_CODES"].is_array());
    }

    struct Rejecting;

    impl MqttResultWaiter for Rejecting {
        fn wait<'a>(
            &'a self,
            _serial: &'a Value,
        ) -> BoxFuture<'a, Result<Option<RemoteControlResult>, Error>> {
            Box::pin(async { Ok(Some(validate(&json!({"controlState": 2}))?)) })
        }
    }

    #[test]
    fn a_rejection_over_mqtt_falls_back_to_polling() {
        let config = BydConfig::new("user@example.com", "p");
        let session = Session::new("1", "s", "e", 0.0, 1.0);
        let key = openbyd_protocol::hashing::md5_hex("e");
        let sealed = |data: Value| {
            let text = data.to_string();
            Reply::Outer(json!({
                "code": "0",
                "respondData": openbyd_protocol::aes::aes_encrypt_hex(&text, &key).unwrap(),
            }))
        };
        let env = ScriptedEnv::new(
            0,
            "s",
            vec![
                sealed(json!({"requestSerial": "S"})),
                sealed(json!({"controlState": 1})),
            ],
        );
        let cx = Context::new(&env, &config, &session);
        let hooks = CommandHooks {
            mqtt_result_waiter: Some(&Rejecting),
            on_trigger_dispatched: None,
        };
        let options = RemoteControlOptions {
            command_retries: 1,
            ..Default::default()
        };
        let flow = poll_remote_control(
            cx,
            "TESTVIN0000000001",
            RemoteCommand::Lock,
            None,
            Some("pin"),
            options,
            hooks,
        );
        let result = block_on(flow).unwrap().unwrap();
        assert!(result.success);
        assert_eq!(env.unused_replies(), 0);
    }

    #[test]
    fn arguments_are_checked() {
        let base = json!({
            "config": {"username": "user@example.com", "password": "p"},
            "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
            "now_ms": 5, "seed": "x", "replies": [],
            "args": {"vin": "V", "command": "LOCKDOOR", "endpoint": "/e"},
        });
        let kind = |name: &str, key: &str, value: Value| {
            let mut args = base.clone();
            args["args"][key] = value;
            parity_call(name, &args)
                .unwrap()
                .err()
                .map(|error| error.kind)
        };
        for name in [
            "api.control.poll_remote_control",
            "api.control._poll_remote_control_once",
            "api.control._fetch_control_endpoint",
        ] {
            assert_eq!(
                kind(name, "command", json!("NOPE")).unwrap(),
                "BadArguments"
            );
            assert_eq!(
                kind(name, "control_params", json!([])).unwrap(),
                "BadArguments"
            );
        }
        let poll = "api.control.poll_remote_control";
        assert_eq!(
            kind(poll, "poll_interval", json!("x")).unwrap(),
            "BadArguments"
        );
        assert_eq!(
            kind(poll, "poll_attempts", json!(1.5)).unwrap(),
            "BadArguments"
        );
        assert_eq!(
            kind(
                "api.control.verify_control_password",
                "command_pwd",
                json!(1)
            )
            .unwrap(),
            "BadArguments"
        );
        assert!(parity_call("api.control.nothing", &base).is_none());
    }
}
