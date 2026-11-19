//! Smart charging schedule model (`pybyd/models/smart_charging.py`).
//!
//! Populated from `/control/smartCharge/homePage`. The endpoint returns
//! two nested DTOs that describe the user's configured schedules:
//!
//! * `smartChargeDto`: the simple "charge between X and Y" schedule
//! * `smartJourneyDto`: the departure-time / off-peak-window schedule
//!
//! Both DTOs use `HH:MM` time-of-day strings on the wire (no seconds).
//! `endChargeTime` may also be the sentinel `"full"` meaning *charge
//! until the battery is full within the start-time window*.
//!
//! The `chargeWay` field selects how often the schedule repeats:
//!
//! * `"s"`: single one-shot charge
//! * `"e"`: every day
//! * `"0,1,2,3,4"`: comma-separated weekday indices (`0` = Monday)

use std::borrow::Cow;

use chrono::NaiveTime;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields, PyDateTime};
use crate::coerce::{self, ValidationError};
use crate::parity::Outcome;
use crate::pyjson::{py_int_from_str, py_str};

/// `_parse_hhmm`: parse a BYD `HH:MM` time-of-day string to a time.
///
/// Returns `None` for sentinel-like inputs (the base validator already
/// drops `""` / `"--"`, but `"full"` is endpoint-specific and handled
/// separately on [`SmartChargeDto::charge_until_full`]) and for anything
/// that is not a valid time.
fn parse_hhmm(value: &Value) -> Option<NaiveTime> {
    if value.is_null() {
        return None;
    }
    let text = py_str(value);
    let text = coerce::py_strip(&text);
    if text.is_empty() || text.to_lowercase() == "full" {
        return None;
    }
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() < 2 {
        return None;
    }
    let number = |part: &str| py_int_from_str(part);
    let hour = number(parts[0])?;
    let minute = number(parts[1])?;
    let second = match parts.get(2) {
        Some(part) => number(part)?,
        None => 0,
    };
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return None;
    }
    NaiveTime::from_hms_opt(hour as u32, minute as u32, second as u32)
}

/// `_parse_status_bool`: parse the BYD status string (`"1"`/`"0"`) to `bool`.
fn parse_status_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Null => None,
        Value::Bool(flag) => Some(*flag),
        other => {
            let text = py_str(other);
            let text = coerce::py_strip(&text);
            if text.is_empty() {
                None
            } else {
                Some(text == "1")
            }
        }
    }
}

/// `_parse_charge_way` and `_parse_limit_str`: the value's text, stripped,
/// or `None` when that is empty. Interpretation lives on the model.
fn parse_trimmed_text(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let text = py_str(value);
    let text = coerce::py_strip(&text);
    (!text.is_empty()).then(|| text.to_owned())
}

/// Writes a time the way pydantic's JSON mode does: `HH:MM:SS`.
fn ser_opt_time<S: Serializer>(
    value: &Option<NaiveTime>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(time) => serializer.serialize_str(&time.format("%H:%M:%S").to_string()),
        None => serializer.serialize_none(),
    }
}

/// The simple "charge within a window" schedule (`smartChargeDto`).
///
/// `end_time` is `None` when the wire value is the `"full"` sentinel;
/// use [`Self::charge_until_full`] to distinguish that from "no schedule".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SmartChargeDto {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// When charging may start (`startChargeTime` on the wire).
    #[serde(serialize_with = "ser_opt_time")]
    pub start_time: Option<NaiveTime>,
    /// When charging must end (`endChargeTime` on the wire).
    #[serde(serialize_with = "ser_opt_time")]
    pub end_time: Option<NaiveTime>,
    /// `true` when `endChargeTime` was the wire sentinel `"full"`.
    pub charge_until_full: bool,
    /// How often the schedule repeats (`"s"`, `"e"` or weekday indices).
    pub charge_way: Option<String>,
    /// Whether the schedule is active.
    pub status: Option<bool>,
    /// When the schedule was last updated.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub update_time: Option<PyDateTime>,
    /// When the schedule was created.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub create_time: Option<PyDateTime>,
    /// When the schedule last ran.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub exe_time: Option<PyDateTime>,
}

