//! Preconditioning plan models (`pybyd/models/standby.py`).
//!
//! App 3.6.0 schedules climate through these plans (the
//! `/control/standby-vehicles/*` endpoints) instead of `BOOKINGAIR`. A plan
//! holds one or more remote commands (`OPENAIR`, `VENTILATIONHEATING`,
//! `STEERINGWHEELHEATING`, each with its `controlParamsMap`) and an optional
//! schedule: once, every day or weekly. The next run comes back in
//! `executeTime`.
//!
//! Every plan runs the A/C; seats and steering wheel heat are extras on top
//! of it, and the app has no way to save a plan without `OPENAIR`.
//!
//! Three kinds of thing live here:
//!
//! - reply models, parsed like every other pyBYD model ([`BydModel`]):
//!   [`StandbyPlanCommand`], [`StandbyPlan`], [`StandbyPlanList`] and
//!   [`StandbyPlanRunResult`];
//! - the plain settings pyBYD builds from keyword arguments:
//!   [`PlanClimate`], [`PlanSeats`] and [`StandbySchedule`], each with a
//!   `from_kwargs` that accepts exactly what the Python constructor accepts;
//! - the datetime arithmetic `pybyd/_api/standby.py` does on a schedule's
//!   run time ([`epoch_micros`], [`add_days`], [`from_epoch_millis`]), kept
//!   here because the client crate has no calendar of its own.

use std::borrow::Cow;
use std::collections::BTreeSet;

use chrono::{Duration, NaiveDate, NaiveDateTime};
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields, Plan, PyDateTime, py_equals_int, py_truthy};
use super::control::{ClimateStartArgs, ClimateStartParams, SeatClimateArgs, SeatClimateParams};
use crate::coerce::{self, ValidationError, py_decimal, py_is_digit, py_strip};
use crate::constants::{
    self, DEFAULT_TEMP_OFFSET_C, VALID_CLIMATE_DURATIONS, minutes_to_time_span,
};
use crate::parity::{Args, Outcome, ParityError};
use crate::pyjson::{self, py_str};

/// The `commandType` of a plan's A/C command.
pub const COMMAND_CLIMATE: &str = "OPENAIR";
/// The `commandType` of a plan's seat heating and ventilation command.
pub const COMMAND_SEATS: &str = "VENTILATIONHEATING";
/// The `commandType` of a plan's steering wheel heat command.
pub const COMMAND_STEERING_WHEEL: &str = "STEERINGWHEELHEATING";

/// `executeType` of a plan that runs once.
pub const EXECUTE_ONCE: &str = "0";
/// `executeType` of a plan that runs every day.
pub const EXECUTE_DAILY: &str = "1";
/// `executeType` of a plan that runs on chosen weekdays (also sent for "not scheduled").
pub const EXECUTE_WEEKLY: &str = "2";

/// `planType` of the built-in plan, which cannot be deleted.
pub const PLAN_TYPE_PRESET: i64 = 1;
/// `planType` of a plan the user made.
pub const PLAN_TYPE_USER: i64 = 2;

/// Plans only do on and off: `1` is on, `3` is off, `0` means the car has no such seat feature.
const SEAT_ON: i64 = 1;
const SEAT_OFF: i64 = 3;

/// Whether the first byte of `text` after JSON whitespace is `expected`.
fn starts_with_json_byte(text: &str, expected: u8) -> bool {
    text.as_bytes()
        .iter()
        .find(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        == Some(&expected)
}

/// `_json_or_raw`: `controlParams` and `controlParamsMap` come JSON-encoded
/// as strings. Text that is not JSON gives null; anything else is itself.
///
/// Callers keep only one JSON container kind (`b'{'` for `controlParamsMap`,
/// `b'['` for `controlParams`) and discard scalars and containers of the other
/// kind. Text starting with `expected_start` (after JSON whitespace) that
/// Python decodes into something serde_json cannot hold (`NaN` and `Infinity`,
/// numbers outside `f64`, integers beyond 64 bits within Python's 4,300-digit
/// limit, lone surrogates, or containers nested deeper than 127 levels:
/// [`pyjson::beyond_serde`]) is refused as `Unsupported`, because a plan
/// decoded differently would be saved back differently and its settings reach
/// a car. Any other such text would be discarded by pyBYD, so it is left to
/// [`pyjson::loads`] and yields the same discarded result as pyBYD.
fn json_or_raw(value: &Value, expected_start: u8) -> Result<Cow<'_, Value>, ValidationError> {
    Ok(match value {
        Value::String(text)
            if starts_with_json_byte(text, expected_start) && pyjson::beyond_serde(text) =>
        {
            return Err(ValidationError::raised("Unsupported", "json_outside_serde"));
        }
        Value::String(text) => Cow::Owned(pyjson::loads(text).unwrap_or(Value::Null)),
        other => Cow::Borrowed(other),
    })
}

/// A `ValueError` raised in a model validator, which pydantic reports as a `ValidationError`.
fn value_error() -> ValidationError {
    ValidationError::new("value_error")
}

/// Keyword arguments as the value [`Fields::plain`] reads.
fn kwargs_value(kwargs: &Map<String, Value>) -> Value {
    Value::Object(kwargs.clone())
}

// ---------------------------------------------------------------------------
// Plan settings
// ---------------------------------------------------------------------------

/// A/C settings of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PlanClimate {
    /// Setpoint in °C.
    #[serde(serialize_with = "base::ser_float")]
    pub temperature: f64,
    /// Run length in minutes (10-30).
    pub duration: i64,
    /// Internal air circulation (`cycleMode` 2) instead of external.
    pub recirculation: bool,
}

impl PlanClimate {
    /// `PlanClimate(**kwargs)`: `temperature` and `duration` are required,
    /// other keywords are ignored.
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> Result<Self, ValidationError> {
        let values = kwargs_value(kwargs);
        let mut fields = Fields::plain(&values)?;
        let model = Self {
            temperature: fields.required("temperature", coerce::float),
            duration: fields.required("duration", coerce::int),
            recirculation: fields.get("recirculation", false, coerce::boolean),
        };
        fields.finish()?;
        Ok(model)
    }

    /// `model_dump(mode="json")`.
    pub fn model_dump(&self) -> Value {
        serde_json::to_value(self).expect("plan settings serialise to JSON")
    }
}

/// Front seat settings of a plan, `None` where the car lacks the feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct PlanSeats {
    /// Driver seat heating.
    pub driver_heat: Option<bool>,
    /// Driver seat ventilation.
    pub driver_ventilation: Option<bool>,
    /// Passenger seat heating.
    pub passenger_heat: Option<bool>,
    /// Passenger seat ventilation.
    pub passenger_ventilation: Option<bool>,
}

impl PlanSeats {
    /// `PlanSeats(**kwargs)`; other keywords are ignored.
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> Result<Self, ValidationError> {
        let values = kwargs_value(kwargs);
        let mut fields = Fields::plain(&values)?;
        let model = Self {
            driver_heat: fields.opt("driver_heat", coerce::boolean),
            driver_ventilation: fields.opt("driver_ventilation", coerce::boolean),
            passenger_heat: fields.opt("passenger_heat", coerce::boolean),
            passenger_ventilation: fields.opt("passenger_ventilation", coerce::boolean),
        };
        fields.finish()?;
        Ok(model)
    }

    /// Whether any seat is set to heat or ventilate.
    pub fn active(&self) -> bool {
        self.values().into_iter().any(|value| value == Some(true))
    }

    /// The four settings in declaration order.
    fn values(&self) -> [Option<bool>; 4] {
        [
            self.driver_heat,
            self.driver_ventilation,
            self.passenger_heat,
            self.passenger_ventilation,
        ]
    }

