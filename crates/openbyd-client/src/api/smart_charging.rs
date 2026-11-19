//! Smart charging control endpoints (`pybyd/_api/smart_charging.py`).
//!
//! - `/control/smartCharge/homePage` (read schedule and live state of charge)
//! - `/control/smartCharge/getAcCurrentLimit` (supported and active AC current limits)
//! - `/control/smartCharge/changeChargeStatue` (smart charging on/off, or start/stop charging)
//! - `/control/smartCharge/changeResult` (poll until a change settles)
//! - `/control/smartCharge/saveOrUpdate` (save the schedule)
//!
//! Serials are the JSON value BYD sent, as in [`crate::api::control`].

use std::future::Future;

use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::control::CommandAck;
use openbyd_protocol::models::smart_charging::{AcCurrentLimit, SmartChargingSchedule};
use openbyd_protocol::models::vehicle::EnergyType;
use openbyd_protocol::parity::{Args, Outcome, ParityError};
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, post_token_json};
use crate::api::control::{inner_base, truthy, validate, with_vin};
use crate::env::Env;
use crate::scripted::Scenario;

/// Where the schedule and live charging state are read.
pub const HOMEPAGE_ENDPOINT: &str = "/control/smartCharge/homePage";
/// Smart charging on/off, and charging start/stop.
pub const TOGGLE_ENDPOINT: &str = "/control/smartCharge/changeChargeStatue";
/// Where a change's progress is polled.
pub const RESULT_ENDPOINT: &str = "/control/smartCharge/changeResult";
/// Where a schedule is saved.
pub const SAVE_ENDPOINT: &str = "/control/smartCharge/saveOrUpdate";
/// Where the AC current limits are read.
pub const GET_AC_LIMIT_ENDPOINT: &str = "/control/smartCharge/getAcCurrentLimit";

/// Codes meaning the cloud accepted the request but could not reach the
/// vehicle right now (weak signal, no coverage). Recoverable: retry once the
/// vehicle is back online.
pub const VEHICLE_UNREACHABLE_CODES: [&str; 1] = ["6002"];

const NOT_SUPPORTED: ErrorCodes<'static> = ErrorCodes {
    not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
    extra_code_map: &[],
};

/// Posts `inner` to `endpoint` and returns the reply's data, or an empty
/// object when it is not an object.
async fn post_for_object<E: Env>(
    cx: Context<'_, E>,
    endpoint: &str,
    inner: &Map<String, Value>,
    vin: &str,
    codes: ErrorCodes<'_>,
) -> Result<Map<String, Value>, Error> {
    let decoded = post_token_json(cx, endpoint, inner, None, Some(vin), codes, None).await?;
    Ok(match decoded {
        Value::Object(raw) => raw,
        _ => Map::new(),
    })
}

/// `raw.get("requestSerial")`, null when missing.
fn serial_of(raw: &Map<String, Value>) -> Value {
    raw.get("requestSerial").cloned().unwrap_or(Value::Null)
}

/// pyBYD's `_api.charging.fetch_charging_homepage`, which
/// [`fetch_charging_schedule`] builds on. Kept here, private, until
/// `api::charging` is ported; the two must stay identical.
async fn fetch_charging_homepage<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
) -> Result<Map<String, Value>, Error> {
    let inner = inner_base(cx, vin, None);
    post_for_object(cx, HOMEPAGE_ENDPOINT, &inner, vin, NOT_SUPPORTED).await
}

/// Reads the configured smart charging schedule from
/// `/control/smartCharge/homePage` (the nested `smartChargeDto` and
/// `smartJourneyDto` blocks rather than the live charging fields).
pub async fn fetch_charging_schedule<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
) -> Result<SmartChargingSchedule, Error> {
    let raw = fetch_charging_homepage(cx, vin).await?;
    validate(&with_vin(vin, &raw, false))
}

/// Turns smart charging on or off and returns the acknowledgement.
pub async fn toggle_smart_charging<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    enable: bool,
) -> Result<CommandAck, Error> {
    let mut inner = inner_base(cx, vin, None);
    inner.insert(
        "smartChargeSwitch".into(),
        json!(if enable { "1" } else { "0" }),
    );
    let raw = post_for_object(cx, TOGGLE_ENDPOINT, &inner, vin, NOT_SUPPORTED).await?;
    validate(&with_vin(vin, &raw, true))
}