const CHARGE_KEY_ALIASES: &[(&str, &str)] = &[
    ("startChargeTime", "start_time"),
    ("endChargeTime", "end_time"),
];

/// `_capture_charge_until_full`: sets `chargeUntilFull` when the wire
/// `endChargeTime` is `"full"`. It runs before the base cleaning pass, so the
/// flag is also part of `raw`.
fn capture_charge_until_full(values: &Value) -> Cow<'_, Value> {
    if let Value::Object(map) = values
        && let Some(Value::String(end)) = map.get("endChargeTime")
        && coerce::py_strip(end).to_lowercase() == "full"
    {
        let mut map = map.clone();
        map.insert("chargeUntilFull".to_owned(), Value::Bool(true));
        return Cow::Owned(Value::Object(map));
    }
    Cow::Borrowed(values)
}

impl SmartChargeDto {
    /// Whether the schedule is currently active.
    pub fn is_enabled(&self) -> bool {
        self.status == Some(true)
    }

    /// Parsed weekday indices for `charge_way`, when applicable.
    ///
    /// `None` for the named modes (`"s"` single, `"e"` every day) and for
    /// empty or unknown values. A list for explicit weekday selections like
    /// `"0,1,2,3,4"` (`0` = Monday). Also `None` when an index does not fit
    /// in `i64`, where Python would return a larger integer.
    pub fn repeat_days(&self) -> Option<Vec<i64>> {
        repeat_days(self.charge_way.as_deref())
    }
}

fn repeat_days(charge_way: Option<&str>) -> Option<Vec<i64>> {
    let way = charge_way?;
    if way.is_empty() || way == "s" || way == "e" {
        return None;
    }
    let mut days = Vec::new();
    for part in way.split(',') {
        let part = coerce::py_strip(part);
        if part.is_empty() {
            continue;
        }
        days.push(i64::try_from(py_int_from_str(part)?).ok()?);
    }
    Some(days)
}

impl BydModel for SmartChargeDto {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let values = capture_charge_until_full(raw);
        let mut fields = Fields::cleaned(&values, CHARGE_KEY_ALIASES)?;
        let model = Self {
            raw: fields.raw(),
            start_time: fields.get("start_time", None, |v| Ok(parse_hhmm(v))),
            end_time: fields.get("end_time", None, |v| Ok(parse_hhmm(v))),
            charge_until_full: fields.get("charge_until_full", false, coerce::boolean),
            charge_way: fields.get("charge_way", None, |v| Ok(parse_trimmed_text(v))),
            status: fields.get("status", None, |v| Ok(parse_status_bool(v))),
            update_time: fields.get("update_time", None, base::byd_timestamp),
            create_time: fields.get("create_time", None, base::byd_timestamp),
            exe_time: fields.get("exe_time", None, base::byd_timestamp),
        };
        fields.finish()?;
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([
            ("is_enabled", json!(self.is_enabled())),
            ("repeat_days", json!(self.repeat_days())),
        ])
    }
}

/// Departure-time / off-peak-window schedule (`smartJourneyDto`).
///
/// `use_vehicle_time` is when the vehicle should be ready;
/// `discount_start` / `discount_end` define the off-peak tariff window the
/// charger will prefer (`startDiscountPrice` / `endDiscountPrice` on the wire).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SmartJourneyDto {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// When the vehicle should be ready.
    #[serde(serialize_with = "ser_opt_time")]
    pub use_vehicle_time: Option<NaiveTime>,
    /// Start of the off-peak window.
    #[serde(serialize_with = "ser_opt_time")]
    pub discount_start: Option<NaiveTime>,
    /// End of the off-peak window.
    #[serde(serialize_with = "ser_opt_time")]
    pub discount_end: Option<NaiveTime>,
    /// How often the schedule repeats.
    pub charge_way: Option<String>,
    /// Whether the schedule is active.
    pub status: Option<bool>,
    /// When the schedule was last updated.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub update_time: Option<PyDateTime>,
    /// When the schedule was created.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub create_time: Option<PyDateTime>,
}

const JOURNEY_KEY_ALIASES: &[(&str, &str)] = &[
    ("useVehicleTime", "use_vehicle_time"),
    ("startDiscountPrice", "discount_start"),
    ("endDiscountPrice", "discount_end"),
];

