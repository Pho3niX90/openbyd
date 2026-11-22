//! Preconditioning plan endpoints (`pybyd/_api/standby.py`).
//!
//! App 3.6.0 schedules climate through these:
//!
//! - `/control/standby-vehicles/list` (read plans)
//! - `/control/standby-vehicles/save` (create, or edit commands and schedule)
//! - `/control/standby-vehicles/setDefault` (make a plan the default)
//! - `/control/standby-vehicles/delete` (delete a user plan)
//! - `/control/standby-vehicles/send` (run a plan now)
//! - `/control/standby-vehicles/send-result` (poll the run)
//!
//! `list` answers `1001` unless the inner payload carries `userId`.
//!
//! These create, change, delete and run climate plans on real cars. Every
//! request body follows pyBYD's key by key; the trace cases in
//! `parity/groups/api_standby.py` hold them to it.

use openbyd_protocol::coerce::{py_decimal, py_is_digit};
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::{BydModel, PyDateTime, datetime_fromisoformat};
use openbyd_protocol::models::standby::{
    COMMAND_CLIMATE, EXECUTE_DAILY, EXECUTE_ONCE, EXECUTE_WEEKLY, PLAN_TYPE_USER, Repeat,
    StandbyPlan, StandbyPlanList, StandbySchedule, add_days, epoch_micros, from_epoch_millis,
};
use openbyd_protocol::parity::{Args, Outcome, ParityError};
use openbyd_protocol::pyjson::{DumpOptions, dumps};
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_token_json,
};
use crate::api::control::{inner_base, model_error, truthy, validate, validate_owned};
use crate::api::smart_charging::{FetchFn, is_charge_change_ready};
use crate::env::Env;
use crate::scripted::{Scenario, ScriptedEnv};

/// Where a vehicle's plans are read.
pub const LIST_ENDPOINT: &str = "/control/standby-vehicles/list";
/// Where a plan is created, or its commands and schedule changed.
pub const SAVE_ENDPOINT: &str = "/control/standby-vehicles/save";
/// Where a plan is made the default.
pub const SET_DEFAULT_ENDPOINT: &str = "/control/standby-vehicles/setDefault";
/// Where a user plan is deleted.
pub const DELETE_ENDPOINT: &str = "/control/standby-vehicles/delete";
/// Where a plan is run now.
pub const SEND_ENDPOINT: &str = "/control/standby-vehicles/send";
/// Where a run's progress is polled.
pub const SEND_RESULT_ENDPOINT: &str = "/control/standby-vehicles/send-result";

/// Codes meaning a plan's run time was refused: a once plan less than 20
/// minutes ahead or more than 30 days out (#91).
pub const SCHEDULE_REFUSED_CODES: [&str; 1] = ["6111"];

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

/// Fetches the vehicle's preconditioning plans.
///
/// The request carries `timeZone` and the session's `userId`. The reply's
/// data is either the list itself or `{"data": [...]}`; data that is
/// neither a list, an object nor empty is pyBYD's `AttributeError`.
pub async fn fetch_standby_plans<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
) -> Result<StandbyPlanList, Error> {
    let mut inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        None,
    );
    inner.insert("timeZone".into(), json!(cx.config.time_zone));
    inner.insert("userId".into(), json!(cx.session.user_id()));
    let decoded = post_token_json(
        cx,
        LIST_ENDPOINT,
        &inner,
        None,
        Some(vin),
        NOT_SUPPORTED,
        None,
    )
    .await?;
    // `decoded if isinstance(decoded, list) else (decoded or {}).get("data") or []`
    let plans = match decoded {
        Value::Array(_) => decoded,
        Value::Object(mut data) => match data.shift_remove("data") {
            Some(plans) if truthy(&plans) => plans,
            _ => json!([]),
        },
        other if !truthy(&other) => json!([]),
        _ => {
            return Err(Error::new(
                ErrorKind::AttributeError,
                "object has no attribute 'get'",
            ));
        }
    };
    validate_owned(json!({"vin": vin, "data": plans}))
}

/// `_plan_id`: the app sends the 64-bit id as a JSON number everywhere but
/// `send`. Text that `str.isdigit()` accepts goes through `int()` (which
/// refuses superscripts and the like with a `ValueError`); anything else is
/// sent as it is.
///
/// An id of more than 64 bits, which pyBYD would send as a longer number,
/// is refused here: `serde_json` has no such number.
fn plan_id_value(plan_id: &str) -> Result<Value, Error> {
    /// `sys.get_int_max_str_digits()`: `int()` refuses longer text, zeros included.
    const INT_MAX_STR_DIGITS: usize = 4300;
    if plan_id.is_empty() || !plan_id.chars().all(py_is_digit) {
        return Ok(json!(plan_id));
    }
    if plan_id.chars().count() > INT_MAX_STR_DIGITS {
        return Err(Error::new(
            ErrorKind::ValueError,
            "Exceeds the limit (4300 digits) for integer string conversion",
        ));
    }
    let mut number: u64 = 0;
    for c in plan_id.chars() {
        let digit = py_decimal(c).ok_or_else(|| {
            Error::new(
                ErrorKind::ValueError,
                format!("invalid literal for int() with base 10: {plan_id:?}"),
            )
        })?;
        number = number
            .checked_mul(10)
            .and_then(|number| number.checked_add(u64::from(digit)))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::ValueError,
                    "Unsupported: a plan id beyond 64 bits",
                )
            })?;
    }
    Ok(json!(number))
}

