//! Remote control commands, their parameters and their results (`pybyd/models/control.py`).
//!
//! Three kinds of thing live here:
//!
//! - the enums: [`RemoteCommand`] (the `commandType` sent to the car),
//!   [`ControlState`] and [`CommandLifecycleStatus`];
//! - reply models, parsed like every other pyBYD model ([`BydModel`]):
//!   [`RemoteControlResult`], [`CommandAck`], [`ChargeChangeResult`],
//!   [`CommandAckEvent`], [`CommandLifecycleEvent`],
//!   [`CommandAckDiagnostics`] and [`VerifyControlPasswordResponse`];
//! - command parameters, which go the other way: built from arguments,
//!   validated like pyBYD's frozen `extra="forbid"` pydantic models, and
//!   serialised with `to_control_params_map()` into the `controlParamsMap`
//!   sent to the car: [`ClimateStartParams`], [`ClimateScheduleParams`],
//!   [`SeatClimateParams`] and [`BatteryHeatParams`].
//!
//! A parameter set is built either from typed arguments
//! (`ClimateStartParams::new(ClimateStartArgs { temperature: Some(21.0), ..Default::default() })`)
//! or from keyword arguments as JSON (`from_kwargs`), which accepts exactly
//! what the Python constructor accepts, by field name or camelCase alias.

use indexmap::IndexMap;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields, aliased, py_truthy, to_camel};
use super::hvac::HvacStatus;
use super::realtime::{SeatHeatVentState, StearingWheelHeat, VehicleRealtimeData};
use crate::coerce::{self, Failures, IntEnum, StrEnum, ValidationError, py_decimal};
use crate::constants::{self, DEFAULT_TEMP_OFFSET_C};
use crate::parity::{Args, Outcome, ParityError};
use crate::pyjson::py_str;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Remote control `commandType` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteCommand {
    /// `LOCKDOOR`.
    Lock,
    /// `OPENDOOR`.
    Unlock,
    /// `OPENAIR`.
    StartClimate,
    /// `CLOSEAIR`.
    StopClimate,
    /// `BOOKINGAIR`.
    ScheduleClimate,
    /// `FINDCAR`.
    FindCar,
    /// `FLASHLIGHTNOWHISTLE`.
    FlashLights,
    /// `CLOSEWINDOW`.
    CloseWindows,
    /// `OPENWINDOW`: opens the windows to a vent crack (about 10 %), not fully.
    OpenWindows,
    /// `VENTILATIONHEATING`: seat heating and ventilation, steering wheel heat.
    SeatClimate,
    /// `BATTERYHEAT`.
    BatteryHeat,
    /// `OPENTRUNK`.
    OpenTrunk,
    /// `CLOSETRUNK`.
    CloseTrunk,
    /// `SMARTCHARGESTART`: synthetic, never sent as a `commandType`. Charging
    /// is toggled through `/control/smartCharge/changeChargeStatue`; the member
    /// exists so the gating table can express its `functionNo`.
    StartCharge,
    /// `SMARTCHARGESTOP`: synthetic, like [`Self::StartCharge`].
    StopCharge,
}

impl RemoteCommand {
    /// Every member, in definition order.
    pub const ALL: [Self; 15] = [
        Self::Lock,
        Self::Unlock,
        Self::StartClimate,
        Self::StopClimate,
        Self::ScheduleClimate,
        Self::FindCar,
        Self::FlashLights,
        Self::CloseWindows,
        Self::OpenWindows,
        Self::SeatClimate,
        Self::BatteryHeat,
        Self::OpenTrunk,
        Self::CloseTrunk,
        Self::StartCharge,
        Self::StopCharge,
    ];

    /// The `commandType` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lock => "LOCKDOOR",
            Self::Unlock => "OPENDOOR",
            Self::StartClimate => "OPENAIR",
            Self::StopClimate => "CLOSEAIR",
            Self::ScheduleClimate => "BOOKINGAIR",
            Self::FindCar => "FINDCAR",
            Self::FlashLights => "FLASHLIGHTNOWHISTLE",
            Self::CloseWindows => "CLOSEWINDOW",
            Self::OpenWindows => "OPENWINDOW",
            Self::SeatClimate => "VENTILATIONHEATING",
            Self::BatteryHeat => "BATTERYHEAT",
            Self::OpenTrunk => "OPENTRUNK",
            Self::CloseTrunk => "CLOSETRUNK",
            Self::StartCharge => "SMARTCHARGESTART",
            Self::StopCharge => "SMARTCHARGESTOP",
        }
    }

    /// The Python member name (`"LOCK"`, `"START_CLIMATE"`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::Lock => "LOCK",
            Self::Unlock => "UNLOCK",
            Self::StartClimate => "START_CLIMATE",
            Self::StopClimate => "STOP_CLIMATE",
            Self::ScheduleClimate => "SCHEDULE_CLIMATE",
            Self::FindCar => "FIND_CAR",
            Self::FlashLights => "FLASH_LIGHTS",
            Self::CloseWindows => "CLOSE_WINDOWS",
            Self::OpenWindows => "OPEN_WINDOWS",
            Self::SeatClimate => "SEAT_CLIMATE",
            Self::BatteryHeat => "BATTERY_HEAT",
            Self::OpenTrunk => "OPEN_TRUNK",
            Self::CloseTrunk => "CLOSE_TRUNK",
            Self::StartCharge => "START_CHARGE",
            Self::StopCharge => "STOP_CHARGE",
        }
    }

    /// `RemoteCommand(value)`: the member with this `commandType`, if any.
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|command| command.as_str() == value)
    }
}

impl StrEnum for RemoteCommand {
    const MEMBERS: &'static [Self] = &Self::ALL;
    fn value(self) -> &'static str {
        self.as_str()
    }
}

impl Serialize for RemoteCommand {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Control command execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ControlState {
    /// Still running (also BYD's `res: 1`).
    #[default]
    Pending = 0,
    /// Finished successfully (`res: 2`).
    Success = 1,
    /// Finished with a failure (any other `res`).
    Failure = 2,
}

impl ControlState {
    /// The Python member name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Success => "SUCCESS",
            Self::Failure => "FAILURE",
        }
    }
}

impl IntEnum for ControlState {
    const MEMBERS: &'static [Self] = &[Self::Pending, Self::Success, Self::Failure];
    const MISSING: Option<Self> = None;
    fn value(self) -> i64 {
        self as i64
    }
}

/// Lifecycle state for deterministic command ACK correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CommandLifecycleStatus {
    /// `"registered"`.
    #[default]
    Registered,
    /// `"matched"`.
    Matched,
    /// `"expired"`.
    Expired,
    /// `"uncorrelated"`.
    Uncorrelated,
}

impl CommandLifecycleStatus {
    /// The status string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Matched => "matched",
            Self::Expired => "expired",
            Self::Uncorrelated => "uncorrelated",
        }
    }

    /// The Python member name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Registered => "REGISTERED",
            Self::Matched => "MATCHED",
            Self::Expired => "EXPIRED",
            Self::Uncorrelated => "UNCORRELATED",
        }
    }
}

impl StrEnum for CommandLifecycleStatus {
    const MEMBERS: &'static [Self] = &[
        Self::Registered,
        Self::Matched,
        Self::Expired,
        Self::Uncorrelated,
    ];
    fn value(self) -> &'static str {
        self.as_str()
    }
}

impl Serialize for CommandLifecycleStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Python helpers the validators need
// ---------------------------------------------------------------------------

/// What Python's `int(value)` does with a JSON value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PyIntOf {
    /// An integer that fits `i64`.
    Int(i64),
    /// A valid integer outside `i64`.
    Big,
    /// `int()` raised `ValueError` (unparseable text).
    ValueError,
    /// `int()` raised `TypeError` (`None`, a list or a dict).
    TypeError,
}

/// Python's `int(value)` for JSON: floats truncate, text is read in base 10
/// the way CPython reads it (any Unicode decimal digits, Unicode whitespace
/// around it, `_` between digits).
fn python_int(value: &Value) -> PyIntOf {
    match value {
        Value::Null | Value::Array(_) | Value::Object(_) => PyIntOf::TypeError,
        Value::Bool(flag) => PyIntOf::Int(i64::from(*flag)),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                PyIntOf::Int(int)
            } else if number.as_u64().is_some() {
                PyIntOf::Big
            } else {
                let whole = number.as_f64().unwrap_or(0.0).trunc();
                // 2**63 is exactly representable; i64 holds [-2**63, 2**63).
                let limit = -(i64::MIN as f64);
                if (-limit..limit).contains(&whole) {
                    PyIntOf::Int(whole as i64)
                } else {
                    PyIntOf::Big
                }
            }
        }
        Value::String(text) => python_int_text(text),
    }
}