impl SmartJourneyDto {
    /// Whether the journey schedule is currently active.
    pub fn is_enabled(&self) -> bool {
        self.status == Some(true)
    }
}

impl BydModel for SmartJourneyDto {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let mut fields = Fields::cleaned(raw, JOURNEY_KEY_ALIASES)?;
        let model = Self {
            raw: fields.raw(),
            use_vehicle_time: fields.get("use_vehicle_time", None, |v| Ok(parse_hhmm(v))),
            discount_start: fields.get("discount_start", None, |v| Ok(parse_hhmm(v))),
            discount_end: fields.get("discount_end", None, |v| Ok(parse_hhmm(v))),
            charge_way: fields.get("charge_way", None, |v| Ok(parse_trimmed_text(v))),
            status: fields.get("status", None, |v| Ok(parse_status_bool(v))),
            update_time: fields.get("update_time", None, base::byd_timestamp),
            create_time: fields.get("create_time", None, base::byd_timestamp),
        };
        fields.finish()?;
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([("is_enabled", json!(self.is_enabled()))])
    }
}

/// Top-level schedule snapshot from `/control/smartCharge/homePage`.
///
/// Composes the two nested DTOs plus the response-level metadata
/// (`vehicleTimeZone`, `soc`, `type`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SmartChargingSchedule {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// State of charge in percent.
    pub soc: Option<i64>,
    /// Schedule type.
    pub r#type: Option<String>,
    /// The vehicle's time zone (`vehicleTimeZone` on the wire).
    pub time_zone: Option<String>,
    /// The simple schedule (`smartChargeDto` on the wire).
    pub charge: Option<SmartChargeDto>,
    /// The departure-time schedule (`smartJourneyDto` on the wire).
    pub journey: Option<SmartJourneyDto>,
    /// When the snapshot was last updated.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub update_time: Option<PyDateTime>,
}

const SCHEDULE_KEY_ALIASES: &[(&str, &str)] = &[
    ("smartChargeDto", "charge"),
    ("smartJourneyDto", "journey"),
    ("vehicleTimeZone", "time_zone"),
];

impl SmartChargingSchedule {
    /// Whether *any* schedule is currently active.
    pub fn is_enabled(&self) -> bool {
        self.charge.as_ref().is_some_and(SmartChargeDto::is_enabled)
            || self
                .journey
                .as_ref()
                .is_some_and(SmartJourneyDto::is_enabled)
    }
}

impl BydModel for SmartChargingSchedule {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        let mut fields = Fields::cleaned(raw, SCHEDULE_KEY_ALIASES)?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            soc: fields.opt("soc", coerce::int),
            r#type: fields.opt("type", coerce::string),
            time_zone: fields.opt("time_zone", coerce::string),
            charge: fields.opt("charge", |v| SmartChargeDto::parse(v, context)),
            journey: fields.opt("journey", |v| SmartJourneyDto::parse(v, context)),
            update_time: fields.get("update_time", None, base::byd_timestamp),
        };
        fields.finish()?;
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([("is_enabled", json!(self.is_enabled()))])
    }
}

/// `_parse_limit_list`: coerce `currentLimitList` items to non-empty strings.
fn parse_limit_list(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().filter_map(parse_trimmed_text).collect(),
        _ => Vec::new(),
    }
}

/// Supported and active AC charging current limits from `/control/smartCharge/getAcCurrentLimit`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AcCurrentLimit {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// The limit currently in force, as sent.
    pub current_ev_current_limit: Option<String>,
    /// The limits the vehicle supports, as sent.
    pub current_limit_list: Vec<String>,
}

impl BydModel for AcCurrentLimit {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let mut fields = Fields::cleaned(raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            current_ev_current_limit: fields.get("current_ev_current_limit", None, |v| {
                Ok(parse_trimmed_text(v))
            }),
            current_limit_list: fields.get("current_limit_list", Vec::new(), |v| {
                Ok(parse_limit_list(v))
            }),
        };
        fields.finish()?;
        Ok(model)
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.smart_charging.SmartChargeDto" => base::model_call::<SmartChargeDto>(args),
        "models.smart_charging.SmartJourneyDto" => base::model_call::<SmartJourneyDto>(args),
        "models.smart_charging.SmartChargingSchedule" => {
            base::model_call::<SmartChargingSchedule>(args)
        }
        "models.smart_charging.AcCurrentLimit" => base::model_call::<AcCurrentLimit>(args),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(text: &str) -> Option<NaiveTime> {
        parse_hhmm(&json!(text))
    }