/// Starts or stops charging at `/control/smartCharge/changeChargeStatue`
/// and returns the reply (an empty object when it is not one) with its
/// `requestSerial` (null when missing), which the result poll needs.
///
/// The request always carries an empty `timeZone`, as the BYD app sends.
/// BYD's stop (`start: false`) is a known no-op: it answers success while
/// the car keeps charging.
pub async fn trigger_charge_change<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    start: bool,
) -> Result<(Value, Value), Error> {
    let mut inner = inner_base(cx, vin, None);
    inner.insert("timeZone".into(), json!(""));
    inner.insert("status".into(), json!(if start { "1" } else { "0" }));
    let raw = post_for_object(cx, TOGGLE_ENDPOINT, &inner, vin, NOT_SUPPORTED).await?;
    let serial = serial_of(&raw);
    Ok((Value::Object(raw), serial))
}

/// Polls `/control/smartCharge/changeResult` for a change's progress and
/// returns the reply with the serial to poll with next: the reply's
/// `requestSerial` when truthy, else `request_serial`.
///
/// The caller reads `res`: 1 pending, 2 success, anything else a failure
/// (see [`is_charge_change_ready`]).
pub async fn poll_charge_result<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    request_serial: &Value,
) -> Result<(Value, Value), Error> {
    let inner = inner_base(cx, vin, Some(request_serial));
    let raw = post_for_object(cx, RESULT_ENDPOINT, &inner, vin, NOT_SUPPORTED).await?;
    let serial = match raw.get("requestSerial") {
        Some(serial) if truthy(serial) => serial.clone(),
        _ => request_serial.clone(),
    };
    Ok((Value::Object(raw), serial))
}

/// A `fetch_fn` for the client's generic trigger-and-poll routine: called
/// with the trigger endpoint first (no serial), then with the poll endpoint
/// and the serial so far, it returns the reply and the next serial.
///
/// pyBYD's `make_*_fetch_fn` return closures of this shape; here they
/// return values that implement it.
pub trait FetchFn: Sync {
    /// Fetches `endpoint` for `vin`.
    fn fetch<'a, E: Env>(
        &'a self,
        cx: Context<'a, E>,
        endpoint: &'a str,
        vin: &'a str,
        request_serial: Option<&'a Value>,
    ) -> impl Future<Output = Result<(Value, Value), Error>> + Send + 'a;
}

/// `request_serial or ""`.
fn serial_or_empty(request_serial: Option<&Value>) -> Value {
    match request_serial {
        Some(serial) if truthy(serial) => serial.clone(),
        _ => json!(""),
    }
}

/// The `fetch_fn` of a charging start/stop: [`RESULT_ENDPOINT`] polls with
/// [`poll_charge_result`], any other endpoint triggers with
/// [`trigger_charge_change`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeChargeFetchFn {
    /// Start (`true`) or stop charging.
    pub start: bool,
}

/// Builds the `fetch_fn` for starting or stopping charging.
pub fn make_change_charge_fetch_fn(start: bool) -> ChangeChargeFetchFn {
    ChangeChargeFetchFn { start }
}

impl FetchFn for ChangeChargeFetchFn {
    async fn fetch<'a, E: Env>(
        &'a self,
        cx: Context<'a, E>,
        endpoint: &'a str,
        vin: &'a str,
        request_serial: Option<&'a Value>,
    ) -> Result<(Value, Value), Error> {
        if endpoint == RESULT_ENDPOINT {
            poll_charge_result(cx, vin, &serial_or_empty(request_serial)).await
        } else {
            trigger_charge_change(cx, vin, self.start).await
        }
    }
}

/// The `fetch_fn` of a schedule save: [`RESULT_ENDPOINT`] polls with
/// [`poll_charge_result`], any other endpoint saves with
/// [`trigger_save_charging_schedule`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveChargingScheduleFetchFn {
    /// Start as `"HH:MM"`.
    pub start_charge_time: String,
    /// End as `"HH:MM"`, or `"full"`.
    pub end_charge_time: String,
    /// Repeat selector.
    pub charge_way: String,
    /// Whether the schedule is active.
    pub enabled: bool,
}

/// Builds the `fetch_fn` for saving a charging schedule.
pub fn make_save_charging_schedule_fetch_fn(
    start_charge_time: &str,
    end_charge_time: &str,
    charge_way: &str,
    enabled: bool,
) -> SaveChargingScheduleFetchFn {
    SaveChargingScheduleFetchFn {
        start_charge_time: start_charge_time.to_owned(),
        end_charge_time: end_charge_time.to_owned(),
        charge_way: charge_way.to_owned(),
        enabled,
    }
}