/// CPython's `int(str)`: non-ASCII decimal digits become ASCII, non-ASCII
/// whitespace becomes a space, any other non-ASCII character fails, then
/// ASCII whitespace is stripped and a base-10 literal must remain.
fn python_int_text(text: &str) -> PyIntOf {
    let mut mapped = String::with_capacity(text.len());
    for c in text.chars() {
        if u32::from(c) < 0x7f {
            mapped.push(c);
        } else if c.is_whitespace() {
            mapped.push(' ');
        } else if let Some(digit) = py_decimal(c) {
            mapped.push(char::from_digit(digit, 10).expect("a decimal digit"));
        } else {
            return PyIntOf::ValueError;
        }
    }
    let trimmed = mapped.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b');
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let well_formed = digits.starts_with(|c: char| c.is_ascii_digit())
        && digits.ends_with(|c: char| c.is_ascii_digit())
        && digits.chars().all(|c| c.is_ascii_digit() || c == '_')
        && !digits.contains("__");
    if !well_formed {
        return PyIntOf::ValueError;
    }
    let mut value: i128 = 0;
    for c in digits.chars().filter(char::is_ascii_digit) {
        value = value * 10 + i128::from(c as u8 - b'0');
        if value > i128::from(i64::MAX) + 1 {
            return PyIntOf::Big;
        }
    }
    let value = if negative { -value } else { value };
    i64::try_from(value).map_or(PyIntOf::Big, PyIntOf::Int)
}

/// `merged.get("requestSerial") or merged.get("request_serial")`, kept only when it is a string.
fn request_serial_of(merged: &Map<String, Value>) -> Value {
    let first = merged.get("requestSerial").unwrap_or(&Value::Null);
    let serial = if py_truthy(first) {
        first
    } else {
        merged.get("request_serial").unwrap_or(&Value::Null)
    };
    if serial.is_string() {
        serial.clone()
    } else {
        Value::Null
    }
}

/// `str(value)` unless the value is `None` or already a string.
fn text_or_none(value: Option<&Value>) -> Value {
    match value {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(text)) => json!(text),
        Some(other) => json!(py_str(other)),
    }
}

/// A `ValueError` raised in a model validator, which pydantic reports as a `ValidationError`.
fn value_error() -> ValidationError {
    ValidationError::new("value_error")
}

/// A `TypeError` raised in a model validator, which escapes `model_validate`.
fn type_error() -> ValidationError {
    ValidationError::raised("TypeError", "type_error")
}

// ---------------------------------------------------------------------------
// Command result
// ---------------------------------------------------------------------------

/// Result of a remote control command.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RemoteControlResult {
    /// Original API response dict (after the shape normalisation).
    pub raw: Map<String, Value>,
    /// Pending, success or failure.
    #[serde(serialize_with = "base::ser_int_enum")]
    pub control_state: ControlState,
    /// Whether the command finished successfully.
    pub success: bool,
    /// Serial of the request this result belongs to.
    pub request_serial: Option<String>,
}

/// `RemoteControlResult._normalize_shapes`.
///
/// BYD's `res` field (from `/remoteControlResult` and MQTT acks) is mapped
/// onto `controlState`: `1` is still running, `2` success, anything else a
/// failure. The polled `{"controlState": 0/1/2}` shape gets `success` filled in.
fn normalize_result_shapes(values: &Value) -> Result<Value, ValidationError> {
    let Value::Object(values) = values else {
        return Ok(values.clone());
    };
    let mut merged = values.clone();
    if merged.contains_key("res")
        && !merged.contains_key("controlState")
        && !merged.contains_key("control_state")
    {
        let state = match python_int(&merged["res"]) {
            PyIntOf::Int(1) => ControlState::Pending,
            PyIntOf::Int(2) => ControlState::Success,
            PyIntOf::Int(_) | PyIntOf::Big => ControlState::Failure,
            PyIntOf::ValueError => return Err(value_error()),
            PyIntOf::TypeError => return Err(type_error()),
        };
        merged.insert("controlState".to_owned(), json!(state as i64));
        if !merged.contains_key("success") {
            merged.insert("success".to_owned(), json!(state == ControlState::Success));
        }
    }
    if merged.contains_key("controlState") && !merged.contains_key("success") {
        let state = match python_int(&merged["controlState"]) {
            PyIntOf::Int(1) => ControlState::Success,
            PyIntOf::Int(2) => ControlState::Failure,
            // `ControlState(...)` or `int(...)` raised ValueError, or it was 0.
            PyIntOf::Int(_) | PyIntOf::Big | PyIntOf::ValueError => ControlState::Pending,
            PyIntOf::TypeError => return Err(type_error()),
        };
        merged.insert("controlState".to_owned(), json!(state as i64));
        merged.insert("success".to_owned(), json!(state == ControlState::Success));
    }
    Ok(Value::Object(merged))
}

impl BydModel for RemoteControlResult {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let values = normalize_result_shapes(raw)?;
        let mut fields = Fields::cleaned(&values, &[])?;
        let model = Self {
            raw: fields.raw(),
            control_state: fields.get(
                aliased("control_state", &["controlState"]),
                ControlState::Pending,
                coerce::int_enum,
            ),
            success: fields.get("success", false, coerce::boolean),
            request_serial: fields.opt(
                aliased("request_serial", &["requestSerial"]),
                coerce::string,
            ),
        };
        fields.finish()?;
        Ok(model)
    }
}

// ---------------------------------------------------------------------------
// Command acknowledgement responses
// ---------------------------------------------------------------------------

/// Generic acknowledgement response for write/toggle endpoints.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandAck {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// The endpoint's result text; anything that is not a string becomes `None`.
    pub result: Option<String>,
    /// Serial of the request, if the endpoint returned one.
    pub request_serial: Option<String>,
}

impl BydModel for CommandAck {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        // _coerce_result: `result` is a string or None.
        let values = match raw {
            Value::Object(values)
                if values
                    .get("result")
                    .is_some_and(|result| !result.is_null() && !result.is_string()) =>
            {
                let mut values = values.clone();
                values.insert("result".to_owned(), Value::Null);
                Value::Object(values)
            }
            other => other.clone(),
        };
        let mut fields = Fields::cleaned(&values, &[])?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            result: fields.opt("result", coerce::string),
            request_serial: fields.opt(
                aliased("request_serial", &["requestSerial"]),
                coerce::string,
            ),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Terminal result of `/control/smartCharge/changeChargeStatue`, from the
/// MQTT `smartCharge` push or the `/control/smartCharge/changeResult` poll.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChargeChangeResult {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// BYD `res` code: `1` pending (keep polling), `2` success, anything else a failure.
    pub res: Option<i64>,
    /// Locale-dependent message; branch on [`Self::res`], not on this.
    pub message: Option<String>,
    /// Serial of the request.
    pub request_serial: Option<String>,
}

impl BydModel for ChargeChangeResult {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        // `_capture_raw` stores a copy of the input as `raw` unless it has one,
        // which is what the base validator does anyway.
        let mut fields = Fields::cleaned(raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            res: fields.opt("res", coerce::int),
            message: fields.opt("message", coerce::string),
            request_serial: fields.opt(
                aliased("request_serial", &["requestSerial"]),
                coerce::string,
            ),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Structured MQTT remote-control acknowledgement event.
///
/// `request_serial` is the only deterministic correlation key and may be
/// `None` for diagnostics-only (uncorrelated) MQTT events.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandAckEvent {
    /// The `raw` dict the event carried (`{}` when it had none or a non-dict).
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// Serial of the request this event acknowledges.
    pub request_serial: Option<String>,
    /// The MQTT message UUID, if any.
    pub raw_uuid: Option<String>,
    /// Result text (anything else is turned into text with Python's `str()`).
    pub result: Option<String>,
    /// Whether the command succeeded; anything but a JSON boolean is `false`.
    pub success: bool,
    /// Event time as sent.
    pub timestamp: Option<i64>,
}

impl CommandAckEvent {
    /// Whether this event can be matched deterministically (it has a VIN and a serial).
    pub fn is_correlated(&self) -> bool {
        !self.vin.is_empty()
            && self
                .request_serial
                .as_deref()
                .is_some_and(|s| !s.is_empty())
    }
}

/// `CommandAckEvent._normalize_shape`.
fn normalize_ack_event(values: &Value) -> Result<Value, ValidationError> {
    let Value::Object(values) = values else {
        return Ok(values.clone());
    };
    let mut merged = values.clone();
    let serial = request_serial_of(&merged);
    let raw = match merged.get("raw") {
        Some(Value::Object(raw)) => Value::Object(raw.clone()),
        _ => json!({}),
    };
    let raw_uuid = match merged.get("raw_uuid") {
        Some(Value::String(text)) => json!(text),
        _ => Value::Null,
    };
    let timestamp = match merged.get("timestamp") {
        None | Some(Value::Null) => Value::Null,
        // isinstance(True, int) holds, so booleans are kept as they are.
        Some(value @ Value::Bool(_)) => value.clone(),
        Some(value) => match python_int(value) {
            PyIntOf::Int(int) => json!(int),
            PyIntOf::Big => return Err(ValidationError::unsupported_int()),
            PyIntOf::ValueError | PyIntOf::TypeError => Value::Null,
        },
    };
    let result = text_or_none(merged.get("result"));
    let success = match merged.get("success") {
        Some(Value::Bool(flag)) => *flag,
        _ => false,
    };
    merged.insert("requestSerial".to_owned(), serial);
    merged.insert("raw_uuid".to_owned(), raw_uuid);
    merged.insert("timestamp".to_owned(), timestamp);
    merged.insert("result".to_owned(), result);
    merged.insert("success".to_owned(), json!(success));
    merged.insert("raw".to_owned(), raw);
    Ok(Value::Object(merged))
}

impl BydModel for CommandAckEvent {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let values = normalize_ack_event(raw)?;
        let mut fields = Fields::cleaned(&values, &[])?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            request_serial: fields.opt(
                aliased("request_serial", &["requestSerial"]),
                coerce::string,
            ),
            raw_uuid: fields.opt("raw_uuid", coerce::string),
            result: fields.opt("result", coerce::string),
            success: fields.get("success", false, coerce::boolean),
            timestamp: fields.opt("timestamp", coerce::int),
        };
        fields.finish()?;
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([("is_correlated", json!(self.is_correlated()))])
    }
}