    /// `model_dump(mode="json")`.
    pub fn model_dump(&self) -> Value {
        serde_json::to_value(self).expect("plan settings serialise to JSON")
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// One remote command stored in a plan, for example `OPENAIR`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StandbyPlanCommand {
    /// Original API dict, or the keyword arguments of a command built here.
    pub raw: Map<String, Value>,
    /// The command, such as `OPENAIR`.
    pub command_type: String,
    /// The cloud's id of the stored command, as text.
    pub command_id: Option<String>,
    /// The command's parameters; an empty object when BYD sent none or
    /// something that is not an object.
    pub control_params_map: Map<String, Value>,
}

/// `_decode_map`: the decoded `controlParamsMap` when it is an object, else an empty one.
///
/// Only JSON object text (first non-whitespace byte `{`) that serde_json
/// cannot hold is refused as `Unsupported`; non-object JSON text (scalars,
/// arrays) is discarded to an empty map just as pyBYD discards it.
fn decode_map(value: &Value) -> Result<Map<String, Value>, ValidationError> {
    Ok(match json_or_raw(value, b'{')? {
        Cow::Borrowed(Value::Object(map)) => map.clone(),
        Cow::Owned(Value::Object(map)) => map,
        _ => Map::new(),
    })
}

/// The time-span code of a `duration` given as any JSON value: pyBYD looks
/// it up in a dict, so `10.0` finds 10 minutes, a list or an object is
/// unhashable (`TypeError`) and anything else is a `ValueError`.
fn time_span_of(duration: &Value) -> Result<i64, ValidationError> {
    if matches!(duration, Value::Array(_) | Value::Object(_)) {
        return Err(ValidationError::raised("TypeError", "unhashable"));
    }
    VALID_CLIMATE_DURATIONS
        .into_iter()
        .find(|minutes| py_equals_int(duration, *minutes))
        .and_then(|minutes| minutes_to_time_span(minutes).ok())
        .ok_or_else(|| ValidationError::raised("ValueError", "duration"))
}

impl StandbyPlanCommand {
    /// `cls(command_type=..., control_params_map=...)`.
    fn built(command_type: &str, control_params_map: Map<String, Value>) -> Self {
        let mut raw = Map::new();
        raw.insert("command_type".to_owned(), json!(command_type));
        raw.insert(
            "control_params_map".to_owned(),
            Value::Object(control_params_map.clone()),
        );
        Self {
            raw,
            command_type: command_type.to_owned(),
            command_id: None,
            control_params_map,
        }
    }

    /// The `OPENAIR` command of validated start parameters. `airSet`
    /// belongs to the direct start; plans do not send it.
    fn from_climate_params(params: &ClimateStartParams) -> Self {
        let mut map = params.to_control_params_map();
        map.shift_remove("airSet");
        Self::built(COMMAND_CLIMATE, map)
    }

    /// A/C at `temperature` °C for `duration` minutes, as the app saves it.
    ///
    /// `temp_offset` is the °C offset of the vehicle's scale
    /// ([`DEFAULT_TEMP_OFFSET_C`] unless the car says otherwise). A
    /// duration that is not 10, 15, 20, 25 or 30 minutes fails with a
    /// `ValueError`; a temperature outside the scale with a
    /// `ValidationError`.
    pub fn climate(
        temperature: f64,
        duration: i64,
        recirculation: bool,
        temp_offset: f64,
    ) -> Result<Self, ValidationError> {
        let time_span = minutes_to_time_span(duration)
            .map_err(|_| ValidationError::raised("ValueError", "duration"))?;
        let params = ClimateStartParams::new(ClimateStartArgs {
            temperature: Some(temperature),
            temp_offset,
            time_span: Some(time_span),
            cycle_mode: if recirculation { 2 } else { 1 },
            ..ClimateStartArgs::default()
        })?;
        Ok(Self::from_climate_params(&params))
    }

    /// [`Self::climate`] with the arguments as Python would take them:
    /// `temperature` and `temp_offset` are read as `ClimateStartParams`
    /// reads its fields (so `"21"` is 21 °C and null no setpoint),
    /// `duration` is looked up like a dict key and `recirculation` is truthy
    /// or not.
    pub fn climate_from_values(
        temperature: &Value,
        duration: &Value,
        recirculation: &Value,
        temp_offset: &Value,
    ) -> Result<Self, ValidationError> {
        let time_span = time_span_of(duration)?;
        let mut kwargs = Map::new();
        kwargs.insert("temperature".to_owned(), temperature.clone());
        kwargs.insert("temp_offset".to_owned(), temp_offset.clone());
        kwargs.insert("time_span".to_owned(), json!(time_span));
        let cycle_mode = if py_truthy(recirculation) { 2 } else { 1 };
        kwargs.insert("cycle_mode".to_owned(), json!(cycle_mode));
        let params = ClimateStartParams::from_kwargs(&kwargs)?;
        Ok(Self::from_climate_params(&params))
    }

    /// Front seat heat and ventilation, on or off. `None` sends `0` (no such feature).
    pub fn seats(settings: &PlanSeats) -> Self {
        let level = |setting: Option<bool>| match setting {
            None => 0,
            Some(true) => SEAT_ON,
            Some(false) => SEAT_OFF,
        };
        let params = SeatClimateParams::new(SeatClimateArgs {
            main_heat: level(settings.driver_heat),
            main_ventilation: level(settings.driver_ventilation),
            copilot_heat: level(settings.passenger_heat),
            copilot_ventilation: level(settings.passenger_ventilation),
            ..SeatClimateArgs::default()
        })
        .expect("seat levels are 0, 1 or 3");
        // Plans carry the seat levels only, and chairType 8 instead of a
        // single seat (it only shows up in plans, likely "all seats").
        let mut map = params.to_control_params_map();
        for excluded in ["chairType", "remoteMode", "steeringWheelHeatState"] {
            map.shift_remove(excluded);
        }
        map.insert("chairType".to_owned(), json!(8));
        Self::built(COMMAND_SEATS, map)
    }

    /// Steering wheel heat; plans use their own command type for it.
    pub fn steering_wheel(on: bool) -> Self {
        let mut map = Map::new();
        map.insert(
            "steeringWheelHeatState".to_owned(),
            json!(if on { SEAT_ON } else { SEAT_OFF }),
        );
        map.insert("chairType".to_owned(), json!(5));
        Self::built(COMMAND_STEERING_WHEEL, map)
    }
}

impl BydModel for StandbyPlanCommand {
    fn raw_mut(&mut self) -> Option<&mut Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            command_type: fields.get("command_type", String::new(), coerce::string),
            // _id_to_str: `str(value)`, whatever BYD sent.
            command_id: fields.get("command_id", None, |value| Ok(Some(py_str(value)))),
            control_params_map: fields.get("control_params_map", Map::new(), decode_map),
        };
        fields.finish()?;
        Ok(model)
    }
}

// ---------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------

/// How often a plan runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Repeat {
    /// Not scheduled.
    #[default]
    Off,
    /// Once, at a given time.
    Once,
    /// Every day at a given time.
    Daily,
    /// On chosen weekdays at a given time of day.
    Weekly,
}

impl Repeat {
    /// The Python literal: `"off"`, `"once"`, `"daily"` or `"weekly"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Once => "once",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
        }
    }

    /// `Literal["off", "once", "daily", "weekly"]`: only these exact strings pass.
    fn from_value(value: &Value) -> Result<Self, ValidationError> {
        match value.as_str() {
            Some("off") => Ok(Self::Off),
            Some("once") => Ok(Self::Once),
            Some("daily") => Ok(Self::Daily),
            Some("weekly") => Ok(Self::Weekly),
            _ => Err(ValidationError::new("literal_error")),
        }
    }
}

/// A Python `datetime` as [`StandbySchedule`]'s `at` holds it: timezone aware or naive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanTime {
    /// A datetime with a UTC offset.
    Aware(PyDateTime),
    /// A datetime without one. Only a schedule that does not use `at`
    /// (`off`, `weekly`) can hold it.
    Naive(NaiveDateTime),
}

impl PlanTime {
    /// The aware datetime, if this is one.
    pub fn aware(&self) -> Option<PyDateTime> {
        match self {
            Self::Aware(datetime) => Some(*datetime),
            Self::Naive(_) => None,
        }
    }

    /// The text pydantic writes for it in `model_dump(mode="json")`.
    pub fn to_json(&self) -> String {
        match self {
            Self::Aware(datetime) => datetime.to_json(),
            Self::Naive(local) => {
                let mut text = PyDateTime::utc(*local).to_json();
                text.truncate(text.len() - 1); // the "Z"
                text
            }
        }
    }
}

impl From<PyDateTime> for PlanTime {
    fn from(datetime: PyDateTime) -> Self {
        Self::Aware(datetime)
    }
}

/// When a plan runs, as sent to `save`.
///
/// Build one with [`off`](Self::off), [`once`](Self::once),
/// [`daily`](Self::daily) or [`weekly`](Self::weekly). `once` and `daily`
/// carry the run as an aware datetime, since the app sends those as
/// `executeTime`; `weekly` sends `startTime` and `weeklyDays` and the cloud
/// works out the next run.
///
/// A value of this type has passed pyBYD's validation: `once` and `daily`
/// have an aware `at`, `weekly` has an `"HH:MM"` start and days 0-6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandbySchedule {
    repeat: Repeat,
    at: Option<PlanTime>,
    start_time: String,
    weekly_days: Vec<i64>,
    /// See [`Self::with_daily_after`]. Not one of pyBYD's fields.
    daily_after: Option<PyDateTime>,
    /// See [`Self::daily_in_zone`]: `at` with its time zone.
    zoned: Option<wallclock::DateTime>,
}