/// `numerator / denominator` rounded to the nearest `f64`, ties to even:
/// Python's true division of two integers.
fn true_divide(numerator: i64, denominator: u64) -> f64 {
    if numerator == 0 {
        return 0.0;
    }
    let size = u128::from(numerator.unsigned_abs());
    let denominator = u128::from(denominator);
    // Scale so the quotient has at least 55 bits, two more than an `f64`
    // holds, and fold the remainder into the lowest: the conversion then
    // rounds exactly once.
    let bits = |value: u128| 128 - value.leading_zeros() as i32;
    let shift = (56 - (bits(size) - bits(denominator))).max(0);
    let scaled = size << shift;
    let mut quotient = scaled / denominator;
    if scaled % denominator != 0 {
        quotient |= 1;
    }
    let value = quotient as f64 / 2f64.powi(shift);
    if numerator < 0 { -value } else { value }
}

/// `_epoch_ms`: `int(value.timestamp() * 1000)`. `timestamp()` is a float,
/// so this is not always the exact millisecond: a run at `…:20.001` can
/// come out as `…:20.000`. The app's own times are whole seconds.
fn epoch_ms(at: &PyDateTime) -> i64 {
    let seconds = true_divide(epoch_micros(at), 1_000_000);
    (seconds * 1000.0) as i64
}

/// `_next_daily`: the app sends the next occurrence of the time, so a stale
/// `at` is moved forward a day at a time until it is after `now`.
///
/// pyBYD's days are calendar days in the datetime's time zone; `at` has
/// only a UTC offset, so the days here are 24 hours. `following`
/// ([`StandbySchedule::with_daily_after`]) is the caller's own next day:
/// when `at` has passed it is the next candidate, and the 24-hour steps
/// start from it.
fn next_daily(
    at: &PyDateTime,
    following: Option<&PyDateTime>,
    now: &PyDateTime,
) -> Result<PyDateTime, Error> {
    const DAY_MICROS: i64 = 86_400_000_000;
    if epoch_micros(at) > epoch_micros(now) {
        return Ok(*at);
    }
    let from = match following {
        Some(following) if epoch_micros(following) > epoch_micros(at) => following,
        _ => at,
    };
    let behind = epoch_micros(now) - epoch_micros(from);
    if behind < 0 {
        return Ok(*from);
    }
    add_days(from, behind / DAY_MICROS + 1).ok_or_else(|| {
        // Python's `datetime + timedelta` past year 9999.
        Error::new(
            ErrorKind::ValueError,
            "OverflowError: date value out of range",
        )
    })
}

/// `_next_daily` for a time with its zone
/// ([`StandbySchedule::daily_in_zone`]): calendar days on the zone's wall
/// clock, compared with `now` as instants, exactly as Python does it.
fn next_daily_in_zone(at: &wallclock::DateTime, now: &PyDateTime) -> Result<PyDateTime, Error> {
    const DAY_MICROS: i64 = 86_400_000_000;
    let overflow = |_| {
        Error::new(
            ErrorKind::ValueError,
            "OverflowError: date value out of range",
        )
    };
    let now = epoch_micros(now);
    let mut next = at.clone();
    // pyBYD adds a day at a time. Years of days are skipped here in one
    // step, which lands on the same wall-clock time; the last few are
    // still taken singly. Three days short is always short: a zone's
    // offsets differ by less than two days.
    let skipped = (now - next.timestamp_micros()).div_euclid(DAY_MICROS) - 3;
    if skipped >= 1 {
        let days = wallclock::TimeDelta::from_days(skipped).map_err(overflow)?;
        next = next.checked_add(days).map_err(overflow)?;
    }
    while next.timestamp_micros() <= now {
        next = next
            .checked_add(wallclock::TimeDelta::DAY)
            .map_err(overflow)?;
    }
    Ok(PyDateTime::from(&next))
}

/// `save` fields for a schedule, as app 3.6.0 sends them.
///
/// A daily schedule whose time has passed is sent as its next occurrence
/// after `now`; without `now`, the environment's clock is read (pyBYD's
/// `datetime.now(UTC)`), and only then.
pub fn schedule_fields<E: Env>(
    env: &E,
    schedule: &StandbySchedule,
    now: Option<&PyDateTime>,
) -> Result<Map<String, Value>, Error> {
    let mut fields = Map::new();
    let timed = schedule.at().and_then(|at| at.aware());
    match (schedule.repeat(), timed) {
        (Repeat::Once, Some(at)) => {
            fields.insert("executeType".into(), json!(EXECUTE_ONCE));
            fields.insert("executeTime".into(), json!(epoch_ms(&at)));
        }
        (Repeat::Daily, Some(at)) => {
            let now = match now {
                Some(now) => *now,
                None => from_epoch_millis(env.now_ms()).ok_or_else(|| {
                    Error::new(
                        ErrorKind::ValueError,
                        "the clock is outside years 1 to 9999",
                    )
                })?,
            };
            let next = match schedule.zoned_at() {
                Some(zoned) => next_daily_in_zone(zoned, &now)?,
                None => next_daily(&at, schedule.daily_after().as_ref(), &now)?,
            };
            fields.insert("executeType".into(), json!(EXECUTE_DAILY));
            fields.insert("executeTime".into(), json!(epoch_ms(&next)));
        }
        (Repeat::Weekly, _) => {
            let days: Vec<String> = schedule.weekly_days().iter().map(i64::to_string).collect();
            fields.insert("executeType".into(), json!(EXECUTE_WEEKLY));
            fields.insert("executeTime".into(), json!(0));
            fields.insert("startTime".into(), json!(schedule.start_time()));
            fields.insert("weeklyDays".into(), json!(days.join(",")));
        }
        // The app's "Scheduled time" toggle off. (`once` and `daily` always
        // carry an aware time: `StandbySchedule` checks it.)
        (Repeat::Off | Repeat::Once | Repeat::Daily, _) => {
            fields.insert("executeType".into(), json!(EXECUTE_WEEKLY));
            fields.insert("executeTime".into(), json!(0));
            fields.insert("startTime".into(), json!(""));
            fields.insert("weeklyDays".into(), json!(""));
        }
    }
    Ok(fields)
}