/// Lifecycle event emitted by the client command ACK registry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandLifecycleEvent {
    /// Original dict (after the shape normalisation).
    pub raw: Map<String, Value>,
    /// Registered, matched, expired or uncorrelated.
    pub status: CommandLifecycleStatus,
    /// Vehicle identification number.
    pub vin: String,
    /// Serial of the request.
    pub request_serial: Option<String>,
    /// The command, as text.
    pub command: Option<String>,
    /// Event time; `0` when missing or unreadable.
    pub timestamp: i64,
    /// The acknowledgement that matched, if any.
    pub ack: Option<CommandAckEvent>,
    /// Why the event happened, as text.
    pub reason: Option<String>,
}

/// `CommandLifecycleEvent._normalize_shape`.
fn normalize_lifecycle_event(values: &Value) -> Result<Value, ValidationError> {
    let Value::Object(values) = values else {
        return Ok(values.clone());
    };
    let mut merged = values.clone();
    let serial = request_serial_of(&merged);
    let command = text_or_none(merged.get("command"));
    let timestamp = match merged.get("timestamp") {
        None | Some(Value::Null) => json!(0),
        Some(value @ Value::Bool(_)) => value.clone(),
        Some(value) => match python_int(value) {
            PyIntOf::Int(int) => json!(int),
            PyIntOf::Big => return Err(ValidationError::unsupported_int()),
            PyIntOf::ValueError | PyIntOf::TypeError => json!(0),
        },
    };
    let reason = text_or_none(merged.get("reason"));
    merged.insert("requestSerial".to_owned(), serial);
    merged.insert("command".to_owned(), command);
    merged.insert("timestamp".to_owned(), timestamp);
    merged.insert("reason".to_owned(), reason);
    Ok(Value::Object(merged))
}

impl BydModel for CommandLifecycleEvent {
    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        let values = normalize_lifecycle_event(raw)?;
        let mut fields = Fields::cleaned(&values, &[])?;
        let model = Self {
            raw: fields.raw(),
            status: fields.required("status", coerce::str_enum),
            vin: fields.get("vin", String::new(), coerce::string),
            request_serial: fields.opt(
                aliased("request_serial", &["requestSerial"]),
                coerce::string,
            ),
            command: fields.opt("command", coerce::string),
            timestamp: fields.required("timestamp", coerce::int),
            ack: fields.opt("ack", |value| CommandAckEvent::parse(value, context)),
            reason: fields.opt("reason", coerce::string),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Snapshot counters for command ACK registry diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandAckDiagnostics {
    /// Original dict.
    pub raw: Map<String, Value>,
    /// Commands waiting for an ACK.
    pub pending: i64,
    /// Commands whose ACK arrived.
    pub matched: i64,
    /// Commands that timed out.
    pub expired: i64,
    /// ACKs that matched no command.
    pub uncorrelated: i64,
    /// Pending commands per VIN.
    pub pending_by_vin: IndexMap<String, i64>,
}

impl BydModel for CommandAckDiagnostics {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let mut fields = Fields::cleaned(raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            pending: fields.get("pending", 0, coerce::int),
            matched: fields.get("matched", 0, coerce::int),
            expired: fields.get("expired", 0, coerce::int),
            uncorrelated: fields.get("uncorrelated", 0, coerce::int),
            pending_by_vin: fields.get("pending_by_vin", IndexMap::new(), |value| {
                coerce::dict(value, coerce::int)
            }),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Response from the control password verification endpoint.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerifyControlPasswordResponse {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// Whether the password was accepted; `None` unless BYD sent a JSON boolean.
    pub ok: Option<bool>,
}

impl BydModel for VerifyControlPasswordResponse {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        // _coerce_ok: `ok` is a bool or None.
        let values = match raw {
            Value::Object(values)
                if values
                    .get("ok")
                    .is_some_and(|ok| !ok.is_null() && !ok.is_boolean()) =>
            {
                let mut values = values.clone();
                values.insert("ok".to_owned(), Value::Null);
                Value::Object(values)
            }
            other => other.clone(),
        };
        let mut fields = Fields::cleaned(&values, &[])?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            ok: fields.opt("ok", coerce::boolean),
        };
        fields.finish()?;
        Ok(model)
    }
}

// ---------------------------------------------------------------------------
// Control parameter payloads (serialised to `controlParamsMap`)
// ---------------------------------------------------------------------------

/// Reads keyword arguments the way a `ControlParams` model does: frozen,
/// `extra="forbid"`, camelCase aliases and field names both accepted.
///
/// The alias is looked up first; when both the alias and the field name are
/// given, the one not used is an extra input and fails.
struct Kwargs<'a> {
    values: &'a Map<String, Value>,
    used: Vec<&'a str>,
    failures: Failures,
}

impl<'a> Kwargs<'a> {
    fn new(values: &'a Map<String, Value>) -> Self {
        Self {
            values,
            used: Vec::new(),
            failures: Failures::default(),
        }
    }

    fn take(&mut self, name: &str) -> Option<&'a Value> {
        let alias = to_camel(name);
        let (key, value) = self
            .values
            .get_key_value(alias.as_str())
            .or_else(|| self.values.get_key_value(name))?;
        self.used.push(key.as_str());
        Some(value)
    }

    fn get<T>(
        &mut self,
        name: &'static str,
        default: T,
        convert: impl FnOnce(&Value) -> Result<T, ValidationError>,
    ) -> T {
        match self.take(name).map(convert) {
            None => default,
            Some(Ok(value)) => value,
            Some(Err(error)) => {
                self.failures.record(error.at(name));
                default
            }
        }
    }

    /// `T | None = None`.
    fn opt<T>(
        &mut self,
        name: &'static str,
        convert: impl FnOnce(&Value) -> Result<T, ValidationError>,
    ) -> Option<T> {
        self.get(name, None, |value| coerce::optional(value, convert))
    }

    /// A required field (no default).
    fn required<T: Default>(
        &mut self,
        name: &'static str,
        convert: impl FnOnce(&Value) -> Result<T, ValidationError>,
    ) -> T {
        if self.take(name).is_none() {
            self.failures
                .record(ValidationError::new("missing").at(name));
            return T::default();
        }
        self.used.pop();
        self.get(name, T::default(), convert)
    }

    fn finish(mut self) -> Result<(), ValidationError> {
        for key in self.values.keys() {
            if !self.used.contains(&key.as_str()) {
                self.failures
                    .record(ValidationError::new("extra_forbidden").at(key.clone()));
            }
        }
        self.failures.finish()
    }
}

/// `int` with `Field(ge=..., le=...)`.
fn bounded(ge: Option<i64>, le: Option<i64>) -> impl Fn(&Value) -> Result<i64, ValidationError> {
    move |value| coerce::int_bounded(value, ge, le)
}

/// `str` with `str_strip_whitespace=True` (pydantic strips Unicode White_Space).
fn stripped(value: &Value) -> Result<String, ValidationError> {
    coerce::string(value).map(|text| text.trim().to_owned())
}

/// Checks a value against `Field(ge=..., le=...)` for a typed constructor.
fn check_bounds(
    name: &'static str,
    value: i64,
    ge: Option<i64>,
    le: Option<i64>,
    failures: &mut Failures,
) {
    if let Err(error) = coerce::int_bounded(&json!(value), ge, le) {
        failures.record(error.at(name));
    }
}

/// `celsius_to_scale(value, offset=offset)`, or `None` where it raises.
///
/// Besides the range check, Python's `int(round(nan))` raises when both the
/// temperature and the offset are the same infinity.
fn temp_scale(value: f64, offset: f64) -> Option<i64> {
    if (value - offset).is_nan() {
        return None;
    }
    constants::celsius_to_scale(value, offset).ok()
}