/// `_HHMM.fullmatch(text)`: `([01]\d|2[0-3]):[0-5]\d`. Python's `\d`
/// matches any Unicode decimal digit, so `"1٢:3٤"` passes.
fn is_hhmm(text: &str) -> bool {
    let mut chars = text.chars();
    let (Some(h1), Some(h2), Some(colon), Some(m1), Some(m2), None) = (
        chars.next(),
        chars.next(),
        chars.next(),
        chars.next(),
        chars.next(),
        chars.next(),
    ) else {
        return false;
    };
    let digit = |c: char| py_decimal(c).is_some();
    let hour = match h1 {
        '0' | '1' => digit(h2),
        '2' => ('0'..='3').contains(&h2),
        _ => false,
    };
    hour && colon == ':' && ('0'..='5').contains(&m1) && digit(m2)
}

impl StandbySchedule {
    /// `StandbySchedule(repeat=..., at=..., start_time=..., weekly_days=...)`
    /// with typed arguments, checked as pyBYD's `_check` does.
    pub fn new(
        repeat: Repeat,
        at: Option<PlanTime>,
        start_time: &str,
        weekly_days: &[i64],
    ) -> Result<Self, ValidationError> {
        let schedule = Self {
            repeat,
            at,
            start_time: start_time.to_owned(),
            weekly_days: weekly_days.to_vec(),
            daily_after: None,
            zoned: None,
        };
        schedule.check()?;
        Ok(schedule)
    }

    /// `StandbySchedule(**kwargs)`: `repeat` is required; `at` is read as
    /// pydantic reads a `datetime` (ISO text, a date, or a Unix timestamp
    /// in seconds or milliseconds, which is UTC); other keywords are ignored.
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> Result<Self, ValidationError> {
        let values = kwargs_value(kwargs);
        let mut fields = Fields::plain(&values)?;
        let schedule = Self {
            repeat: fields.required("repeat", Repeat::from_value),
            at: fields.opt("at", pydantic_datetime),
            start_time: fields.get("start_time", String::new(), coerce::string),
            weekly_days: fields.get("weekly_days", Vec::new(), |value| {
                coerce::list(value, coerce::int)
            }),
            daily_after: None,
            zoned: None,
        };
        fields.finish()?;
        schedule.check()?;
        Ok(schedule)
    }

    /// `_check`, the model's after-validator.
    fn check(&self) -> Result<(), ValidationError> {
        match self.repeat {
            Repeat::Once | Repeat::Daily => {
                if !matches!(self.at, Some(PlanTime::Aware(_))) {
                    return Err(value_error());
                }
            }
            Repeat::Weekly => {
                if !is_hhmm(&self.start_time) {
                    return Err(value_error());
                }
                if self.weekly_days.is_empty()
                    || self.weekly_days.iter().any(|day| !(0..=6).contains(day))
                {
                    return Err(value_error());
                }
            }
            Repeat::Off => {}
        }
        Ok(())
    }

    /// Not scheduled.
    pub fn off() -> Self {
        Self {
            repeat: Repeat::Off,
            at: None,
            start_time: String::new(),
            weekly_days: Vec::new(),
            daily_after: None,
            zoned: None,
        }
    }

    /// Run once at `at`.
    pub fn once(at: PyDateTime) -> Self {
        Self {
            repeat: Repeat::Once,
            at: Some(PlanTime::Aware(at)),
            ..Self::off()
        }
    }

    /// Run every day at `at`'s time, starting at `at`.
    ///
    /// The app sends the next occurrence of the time, so `at` should be in
    /// the future. A past one is moved forward when it is saved, 24 hours
    /// at a time: `at` carries a fixed UTC offset, not a time zone, so
    /// across a clock change the time of day shifts by the hour. For a
    /// time of day that stays put, use
    /// [`daily_in_zone`](Self::daily_in_zone).
    pub fn daily(at: PyDateTime) -> Self {
        Self {
            repeat: Repeat::Daily,
            at: Some(PlanTime::Aware(at)),
            ..Self::off()
        }
    }

    /// Run every day at `at`'s time of day in `at`'s time zone, starting
    /// at `at`: pyBYD's `StandbySchedule.daily` given a datetime with a
    /// `ZoneInfo`.
    ///
    /// A time that has passed when the plan is saved is moved forward by
    /// calendar days in that zone, as Python moves it, so 08:00 stays
    /// 08:00 when the clocks change.
    pub fn daily_in_zone(at: wallclock::DateTime) -> Self {
        Self {
            repeat: Repeat::Daily,
            at: Some(PlanTime::Aware(PyDateTime::from(&at))),
            zoned: Some(at),
            ..Self::off()
        }
    }

    /// [`at`](Self::at) with its time zone, for a schedule made by
    /// [`daily_in_zone`](Self::daily_in_zone).
    pub fn zoned_at(&self) -> Option<&wallclock::DateTime> {
        self.zoned.as_ref()
    }

    /// Names the run after `at` of a daily schedule, for a caller that
    /// knows the next day's run but has no [`wallclock::DateTime`] to give
    /// [`daily_in_zone`](Self::daily_in_zone) (the Python package, whose
    /// callers may use any `tzinfo`).
    ///
    /// pyBYD moves a daily time that has passed on by calendar days in the
    /// datetime's own time zone, so 08:00 stays 08:00 when the clocks
    /// change. A [`PyDateTime`] has only an offset. With `following` (the
    /// same time of day one calendar day after `at`, worked out by a
    /// caller that knows the zone), a save that finds `at` has passed
    /// sends `following` instead of `at` plus 24 hours. Only once
    /// `following` has passed too is it moved on by 24 hours at a time.
    ///
    /// Ignored unless the schedule is daily and `following` is after `at`.
    /// It is not part of [`model_dump`](Self::model_dump).
    #[must_use]
    pub fn with_daily_after(mut self, following: PyDateTime) -> Self {
        self.daily_after = Some(following);
        self
    }

    /// The run after [`at`](Self::at), if [`with_daily_after`](Self::with_daily_after) gave one.
    pub fn daily_after(&self) -> Option<PyDateTime> {
        self.daily_after
    }

    /// Run on `weekly_days` (`0` = Monday … `6` = Sunday) at `start_time`
    /// (`"HH:MM"`). The days are sorted and duplicates dropped.
    pub fn weekly(start_time: &str, weekly_days: &[i64]) -> Result<Self, ValidationError> {
        let days: BTreeSet<i64> = weekly_days.iter().copied().collect();
        let days: Vec<i64> = days.into_iter().collect();
        Self::new(Repeat::Weekly, None, start_time, &days)
    }

    /// How often the plan runs.
    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    /// Run time for `once` and `daily` (always aware for those).
    pub fn at(&self) -> Option<PlanTime> {
        self.at
    }

    /// `"HH:MM"` for `weekly`.
    pub fn start_time(&self) -> &str {
        &self.start_time
    }

    /// `0` = Monday … `6` = Sunday, for `weekly`.
    pub fn weekly_days(&self) -> &[i64] {
        &self.weekly_days
    }

    /// `model_dump(mode="json")`.
    pub fn model_dump(&self) -> Value {
        json!({
            "repeat": self.repeat.as_str(),
            "at": self.at.map(|at| at.to_json()),
            "start_time": self.start_time,
            "weekly_days": self.weekly_days,
        })
    }
}

// ---------------------------------------------------------------------------
// pydantic's datetime
// ---------------------------------------------------------------------------

/// pydantic reads a number up to this size (either sign) as seconds, a larger one as milliseconds.
const MS_WATERSHED: i64 = 20_000_000_000;
/// 0001-01-01T00:00:00, the first second a Python `datetime` holds.
const MIN_DATETIME_SECONDS: i64 = -62_135_596_800;
/// 9999-12-31T23:59:59, the last one.
const MAX_DATETIME_SECONDS: i64 = 253_402_300_799;

/// The naive UTC datetime `seconds` and `micros` after the epoch, if Python can hold it.
fn from_seconds(seconds: i64, micros: i64) -> Option<NaiveDateTime> {
    let seconds = seconds.checked_add(micros.div_euclid(1_000_000))?;
    let micros = micros.rem_euclid(1_000_000);
    if !(MIN_DATETIME_SECONDS..=MAX_DATETIME_SECONDS).contains(&seconds) {
        return None;
    }
    let nanos = u32::try_from(micros * 1000).ok()?;
    chrono::DateTime::from_timestamp(seconds, nanos).map(|datetime| datetime.naive_utc())
}