impl FetchFn for SaveChargingScheduleFetchFn {
    async fn fetch<'a, E: Env>(
        &'a self,
        cx: Context<'a, E>,
        endpoint: &'a str,
        vin: &'a str,
        request_serial: Option<&'a Value>,
    ) -> Result<(Value, Value), Error> {
        if endpoint == RESULT_ENDPOINT {
            poll_charge_result(cx, vin, &serial_or_empty(request_serial)).await
        } else {
            trigger_save_charging_schedule(
                cx,
                vin,
                &self.start_charge_time,
                &self.end_charge_time,
                &self.charge_way,
                self.enabled,
            )
            .await
        }
    }
}

/// Whether a charge change reply is final: `res` is an integer other than
/// 1 (Python's `isinstance(res, int) and res != 1`, so `false` counts as
/// 0 and is final, `true` as 1 and is not, and `2.0` is not an integer).
/// The trigger's reply, with only `requestSerial`, is not final.
pub fn is_charge_change_ready(payload: &Map<String, Value>) -> bool {
    match payload.get("res") {
        Some(Value::Bool(flag)) => !flag,
        Some(Value::Number(number)) => match (number.as_i64(), number.as_u64()) {
            (Some(int), _) => int != 1,
            (None, Some(_)) => true,
            _ => false,
        },
        _ => false,
    }
}

/// Saves a charging schedule at `/control/smartCharge/saveOrUpdate` and
/// returns the reply with its `requestSerial`, to poll
/// [`RESULT_ENDPOINT`] with until `res` is 2 or a failure.
///
/// `start_charge_time` and `end_charge_time` are `"HH:MM"` (the end may be
/// `"full"`); `charge_way` is `"s"` (once), `"e"` (every day) or weekday
/// indices such as `"0,1,2,3,4"` (0 is Monday). 6002 (vehicle unreachable)
/// fails with `BydDataUnavailableError`.
pub async fn trigger_save_charging_schedule<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    start_charge_time: &str,
    end_charge_time: &str,
    charge_way: &str,
    enabled: bool,
) -> Result<(Value, Value), Error> {
    let mut inner = inner_base(cx, vin, None);
    inner.insert("startChargeTime".into(), json!(start_charge_time));
    inner.insert("endChargeTime".into(), json!(end_charge_time));
    inner.insert("chargeWay".into(), json!(charge_way));
    inner.insert("status".into(), json!(if enabled { "1" } else { "0" }));
    inner.insert("timeZone".into(), json!(""));
    let codes = ErrorCodes {
        not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
        extra_code_map: &[(
            &VEHICLE_UNREACHABLE_CODES,
            ErrorKind::BydDataUnavailableError,
        )],
    };
    let raw = post_for_object(cx, SAVE_ENDPOINT, &inner, vin, codes).await?;
    let serial = serial_of(&raw);
    Ok((Value::Object(raw), serial))
}

/// pyBYD's `energy_type` argument, `EnergyType | int | str`: an integer
/// (an `EnergyType` is one) is sent as `str(int(value))`, text as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnergyTypeArg {
    /// An integer or an `EnergyType`.
    Int(i64),
    /// Text, sent unchanged.
    Text(String),
}

impl Default for EnergyTypeArg {
    fn default() -> Self {
        EnergyType::Ev.into()
    }
}

impl From<EnergyType> for EnergyTypeArg {
    fn from(energy_type: EnergyType) -> Self {
        Self::Int(energy_type as i64)
    }
}