/// `_check_temp_range`: both setpoints must fit the vehicle's scale.
fn check_temps(temperatures: [Option<f64>; 2], offset: f64) -> Result<(), ValidationError> {
    for value in temperatures.into_iter().flatten() {
        if temp_scale(value, offset).is_none() {
            return Err(value_error());
        }
    }
    Ok(())
}

/// The shared tail of `ClimateStartParams.to_control_params_map`: rename the
/// setpoints to BYD's keys, mirror the driver's to the passenger's, and add
/// `airSet` in immediate mode.
fn finish_climate_map(
    mut data: Map<String, Value>,
    remote_mode: i64,
    air_set: Option<&str>,
) -> Map<String, Value> {
    if let Some(main) = data.shift_remove("temperature") {
        data.insert("mainSettingTemp".to_owned(), main);
    }
    if let Some(copilot) = data.shift_remove("copilotTemperature") {
        data.insert("copilotSettingTemp".to_owned(), copilot);
    }
    if !data.contains_key("copilotSettingTemp")
        && let Some(main) = data.get("mainSettingTemp").cloned()
    {
        data.insert("copilotSettingTemp".to_owned(), main);
    }
    if remote_mode == 4 {
        data.insert("airSet".to_owned(), json!(air_set));
    }
    data
}

/// Builds `model_dump(by_alias=True, exclude_none=True)` from `(field, value)` pairs.
fn by_alias_without_none(entries: &[(&str, Value)]) -> Map<String, Value> {
    entries
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(name, value)| (to_camel(name), value.clone()))
        .collect()
}

/// Builds `model_dump(mode="json")` from `(field, value)` pairs.
fn by_name(entries: &[(&str, Value)]) -> Map<String, Value> {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

/// Base class for control-parameter models. It has no fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ControlParams;

impl ControlParams {
    /// `ControlParams(**kwargs)`: any keyword argument is an extra input and fails.
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> Result<Self, ValidationError> {
        Kwargs::new(kwargs).finish().map(|()| Self)
    }

    /// The (empty) `controlParamsMap`.
    pub fn to_control_params_map(&self) -> Map<String, Value> {
        Map::new()
    }
}

/// The arguments of [`ClimateStartParams`], with pyBYD's defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct ClimateStartArgs {
    /// Driver temperature setpoint in °C (15-31 by default).
    pub temperature: Option<f64>,
    /// Passenger temperature setpoint in °C. Defaults to `temperature`.
    pub copilot_temperature: Option<f64>,
    /// °C offset of the vehicle's 1-17 scale (14 → 15-31 °C, 16 → 17-33 °C). Not sent.
    pub temp_offset: f64,
    /// Air recirculation: 1=external (fresh), 2=internal (recirculate). `>= 0`.
    pub cycle_mode: i64,
    /// Run duration code (1=10min, 2=15min, 3=20min, 4=25min, 5=30min).
    pub time_span: Option<i64>,
    /// Command mode: 4=immediate start. `>= 0`.
    pub remote_mode: i64,
    /// `>= 0`.
    pub air_accuracy: i64,
    /// `>= 0`.
    pub air_conditioning_mode: i64,
    /// Fan level, `>= 0`.
    pub wind_level: Option<i64>,
    /// OPENAIR operation: `None` starts a session, `"14"` adjusts the setpoint of a running one.
    pub air_set: Option<String>,
}

impl Default for ClimateStartArgs {
    fn default() -> Self {
        Self {
            temperature: None,
            copilot_temperature: None,
            temp_offset: DEFAULT_TEMP_OFFSET_C,
            cycle_mode: 2,
            time_span: None,
            remote_mode: 4,
            air_accuracy: 1,
            air_conditioning_mode: 1,
            wind_level: None,
            air_set: None,
        }
    }
}

/// Parameters for starting HVAC immediately (commandType `OPENAIR`).
///
/// Temperatures are given in °C and converted to BYD's 1-17 scale on
/// serialisation, using `temp_offset` (see [`constants::temp_offset_from_range`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ClimateStartParams(ClimateStartArgs);

impl ClimateStartParams {
    /// Validates the arguments as pyBYD's constructor does.
    pub fn new(args: ClimateStartArgs) -> Result<Self, ValidationError> {
        let mut failures = Failures::default();
        check_bounds("cycle_mode", args.cycle_mode, Some(0), None, &mut failures);
        if let Some(time_span) = args.time_span {
            check_bounds("time_span", time_span, Some(1), Some(5), &mut failures);
        }
        check_bounds(
            "remote_mode",
            args.remote_mode,
            Some(0),
            None,
            &mut failures,
        );
        check_bounds(
            "air_accuracy",
            args.air_accuracy,
            Some(0),
            None,
            &mut failures,
        );
        check_bounds(
            "air_conditioning_mode",
            args.air_conditioning_mode,
            Some(0),
            None,
            &mut failures,
        );
        if let Some(wind_level) = args.wind_level {
            check_bounds("wind_level", wind_level, Some(0), None, &mut failures);
        }
        failures.finish()?;
        check_temps(
            [args.temperature, args.copilot_temperature],
            args.temp_offset,
        )?;
        Ok(Self(args))
    }

    /// `ClimateStartParams(**kwargs)`.
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> Result<Self, ValidationError> {
        let mut reader = Kwargs::new(kwargs);
        let args = ClimateStartArgs {
            temperature: reader.opt("temperature", coerce::float),
            copilot_temperature: reader.opt("copilot_temperature", coerce::float),
            temp_offset: reader.get("temp_offset", DEFAULT_TEMP_OFFSET_C, coerce::float),
            cycle_mode: reader.get("cycle_mode", 2, bounded(Some(0), None)),
            time_span: reader.opt("time_span", bounded(Some(1), Some(5))),
            remote_mode: reader.get("remote_mode", 4, bounded(Some(0), None)),
            air_accuracy: reader.get("air_accuracy", 1, bounded(Some(0), None)),
            air_conditioning_mode: reader.get("air_conditioning_mode", 1, bounded(Some(0), None)),
            wind_level: reader.opt("wind_level", bounded(Some(0), None)),
            air_set: reader.opt("air_set", stripped),
        };
        reader.finish()?;
        Self::new(args)
    }

    /// The validated arguments.
    pub fn args(&self) -> &ClimateStartArgs {
        &self.0
    }

    fn scale(&self, value: Option<f64>) -> Value {
        json!(value.map(|value| temp_scale(value, self.0.temp_offset).expect("checked in new")))
    }

    /// Every serialised field in declaration order (`temp_offset` and `air_set` are excluded).
    fn entries(&self) -> Vec<(&'static str, Value)> {
        let args = &self.0;
        vec![
            ("temperature", self.scale(args.temperature)),
            ("copilot_temperature", self.scale(args.copilot_temperature)),
            ("cycle_mode", json!(args.cycle_mode)),
            ("time_span", json!(args.time_span)),
            ("remote_mode", json!(args.remote_mode)),
            ("air_accuracy", json!(args.air_accuracy)),
            ("air_conditioning_mode", json!(args.air_conditioning_mode)),
            ("wind_level", json!(args.wind_level)),
        ]
    }

    /// `model_dump(mode="json")`: field names, setpoints on BYD's scale.
    pub fn model_dump(&self) -> Map<String, Value> {
        by_name(&self.entries())
    }

    /// The `controlParamsMap` for `OPENAIR`.
    ///
    /// Setpoints are sent as `mainSettingTemp` / `copilotSettingTemp` on
    /// BYD's scale, the passenger's mirroring the driver's when not given;
    /// in immediate mode (`remote_mode == 4`) `airSet` is always present,
    /// `null` to start a session.
    pub fn to_control_params_map(&self) -> Map<String, Value> {
        finish_climate_map(
            by_alias_without_none(&self.entries()),
            self.0.remote_mode,
            self.0.air_set.as_deref(),
        )
    }
}

/// The arguments of [`ClimateScheduleParams`], with pyBYD's defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct ClimateScheduleArgs {
    /// Driver temperature setpoint in °C.
    pub temperature: Option<f64>,
    /// Passenger temperature setpoint in °C. Defaults to `temperature`.
    pub copilot_temperature: Option<f64>,
    /// °C offset of the vehicle's 1-17 scale. Not sent.
    pub temp_offset: f64,
    /// Air recirculation, `>= 0`.
    pub cycle_mode: i64,
    /// Run duration code 1-5.
    pub time_span: Option<i64>,
    /// Schedule mode: 1=create, 2=modify, 3=remove.
    pub remote_mode: i64,
    /// `>= 0`.
    pub air_accuracy: i64,
    /// `>= 0`.
    pub air_conditioning_mode: i64,
    /// Fan level, `>= 0`.
    pub wind_level: Option<i64>,
    /// Not sent for schedules (only immediate mode sends `airSet`).
    pub air_set: Option<String>,
    /// Schedule booking ID (required for modify/remove), `>= 1`.
    pub booking_id: Option<i64>,
    /// Schedule time as epoch seconds, `>= 1`.
    pub booking_time: Option<i64>,
    /// A/C switch (0=off, 1=on). Usually 0 for schedule creation.
    pub ac_switch: i64,
    /// Fan mode (only included on create), `>= 0`.
    pub wind_mode: Option<i64>,
}