/// An integer Unix timestamp: seconds, or milliseconds past the watershed.
fn from_int_timestamp(timestamp: i64) -> Option<NaiveDateTime> {
    let size = timestamp.checked_abs()?;
    if size <= MS_WATERSHED {
        from_seconds(timestamp, 0)
    } else {
        from_seconds(
            timestamp.div_euclid(1000),
            timestamp.rem_euclid(1000) * 1000,
        )
    }
}

/// A float Unix timestamp: the whole part is read as an integer one, and the
/// fraction is rounded to microseconds (of a second, or of a millisecond
/// past the watershed, where it is rounded to whole microseconds).
fn from_float_timestamp(timestamp: f64) -> Option<NaiveDateTime> {
    if !timestamp.is_finite() {
        return None;
    }
    let whole = timestamp.floor();
    if whole.abs() >= 9.0e18 {
        return None;
    }
    let fraction = timestamp - whole;
    let whole = whole as i64;
    if timestamp.abs() > MS_WATERSHED as f64 {
        from_seconds(
            whole.div_euclid(1000),
            whole.rem_euclid(1000) * 1000 + (fraction * 1000.0).round() as i64,
        )
    } else {
        from_seconds(whole, (fraction * 1_000_000.0).round() as i64)
    }
}

/// Exactly `count` ASCII digits at `position`, as a number.
fn digits(bytes: &[u8], position: usize, count: usize) -> Option<u32> {
    let slice = bytes.get(position..position + count)?;
    slice.iter().try_fold(0, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u32::from(byte - b'0'))
    })
}

/// The UTF-8 bytes of U+2212 MINUS SIGN, which ISO 8601 allows for a negative offset.
const MINUS_SIGN: &[u8] = "\u{2212}".as_bytes();

/// pydantic's RFC 3339 reader (the `speedate` crate): `YYYY-MM-DD`, then
/// `T`, `t`, a space or `_`, then `HH:MM[:SS[.ffffff]]` and an optional
/// `Z`, `z` or `±HH[:]MM` offset. Fraction digits past six are dropped. A
/// date alone is midnight. Returns the wall-clock time and the offset in
/// seconds, or `None` for text that is not such a datetime.
fn rfc3339(bytes: &[u8]) -> Option<(NaiveDateTime, Option<i64>)> {
    let (year, month, day) = (
        digits(bytes, 0, 4)?,
        digits(bytes, 5, 2)?,
        digits(bytes, 8, 2)?,
    );
    if bytes[4] != b'-' || bytes[7] != b'-' || year == 0 {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)?;
    if bytes.len() == 10 {
        return Some((date.and_hms_opt(0, 0, 0)?, None));
    }
    if !matches!(bytes[10], b'T' | b't' | b' ' | b'_') {
        return None;
    }
    let (hour, minute) = (digits(bytes, 11, 2)?, digits(bytes, 14, 2)?);
    if bytes[13] != b':' {
        return None;
    }
    let mut position = 16;
    let (mut second, mut micros) = (0, 0);
    if bytes.get(position) == Some(&b':') {
        second = digits(bytes, position + 1, 2)?;
        position += 3;
        if matches!(bytes.get(position), Some(b'.' | b',')) {
            position += 1;
            let mut count = 0;
            while let Some(byte) = bytes.get(position).filter(|byte| byte.is_ascii_digit()) {
                if count < 6 {
                    micros = micros * 10 + u32::from(byte - b'0');
                }
                count += 1;
                position += 1;
            }
            if count == 0 {
                return None;
            }
            if count < 6 {
                micros *= 10_u32.pow(6 - count);
            }
        }
    }
    // chrono reads second 60 as a leap second; pydantic refuses it.
    if second > 59 {
        return None;
    }
    let local = date.and_hms_micro_opt(hour, minute, second, micros)?;
    let Some(&marker) = bytes.get(position) else {
        return Some((local, None));
    };
    position += 1;
    let offset = if matches!(marker, b'Z' | b'z') {
        0
    } else {
        let sign = match marker {
            b'+' => 1,
            b'-' => -1,
            _ if bytes[position - 1..].starts_with(MINUS_SIGN) => {
                position += MINUS_SIGN.len() - 1;
                -1
            }
            _ => return None,
        };
        let hours = digits(bytes, position, 2)?;
        position += 2;
        if bytes.get(position) == Some(&b':') {
            position += 1;
        }
        let minutes = digits(bytes, position, 2)?;
        position += 2;
        let seconds = i64::from(hours) * 3600 + i64::from(minutes) * 60;
        if minutes > 59 || seconds >= 86_400 {
            return None;
        }
        sign * seconds
    };
    (position == bytes.len()).then_some((local, Some(offset)))
}

/// Text that is a Unix timestamp to pydantic: `[+-]digits`, or a decimal
/// with a point (and then an exponent is allowed too).
fn text_timestamp(text: &str) -> Option<NaiveDateTime> {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned.is_empty() {
        return None;
    }
    if unsigned.bytes().all(|byte| byte.is_ascii_digit()) {
        return from_int_timestamp(text.parse().ok()?);
    }
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (unsigned, None),
    };
    let (whole, fraction) = mantissa.split_once('.')?;
    let all_digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if !all_digits(whole) || !all_digits(fraction) || (whole.is_empty() && fraction.is_empty()) {
        return None;
    }
    if let Some(exponent) = exponent {
        let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if exponent.is_empty() || !all_digits(exponent) {
            return None;
        }
    }
    from_float_timestamp(text.parse().ok()?)
}

/// A `datetime` field in pydantic's lax mode, for a JSON value: ISO text
/// (naive without an offset), a date (midnight, naive), or a Unix timestamp
/// as a number or numeric text (UTC; seconds up to 2e10 either way,
/// milliseconds beyond). Booleans, lists and objects are refused.
pub fn pydantic_datetime(value: &Value) -> Result<PlanTime, ValidationError> {
    let utc = |local: Option<NaiveDateTime>| {
        local
            .map(|local| PlanTime::Aware(PyDateTime::utc(local)))
            .ok_or_else(|| ValidationError::new("datetime_parsing"))
    };
    match value {
        Value::Number(number) => match (number.as_i64(), number.as_u64()) {
            (Some(int), _) => utc(from_int_timestamp(int)),
            (None, Some(_)) => utc(None),
            _ => utc(number.as_f64().and_then(from_float_timestamp)),
        },
        Value::String(text) => {
            let bytes = text.as_bytes();
            if let Some((local, offset)) = (bytes.len() >= 10).then(|| rfc3339(bytes)).flatten() {
                return Ok(match offset {
                    Some(seconds) => PlanTime::Aware(PyDateTime {
                        local,
                        offset_us: seconds * 1_000_000,
                    }),
                    None => PlanTime::Naive(local),
                });
            }
            text_timestamp(text)
                .map(|local| PlanTime::Aware(PyDateTime::utc(local)))
                .ok_or_else(|| ValidationError::new("datetime_from_date_parsing"))
        }
        _ => Err(ValidationError::new("datetime_type")),
    }
}

// ---------------------------------------------------------------------------
// Datetime arithmetic for the save payload
// ---------------------------------------------------------------------------

/// Microseconds from the Unix epoch to `at`, exactly: what Python's
/// `at - datetime(1970, 1, 1, tzinfo=UTC)` holds.
pub fn epoch_micros(at: &PyDateTime) -> i64 {
    at.local.and_utc().timestamp_micros() - at.offset_us
}

/// `at + timedelta(days=days)`: the same wall-clock time and offset, `days`
/// later. `None` where Python raises `OverflowError` (past year 9999 or
/// before year 1).
pub fn add_days(at: &PyDateTime, days: i64) -> Option<PyDateTime> {
    let local = at.local.checked_add_signed(Duration::try_days(days)?)?;
    let seconds = local.and_utc().timestamp();
    (MIN_DATETIME_SECONDS..=MAX_DATETIME_SECONDS)
        .contains(&seconds)
        .then_some(PyDateTime {
            local,
            offset_us: at.offset_us,
        })
}

/// The UTC datetime `millis` after the Unix epoch, or `None` outside
/// Python's years 1 to 9999.
pub fn from_epoch_millis(millis: i64) -> Option<PyDateTime> {
    from_seconds(millis.div_euclid(1000), millis.rem_euclid(1000) * 1000).map(PyDateTime::utc)
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// A preconditioning plan (one `list` entry).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StandbyPlan {
    /// Original API dict (with `controlParamsList` filled from
    /// `controlParams` when it was missing).
    pub raw: Map<String, Value>,
    /// Plan id, kept as a string since it is a 64-bit number.
    pub id: String,
    /// The plan's name.
    pub plan_name: String,
    /// [`PLAN_TYPE_PRESET`] or [`PLAN_TYPE_USER`].
    pub plan_type: Option<i64>,
    /// `1` for the default plan.
    pub default_plan: Option<i64>,
    /// `"0"` = once, `"1"` = every day, `"2"` = weekly.
    pub execute_type: Option<String>,
    /// Weekly start as `"HH:MM"`, empty when not scheduled weekly.
    pub start_time: String,
    /// Days the plan runs, `0` = Monday … `6` = Sunday.
    pub weekly_days: Vec<i64>,
    /// Next run, `None` when not scheduled.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub execute_time: Option<PyDateTime>,
    /// The run time BYD records next to `executeTime`.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub rec_execute_time: Option<PyDateTime>,
    /// The time zone the plan was saved in.
    pub time_zone: Option<String>,
    /// What the plan does.
    pub commands: Vec<StandbyPlanCommand>,
}