/// Builds the `save` payload the way app 3.6.0 sends it.
///
/// A plan without an `id` is created; the response does not carry the new
/// id, only the next `list` does. `commandId` is left out of
/// `controlParams` as the app does, and `OPENAIR` goes first.
///
/// Fails with a `ValueError` when the plan has no `OPENAIR` command, since
/// every plan runs the A/C, before anything is read from the environment.
pub fn build_save_inner<E: Env>(
    env: &E,
    config: &BydConfig,
    vin: &str,
    plan: &StandbyPlan,
    schedule: &StandbySchedule,
    now: Option<&PyDateTime>,
) -> Result<Map<String, Value>, Error> {
    // `sorted(..., key=lambda command: command.command_type != "OPENAIR")`: stable.
    let (climate, others): (Vec<_>, Vec<_>) = plan
        .commands
        .iter()
        .partition(|command| command.command_type == COMMAND_CLIMATE);
    if climate.is_empty() {
        return Err(Error::new(
            ErrorKind::ValueError,
            "a plan needs an OPENAIR command, seats and steering wheel only go on top of the A/C",
        ));
    }
    let mut inner = build_inner_base(config, env.now_ms(), &env.random_bytes(), Some(vin), None);
    if !plan.id.is_empty() {
        inner.insert("id".into(), plan_id_value(&plan.id)?);
    }
    inner.insert("planName".into(), json!(plan.plan_name));
    inner.insert(
        "planType".into(),
        json!(plan.plan_type.unwrap_or(PLAN_TYPE_USER)),
    );
    inner.insert("defaultPlan".into(), json!(plan.default_plan.unwrap_or(0)));
    for (key, value) in schedule_fields(env, schedule, now)? {
        inner.insert(key, value);
    }
    let commands: Vec<Value> = climate
        .into_iter()
        .chain(others)
        .map(|command| {
            json!({
                "commandType": command.command_type,
                "controlParamsMap": command.control_params_map,
            })
        })
        .collect();
    inner.insert(
        "controlParams".into(),
        json!(dumps(&Value::Array(commands), DumpOptions::COMPACT_UNICODE)),
    );
    inner.insert("timeZone".into(), json!(config.time_zone));
    Ok(inner)
}

/// Posts `save`. The response only carries a `message`.
pub async fn save_standby_plan<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    plan: &StandbyPlan,
    schedule: &StandbySchedule,
) -> Result<Map<String, Value>, Error> {
    let inner = build_save_inner(cx.env, cx.config, vin, plan, schedule, None)?;
    let codes = ErrorCodes {
        extra_code_map: &[(&SCHEDULE_REFUSED_CODES, ErrorKind::BydStandbyScheduleError)],
        ..NOT_SUPPORTED
    };
    post_for_object(cx, SAVE_ENDPOINT, &inner, vin, codes).await
}

/// `_post_plan_id`: a request naming one plan by its id.
async fn post_plan_id<E: Env>(
    cx: Context<'_, E>,
    endpoint: &str,
    vin: &str,
    plan_id: &str,
) -> Result<Map<String, Value>, Error> {
    let mut inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        None,
    );
    inner.insert("id".into(), plan_id_value(plan_id)?);
    inner.insert("timeZone".into(), json!(cx.config.time_zone));
    post_for_object(cx, endpoint, &inner, vin, NOT_SUPPORTED).await
}

/// Posts `setDefault`. The cloud clears the flag on the other plans.
pub async fn set_default_standby_plan<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    plan_id: &str,
) -> Result<Map<String, Value>, Error> {
    post_plan_id(cx, SET_DEFAULT_ENDPOINT, vin, plan_id).await
}

/// Posts `delete`. The app only offers it on user plans.
pub async fn delete_standby_plan<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    plan_id: &str,
) -> Result<Map<String, Value>, Error> {
    post_plan_id(cx, DELETE_ENDPOINT, vin, plan_id).await
}

/// Posts `send` to run a plan now, and returns the reply (an empty object
/// when it is not one) with its `requestSerial` (null when missing).
///
/// The app sends `id` and `executeTime` as strings here and no `vin`;
/// `cancelReservation: "0"` keeps the schedule. The clock is read once, for
/// both the request's `timeStamp` and `executeTime`.
pub async fn trigger_send_standby_plan<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    plan_id: &str,
) -> Result<(Value, Value), Error> {
    let now_ms = cx.env.now_ms();
    let mut inner = build_inner_base(cx.config, now_ms, &cx.env.random_bytes(), None, None);
    inner.insert("id".into(), json!(plan_id));
    inner.insert("cancelReservation".into(), json!("0"));
    inner.insert("executeTime".into(), json!(now_ms.to_string()));
    inner.insert("timeZone".into(), json!(cx.config.time_zone));
    let raw = post_for_object(cx, SEND_ENDPOINT, &inner, vin, NOT_SUPPORTED).await?;
    let serial = raw.get("requestSerial").cloned().unwrap_or(Value::Null);
    Ok((Value::Object(raw), serial))
}