impl Default for ClimateScheduleArgs {
    fn default() -> Self {
        Self {
            temperature: None,
            copilot_temperature: None,
            temp_offset: DEFAULT_TEMP_OFFSET_C,
            cycle_mode: 2,
            time_span: None,
            remote_mode: 1,
            air_accuracy: 1,
            air_conditioning_mode: 1,
            wind_level: None,
            air_set: None,
            booking_id: None,
            booking_time: None,
            ac_switch: 0,
            wind_mode: None,
        }
    }
}

/// Parameters for scheduling HVAC (commandType `BOOKINGAIR`).
#[derive(Debug, Clone, PartialEq)]
pub struct ClimateScheduleParams(ClimateScheduleArgs);

impl ClimateScheduleParams {
    /// Validates the arguments as pyBYD's constructor does.
    pub fn new(args: ClimateScheduleArgs) -> Result<Self, ValidationError> {
        let mut failures = Failures::default();
        check_bounds("cycle_mode", args.cycle_mode, Some(0), None, &mut failures);
        if let Some(time_span) = args.time_span {
            check_bounds("time_span", time_span, Some(1), Some(5), &mut failures);
        }
        check_bounds(
            "remote_mode",
            args.remote_mode,
            Some(1),
            Some(3),
            &mut failures,
        );
        check_bounds(
            "air_accuracy",
            args.air_accuracy,
            Some(0),
            None,
            &mut failures,
        );
        check_bounds(
            "air_conditioning_mode",
            args.air_conditioning_mode,
            Some(0),
            None,
            &mut failures,
        );
        if let Some(wind_level) = args.wind_level {
            check_bounds("wind_level", wind_level, Some(0), None, &mut failures);
        }
        if let Some(booking_id) = args.booking_id {
            check_bounds("booking_id", booking_id, Some(1), None, &mut failures);
        }
        if let Some(booking_time) = args.booking_time {
            check_bounds("booking_time", booking_time, Some(1), None, &mut failures);
        }
        check_bounds("ac_switch", args.ac_switch, Some(0), Some(1), &mut failures);
        if let Some(wind_mode) = args.wind_mode {
            check_bounds("wind_mode", wind_mode, Some(0), None, &mut failures);
        }
        failures.finish()?;
        check_temps(
            [args.temperature, args.copilot_temperature],
            args.temp_offset,
        )?;
        Ok(Self(args))
    }

    /// `ClimateScheduleParams(**kwargs)`.
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> Result<Self, ValidationError> {
        let mut reader = Kwargs::new(kwargs);
        let args = ClimateScheduleArgs {
            temperature: reader.opt("temperature", coerce::float),
            copilot_temperature: reader.opt("copilot_temperature", coerce::float),
            temp_offset: reader.get("temp_offset", DEFAULT_TEMP_OFFSET_C, coerce::float),
            cycle_mode: reader.get("cycle_mode", 2, bounded(Some(0), None)),
            time_span: reader.opt("time_span", bounded(Some(1), Some(5))),
            remote_mode: reader.get("remote_mode", 1, bounded(Some(1), Some(3))),
            air_accuracy: reader.get("air_accuracy", 1, bounded(Some(0), None)),
            air_conditioning_mode: reader.get("air_conditioning_mode", 1, bounded(Some(0), None)),
            wind_level: reader.opt("wind_level", bounded(Some(0), None)),
            air_set: reader.opt("air_set", stripped),
            booking_id: reader.opt("booking_id", bounded(Some(1), None)),
            booking_time: reader.opt("booking_time", bounded(Some(1), None)),
            ac_switch: reader.get("ac_switch", 0, bounded(Some(0), Some(1))),
            wind_mode: reader.opt("wind_mode", bounded(Some(0), None)),
        };
        reader.finish()?;
        Self::new(args)
    }

    /// The validated arguments.
    pub fn args(&self) -> &ClimateScheduleArgs {
        &self.0
    }

    fn scale(&self, value: Option<f64>) -> Value {
        json!(value.map(|value| temp_scale(value, self.0.temp_offset).expect("checked in new")))
    }

    fn entries(&self) -> Vec<(&'static str, Value)> {
        let args = &self.0;
        vec![
            ("temperature", self.scale(args.temperature)),
            ("copilot_temperature", self.scale(args.copilot_temperature)),
            ("cycle_mode", json!(args.cycle_mode)),
            ("time_span", json!(args.time_span)),
            ("remote_mode", json!(args.remote_mode)),
            ("air_accuracy", json!(args.air_accuracy)),
            ("air_conditioning_mode", json!(args.air_conditioning_mode)),
            ("wind_level", json!(args.wind_level)),
            ("booking_id", json!(args.booking_id)),
            ("booking_time", json!(args.booking_time)),
            ("ac_switch", json!(args.ac_switch)),
            ("wind_mode", json!(args.wind_mode)),
        ]
    }

    /// `model_dump(mode="json")`.
    pub fn model_dump(&self) -> Map<String, Value> {
        by_name(&self.entries())
    }

    /// The `controlParamsMap` for `BOOKINGAIR` (same rules as [`ClimateStartParams::to_control_params_map`];
    /// `remote_mode` is 1-3 here, so `airSet` is never sent).
    pub fn to_control_params_map(&self) -> Map<String, Value> {
        finish_climate_map(
            by_alias_without_none(&self.entries()),
            self.0.remote_mode,
            self.0.air_set.as_deref(),
        )
    }
}

/// The integer fields of [`SeatClimateArgs`], in declaration order, with their bounds.
const SEAT_INT_FIELDS: [(&str, i64, i64); 14] = [
    ("remote_mode", 1, 4),
    ("main_heat", 0, 3),
    ("main_ventilation", 0, 3),
    ("copilot_heat", 0, 3),
    ("copilot_ventilation", 0, 3),
    ("lr_seat_heat_state", 0, 3),
    ("lr_seat_ventilation_state", 0, 3),
    ("lr_third_heat_state", 0, 3),
    ("lr_third_ventilation_state", 0, 3),
    ("rr_seat_heat_state", 0, 3),
    ("rr_seat_ventilation_state", 0, 3),
    ("rr_third_heat_state", 0, 3),
    ("rr_third_ventilation_state", 0, 3),
    ("steering_wheel_heat_state", 0, 3),
];

/// `_CHAIR_TYPE_FOR_PARAM`: the `chairType` a changed parameter targets.
///
/// The third-row fields are not listed, so changing one sends no `chairType` (as in pyBYD).
fn chair_type_for_param(param_key: &str) -> Option<&'static str> {
    Some(match param_key {
        "main_heat" | "main_ventilation" => "1",
        "copilot_heat" | "copilot_ventilation" => "2",
        "lr_seat_heat_state" | "lr_seat_ventilation_state" => "3",
        "rr_seat_heat_state" | "rr_seat_ventilation_state" => "4",
        "steering_wheel_heat_state" => "5",
        _ => return None,
    })
}

/// The arguments of [`SeatClimateParams`], with pyBYD's defaults.
///
/// Command values use an inverted scale compared with status readings:
/// `1` = high, `2` = low, `3` = off, `0` = not applicable. Steering wheel
/// heat is `1` = on, `3` = off.
#[derive(Debug, Clone, PartialEq)]
pub struct SeatClimateArgs {
    /// Which seat the command targets: "1"=driver, "2"=copilot, "3"=rear left, "4"=rear right, "5"=steering wheel.
    pub chair_type: Option<String>,
    /// Always `1` for seat/steering wheel commands (1-4 accepted).
    pub remote_mode: i64,
    /// Driver seat heat.
    pub main_heat: i64,
    /// Driver seat ventilation.
    pub main_ventilation: i64,
    /// Passenger seat heat.
    pub copilot_heat: i64,
    /// Passenger seat ventilation.
    pub copilot_ventilation: i64,
    /// Rear left seat heat.
    pub lr_seat_heat_state: i64,
    /// Rear left seat ventilation.
    pub lr_seat_ventilation_state: i64,
    /// Third-row left seat heat.
    pub lr_third_heat_state: i64,
    /// Third-row left seat ventilation.
    pub lr_third_ventilation_state: i64,
    /// Rear right seat heat.
    pub rr_seat_heat_state: i64,
    /// Rear right seat ventilation.
    pub rr_seat_ventilation_state: i64,
    /// Third-row right seat heat.
    pub rr_third_heat_state: i64,
    /// Third-row right seat ventilation.
    pub rr_third_ventilation_state: i64,
    /// Steering wheel heat: 1=on, 3=off.
    pub steering_wheel_heat_state: i64,
}