/// `StandbyPlan._commands_from_control_params`: falls back to the
/// `controlParams` string when `controlParamsList` is absent or empty.
///
/// Only JSON array text (first non-whitespace byte `[`) that serde_json cannot
/// hold is refused as `Unsupported`; non-array JSON text (scalars, objects)
/// leaves `values` unchanged just as pyBYD leaves it unchanged.
fn commands_from_control_params(values: &Value) -> Result<Cow<'_, Value>, ValidationError> {
    let Value::Object(entries) = values else {
        return Ok(Cow::Borrowed(values));
    };
    if entries.get("controlParamsList").is_some_and(py_truthy) {
        return Ok(Cow::Borrowed(values));
    }
    let Some(control_params) = entries.get("controlParams") else {
        return Ok(Cow::Borrowed(values));
    };
    Ok(match json_or_raw(control_params, b'[')?.into_owned() {
        decoded @ Value::Array(_) => {
            let mut merged = entries.clone();
            merged.insert("controlParamsList".to_owned(), decoded);
            Cow::Owned(Value::Object(merged))
        }
        _ => Cow::Borrowed(values),
    })
}

/// `StandbyPlan._parse_days`: BYD sends the days as `"4,5,6"`. Each part
/// that `str.isdigit()` accepts once stripped is read with `int()`, the rest
/// are skipped; the result is sorted without duplicates. A list is
/// validated as it is.
fn parse_days(value: &Value) -> Result<Vec<i64>, ValidationError> {
    let Value::String(text) = value else {
        return coerce::list(value, coerce::int);
    };
    let mut days = BTreeSet::new();
    let mut too_large = false;
    for part in text.split(',') {
        let part = py_strip(part);
        if part.is_empty() || !part.chars().all(py_is_digit) {
            continue;
        }
        // `isdigit()` also accepts superscripts and the like, which `int()` refuses.
        let mut day: i64 = 0;
        let mut fits = true;
        for c in part.chars() {
            let digit = py_decimal(c).ok_or_else(value_error)?;
            match day
                .checked_mul(10)
                .and_then(|day| day.checked_add(i64::from(digit)))
            {
                Some(next) => day = next,
                None => fits = false,
            }
        }
        if fits {
            days.insert(day);
        } else {
            too_large = true;
        }
    }
    if too_large {
        return Err(ValidationError::unsupported_int());
    }
    Ok(days.into_iter().collect())
}

/// `_COMMAND_TO_SEAT.get(params.get(key, 0))`: 1 and 2 are on, 3 is off,
/// anything else unknown. pyBYD looks the value up in a dict, so a list or
/// an object fails with `TypeError` (unhashable).
fn seat_setting(params: &Map<String, Value>, key: &str) -> Result<Option<bool>, ValidationError> {
    match params.get(key) {
        None => Ok(None),
        Some(Value::Array(_) | Value::Object(_)) => {
            Err(ValidationError::raised("TypeError", "unhashable"))
        }
        Some(value) if py_equals_int(value, 1) || py_equals_int(value, 2) => Ok(Some(true)),
        Some(value) if py_equals_int(value, 3) => Ok(Some(false)),
        Some(_) => Ok(None),
    }
}

/// `isinstance(value, int)` for a JSON value: an integer or a boolean, as a float.
fn python_int_as_float(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
        Value::Number(number) => match (number.as_i64(), number.as_u64()) {
            (Some(int), _) => Some(int as f64),
            (None, Some(int)) => Some(int as f64),
            _ => None,
        },
        _ => None,
    }
}

impl StandbyPlan {
    /// The plan's command of `command_type`, if it has one.
    pub fn command(&self, command_type: &str) -> Option<&StandbyPlanCommand> {
        self.commands
            .iter()
            .find(|command| command.command_type == command_type)
    }

    /// The plan's A/C settings, `None` when it does not run the A/C.
    ///
    /// `temp_offset` is the °C offset of the vehicle's scale
    /// ([`DEFAULT_TEMP_OFFSET_C`] by default in pyBYD). A setpoint that is
    /// not an integer reads as the lowest one, a duration code that is
    /// unknown as 10 minutes.
    pub fn climate(&self, temp_offset: f64) -> Option<PlanClimate> {
        let params = &self.command(COMMAND_CLIMATE)?.control_params_map;
        let temperature = match python_int_as_float(params.get("mainSettingTemp")) {
            Some(scale) => scale + temp_offset,
            None => temp_offset + 1.0,
        };
        let duration = match params.get("timeSpan") {
            Some(Value::Bool(flag)) => constants::time_span_to_minutes(i64::from(*flag)),
            Some(Value::Number(number)) => {
                number.as_i64().and_then(constants::time_span_to_minutes)
            }
            _ => None,
        };
        Some(PlanClimate {
            temperature,
            duration: duration.unwrap_or(10),
            recirculation: params
                .get("cycleMode")
                .is_some_and(|mode| py_equals_int(mode, 2)),
        })
    }

    /// The plan's front seat settings, `None` when it has none.
    ///
    /// Fails with pyBYD's `TypeError` when a seat value in the command is
    /// a list or an object.
    pub fn seats(&self) -> Result<Option<PlanSeats>, ValidationError> {
        let Some(command) = self.command(COMMAND_SEATS) else {
            return Ok(None);
        };
        let params = &command.control_params_map;
        Ok(Some(PlanSeats {
            driver_heat: seat_setting(params, "mainHeat")?,
            driver_ventilation: seat_setting(params, "mainVentilation")?,
            passenger_heat: seat_setting(params, "copilotHeat")?,
            passenger_ventilation: seat_setting(params, "copilotVentilation")?,
        }))
    }

    /// Whether the plan heats the steering wheel.
    pub fn steering_wheel_heat(&self) -> bool {
        self.command(COMMAND_STEERING_WHEEL).is_some_and(|command| {
            command
                .control_params_map
                .get("steeringWheelHeatState")
                .is_some_and(|state| py_equals_int(state, SEAT_ON))
        })
    }

    /// The built-in plan, which cannot be deleted.
    pub fn preset(&self) -> bool {
        self.plan_type == Some(PLAN_TYPE_PRESET)
    }

    /// Whether the plan is set to run.
    ///
    /// Weekly needs both a time and days; days without a time (or the
    /// other way round) is saved but never runs.
    pub fn scheduled(&self) -> bool {
        if self.execute_type.as_deref() == Some(EXECUTE_WEEKLY) {
            !self.start_time.is_empty() && !self.weekly_days.is_empty()
        } else {
            self.execute_time.is_some()
        }
    }

    /// The plan's current schedule, for saving it back unchanged.
    ///
    /// Fails with pyBYD's `ValidationError` when the plan is weekly with
    /// a start that is not `"HH:MM"` or a day outside 0-6.
    pub fn schedule(&self) -> Result<StandbySchedule, ValidationError> {
        if !self.scheduled() {
            return Ok(StandbySchedule::off());
        }
        if self.execute_type.as_deref() == Some(EXECUTE_WEEKLY) {
            return StandbySchedule::weekly(&self.start_time, &self.weekly_days);
        }
        let Some(at) = self.execute_time else {
            return Ok(StandbySchedule::off());
        };
        Ok(if self.execute_type.as_deref() == Some(EXECUTE_DAILY) {
            StandbySchedule::daily(at)
        } else {
            StandbySchedule::once(at)
        })
    }
}

/// A property that raises in pyBYD, as the property map holds it: `{"error": "<class>"}`.
fn property_error(error: &ValidationError) -> Value {
    json!({ "error": error.exception })
}