/// Posts `send-result` once, and returns the reply, with `requestSerial`
/// set to `request_serial` when it has none, and `request_serial` itself.
///
/// The caller reads `res` ([`is_send_ready`]).
pub async fn poll_send_result<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    request_serial: &Value,
) -> Result<(Value, Value), Error> {
    let inner = inner_base(cx, vin, Some(request_serial));
    let mut raw =
        post_for_object(cx, SEND_RESULT_ENDPOINT, &inner, vin, ErrorCodes::default()).await?;
    raw.entry("requestSerial")
        .or_insert_with(|| request_serial.clone());
    Ok((Value::Object(raw), request_serial.clone()))
}

/// The `fetch_fn` of a plan run for the client's trigger-and-poll routine:
/// [`SEND_RESULT_ENDPOINT`] polls with [`poll_send_result`], any other
/// endpoint triggers with [`trigger_send_standby_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendFetchFn {
    /// The plan to run.
    pub plan_id: String,
}

/// Builds the `fetch_fn` for running a plan.
pub fn make_send_fetch_fn(plan_id: &str) -> SendFetchFn {
    SendFetchFn {
        plan_id: plan_id.to_owned(),
    }
}

impl FetchFn for SendFetchFn {
    async fn fetch<'a, E: Env>(
        &'a self,
        cx: Context<'a, E>,
        endpoint: &'a str,
        vin: &'a str,
        request_serial: Option<&'a Value>,
    ) -> Result<(Value, Value), Error> {
        if endpoint == SEND_RESULT_ENDPOINT {
            // `request_serial or ""`
            let serial = match request_serial {
                Some(serial) if truthy(serial) => serial.clone(),
                _ => json!(""),
            };
            poll_send_result(cx, vin, &serial).await
        } else {
            trigger_send_standby_plan(cx, vin, &self.plan_id).await
        }
    }
}

/// Whether a run's reply is final: `res` 1 is in progress, any other
/// integer is terminal (Python's `isinstance(res, int) and res != 1`, as
/// [`is_charge_change_ready`]).
pub fn is_send_ready(payload: &Map<String, Value>) -> bool {
    is_charge_change_ready(payload)
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

fn kwargs<'a>(args: &Args<'a>, key: &str) -> Result<&'a Map<String, Value>, ParityError> {
    args.value(key)
        .as_object()
        .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be an object")))
}

/// The `now` of a pure call: ISO text read with `datetime.fromisoformat`,
/// aware or naive, or null.
fn now_argument(args: &Args<'_>) -> Result<Option<(PyDateTime, bool)>, ParityError> {
    let Some(text) = args.opt_str("now")? else {
        return Ok(None);
    };
    let (local, offset) = datetime_fromisoformat(text)
        .ok_or_else(|| ParityError::bad_arguments("now must be an ISO datetime"))?;
    let now = PyDateTime {
        local,
        offset_us: offset.unwrap_or(0),
    };
    Ok(Some((now, offset.is_some())))
}

/// A failure of a pure call as pyBYD raises it: `OverflowError` has no
/// kind of its own (see [`next_daily`]).
fn pure_error(error: Error) -> ParityError {
    if error.kind() == ErrorKind::ValueError && error.message().starts_with("OverflowError") {
        return ParityError::new("OverflowError", error.message());
    }
    error.into()
}

/// Moves a daily schedule's `at` into the IANA time zone `zone`
/// (`at.astimezone(ZoneInfo(zone))`), so its days follow that zone's wall
/// clock across daylight-saving changes.
///
/// Non-daily schedules and a `None` zone are returned unchanged. Fails with
/// [`wallclock::Error`] on an unknown or malformed zone key, or when moving
/// `at` into `zone` overflows Python's year range 1 to 9999.
pub fn schedule_in_zone(
    schedule: StandbySchedule,
    zone: Option<&str>,
) -> Result<StandbySchedule, wallclock::Error> {
    let (Some(zone), Repeat::Daily) = (zone, schedule.repeat()) else {
        return Ok(schedule);
    };
    let Some(at) = schedule.at().and_then(|at| at.aware()) else {
        return Ok(schedule);
    };
    let tz = wallclock::Tz::zone(zone)?;
    let Some(zoned) = at.to_zoned() else {
        return Err(wallclock::Error::InvalidValue("year is out of range"));
    };
    let at = zoned.astimezone(&tz)?;
    Ok(StandbySchedule::daily_in_zone(at))
}

/// The `zone` argument of the parity calls: [`schedule_in_zone`] mapped to [`ParityError`].
fn in_zone(schedule: StandbySchedule, zone: Option<&str>) -> Result<StandbySchedule, ParityError> {
    schedule_in_zone(schedule, zone)
        .map_err(|error| ParityError::new(error.python_exception(), error.to_string()))
}