impl Default for SeatClimateArgs {
    fn default() -> Self {
        Self {
            chair_type: None,
            remote_mode: 1,
            main_heat: 3,
            main_ventilation: 0,
            copilot_heat: 3,
            copilot_ventilation: 0,
            lr_seat_heat_state: 0,
            lr_seat_ventilation_state: 0,
            lr_third_heat_state: 0,
            lr_third_ventilation_state: 0,
            rr_seat_heat_state: 0,
            rr_seat_ventilation_state: 0,
            rr_third_heat_state: 0,
            rr_third_ventilation_state: 0,
            steering_wheel_heat_state: 3,
        }
    }
}

impl SeatClimateArgs {
    /// The integer field called `name`.
    fn int_field(&mut self, name: &str) -> Option<&mut i64> {
        Some(match name {
            "remote_mode" => &mut self.remote_mode,
            "main_heat" => &mut self.main_heat,
            "main_ventilation" => &mut self.main_ventilation,
            "copilot_heat" => &mut self.copilot_heat,
            "copilot_ventilation" => &mut self.copilot_ventilation,
            "lr_seat_heat_state" => &mut self.lr_seat_heat_state,
            "lr_seat_ventilation_state" => &mut self.lr_seat_ventilation_state,
            "lr_third_heat_state" => &mut self.lr_third_heat_state,
            "lr_third_ventilation_state" => &mut self.lr_third_ventilation_state,
            "rr_seat_heat_state" => &mut self.rr_seat_heat_state,
            "rr_seat_ventilation_state" => &mut self.rr_seat_ventilation_state,
            "rr_third_heat_state" => &mut self.rr_third_heat_state,
            "rr_third_ventilation_state" => &mut self.rr_third_ventilation_state,
            "steering_wheel_heat_state" => &mut self.steering_wheel_heat_state,
            _ => return None,
        })
    }

    fn int_values(&self) -> [i64; 14] {
        [
            self.remote_mode,
            self.main_heat,
            self.main_ventilation,
            self.copilot_heat,
            self.copilot_ventilation,
            self.lr_seat_heat_state,
            self.lr_seat_ventilation_state,
            self.lr_third_heat_state,
            self.lr_third_ventilation_state,
            self.rr_seat_heat_state,
            self.rr_seat_ventilation_state,
            self.rr_third_heat_state,
            self.rr_third_ventilation_state,
            self.steering_wheel_heat_state,
        ]
    }
}

/// The seat and steering wheel readings [`SeatClimateParams::from_current_state`] reads.
///
/// Each value is the integer value of the status enum (`SeatHeatVentState`:
/// -1 unknown, 0 no data, 1 off, 2 low, 3 high; `StearingWheelHeat`: -1 on,
/// 0 no data, 1 off), or `None` when the model has no value. Build one from
/// an `HvacStatus` or a `VehicleRealtimeData` by taking `.value()` of each
/// field of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SeatClimateState {
    /// `main_seat_heat_state`.
    pub main_seat_heat_state: Option<i64>,
    /// `main_seat_ventilation_state`.
    pub main_seat_ventilation_state: Option<i64>,
    /// `copilot_seat_heat_state`.
    pub copilot_seat_heat_state: Option<i64>,
    /// `copilot_seat_ventilation_state`.
    pub copilot_seat_ventilation_state: Option<i64>,
    /// `lr_seat_heat_state`.
    pub lr_seat_heat_state: Option<i64>,
    /// `lr_seat_ventilation_state`.
    pub lr_seat_ventilation_state: Option<i64>,
    /// `rr_seat_heat_state`.
    pub rr_seat_heat_state: Option<i64>,
    /// `rr_seat_ventilation_state`.
    pub rr_seat_ventilation_state: Option<i64>,
    /// `steering_wheel_heat_state`.
    pub steering_wheel_heat_state: Option<i64>,
}

impl SeatClimateState {
    /// `_SEAT_ATTR_TO_PARAM`, in order: each status reading.
    fn seat_readings(&self) -> [Option<i64>; 8] {
        [
            self.main_seat_heat_state,
            self.main_seat_ventilation_state,
            self.copilot_seat_heat_state,
            self.copilot_seat_ventilation_state,
            self.lr_seat_heat_state,
            self.lr_seat_ventilation_state,
            self.rr_seat_heat_state,
            self.rr_seat_ventilation_state,
        ]
    }
}

/// Both models carry the same nine fields; this reads them as enum values.
macro_rules! seat_climate_state_from {
    ($model:ty) => {
        impl From<&$model> for SeatClimateState {
            fn from(model: &$model) -> Self {
                let seat = |state: Option<SeatHeatVentState>| state.map(|state| state.value());
                Self {
                    main_seat_heat_state: seat(model.main_seat_heat_state),
                    main_seat_ventilation_state: seat(model.main_seat_ventilation_state),
                    copilot_seat_heat_state: seat(model.copilot_seat_heat_state),
                    copilot_seat_ventilation_state: seat(model.copilot_seat_ventilation_state),
                    lr_seat_heat_state: seat(model.lr_seat_heat_state),
                    lr_seat_ventilation_state: seat(model.lr_seat_ventilation_state),
                    rr_seat_heat_state: seat(model.rr_seat_heat_state),
                    rr_seat_ventilation_state: seat(model.rr_seat_ventilation_state),
                    steering_wheel_heat_state: model
                        .steering_wheel_heat_state
                        .map(|state: StearingWheelHeat| state.value()),
                }
            }
        }
    };
}
seat_climate_state_from!(HvacStatus);
seat_climate_state_from!(VehicleRealtimeData);

/// `SeatHeatVentState.to_command_level`: HIGH(3)→1, LOW(2)→2, OFF(1)→3, anything else 0.
fn seat_command_level(status: i64) -> i64 {
    match status {
        1 => 3,
        2 => 2,
        3 => 1,
        _ => 0,
    }
}

/// Parameters for seat heating/ventilation (commandType `VENTILATIONHEATING`).
///
/// BYD needs every seat value in every command, so build the parameters
/// from the current state with [`Self::from_current_state`] and change one
/// value with [`Self::with_change`], which also sets `chair_type`.
#[derive(Debug, Clone, PartialEq)]
pub struct SeatClimateParams(SeatClimateArgs);

impl SeatClimateParams {
    /// Validates the arguments as pyBYD's constructor does.
    pub fn new(args: SeatClimateArgs) -> Result<Self, ValidationError> {
        let mut failures = Failures::default();
        for ((name, ge, le), value) in SEAT_INT_FIELDS.into_iter().zip(args.int_values()) {
            check_bounds(name, value, Some(ge), Some(le), &mut failures);
        }
        failures.finish()?;
        Ok(Self(args))
    }

    /// `SeatClimateParams(**kwargs)`.
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> Result<Self, ValidationError> {
        let mut reader = Kwargs::new(kwargs);
        let mut args = SeatClimateArgs {
            chair_type: reader.opt("chair_type", stripped),
            ..SeatClimateArgs::default()
        };
        for (name, ge, le) in SEAT_INT_FIELDS {
            let field = args.int_field(name).expect("listed fields exist");
            *field = reader.get(name, *field, bounded(Some(ge), Some(le)));
        }
        reader.finish()?;
        Self::new(args)
    }

    /// Builds the parameters from the current vehicle state: the HVAC status
    /// first, the realtime data as fallback for each value.
    ///
    /// Seat readings are converted to the command scale (HIGH→1, LOW→2,
    /// OFF→3, no data→0); steering wheel heat is `1` when on, `3` otherwise.
    pub fn from_current_state(
        hvac: Option<&SeatClimateState>,
        realtime: Option<&SeatClimateState>,
    ) -> Self {
        let pick = |reading: fn(&SeatClimateState) -> Option<i64>| {
            hvac.and_then(reading)
                .or_else(|| realtime.and_then(reading))
        };
        let seats: Vec<i64> = (0..8)
            .map(|index| {
                let reading = hvac
                    .and_then(|state| state.seat_readings()[index])
                    .or_else(|| realtime.and_then(|state| state.seat_readings()[index]));
                reading.map_or(0, seat_command_level)
            })
            .collect();
        let steering = match pick(|state| state.steering_wheel_heat_state) {
            Some(-1) => 1,
            _ => 3,
        };
        Self(SeatClimateArgs {
            main_heat: seats[0],
            main_ventilation: seats[1],
            copilot_heat: seats[2],
            copilot_ventilation: seats[3],
            lr_seat_heat_state: seats[4],
            lr_seat_ventilation_state: seats[5],
            rr_seat_heat_state: seats[6],
            rr_seat_ventilation_state: seats[7],
            steering_wheel_heat_state: steering,
            ..SeatClimateArgs::default()
        })
    }