impl BydModel for StandbyPlan {
    fn raw_mut(&mut self) -> Option<&mut Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        let values = commands_from_control_params(raw)?;
        static PLAN: Plan = Plan::new(&[("controlParamsList", "commands")]);
        let mut fields = Fields::planned(&PLAN, &values, &[])?;
        let model = Self {
            raw: fields.raw(),
            // _id_to_str and _type_to_str: `str(value)`, whatever BYD sent.
            id: fields.get("id", String::new(), |value| Ok(py_str(value))),
            plan_name: fields.get("plan_name", String::new(), coerce::string),
            plan_type: fields.opt("plan_type", coerce::int),
            default_plan: fields.opt("default_plan", coerce::int),
            execute_type: fields.get("execute_type", None, |value| Ok(Some(py_str(value)))),
            start_time: fields.get("start_time", String::new(), coerce::string),
            weekly_days: fields.get("weekly_days", Vec::new(), parse_days),
            execute_time: fields.get("execute_time", None, base::byd_timestamp),
            rec_execute_time: fields.get("rec_execute_time", None, base::byd_timestamp),
            time_zone: fields.opt("time_zone", coerce::string),
            commands: fields.get("commands", Vec::new(), |value| {
                coerce::list(value, |item| StandbyPlanCommand::parse(item, context))
            }),
        };
        fields.finish()?;
        Ok(model)
    }

    /// `seats` and `schedule` raise in pyBYD for some plans (see
    /// [`StandbyPlan::seats`] and [`StandbyPlan::schedule`]); such a
    /// property is `{"error": "<exception class>"}` here.
    fn properties(&self) -> Map<String, Value> {
        let seats = match self.seats() {
            Ok(seats) => json!(seats),
            Err(error) => property_error(&error),
        };
        let schedule = match self.schedule() {
            Ok(schedule) => schedule.model_dump(),
            Err(error) => property_error(&error),
        };
        base::properties([
            ("seats", seats),
            ("steering_wheel_heat", json!(self.steering_wheel_heat())),
            ("preset", json!(self.preset())),
            ("scheduled", json!(self.scheduled())),
            ("schedule", schedule),
        ])
    }
}

/// Preconditioning plans for a vehicle (`list` response).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StandbyPlanList {
    /// Original dict: `{"vin": ..., "data": [...]}`.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// The vehicle's plans.
    pub plans: Vec<StandbyPlan>,
    /// The cloud's reply to the save that returned this list, if any.
    ///
    /// Worth showing: a daily or weekly run less than 20 min ahead is saved
    /// but skips today, and this is the only place that says so (#91).
    ///
    /// Normally text. pyBYD copies the save's message in without checking
    /// it, so a message of another type (an object, a number) is kept as the
    /// JSON BYD sent, as pyBYD's dump shows it.
    pub message: Option<Value>,
}

impl BydModel for StandbyPlanList {
    fn raw_mut(&mut self) -> Option<&mut Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[("data", "plans")]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            plans: fields.get("plans", Vec::new(), |value| {
                coerce::list(value, |item| StandbyPlan::parse(item, context))
            }),
            message: fields.opt("message", coerce::string).map(Value::String),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Result of running a plan now (`send`, then `send-result`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StandbyPlanRunResult {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// Serial of the run request.
    pub request_serial: Option<String>,
    /// `1` = in progress, `2` = done, other values = failure.
    pub res: Option<i64>,
    /// Locale-dependent message; branch on [`Self::res`], not on this.
    pub message: Option<String>,
}

impl BydModel for StandbyPlanRunResult {
    fn raw_mut(&mut self) -> Option<&mut Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            request_serial: fields.opt("request_serial", coerce::string),
            res: fields.opt("res", coerce::int),
            message: fields.opt("message", coerce::string),
        };
        fields.finish()?;
        Ok(model)
    }
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

fn kwargs_arg<'a>(args: &Args<'a>, key: &str) -> Result<&'a Map<String, Value>, ParityError> {
    args.value(key)
        .as_object()
        .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be an object")))
}

fn dump_answer(dump: Value) -> Value {
    json!({ "dump": dump })
}

/// `StandbySchedule.weekly(start_time, weekly_days)` for a JSON `start_time`
/// (anything but text is pydantic's `string_type` error) and a list of integers.
fn weekly_call(args: &Args<'_>) -> Outcome {
    let days = args
        .value("weekly_days")
        .as_array()
        .and_then(|days| days.iter().map(Value::as_i64).collect::<Option<Vec<_>>>())
        .ok_or_else(|| ParityError::bad_arguments("weekly_days must be a list of integers"))?;
    let Value::String(start_time) = args.value("start_time") else {
        return Err(ValidationError::new("string_type").into());
    };
    Ok(dump_answer(
        StandbySchedule::weekly(start_time, &days)?.model_dump(),
    ))
}

/// `StandbySchedule.once(at)` and `.daily(at)` for a JSON `at`, read as the constructor reads it.
fn timed_call(repeat: Repeat, args: &Args<'_>) -> Outcome {
    let mut kwargs = Map::new();
    kwargs.insert("repeat".to_owned(), json!(repeat.as_str()));
    kwargs.insert("at".to_owned(), args.value("at").clone());
    Ok(dump_answer(
        StandbySchedule::from_kwargs(&kwargs)?.model_dump(),
    ))
}

fn plan_arg(args: &Args<'_>) -> Result<StandbyPlan, ParityError> {
    Ok(StandbyPlan::parse(args.value("raw"), &Value::Null)?)
}

fn climate_call(args: &Args<'_>) -> Outcome {
    let plan = plan_arg(args)?;
    let temp_offset = match args.value("temp_offset") {
        Value::Null => DEFAULT_TEMP_OFFSET_C,
        value => value
            .as_f64()
            .ok_or_else(|| ParityError::bad_arguments("temp_offset must be a number"))?,
    };
    Ok(json!(
        plan.climate(temp_offset)
            .map(|climate| climate.model_dump())
    ))
}