/// `schedule_fields` and `build_save_inner`: plain calls that read the
/// scripted clock (`now_ms`) and random values (`seed`).
fn pure_call(name: &str, args: &Args<'_>) -> Outcome {
    let seed = args.opt_str("seed")?.unwrap_or("");
    let env = ScriptedEnv::new(args.i64("now_ms")?, seed, Vec::new());
    let (config, plan) = if name == "api.standby.build_save_inner" {
        let config = BydConfig::from_kwargs(kwargs(args, "config")?)?;
        let plan = StandbyPlan::parse(args.value("plan"), &Value::Null)?;
        (Some(config), Some(plan))
    } else {
        (None, None)
    };
    let schedule = StandbySchedule::from_kwargs(kwargs(args, "schedule")?)?;
    let schedule = in_zone(schedule, args.opt_str("zone")?)?;
    let given = now_argument(args)?;
    // Comparing a naive `now` with the schedule's aware time is a
    // `TypeError`, raised where pyBYD compares them: after everything that
    // fails earlier. The earliest datetime stands in to get that far.
    let naive = matches!(given, Some((_, false))) && schedule.repeat() == Repeat::Daily;
    let now = match given {
        _ if naive => from_epoch_millis(-62_135_596_800_000),
        Some((now, true)) => Some(now),
        _ => None,
    };
    let fields = match (config, plan) {
        (Some(config), Some(plan)) => {
            let vin = args.str("vin")?;
            build_save_inner(&env, &config, vin, &plan, &schedule, now.as_ref())
        }
        _ => schedule_fields(&env, &schedule, now.as_ref()),
    };
    if naive && fields.is_ok() {
        return Err(ParityError::new(
            "TypeError",
            "can't compare offset-naive and offset-aware datetimes",
        ));
    }
    fields.map(Value::Object).map_err(pure_error)
}

fn pair(pair: (Value, Value)) -> Value {
    json!([pair.0, pair.1])
}

/// Runs the plan run's `fetch_fn` over `calls` (`[{"endpoint",
/// "request_serial"}]`) in order; the first failure ends the run.
async fn run_fetches<E: Env>(
    cx: Context<'_, E>,
    fetch_fn: &SendFetchFn,
    vin: &str,
    calls: &[Value],
) -> Result<Value, Error> {
    let mut results = Vec::new();
    for call in calls {
        let endpoint = call.get("endpoint").and_then(Value::as_str).unwrap_or("");
        let serial = call
            .get("request_serial")
            .filter(|serial| !serial.is_null());
        results.push(pair(fetch_fn.fetch(cx, endpoint, vin, serial).await?));
    }
    Ok(Value::Array(results))
}

fn flow_call(name: &str, scenario: &Scenario) -> Outcome {
    let args = scenario.args();
    let cx = scenario.context();
    let vin = args.str("vin")?;
    match name {
        "api.standby.fetch_standby_plans" => {
            scenario.run(fetch_standby_plans(cx, vin), |plans| plans.dump())
        }
        "api.standby.save_standby_plan" => {
            let schedule = kwargs(&args, "schedule")?;
            scenario.run(
                async {
                    // pyBYD's caller builds both models before the call.
                    let plan: StandbyPlan = validate(args.value("plan"))?;
                    let schedule = StandbySchedule::from_kwargs(schedule).map_err(model_error)?;
                    save_standby_plan(cx, vin, &plan, &schedule).await
                },
                Value::Object,
            )
        }
        "api.standby.set_default_standby_plan" => scenario.run(
            set_default_standby_plan(cx, vin, args.str("plan_id")?),
            Value::Object,
        ),
        "api.standby.delete_standby_plan" => scenario.run(
            delete_standby_plan(cx, vin, args.str("plan_id")?),
            Value::Object,
        ),
        "api.standby.trigger_send_standby_plan" => scenario.run(
            trigger_send_standby_plan(cx, vin, args.str("plan_id")?),
            pair,
        ),
        "api.standby.poll_send_result" => scenario.run(
            poll_send_result(cx, vin, args.value("request_serial")),
            pair,
        ),
        _ => {
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
            let fetch_fn = make_send_fetch_fn(args.str("plan_id")?);
            scenario.run(run_fetches(cx, &fetch_fn, vin, calls), |value| value)
        }
    }
}