    /// [`Self::from_current_state`] on the parsed models, as pyBYD's
    /// `SeatClimateParams.from_current_state(hvac, realtime)` takes them.
    pub fn from_current_models(
        hvac: Option<&HvacStatus>,
        realtime: Option<&VehicleRealtimeData>,
    ) -> Self {
        let hvac = hvac.map(SeatClimateState::from);
        let realtime = realtime.map(SeatClimateState::from);
        Self::from_current_state(hvac.as_ref(), realtime.as_ref())
    }

    /// A copy with `param_key` set to `value` and `chair_type` set to the seat it belongs to.
    ///
    /// Like pyBYD's `model_copy(update=...)`, this does not validate: the
    /// value is taken as given, an unknown `param_key` changes nothing but
    /// clears `chair_type`, and so does a field with no seat mapping (the
    /// third-row fields, `remote_mode`, `chair_type` itself).
    pub fn with_change(&self, param_key: &str, value: i64) -> Self {
        let mut args = self.0.clone();
        if let Some(field) = args.int_field(param_key) {
            *field = value;
        }
        args.chair_type = chair_type_for_param(param_key).map(str::to_owned);
        Self(args)
    }

    /// The arguments (validated, unless changed through [`Self::with_change`]).
    pub fn args(&self) -> &SeatClimateArgs {
        &self.0
    }

    fn entries(&self) -> Vec<(&'static str, Value)> {
        let mut entries = vec![("chair_type", json!(self.0.chair_type))];
        entries.extend(
            SEAT_INT_FIELDS
                .iter()
                .zip(self.0.int_values())
                .map(|((name, _, _), value)| (*name, json!(value))),
        );
        entries
    }

    /// `model_dump(mode="json")`.
    pub fn model_dump(&self) -> Map<String, Value> {
        by_name(&self.entries())
    }

    /// The `controlParamsMap` for `VENTILATIONHEATING`: every field by its
    /// camelCase name, `chairType` left out when unset.
    pub fn to_control_params_map(&self) -> Map<String, Value> {
        by_alias_without_none(&self.entries())
    }
}

/// Parameters for battery heating (commandType `BATTERYHEAT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryHeatParams {
    /// Heat on or off.
    pub on: bool,
    /// `heatType` for the dedicated endpoint (not part of `controlParamsMap`). Defaults to 2.
    pub heat_type: i64,
}

impl BatteryHeatParams {
    /// Battery heating on or off with the default `heat_type` of 2.
    pub fn new(on: bool) -> Self {
        Self { on, heat_type: 2 }
    }

    /// `BatteryHeatParams(**kwargs)`; `on` is required.
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> Result<Self, ValidationError> {
        let mut reader = Kwargs::new(kwargs);
        let on = reader.required("on", coerce::boolean);
        let heat_type = reader.get("heat_type", 2, coerce::int);
        reader.finish()?;
        Ok(Self { on, heat_type })
    }

    /// `model_dump(mode="json")`: `{"on": 1}` or `{"on": 0}`.
    pub fn model_dump(&self) -> Map<String, Value> {
        by_name(&[("on", json!(i64::from(self.on)))])
    }

    /// The `controlParamsMap` for `BATTERYHEAT`: `{"batteryHeatSwitch": 1|0}`.
    pub fn to_control_params_map(&self) -> Map<String, Value> {
        by_name(&[("batteryHeatSwitch", json!(i64::from(self.on)))])
    }

    /// The parameters for `/control/remoteControlBatteryHeat` (`10100001`):
    /// `{"batteryHeat": 1|0, "heatType": heat_type}`.
    pub fn to_dedicated_control_params_map(&self) -> Map<String, Value> {
        by_name(&[
            ("batteryHeat", json!(i64::from(self.on))),
            ("heatType", json!(self.heat_type)),
        ])
    }
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

fn kwargs_arg<'a>(args: &Args<'a>) -> Result<&'a Map<String, Value>, ParityError> {
    args.value("kwargs")
        .as_object()
        .ok_or_else(|| ParityError::bad_arguments("kwargs must be an object"))
}

/// Applies `changes` (`[[param_key, value], ...]`) with `with_change`.
fn apply_changes(
    mut params: SeatClimateParams,
    args: &Args<'_>,
) -> Result<SeatClimateParams, ParityError> {
    let changes = match args.value("changes") {
        Value::Null => return Ok(params),
        Value::Array(changes) => changes,
        _ => return Err(ParityError::bad_arguments("changes must be a list")),
    };
    for change in changes {
        let (Some(key), Some(value)) = (
            change.get(0).and_then(Value::as_str),
            change.get(1).and_then(Value::as_i64),
        ) else {
            return Err(ParityError::bad_arguments(
                "each change is [string, integer]",
            ));
        };
        params = params.with_change(key, value);
    }
    Ok(params)
}

fn seat_state_arg(value: &Value) -> Result<Option<SeatClimateState>, ParityError> {
    let entries = match value {
        Value::Null => return Ok(None),
        Value::Object(entries) => entries,
        _ => {
            return Err(ParityError::bad_arguments(
                "state must be an object or null",
            ));
        }
    };
    let read = |key: &str| -> Result<Option<i64>, ParityError> {
        match entries.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_i64()
                .map(Some)
                .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be an integer"))),
        }
    };
    Ok(Some(SeatClimateState {
        main_seat_heat_state: read("main_seat_heat_state")?,
        main_seat_ventilation_state: read("main_seat_ventilation_state")?,
        copilot_seat_heat_state: read("copilot_seat_heat_state")?,
        copilot_seat_ventilation_state: read("copilot_seat_ventilation_state")?,
        lr_seat_heat_state: read("lr_seat_heat_state")?,
        lr_seat_ventilation_state: read("lr_seat_ventilation_state")?,
        rr_seat_heat_state: read("rr_seat_heat_state")?,
        rr_seat_ventilation_state: read("rr_seat_ventilation_state")?,
        steering_wheel_heat_state: read("steering_wheel_heat_state")?,
    }))
}

fn params_answer(dump: Map<String, Value>, params: Map<String, Value>) -> Value {
    json!({"dump": dump, "params": params})
}