    #[test]
    fn times_follow_the_python_parser() {
        assert_eq!(time("08:30").unwrap().to_string(), "08:30:00");
        assert_eq!(time(" 8 : 5 ").unwrap().to_string(), "08:05:00");
        assert_eq!(time("12:30:45:99").unwrap().to_string(), "12:30:45");
        assert_eq!(time("٠٨:٣٠").unwrap().to_string(), "08:30:00");
        assert_eq!(time("24:00"), None);
        assert_eq!(time("12:60"), None);
        assert_eq!(time("12:30:60"), None);
        assert_eq!(time("12:30:x"), None);
        assert_eq!(time("full"), None);
        assert_eq!(time(" FULL "), None);
        assert_eq!(time("8"), None);
        assert_eq!(time(""), None);
        assert_eq!(parse_hhmm(&json!(830)), None);
        assert_eq!(parse_hhmm(&json!(null)), None);
        assert_eq!(parse_hhmm(&json!({"a": 1})), None);
        assert_eq!(time("-0:0").unwrap().to_string(), "00:00:00");
        assert_eq!(time("99999999999999999999999999999999999999999:00"), None);
    }

    #[test]
    fn status_is_only_true_for_the_text_one() {
        assert_eq!(parse_status_bool(&json!("1")), Some(true));
        assert_eq!(parse_status_bool(&json!(" 1 ")), Some(true));
        assert_eq!(parse_status_bool(&json!(1)), Some(true));
        assert_eq!(parse_status_bool(&json!("0")), Some(false));
        assert_eq!(parse_status_bool(&json!(1.0)), Some(false));
        assert_eq!(parse_status_bool(&json!("")), None);
        assert_eq!(parse_status_bool(&json!(null)), None);
        assert_eq!(parse_status_bool(&json!(false)), Some(false));
    }

    #[test]
    fn repeat_days_reads_weekday_lists() {
        let way = |text: &str| repeat_days(Some(text));
        assert_eq!(way("0,1, 2 ,,4"), Some(vec![0, 1, 2, 4]));
        assert_eq!(way("s"), None);
        assert_eq!(way("e"), None);
        assert_eq!(way("1,x"), None);
        assert_eq!(way(","), Some(vec![]));
        assert_eq!(way("99999999999999999999"), None);
        assert_eq!(repeat_days(None), None);
        assert_eq!(repeat_days(Some("")), None);
    }

    #[test]
    fn full_end_time_sets_the_flag_and_lands_in_raw() {
        let dto = SmartChargeDto::parse(
            &json!({"startChargeTime": "22:00", "endChargeTime": " Full ", "status": "1"}),
            &Value::Null,
        )
        .unwrap();
        assert!(dto.charge_until_full);
        assert_eq!(dto.end_time, None);
        assert!(dto.is_enabled());
        assert_eq!(dto.raw.get("chargeUntilFull"), Some(&json!(true)));
        assert_eq!(dto.start_time.unwrap().to_string(), "22:00:00");
    }

    #[test]
    fn the_schedule_is_enabled_when_either_part_is() {
        let schedule = |raw| SmartChargingSchedule::parse(&raw, &Value::Null).unwrap();
        assert!(!schedule(json!({})).is_enabled());
        assert!(schedule(json!({"smartJourneyDto": {"status": 1}})).is_enabled());
        assert!(schedule(json!({"smartChargeDto": {"status": "1"}})).is_enabled());
        assert!(!schedule(json!({"smartChargeDto": {"status": "0"}})).is_enabled());
    }

    #[test]
    fn limit_lists_keep_non_empty_text() {
        assert_eq!(
            parse_limit_list(&json!([" 16 ", null, "", 32, 1.5, true])),
            ["16", "32", "1.5", "True"]
        );
        assert!(parse_limit_list(&json!("16")).is_empty());
    }
}