fn constants() -> Value {
    json!({
        "LIST_ENDPOINT": LIST_ENDPOINT,
        "SAVE_ENDPOINT": SAVE_ENDPOINT,
        "SET_DEFAULT_ENDPOINT": SET_DEFAULT_ENDPOINT,
        "DELETE_ENDPOINT": DELETE_ENDPOINT,
        "SEND_ENDPOINT": SEND_ENDPOINT,
        "SEND_RESULT_ENDPOINT": SEND_RESULT_ENDPOINT,
    })
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.standby.constants" => Ok(constants()),
        "api.standby.is_send_ready" => match Args(args).value("payload") {
            Value::Object(payload) => Ok(json!(is_send_ready(payload))),
            _ => Err(ParityError::new(
                "AttributeError",
                "object has no attribute 'get'",
            )),
        },
        "api.standby.schedule_fields" | "api.standby.build_save_inner" => {
            pure_call(name, &Args(args))
        }
        "api.standby.fetch_standby_plans"
        | "api.standby.save_standby_plan"
        | "api.standby.set_default_standby_plan"
        | "api.standby.delete_standby_plan"
        | "api.standby.trigger_send_standby_plan"
        | "api.standby.poll_send_result"
        | "api.standby.make_send_fetch_fn" => {
            Scenario::from_args(args).and_then(|scenario| flow_call(name, &scenario))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use openbyd_protocol::models::standby::{StandbyPlanCommand, pydantic_datetime};
    use openbyd_protocol::session::Session;

    use super::*;
    use crate::scripted::block_on;

    fn aware(text: &str) -> PyDateTime {
        pydantic_datetime(&json!(text)).unwrap().aware().unwrap()
    }

    fn madrid_config() -> BydConfig {
        let mut config = BydConfig::new("user@example.com", "p");
        config.time_zone = "Europe/Madrid".into();
        config
    }

    fn plan(raw: Value) -> StandbyPlan {
        StandbyPlan::parse(&raw, &Value::Null).unwrap()
    }

    #[test]
    fn schedule_fields_match_the_app() {
        // Sunday 22 at 19:10 once, and 23:12 every day saved on Wednesday 18 at 15:42.
        let env = ScriptedEnv::new(0, "s", vec![]);
        let once = StandbySchedule::once(aware("2026-11-22T19:10:00+01:00"));
        assert_eq!(
            Value::Object(schedule_fields(&env, &once, None).unwrap()),
            json!({"executeType": "0", "executeTime": 1_795_371_000_000_i64})
        );
        let daily = StandbySchedule::daily(aware("2026-11-18T23:12:00+01:00"));
        let now = aware("2026-11-18T15:42:00+01:00");
        assert_eq!(
            Value::Object(schedule_fields(&env, &daily, Some(&now)).unwrap()),
            json!({"executeType": "1", "executeTime": 1_795_039_920_000_i64})
        );
        assert_eq!(
            Value::Object(schedule_fields(&env, &StandbySchedule::off(), None).unwrap()),
            json!({"executeType": "2", "executeTime": 0, "startTime": "", "weeklyDays": ""})
        );
        let weekly = StandbySchedule::weekly("12:55", &[5, 4]).unwrap();
        assert_eq!(
            Value::Object(schedule_fields(&env, &weekly, None).unwrap()),
            json!({"executeType": "2", "executeTime": 0, "startTime": "12:55", "weeklyDays": "4,5"})
        );
    }

    #[test]
    fn daily_moves_a_past_time_to_the_next_day() {
        let at = aware("2026-11-18T07:00:00+01:00");
        let later = aware("2026-11-20T08:00:00+01:00");
        let next = next_daily(&at, None, &later).unwrap();
        assert_eq!(next.to_json(), "2026-11-21T07:00:00+01:00");
        // Exactly now is not "after now".
        assert_eq!(
            next_daily(&at, None, &at).unwrap().to_json(),
            "2026-11-19T07:00:00+01:00"
        );
        assert_eq!(next_daily(&later, None, &at).unwrap(), later);
        let last = aware("9999-12-31T00:00:00Z");
        let error = next_daily(&last, None, &last).unwrap_err();
        assert_eq!(pure_error(error).kind, "OverflowError");
        // Without `now` the clock is read: the scripted clock says 2026-11-20T08:00:00+01:00.
        let env = ScriptedEnv::new(epoch_micros(&later) / 1000, "s", vec![]);
        let fields = schedule_fields(&env, &StandbySchedule::daily(at), None).unwrap();
        assert_eq!(fields["executeTime"], json!(epoch_micros(&next) / 1000));
        let far = ScriptedEnv::new(i64::MAX, "s", vec![]);
        assert!(schedule_fields(&far, &StandbySchedule::daily(at), None).is_err());
    }

    #[test]
    fn daily_keeps_its_time_of_day_when_the_clocks_change() {
        // 08:00 in Madrid on the day before the clocks go back, and the
        // same time the day after: 25 hours later, not 24.
        let at = aware("2026-10-24T08:00:00+02:00");
        let following = aware("2026-10-25T08:00:00+01:00");
        let before = aware("2026-10-24T07:59:59+02:00");
        let just_after = aware("2026-10-24T08:00:01+02:00");
        let next = |following: Option<&PyDateTime>, now: &PyDateTime| {
            next_daily(&at, following, now).unwrap().to_json()
        };
        // Still ahead: sent as it is.
        assert_eq!(next(Some(&following), &before), at.to_json());
        // It passed while the request was being built: the caller's next day.
        assert_eq!(next(Some(&following), &just_after), following.to_json());
        assert_eq!(next(Some(&following), &at), following.to_json());
        // Without it, 24 hours on, which is 07:00 that day.
        assert_eq!(next(None, &just_after), "2026-10-25T08:00:00+02:00");
        // Passed as well: 24-hour days from there, which keeps the new offset.
        let days_later = aware("2026-10-27T12:00:00+01:00");
        assert_eq!(
            next(Some(&following), &days_later),
            "2026-10-28T08:00:00+01:00"
        );
        assert_eq!(
            next(Some(&following), &following),
            "2026-10-26T08:00:00+01:00"
        );
        // One that is not after `at` is not a next day.
        assert_eq!(next(Some(&at), &just_after), next(None, &just_after));
        assert_eq!(next(Some(&before), &just_after), next(None, &just_after));

        // Through the request: the scripted clock says one second past 08:00.
        let env = ScriptedEnv::new(epoch_micros(&just_after) / 1000, "s", vec![]);
        let schedule = StandbySchedule::daily(at).with_daily_after(following);
        assert_eq!(schedule.daily_after(), Some(following));
        let fields = schedule_fields(&env, &schedule, None).unwrap();
        assert_eq!(
            fields["executeTime"],
            json!(epoch_micros(&following) / 1000)
        );
        // It is not one of pyBYD's fields, and only a daily schedule reads it.
        assert_eq!(
            schedule.model_dump(),
            StandbySchedule::daily(at).model_dump()
        );
        let once = StandbySchedule::once(at).with_daily_after(following);
        let fields = schedule_fields(&env, &once, None).unwrap();
        assert_eq!(fields["executeTime"], json!(epoch_micros(&at) / 1000));
    }

    #[test]
    fn daily_in_a_zone_moves_by_its_calendar_days() {
        let madrid = wallclock::Tz::zone("Europe/Madrid").unwrap();
        let zoned = |year, month, day, hour| {
            wallclock::DateTime::new(year, month, day, &madrid)
                .unwrap()
                .with_time(hour, 0, 0, 0)
                .unwrap()
        };
        // 08:00 on the eve of the clocks going back, and nobody gave the
        // next day's run: the zone itself says it is 25 hours on.
        let at = zoned(2026, 10, 24, 8);
        let schedule = StandbySchedule::daily_in_zone(at.clone());
        assert_eq!(schedule.zoned_at(), Some(&at));
        assert_eq!(
            schedule.model_dump()["at"],
            json!("2026-10-24T08:00:00+02:00")
        );
        let sent = |now: &str| {
            let env = ScriptedEnv::new(epoch_micros(&aware(now)) / 1000, "s", vec![]);
            schedule_fields(&env, &schedule, None).unwrap()["executeTime"].clone()
        };
        let millis = |text: &str| json!(epoch_micros(&aware(text)) / 1000);
        assert_eq!(
            sent("2026-10-24T07:59:59+02:00"),
            millis("2026-10-24T08:00:00+02:00")
        );
        assert_eq!(
            sent("2026-10-24T08:00:00+02:00"),
            millis("2026-10-25T08:00:00+01:00")
        );
        // Weeks, then years, later: still 08:00 there, summer or winter.
        assert_eq!(
            sent("2026-11-20T12:00:00Z"),
            millis("2026-11-21T08:00:00+01:00")
        );
        assert_eq!(
            sent("2031-07-01T06:00:00Z"),
            millis("2031-07-02T08:00:00+02:00")
        );
        assert_eq!(
            sent("2031-07-01T05:59:59Z"),
            millis("2031-07-01T08:00:00+02:00")
        );

        // A time the next day skips is read as Python reads it: by the
        // rules before the change, an hour later on the clock.
        let schedule = StandbySchedule::daily_in_zone(zoned(2026, 3, 28, 2));
        let env = ScriptedEnv::new(
            epoch_micros(&aware("2026-03-28T12:00:00Z")) / 1000,
            "s",
            vec![],
        );
        assert_eq!(
            schedule_fields(&env, &schedule, None).unwrap()["executeTime"],
            millis("2026-03-29T02:00:00+01:00")
        );

        // Off the end of the calendar, as pyBYD's `OverflowError`.
        let last = StandbySchedule::daily_in_zone(zoned(9999, 12, 31, 8));
        let env = ScriptedEnv::new(
            epoch_micros(&aware("9999-12-31T12:00:00Z")) / 1000,
            "s",
            vec![],
        );
        let error = schedule_fields(&env, &last, None).unwrap_err();
        assert_eq!(pure_error(error).kind, "OverflowError");
        let far = StandbySchedule::daily_in_zone(zoned(2, 1, 1, 8));
        let env = ScriptedEnv::new(i64::MAX, "s", vec![]);
        assert!(schedule_fields(&env, &far, None).is_err());
    }

    #[test]
    fn epoch_milliseconds_follow_python_floats() {
        // int(1.001 * 1000) is 1000 in floating point, and so it is here.
        assert_eq!(epoch_ms(&aware("1970-01-01T00:00:01.001Z")), 1000);
        assert_eq!(epoch_ms(&aware("1970-01-01T00:00:01.002Z")), 1002);
        assert_eq!(epoch_ms(&aware("1969-12-31T23:59:58.5Z")), -1500);
        assert_eq!(epoch_ms(&aware("1970-01-01T00:00:00Z")), 0);
        assert_eq!(true_divide(1, 3), 1.0 / 3.0);
        assert_eq!(true_divide(-1, 1_000_000), -1e-6);
        assert_eq!(true_divide(i64::MAX, 1), 9_223_372_036_854_775_807_f64);
        assert_eq!(
            true_divide(9_007_199_254_740_993, 1),
            9_007_199_254_740_992_f64
        );
        assert_eq!(
            true_divide(9_007_199_254_740_995, 1),
            9_007_199_254_740_996_f64
        );
    }

    #[test]
    fn save_payloads() {
        let env = ScriptedEnv::new(1_700_000_000_000, "s", vec![]);
        let config = madrid_config();
        let preset = plan(json!({
            "id": 1_216_038_691_305_533_440_i64, "planName": "Conducción confort", "planType": 1,
            "defaultPlan": 1,
            "controlParamsList": [{"commandId": "3", "commandType": "OPENAIR",
                                   "controlParamsMap": "{\"mainSettingTemp\":5,\"timeSpan\":1}"}],
        }));
        let weekly = StandbySchedule::weekly("12:55", &[5, 4]).unwrap();
        let inner = build_save_inner(&env, &config, "VIN", &preset, &weekly, None).unwrap();
        assert_eq!(inner["id"], json!(1_216_038_691_305_533_440_i64));
        assert_eq!(inner["weeklyDays"], json!("4,5"));
        assert_eq!(inner["timeZone"], json!("Europe/Madrid"));
        assert_eq!(
            inner["controlParams"],
            json!(
                "[{\"commandType\":\"OPENAIR\",\"controlParamsMap\":{\"mainSettingTemp\":5,\"timeSpan\":1}}]"
            )
        );
        let keys: Vec<&str> = inner.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "deviceType",
                "imeiMD5",
                "networkType",
                "random",
                "timeStamp",
                "version",
                "vin",
                "id",
                "planName",
                "planType",
                "defaultPlan",
                "executeType",
                "executeTime",
                "startTime",
                "weeklyDays",
                "controlParams",
                "timeZone"
            ]
        );
        // A new plan has no id; the A/C goes first.
        let mut new = plan(json!({"planName": "Test plan"}));
        new.commands = vec![
            StandbyPlanCommand::steering_wheel(true),
            StandbyPlanCommand::climate(21.0, 10, false, 14.0).unwrap(),
        ];
        let inner =
            build_save_inner(&env, &config, "VIN", &new, &StandbySchedule::off(), None).unwrap();
        assert!(!inner.contains_key("id"));
        assert_eq!(
            (&inner["planType"], &inner["defaultPlan"]),
            (&json!(2), &json!(0))
        );
        let commands: Value =
            serde_json::from_str(inner["controlParams"].as_str().unwrap()).unwrap();
        assert_eq!(commands[0]["commandType"], json!("OPENAIR"));
        assert_eq!(commands[1]["commandType"], json!("STEERINGWHEELHEATING"));
        // Seats and steering wheel only go on top of the A/C.
        new.commands.pop();
        let error = build_save_inner(&env, &config, "VIN", &new, &StandbySchedule::off(), None)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ValueError);
        assert!(error.message().contains("OPENAIR"));
    }

    #[test]
    fn plan_ids() {
        assert_eq!(
            plan_id_value("1216038691305533440").unwrap(),
            json!(1_216_038_691_305_533_440_i64)
        );
        assert_eq!(plan_id_value("007").unwrap(), json!(7));
        assert_eq!(plan_id_value("\u{661}\u{662}").unwrap(), json!(12));
        assert_eq!(plan_id_value("").unwrap(), json!(""));
        assert_eq!(plan_id_value("7a").unwrap(), json!("7a"));
        assert_eq!(plan_id_value("-7").unwrap(), json!("-7"));
        assert_eq!(
            plan_id_value("18446744073709551615").unwrap(),
            json!(u64::MAX)
        );
        assert_eq!(
            plan_id_value("\u{b2}").unwrap_err().kind(),
            ErrorKind::ValueError
        );
        assert!(plan_id_value("18446744073709551616").is_err());
        // Python's `int()` digit limit counts leading zeros.
        let padded = |zeros: usize| format!("{}7", "0".repeat(zeros));
        assert_eq!(plan_id_value(&padded(4299)).unwrap(), json!(7));
        assert_eq!(
            plan_id_value(&padded(4300)).unwrap_err().kind(),
            ErrorKind::ValueError
        );
    }

    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn the_fetch_fn_can_run_on_any_thread() {
        let config = madrid_config();
        let session = Session::new("1", "s", "e", 0.0, 1.0);
        let env = ScriptedEnv::new(0, "s", vec![]);
        let cx = Context::new(&env, &config, &session);
        let fetch = make_send_fetch_fn("7");
        let future = fetch.fetch(cx, SEND_ENDPOINT, "V", None);
        assert_send(&future);
        let error = block_on(future).unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ScriptExhausted);
        assert_send(&fetch.fetch(cx, SEND_RESULT_ENDPOINT, "V", None));
        assert_send(&save_standby_plan(
            cx,
            "V",
            &plan(json!({})),
            &StandbySchedule::off(),
        ));
        assert!(!is_send_ready(&Map::new()));
    }

    #[test]
    fn arguments_are_checked() {
        let base = json!({
            "config": {"username": "user@example.com", "password": "p"},
            "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
            "now_ms": 5, "seed": "x", "replies": [],
            "args": {"vin": "V", "plan_id": "7", "calls": [], "plan": {}, "schedule": {"repeat": "off"}},
        });
        let kind = |name: &str, key: &str, value: Value| {
            let mut args = base.clone();
            args["args"][key] = value;
            parity_call(name, &args)
                .unwrap()
                .err()
                .map(|error| error.kind)
        };
        let fetch = "api.standby.make_send_fetch_fn";
        assert_eq!(kind(fetch, "calls", json!(1)).unwrap(), "BadArguments");
        assert_eq!(kind(fetch, "calls", json!([{}])).unwrap(), "BadArguments");
        assert_eq!(kind(fetch, "plan_id", json!(7)).unwrap(), "BadArguments");
        assert_eq!(
            kind("api.standby.save_standby_plan", "schedule", json!(1)).unwrap(),
            "BadArguments"
        );
        assert_eq!(
            kind("api.standby.fetch_standby_plans", "vin", json!(1)).unwrap(),
            "BadArguments"
        );
        let pure =
            |name: &str, args: Value| parity_call(name, &args).unwrap().err().map(|e| e.kind);
        assert_eq!(
            pure("api.standby.is_send_ready", json!({"payload": null})).unwrap(),
            "AttributeError"
        );
        let fields = "api.standby.schedule_fields";
        let daily = json!({"repeat": "daily", "at": "2026-11-18T07:00:00Z"});
        assert_eq!(
            pure(
                fields,
                json!({"now_ms": 5, "schedule": daily, "now": "2026-11-18T07:00:00"})
            )
            .unwrap(),
            "TypeError"
        );
        assert_eq!(
            pure(
                fields,
                json!({"now_ms": 5, "schedule": daily, "now": "garbage"})
            )
            .unwrap(),
            "BadArguments"
        );
        assert_eq!(
            pure(fields, json!({"now_ms": 5, "schedule": 1})).unwrap(),
            "BadArguments"
        );
        assert!(
            pure(
                fields,
                json!({"now_ms": 5, "schedule": {"repeat": "off"}, "now": "2026-11-18T07:00:00"})
            )
            .is_none()
        );
        assert_eq!(
            pure(
                "api.standby.build_save_inner",
                json!({"now_ms": 5, "config": {"username": "user@example.com", "password": "p"},
                       "plan": [], "schedule": {"repeat": "off"}, "vin": "V"})
            )
            .unwrap(),
            "ValidationError"
        );
        assert!(constants()["LIST_ENDPOINT"].is_string());
        assert!(parity_call("api.standby.nothing", &base).is_none());
    }
}