fn seat_answer(params: &SeatClimateParams) -> Value {
    params_answer(params.model_dump(), params.to_control_params_map())
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "models.control.RemoteCommand" => coerce::str_enum::<RemoteCommand>(args.value("value"))
            .map(|member| json!({"name": member.name(), "value": member.as_str()}))
            .map_err(ParityError::from),
        "models.control.ControlState" => coerce::int_enum::<ControlState>(args.value("value"))
            .map(|member| json!({"name": member.name(), "value": member as i64}))
            .map_err(ParityError::from),
        "models.control.CommandLifecycleStatus" => {
            coerce::str_enum::<CommandLifecycleStatus>(args.value("value"))
                .map(|member| json!({"name": member.name(), "value": member.as_str()}))
                .map_err(ParityError::from)
        }
        "models.control.RemoteControlResult" => base::model_call::<RemoteControlResult>(args.0),
        "models.control.CommandAck" => base::model_call::<CommandAck>(args.0),
        "models.control.ChargeChangeResult" => base::model_call::<ChargeChangeResult>(args.0),
        "models.control.CommandAckEvent" => base::model_call::<CommandAckEvent>(args.0),
        "models.control.CommandLifecycleEvent" => base::model_call::<CommandLifecycleEvent>(args.0),
        "models.control.CommandAckDiagnostics" => base::model_call::<CommandAckDiagnostics>(args.0),
        "models.control.VerifyControlPasswordResponse" => {
            base::model_call::<VerifyControlPasswordResponse>(args.0)
        }
        "models.control.ControlParams" => kwargs_arg(&args).and_then(|kwargs| {
            let params = ControlParams::from_kwargs(kwargs)?;
            Ok(params_answer(Map::new(), params.to_control_params_map()))
        }),
        "models.control.ClimateStartParams" => kwargs_arg(&args).and_then(|kwargs| {
            let params = ClimateStartParams::from_kwargs(kwargs)?;
            Ok(params_answer(
                params.model_dump(),
                params.to_control_params_map(),
            ))
        }),
        "models.control.ClimateScheduleParams" => kwargs_arg(&args).and_then(|kwargs| {
            let params = ClimateScheduleParams::from_kwargs(kwargs)?;
            Ok(params_answer(
                params.model_dump(),
                params.to_control_params_map(),
            ))
        }),
        "models.control.SeatClimateParams" => kwargs_arg(&args).and_then(|kwargs| {
            let params = SeatClimateParams::from_kwargs(kwargs)?;
            apply_changes(params, &args).map(|params| seat_answer(&params))
        }),
        "models.control.SeatClimateParams.from_current_state" => (|| {
            let hvac = seat_state_arg(args.value("hvac"))?;
            let realtime = seat_state_arg(args.value("realtime"))?;
            let params = SeatClimateParams::from_current_state(hvac.as_ref(), realtime.as_ref());
            apply_changes(params, &args).map(|params| seat_answer(&params))
        })(),
        "models.control.SeatClimateParams.from_models" => (|| {
            fn parse_model<M: BydModel>(raw: &Value) -> Result<Option<M>, ParityError> {
                match raw {
                    Value::Null => Ok(None),
                    raw => Ok(Some(M::parse(raw, &Value::Null)?)),
                }
            }
            let hvac = parse_model::<HvacStatus>(args.value("hvac"))?;
            let realtime = parse_model::<VehicleRealtimeData>(args.value("realtime"))?;
            let params = SeatClimateParams::from_current_models(hvac.as_ref(), realtime.as_ref());
            apply_changes(params, &args).map(|params| seat_answer(&params))
        })(),
        "models.control.BatteryHeatParams" => kwargs_arg(&args).and_then(|kwargs| {
            let params = BatteryHeatParams::from_kwargs(kwargs)?;
            Ok(json!({
                "dump": params.model_dump(),
                "params": params.to_control_params_map(),
                "dedicated": params.to_dedicated_control_params_map(),
            }))
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_int_matches_cpython() {
        assert_eq!(python_int(&json!(" ١٢ ")), PyIntOf::Int(12));
        assert_eq!(python_int(&json!("1_000")), PyIntOf::Int(1000));
        assert_eq!(python_int(&json!("+7")), PyIntOf::Int(7));
        assert_eq!(python_int(&json!("-0")), PyIntOf::Int(0));
        assert_eq!(python_int(&json!("1__0")), PyIntOf::ValueError);
        assert_eq!(python_int(&json!("_1")), PyIntOf::ValueError);
        assert_eq!(python_int(&json!("1.5")), PyIntOf::ValueError);
        assert_eq!(python_int(&json!("\x1c1")), PyIntOf::ValueError);
        assert_eq!(python_int(&json!("é")), PyIntOf::ValueError);
        assert_eq!(python_int(&json!("")), PyIntOf::ValueError);
        assert_eq!(python_int(&json!("9".repeat(40))), PyIntOf::Big);
        assert_eq!(
            python_int(&json!("-9223372036854775808")),
            PyIntOf::Int(i64::MIN)
        );
        assert_eq!(python_int(&json!("9223372036854775808")), PyIntOf::Big);
        assert_eq!(python_int(&json!(u64::MAX)), PyIntOf::Big);
        assert_eq!(python_int(&json!(-2.9)), PyIntOf::Int(-2));
        assert_eq!(python_int(&json!(1e300)), PyIntOf::Big);
        assert_eq!(python_int(&json!(true)), PyIntOf::Int(1));
        assert_eq!(python_int(&json!(null)), PyIntOf::TypeError);
        assert_eq!(python_int(&json!([1])), PyIntOf::TypeError);
    }

    #[test]
    fn huge_timestamps_are_unsupported() {
        for raw in [
            json!({"timestamp": 1e300}),
            json!({"timestamp": "99999999999999999999"}),
        ] {
            assert_eq!(
                CommandAckEvent::parse(&raw, &Value::Null)
                    .unwrap_err()
                    .exception,
                "Unsupported"
            );
            let mut lifecycle = raw.clone();
            lifecycle["status"] = json!("matched");
            assert_eq!(
                CommandLifecycleEvent::parse(&lifecycle, &Value::Null)
                    .unwrap_err()
                    .exception,
                "Unsupported"
            );
        }
    }

    #[test]
    fn typed_constructors_validate_like_pydantic() {
        assert!(ClimateStartParams::new(ClimateStartArgs::default()).is_ok());
        for args in [
            ClimateStartArgs {
                cycle_mode: -1,
                ..Default::default()
            },
            ClimateStartArgs {
                time_span: Some(6),
                ..Default::default()
            },
            ClimateStartArgs {
                wind_level: Some(-1),
                ..Default::default()
            },
            ClimateStartArgs {
                temperature: Some(33.0),
                ..Default::default()
            },
            ClimateStartArgs {
                copilot_temperature: Some(f64::NAN),
                ..Default::default()
            },
            ClimateStartArgs {
                temperature: Some(f64::INFINITY),
                temp_offset: f64::INFINITY,
                ..Default::default()
            },
        ] {
            assert!(ClimateStartParams::new(args).is_err());
        }
        for args in [
            ClimateScheduleArgs {
                remote_mode: 4,
                ..Default::default()
            },
            ClimateScheduleArgs {
                booking_id: Some(0),
                ..Default::default()
            },
            ClimateScheduleArgs {
                booking_time: Some(0),
                ..Default::default()
            },
            ClimateScheduleArgs {
                ac_switch: 2,
                ..Default::default()
            },
            ClimateScheduleArgs {
                wind_mode: Some(-1),
                ..Default::default()
            },
            ClimateScheduleArgs {
                wind_level: Some(-1),
                time_span: Some(0),
                ..Default::default()
            },
            ClimateScheduleArgs {
                temperature: Some(10.0),
                ..Default::default()
            },
        ] {
            assert!(ClimateScheduleParams::new(args).is_err());
        }
        assert!(
            SeatClimateParams::new(SeatClimateArgs {
                main_heat: 4,
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn climate_params_serialise_like_pybyd() {
        let params = ClimateStartParams::new(ClimateStartArgs {
            temperature: Some(29.0),
            time_span: Some(1),
            temp_offset: 16.0,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(params.args().time_span, Some(1));
        assert_eq!(
            Value::Object(params.to_control_params_map()),
            json!({"cycleMode": 2, "timeSpan": 1, "remoteMode": 4, "airAccuracy": 1,
                   "airConditioningMode": 1, "mainSettingTemp": 13, "copilotSettingTemp": 13, "airSet": null})
        );
        let schedule = ClimateScheduleParams::new(ClimateScheduleArgs {
            temperature: Some(21.0),
            booking_id: Some(3),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(schedule.args().booking_id, Some(3));
        assert_eq!(
            schedule.to_control_params_map()["mainSettingTemp"],
            json!(7)
        );
        assert_eq!(schedule.model_dump()["temperature"], json!(7));
        assert!(ControlParams.to_control_params_map().is_empty());
    }

    #[test]
    fn seat_state_comes_from_the_models_hvac_first() {
        let hvac = HvacStatus::parse(
            &json!({"mainSeatHeatState": 3, "stearingWheelHeatState": -1}),
            &Value::Null,
        )
        .unwrap();
        let realtime = VehicleRealtimeData::parse(
            &json!({"mainSeatHeatState": 1, "copilotSeatHeatState": 2, "stearingWheelHeatState": 1}),
            &Value::Null,
        )
        .unwrap();
        let state = SeatClimateState::from(&realtime);
        assert_eq!(state.copilot_seat_heat_state, Some(2));
        assert_eq!(state.lr_seat_heat_state, None);
        let params = SeatClimateParams::from_current_models(Some(&hvac), Some(&realtime));
        let args = params.args();
        assert_eq!((args.main_heat, args.copilot_heat), (1, 2));
        assert_eq!(args.steering_wheel_heat_state, 1);
        let none = SeatClimateParams::from_current_models(None, None);
        assert_eq!(none.args().steering_wheel_heat_state, 3);
    }

    #[test]
    fn seat_params_and_battery_heat() {
        let state = SeatClimateState {
            main_seat_heat_state: Some(3),
            steering_wheel_heat_state: Some(-1),
            ..Default::default()
        };
        let params = SeatClimateParams::from_current_state(Some(&state), None)
            .with_change("rr_seat_heat_state", 1);
        assert_eq!(params.args().main_heat, 1);
        assert_eq!(params.args().steering_wheel_heat_state, 1);
        assert_eq!(params.args().chair_type.as_deref(), Some("4"));
        let fallback = SeatClimateParams::from_current_state(None, Some(&state));
        assert_eq!(fallback.args().main_heat, 1);
        assert!(SeatClimateParams::new(fallback.args().clone()).is_ok());
        let battery = BatteryHeatParams::new(true);
        assert_eq!(
            Value::Object(battery.to_dedicated_control_params_map()),
            json!({"batteryHeat": 1, "heatType": 2})
        );
    }

    #[test]
    fn bad_parity_arguments() {
        let call = |name: &str, args: Value| parity_call(name, &args).unwrap().unwrap_err().kind;
        assert_eq!(
            call("models.control.ClimateStartParams", json!({"kwargs": 1})),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.control.SeatClimateParams",
                json!({"kwargs": {}, "changes": 1})
            ),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.control.SeatClimateParams",
                json!({"kwargs": {}, "changes": [["main_heat", "x"]]})
            ),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.control.SeatClimateParams.from_current_state",
                json!({"hvac": 1})
            ),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.control.SeatClimateParams.from_current_state",
                json!({"hvac": {"main_seat_heat_state": "x"}})
            ),
            "BadArguments"
        );
        assert!(parity_call("models.control.nothing", &json!({})).is_none());
    }
}