fn constants_call() -> Value {
    json!({
        "COMMAND_CLIMATE": COMMAND_CLIMATE,
        "COMMAND_SEATS": COMMAND_SEATS,
        "COMMAND_STEERING_WHEEL": COMMAND_STEERING_WHEEL,
        "EXECUTE_ONCE": EXECUTE_ONCE,
        "EXECUTE_DAILY": EXECUTE_DAILY,
        "EXECUTE_WEEKLY": EXECUTE_WEEKLY,
        "PLAN_TYPE_PRESET": PLAN_TYPE_PRESET,
        "PLAN_TYPE_USER": PLAN_TYPE_USER,
    })
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "models.standby.constants" => Ok(constants_call()),
        "models.standby.PlanClimate" => kwargs_arg(&args, "kwargs")
            .and_then(|kwargs| Ok(dump_answer(PlanClimate::from_kwargs(kwargs)?.model_dump()))),
        "models.standby.PlanSeats" => kwargs_arg(&args, "kwargs").and_then(|kwargs| {
            let seats = PlanSeats::from_kwargs(kwargs)?;
            Ok(json!({
                "dump": seats.model_dump(),
                "properties": {"active": seats.active()},
            }))
        }),
        "models.standby.StandbySchedule" => kwargs_arg(&args, "kwargs").and_then(|kwargs| {
            Ok(dump_answer(
                StandbySchedule::from_kwargs(kwargs)?.model_dump(),
            ))
        }),
        "models.standby.StandbySchedule.off" => {
            Ok(dump_answer(StandbySchedule::off().model_dump()))
        }
        "models.standby.StandbySchedule.once" => timed_call(Repeat::Once, &args),
        "models.standby.StandbySchedule.daily" => timed_call(Repeat::Daily, &args),
        "models.standby.StandbySchedule.weekly" => weekly_call(&args),
        "models.standby.StandbyPlanCommand" => base::model_call::<StandbyPlanCommand>(args.0),
        "models.standby.StandbyPlanCommand.climate" => StandbyPlanCommand::climate_from_values(
            args.value("temperature"),
            args.value("duration"),
            args.value("recirculation"),
            args.value("temp_offset"),
        )
        .map(|command| dump_answer(command.dump()))
        .map_err(ParityError::from),
        "models.standby.StandbyPlanCommand.seats" => {
            kwargs_arg(&args, "settings").and_then(|kwargs| {
                let settings = PlanSeats::from_kwargs(kwargs)?;
                Ok(dump_answer(StandbyPlanCommand::seats(&settings).dump()))
            })
        }
        "models.standby.StandbyPlanCommand.steering_wheel" => Ok(dump_answer(
            StandbyPlanCommand::steering_wheel(py_truthy(args.value("on"))).dump(),
        )),
        "models.standby.StandbyPlan" => base::model_call::<StandbyPlan>(args.0),
        "models.standby.StandbyPlan.command" => plan_arg(&args).map(|plan| {
            let found = args
                .value("command_type")
                .as_str()
                .and_then(|command_type| plan.command(command_type));
            json!(found.map(|command| command.dump()))
        }),
        "models.standby.StandbyPlan.climate" => climate_call(&args),
        "models.standby.StandbyPlanList" => base::model_call::<StandbyPlanList>(args.0),
        "models.standby.StandbyPlanRunResult" => base::model_call::<StandbyPlanRunResult>(args.0),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aware(text: &str) -> PyDateTime {
        pydantic_datetime(&json!(text)).unwrap().aware().unwrap()
    }

    fn kwargs(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn plan_text_serde_cannot_hold_is_refused() {
        // pyBYD keeps `NaN` and the 21-digit integer inside a decoded dict;
        // decoding them as an empty map or a float would save the plan back without them.
        for text in [
            r#"{"mainSettingTemp":5,"extra":NaN}"#,
            r#"{"mainSettingTemp":100000000000000000000}"#,
            r#" {"a": NaN}"#,
            r#"{"a": [1e400]}"#,
        ] {
            let command = json!({"commandType": "OPENAIR", "controlParamsMap": text});
            let error = StandbyPlanCommand::parse(&command, &Value::Null).unwrap_err();
            assert_eq!(error.exception, "Unsupported", "{text}");
            let listed = json!({"controlParamsList": [command]});
            let error = StandbyPlan::parse(&listed, &Value::Null).unwrap_err();
            assert_eq!(error.exception, "Unsupported", "{text}");
            let packed = json!({"controlParams": format!(r#"[{{"commandType":"OPENAIR","controlParamsMap":{text}}}]"#)});
            let error = StandbyPlan::parse(&packed, &Value::Null).unwrap_err();
            assert_eq!(error.exception, "Unsupported", "{text}");
        }

        // `controlParamsMap` text that is not a JSON object (scalars, arrays,
        // or invalid JSON) is discarded by pyBYD to an empty map, so openbyd
        // matches pyBYD even when serde_json cannot hold the decoded scalar/array.
        let deep_array = format!("{}1{}", "[".repeat(128), "]".repeat(128));
        for text in [
            " -Infinity",
            "NaN",
            "9503770829.25e400",
            "100000000000000000000",
            "[NaN]",
            r#"  [1, {"a": Infinity}]"#,
            "NaN and more",
            deep_array.as_str(),
        ] {
            let command = json!({"commandType": "OPENAIR", "controlParamsMap": text});
            let command = StandbyPlanCommand::parse(&command, &Value::Null).unwrap();
            assert!(command.control_params_map.is_empty(), "{text}");
        }

        // `controlParams` text that is not a JSON array leaves the plan alone,
        // so commands come from wherever they would have come from without `controlParams`.
        for text in ["NaN", "1e400", r#"{"a": NaN}"#] {
            let bare = json!({"controlParams": text});
            let plan = StandbyPlan::parse(&bare, &Value::Null).unwrap();
            assert!(plan.commands.is_empty(), "{text}");

            let with_commands = json!({
                "controlParams": text,
                "commands": [{"commandType": "STEERINGWHEELHEATING"}],
            });
            let plan = StandbyPlan::parse(&with_commands, &Value::Null).unwrap();
            assert_eq!(plan.commands.len(), 1, "{text}");
            assert_eq!(
                plan.commands[0].command_type, "STEERINGWHEELHEATING",
                "{text}"
            );
        }

        // `controlParams` text starting with `[` that serde_json cannot hold is refused.
        for text in [
            r#"[{"commandType":"OPENAIR","controlParamsMap":{"a":NaN}}]"#,
            " [NaN]",
        ] {
            let packed = json!({"controlParams": text});
            let error = StandbyPlan::parse(&packed, &Value::Null).unwrap_err();
            assert_eq!(error.exception, "Unsupported", "{text}");
        }

        // Python's int() refuses integers past 4,300 digits with ValueError,
        // which pyBYD's _json_or_raw reads as an empty map; 4,300 digits
        // decodes in Python and stays Unsupported here.
        let at_limit = format!(r#"{{"mainSettingTemp":1{}}}"#, "0".repeat(4299));
        let command = json!({"commandType": "OPENAIR", "controlParamsMap": at_limit});
        let error = StandbyPlanCommand::parse(&command, &Value::Null).unwrap_err();
        assert_eq!(error.exception, "Unsupported");

        let past_limit = format!(r#"{{"mainSettingTemp":1{}}}"#, "0".repeat(4300));
        let command = json!({"commandType": "OPENAIR", "controlParamsMap": past_limit});
        let command = StandbyPlanCommand::parse(&command, &Value::Null).unwrap();
        assert!(command.control_params_map.is_empty());

        // Top-level object nested deeper than 127 levels is refused instead of
        // silently collapsing to an empty map.
        let deep_object = format!(r#"{{"a":{}1{}}}"#, "[".repeat(127), "]".repeat(127));
        let command = json!({"commandType": "OPENAIR", "controlParamsMap": deep_object});
        let error = StandbyPlanCommand::parse(&command, &Value::Null).unwrap_err();
        assert_eq!(error.exception, "Unsupported");
    }

    #[test]
    fn datetimes_as_pydantic_reads_them() {
        let read = |value: Value| pydantic_datetime(&value).map(|at| at.to_json());
        assert_eq!(
            read(json!("2026-11-22T19:10:00+01:00")).unwrap(),
            "2026-11-22T19:10:00+01:00"
        );
        assert_eq!(
            read(json!("2026-11-22 19:10z")).unwrap(),
            "2026-11-22T19:10:00Z"
        );
        assert_eq!(
            read(json!("2026-11-22_19:10:00,5")).unwrap(),
            "2026-11-22T19:10:00.500000"
        );
        assert_eq!(read(json!("2026-11-22")).unwrap(), "2026-11-22T00:00:00");
        assert_eq!(
            read(json!("2026-11-22T19:10:00.1234567\u{2212}0130")).unwrap(),
            "2026-11-22T19:10:00.123456-01:30"
        );
        assert_eq!(read(json!(1_700_000_000)).unwrap(), "2023-11-14T22:13:20Z");
        assert_eq!(
            read(json!(1_700_000_000_123_i64)).unwrap(),
            "2023-11-14T22:13:20.123000Z"
        );
        assert_eq!(
            read(json!(1_700_000_000_123.5)).unwrap(),
            "2023-11-14T22:13:20.123500Z"
        );
        assert_eq!(read(json!(-1.5)).unwrap(), "1969-12-31T23:59:58.500000Z");
        assert_eq!(read(json!(0.999_999_6)).unwrap(), "1970-01-01T00:00:01Z");
        assert_eq!(read(json!("-.5")).unwrap(), "1969-12-31T23:59:59.500000Z");
        assert_eq!(read(json!("1.5e3")).unwrap(), "1970-01-01T00:25:00Z");
        assert_eq!(
            read(json!("-62135596800000")).unwrap(),
            "0001-01-01T00:00:00Z"
        );
        for bad in [
            json!("1e9"),
            json!("."),
            json!("+"),
            json!(""),
            json!("1.5e"),
            json!("1.x"),
            json!("x.1"),
            json!("1.5e+"),
            json!(" 1"),
            json!("0000-01-01T00:00:00"),
            json!("2026-02-29T00:00:00"),
            json!("2026-11-22T19"),
            json!("2026-11-22T19:10:60"),
            json!("2026-11-22T19:10:00."),
            json!("2026-11-22T19:10:00+01"),
            json!("2026-11-22T19:10:00+24:00"),
            json!("2026-11-22T19:10:00+00:60"),
            json!("2026-11-22T19:10:00Zx"),
            json!("2026-11-22T19:10:00\u{e9}01:00"),
            json!("2026-11-22x19:10:00"),
            json!("2026-11-22T19-10"),
            json!("2026/11/22"),
            json!("9223372036854775808"),
            json!(u64::MAX),
            json!(i64::MIN),
            json!(1e300),
            json!(253_402_300_800_000_i64),
            json!(-62_135_596_800_001_i64),
            json!(true),
            json!([]),
            json!({}),
        ] {
            assert!(read(bad.clone()).is_err(), "{bad}");
        }
        assert!(from_float_timestamp(f64::NAN).is_none());
    }

    #[test]
    fn schedules_are_checked() {
        let at = aware("2026-11-22T19:10:00+01:00");
        assert_eq!(StandbySchedule::once(at).repeat(), Repeat::Once);
        assert_eq!(StandbySchedule::daily(at).at(), Some(PlanTime::Aware(at)));
        let weekly = StandbySchedule::weekly("12:55", &[5, 4, 5]).unwrap();
        assert_eq!(weekly.weekly_days(), [4, 5]);
        assert_eq!(weekly.start_time(), "12:55");
        assert_eq!(
            weekly.model_dump(),
            json!({"repeat": "weekly", "at": null, "start_time": "12:55", "weekly_days": [4, 5]})
        );
        for (start, days) in [
            ("25:00", vec![1]),
            ("9:30", vec![1]),
            ("10:00", vec![7]),
            ("10:00", vec![]),
            ("10:00\n", vec![1]),
            ("10-00", vec![1]),
            ("30:00", vec![1]),
        ] {
            assert!(StandbySchedule::weekly(start, &days).is_err(), "{start}");
        }
        assert!(StandbySchedule::weekly("1\u{662}:3\u{664}", &[0]).is_ok());
        assert!(StandbySchedule::new(Repeat::Once, None, "", &[]).is_err());
        let naive = pydantic_datetime(&json!("2026-11-18T07:00:00")).unwrap();
        assert_eq!(naive.aware(), None);
        assert!(StandbySchedule::new(Repeat::Daily, Some(naive), "", &[]).is_err());
        let off = StandbySchedule::new(Repeat::Off, Some(naive), "zz", &[9]).unwrap();
        assert_eq!(off.model_dump()["at"], json!("2026-11-18T07:00:00"));
        assert_eq!(PlanTime::from(at).to_json(), "2026-11-22T19:10:00+01:00");
        assert!(StandbySchedule::from_kwargs(&kwargs(json!({"repeat": "OFF"}))).is_err());
        assert!(StandbySchedule::from_kwargs(&kwargs(json!({}))).is_err());
    }

    #[test]
    fn builders_match_the_app() {
        // The three sections of the plan edit screen, as app 3.6.0 saves them.
        let climate = StandbyPlanCommand::climate(19.0, 10, false, DEFAULT_TEMP_OFFSET_C).unwrap();
        assert_eq!(
            Value::Object(climate.control_params_map.clone()),
            json!({"cycleMode": 1, "timeSpan": 1, "remoteMode": 4, "airAccuracy": 1,
                   "airConditioningMode": 1, "mainSettingTemp": 5, "copilotSettingTemp": 5})
        );
        assert_eq!(climate.raw["command_type"], json!("OPENAIR"));
        let from_values = StandbyPlanCommand::climate_from_values(
            &json!(19),
            &json!(10.0),
            &json!(0),
            &json!(DEFAULT_TEMP_OFFSET_C),
        )
        .unwrap();
        assert_eq!(from_values, climate);
        let recirculating = StandbyPlanCommand::climate(20.0, 10, true, 14.0).unwrap();
        assert_eq!(recirculating.control_params_map["cycleMode"], json!(2));
        assert_eq!(
            StandbyPlanCommand::climate(21.0, 11, false, 14.0)
                .unwrap_err()
                .exception,
            "ValueError"
        );
        assert_eq!(
            StandbyPlanCommand::climate(99.0, 10, false, 14.0)
                .unwrap_err()
                .exception,
            "ValidationError"
        );
        let unhashable = StandbyPlanCommand::climate_from_values(
            &json!(21),
            &json!([10]),
            &json!(false),
            &json!(14),
        );
        assert_eq!(unhashable.unwrap_err().exception, "TypeError");
        let seats = StandbyPlanCommand::seats(&PlanSeats {
            driver_heat: Some(true),
            driver_ventilation: Some(false),
            passenger_heat: Some(false),
            passenger_ventilation: Some(true),
        });
        assert_eq!(
            Value::Object(seats.control_params_map),
            json!({"mainHeat": 1, "mainVentilation": 3, "copilotHeat": 3, "copilotVentilation": 1,
                   "lrSeatHeatState": 0, "lrSeatVentilationState": 0, "lrThirdHeatState": 0,
                   "lrThirdVentilationState": 0, "rrSeatHeatState": 0, "rrSeatVentilationState": 0,
                   "rrThirdHeatState": 0, "rrThirdVentilationState": 0, "chairType": 8})
        );
        let absent = StandbyPlanCommand::seats(&PlanSeats {
            driver_heat: Some(true),
            passenger_heat: Some(false),
            ..PlanSeats::default()
        });
        assert_eq!(absent.control_params_map["mainVentilation"], json!(0));
        assert_eq!(
            Value::Object(StandbyPlanCommand::steering_wheel(true).control_params_map),
            json!({"steeringWheelHeatState": 1, "chairType": 5})
        );
        assert_eq!(
            StandbyPlanCommand::steering_wheel(false).control_params_map["steeringWheelHeatState"],
            json!(3)
        );
        assert!(
            !PlanSeats {
                passenger_heat: Some(false),
                ..PlanSeats::default()
            }
            .active()
        );
    }

    #[test]
    fn readers() {
        let plan = StandbyPlan {
            raw: Map::new(),
            id: String::new(),
            plan_name: String::new(),
            plan_type: None,
            default_plan: None,
            execute_type: Some("1".to_owned()),
            start_time: String::new(),
            weekly_days: Vec::new(),
            execute_time: None,
            rec_execute_time: None,
            time_zone: None,
            commands: vec![StandbyPlanCommand::climate(22.0, 30, false, 14.0).unwrap()],
        };
        assert_eq!(
            plan.climate(14.0),
            Some(PlanClimate {
                temperature: 22.0,
                duration: 30,
                recirculation: false
            })
        );
        assert_eq!(plan.climate(16.0).unwrap().temperature, 24.0);
        assert_eq!(plan.seats().unwrap(), None);
        assert!(!plan.steering_wheel_heat());
        assert!(!plan.preset());
        assert_eq!(plan.schedule().unwrap(), StandbySchedule::off());
        let weekly = StandbyPlan {
            execute_type: Some("2".to_owned()),
            start_time: "25:00".to_owned(),
            weekly_days: vec![1],
            ..plan.clone()
        };
        assert!(weekly.schedule().is_err());
        assert_eq!(
            weekly.properties()["schedule"],
            json!({"error": "ValidationError"})
        );
        let odd = StandbyPlan::parse(
            &json!({"controlParamsList": [
                {"commandType": "VENTILATIONHEATING", "controlParamsMap": {"mainHeat": [1]}},
                {"commandType": "OPENAIR", "controlParamsMap": {"mainSettingTemp": u64::MAX, "timeSpan": true, "cycleMode": 2.0}},
            ]}),
            &Value::Null,
        )
        .unwrap();
        assert_eq!(odd.seats().unwrap_err().exception, "TypeError");
        assert_eq!(odd.properties()["seats"], json!({"error": "TypeError"}));
        let climate = odd.climate(14.0).unwrap();
        assert_eq!(climate.temperature, u64::MAX as f64 + 14.0);
        assert_eq!((climate.duration, climate.recirculation), (10, true));
    }

    #[test]
    fn days_and_arithmetic() {
        assert_eq!(parse_days(&json!("6, 4 ,4,x,,-1")).unwrap(), [4, 6]);
        assert_eq!(
            parse_days(&json!("\u{b2}")).unwrap_err().exception,
            "ValidationError"
        );
        assert_eq!(
            parse_days(&json!("99999999999999999999,1"))
                .unwrap_err()
                .exception,
            "Unsupported"
        );
        assert_eq!(
            parse_days(&json!("99999999999999999999,\u{b2}"))
                .unwrap_err()
                .exception,
            "ValidationError"
        );
        let at = aware("2026-11-18T23:12:00+01:00");
        assert_eq!(epoch_micros(&at), 1_795_039_920_000_000);
        assert_eq!(
            add_days(&at, 2).unwrap().to_json(),
            "2026-11-20T23:12:00+01:00"
        );
        assert!(add_days(&aware("9999-12-31T00:00:00Z"), 1).is_none());
        assert!(add_days(&at, i64::MAX).is_none());
        assert_eq!(
            from_epoch_millis(1_795_039_920_123).unwrap().to_json(),
            "2026-11-18T22:12:00.123000Z"
        );
        assert!(from_epoch_millis(i64::MAX).is_none());
    }

    #[test]
    fn bad_parity_arguments() {
        let call = |name: &str, args: Value| parity_call(name, &args).unwrap().unwrap_err().kind;
        assert_eq!(
            call("models.standby.PlanClimate", json!({"kwargs": 1})),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.standby.StandbySchedule.weekly",
                json!({"start_time": "10:00", "weekly_days": ["1"]})
            ),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.standby.StandbySchedule.weekly",
                json!({"start_time": 5, "weekly_days": [1]})
            ),
            "ValidationError"
        );
        assert_eq!(
            call(
                "models.standby.StandbyPlan.climate",
                json!({"raw": {}, "temp_offset": "x"})
            ),
            "BadArguments"
        );
        assert_eq!(
            call("models.standby.StandbyPlan.command", json!({"raw": 5})),
            "ValidationError"
        );
        assert!(
            parity_call("models.standby.constants", &json!({}))
                .unwrap()
                .is_ok()
        );
        assert!(parity_call("models.standby.nothing", &json!({})).is_none());
    }
}