impl From<i64> for EnergyTypeArg {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<&str> for EnergyTypeArg {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl EnergyTypeArg {
    /// The `energyType` sent.
    pub fn wire(&self) -> String {
        match self {
            Self::Int(value) => value.to_string(),
            Self::Text(text) => text.clone(),
        }
    }
}

/// Reads the supported and active AC charging current limits.
pub async fn fetch_ac_current_limit<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    energy_type: EnergyTypeArg,
) -> Result<AcCurrentLimit, Error> {
    let mut inner = inner_base(cx, vin, None);
    inner.insert("energyType".into(), json!(energy_type.wire()));
    let raw = post_for_object(cx, GET_AC_LIMIT_ENDPOINT, &inner, vin, NOT_SUPPORTED).await?;
    validate(&with_vin(vin, &raw, false))
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

fn energy_type_argument(args: &Args<'_>) -> Result<EnergyTypeArg, ParityError> {
    match args.value("energy_type") {
        Value::Null => Ok(EnergyTypeArg::default()),
        Value::Bool(flag) => Ok(EnergyTypeArg::Int(i64::from(*flag))),
        Value::String(text) => Ok(text.as_str().into()),
        value => value
            .as_i64()
            .map(EnergyTypeArg::Int)
            .ok_or_else(|| ParityError::bad_arguments("energy_type must be an int or a string")),
    }
}

/// Runs a `fetch_fn` over `calls` (`[{"endpoint", "request_serial"}]`) in
/// order and returns every `[reply, serial]`; the first failure ends the run.
async fn run_fetches<E: Env, F: FetchFn>(
    cx: Context<'_, E>,
    fetch_fn: &F,
    vin: &str,
    calls: &[Value],
) -> Result<Value, Error> {
    let mut results = Vec::new();
    for call in calls {
        let endpoint = call.get("endpoint").and_then(Value::as_str).unwrap_or("");
        let serial = call
            .get("request_serial")
            .filter(|serial| !serial.is_null());
        let (reply, next) = fetch_fn.fetch(cx, endpoint, vin, serial).await?;
        results.push(json!([reply, next]));
    }
    Ok(Value::Array(results))
}

fn fetch_fn_call(scenario: &Scenario, save: bool) -> Outcome {
    let args = scenario.args();
    let vin = args.str("vin")?;
    let calls = args
        .value("calls")
        .as_array()
        .ok_or_else(|| ParityError::bad_arguments("calls must be a list"))?;
    if calls
        .iter()
        .any(|call| !call.get("endpoint").is_some_and(Value::is_string))
    {
        return Err(ParityError::bad_arguments("every call needs an endpoint"));
    }
    let cx = scenario.context();
    if save {
        let fetch_fn = make_save_charging_schedule_fetch_fn(
            args.str("start_charge_time")?,
            args.str("end_charge_time")?,
            args.str("charge_way")?,
            args.bool("enabled")?,
        );
        scenario.run(run_fetches(cx, &fetch_fn, vin, calls), |value| value)
    } else {
        let fetch_fn = make_change_charge_fetch_fn(args.bool("start")?);
        scenario.run(run_fetches(cx, &fetch_fn, vin, calls), |value| value)
    }
}

fn pair(pair: (Value, Value)) -> Value {
    json!([pair.0, pair.1])
}

fn flow_call(name: &str, scenario: &Scenario) -> Outcome {
    let args = scenario.args();
    let cx = scenario.context();
    let vin = args.str("vin")?;
    match name {
        "api.smart_charging.fetch_charging_schedule" => {
            scenario.run(fetch_charging_schedule(cx, vin), |model| model.dump())
        }
        "api.smart_charging.toggle_smart_charging" => scenario.run(
            toggle_smart_charging(cx, vin, args.bool("enable")?),
            |model| model.dump(),
        ),
        "api.smart_charging.trigger_charge_change" => {
            scenario.run(trigger_charge_change(cx, vin, args.bool("start")?), pair)
        }
        "api.smart_charging.poll_charge_result" => scenario.run(
            poll_charge_result(cx, vin, args.value("request_serial")),
            pair,
        ),
        "api.smart_charging.trigger_save_charging_schedule" => {
            let enabled = match args.value("enabled") {
                Value::Null => true,
                _ => args.bool("enabled")?,
            };
            scenario.run(
                trigger_save_charging_schedule(
                    cx,
                    vin,
                    args.str("start_charge_time")?,
                    args.str("end_charge_time")?,
                    args.str("charge_way")?,
                    enabled,
                ),
                pair,
            )
        }
        "api.smart_charging.fetch_ac_current_limit" => scenario.run(
            fetch_ac_current_limit(cx, vin, energy_type_argument(&args)?),
            |model| model.dump(),
        ),
        "api.smart_charging.make_change_charge_fetch_fn" => fetch_fn_call(scenario, false),
        _ => fetch_fn_call(scenario, true),
    }
}

fn constants() -> Value {
    json!({
        "_TOGGLE_ENDPOINT": TOGGLE_ENDPOINT,
        "_RESULT_ENDPOINT": RESULT_ENDPOINT,
        "_SAVE_ENDPOINT": SAVE_ENDPOINT,
        "_GET_AC_LIMIT_ENDPOINT": GET_AC_LIMIT_ENDPOINT,
        "VEHICLE_UNREACHABLE_CODES": VEHICLE_UNREACHABLE_CODES,
    })
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.smart_charging.constants" => Ok(constants()),
        "api.smart_charging.is_charge_change_ready" => match Args(args).value("payload") {
            Value::Object(payload) => Ok(json!(is_charge_change_ready(payload))),
            _ => Err(ParityError::new(
                "AttributeError",
                "object has no attribute 'get'",
            )),
        },
        "api.smart_charging.fetch_charging_schedule"
        | "api.smart_charging.toggle_smart_charging"
        | "api.smart_charging.trigger_charge_change"
        | "api.smart_charging.poll_charge_result"
        | "api.smart_charging.trigger_save_charging_schedule"
        | "api.smart_charging.fetch_ac_current_limit"
        | "api.smart_charging.make_change_charge_fetch_fn"
        | "api.smart_charging.make_save_charging_schedule_fetch_fn" => {
            Scenario::from_args(args).and_then(|scenario| flow_call(name, &scenario))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_follows_isinstance_int() {
        let ready = |res: Value| {
            let mut payload = Map::new();
            payload.insert("res".into(), res);
            is_charge_change_ready(&payload)
        };
        assert!(ready(json!(2)));
        assert!(ready(json!(3)));
        assert!(ready(json!(false)));
        assert!(ready(json!(u64::MAX)));
        assert!(!ready(json!(1)));
        assert!(!ready(json!(true)));
        assert!(!ready(json!(2.0)));
        assert!(!ready(json!("2")));
        assert!(!is_charge_change_ready(&Map::new()));
    }

    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn fetch_fns_can_run_on_any_thread() {
        let config = openbyd_protocol::config::BydConfig::new("user@example.com", "p");
        let session = openbyd_protocol::session::Session::new("1", "s", "e", 0.0, 1.0);
        let env = crate::scripted::ScriptedEnv::new(0, "s", vec![]);
        let cx = Context::new(&env, &config, &session);
        let save = make_save_charging_schedule_fetch_fn("22:00", "full", "e", true);
        let future = save.fetch(cx, SAVE_ENDPOINT, "V", None);
        assert_send(&future);
        let error = crate::scripted::block_on(future).unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ScriptExhausted);
        assert_send(&make_change_charge_fetch_fn(true).fetch(cx, RESULT_ENDPOINT, "V", None));
    }

    #[test]
    fn energy_types() {
        assert_eq!(EnergyTypeArg::default().wire(), "0");
        assert_eq!(EnergyTypeArg::from(EnergyType::Hybrid).wire(), "2");
        assert_eq!(EnergyTypeArg::from(7).wire(), "7");
        assert_eq!(EnergyTypeArg::from("EV").wire(), "EV");
        let read = |value: Value| energy_type_argument(&Args(&json!({ "energy_type": value })));
        assert_eq!(read(json!(true)).unwrap(), EnergyTypeArg::Int(1));
        assert_eq!(read(json!(null)).unwrap(), EnergyTypeArg::Int(0));
        assert!(read(json!(1.5)).is_err());
    }

    #[test]
    fn arguments_are_checked() {
        let base = json!({
            "config": {"username": "user@example.com", "password": "p"},
            "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
            "now_ms": 5, "seed": "x", "replies": [],
            "args": {"vin": "V", "start": true, "calls": []},
        });
        let kind = |name: &str, key: &str, value: Value| {
            let mut args = base.clone();
            args["args"][key] = value;
            parity_call(name, &args)
                .unwrap()
                .err()
                .map(|error| error.kind)
        };
        let change = "api.smart_charging.make_change_charge_fetch_fn";
        assert_eq!(kind(change, "calls", json!(1)).unwrap(), "BadArguments");
        assert_eq!(kind(change, "calls", json!([{}])).unwrap(), "BadArguments");
        assert_eq!(
            kind("api.smart_charging.toggle_smart_charging", "vin", json!(1)).unwrap(),
            "BadArguments"
        );
        assert_eq!(
            kind(
                "api.smart_charging.trigger_save_charging_schedule",
                "enabled",
                json!(1)
            )
            .unwrap(),
            "BadArguments"
        );
        assert_eq!(
            kind(
                "api.smart_charging.is_charge_change_ready",
                "payload",
                json!(null)
            )
            .unwrap(),
            "AttributeError"
        );
        assert!(constants()["VEHICLE_UNREACHABLE_CODES"].is_array());
        assert!(parity_call("api.smart_charging.nothing", &base).is_none());
    }
}
