//! What every model shares: sentinel stripping, key aliases, enums, timestamps, units (`pybyd/models/_base.py`).
//!
//! A pyBYD model is a pydantic class. Validating one runs, in this order:
//!
//! 1. the model's own `mode="before"` validators (a subclass's run before
//!    its base's, and within a class the last defined runs first);
//! 2. `BydBaseModel._clean_byd_values`: key aliases, sentinel stripping and
//!    the `raw` copy ([`clean_byd_values`]);
//! 3. one conversion per field, in declaration order ([`Fields`]);
//! 4. the `mode="after"` validators, base first: `_normalise_sentinels`
//!    ([`clear_if`]) and then the subclass's own.
//!
//! A Rust model does the same steps by hand in its [`BydModel::parse`]; see
//! `gps.rs` and `charging.rs`, and `docs/porting-models.md`.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, json};

use crate::coerce::{
    self, Failures, IntEnum, ValidationError, py_decimal, py_is_digit, py_is_space, py_strip,
};
use crate::parity::{Args, Outcome, ParityError};

/// Strings the BYD API uses for "not available"; matched after `str.strip()`.
pub const SENTINELS: &[&str] = &["", "--", "NaN", "nan"];

/// Key aliases shared by several models: BYD spells "steering" as "stearing".
pub const COMMON_KEY_ALIASES: &[(&str, &str)] =
    &[("stearingWheelHeatState", "steeringWheelHeatState")];

// ---------------------------------------------------------------------------
// Enums as Python sees them
// ---------------------------------------------------------------------------

/// A pyBYD `BydEnum` as Python sees it: member names and `EnumClass(value)`.
pub trait PyEnum: IntEnum {
    /// The Python member name, such as `"HYBRID"`.
    fn name(self) -> &'static str;

    /// What `_missing_` returns for a value no member equals. `BydEnum`'s
    /// gives `UNKNOWN` (or the first member when there is none).
    fn missing(_value: &Value) -> Self {
        Self::MISSING.unwrap_or(Self::MEMBERS[0])
    }

    /// The results of the member's public methods, by name, for the parity call.
    fn methods(self) -> Map<String, Value> {
        Map::new()
    }

    /// `EnumClass(value)`: the member whose value equals `value` (Python
    /// equality, so `1.0` and `true` find member `1`), else [`PyEnum::missing`].
    fn call(value: &Value) -> Self {
        Self::MEMBERS
            .iter()
            .copied()
            .find(|member| py_equals_int(value, member.value()))
            .unwrap_or_else(|| Self::missing(value))
    }
}

/// Python's `value == number` for a JSON value and a small integer.
pub(crate) fn py_equals_int(value: &Value, number: i64) -> bool {
    match value {
        Value::Bool(flag) => i64::from(*flag) == number,
        Value::Number(json_number) => match json_number.as_i64() {
            Some(int) => int == number,
            None => json_number.is_f64() && json_number.as_f64() == Some(number as f64),
        },
        _ => false,
    }
}

/// A `BydEnum` (or `IntEnum`) field: the reader to pass to `Fields::get` or `Fields::opt`.
pub fn enum_field<E: IntEnum>(value: &Value) -> Result<E, ValidationError> {
    coerce::int_enum(value)
}

/// Answers an enum's parity call: `{"value": ...}` in, `{"name", "value", ...}` out.
pub(crate) fn enum_call<E: PyEnum>(args: &Value) -> Outcome {
    let member = E::call(Args(args).value("value"));
    let mut answer = Map::new();
    answer.insert("name".to_owned(), json!(member.name()));
    answer.insert("value".to_owned(), json!(member.value()));
    answer.extend(member.methods());
    Ok(Value::Object(answer))
}

/// Python's `bool(value)` for a JSON value.
pub(crate) fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|float| float != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(entries) => !entries.is_empty(),
    }
}

/// Declares a `BydEnum`: an `IntEnum` with `UNKNOWN = -1` that `_missing_` falls back to.
macro_rules! byd_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $value:expr => $python:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$variant_meta])* $variant = $value, )+
        }

        impl $crate::coerce::IntEnum for $name {
            const MEMBERS: &'static [Self] = &[$(Self::$variant),+];
            const MISSING: Option<Self> = Some(Self::Unknown);
            fn value(self) -> i64 {
                self as i64
            }
        }

        impl $crate::models::base::PyEnum for $name {
            fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $python,)+
                }
            }
        }
    };
}
pub(crate) use byd_enum;

// ---------------------------------------------------------------------------
// Sentinel predicates
// ---------------------------------------------------------------------------

/// A number a sentinel predicate can test: Python compares ints and floats by value.
pub trait PyNumber: Copy {
    /// The value as a float, for comparisons.
    fn to_f64(self) -> f64;
}

impl PyNumber for i64 {
    fn to_f64(self) -> f64 {
        self as f64
    }
}

impl PyNumber for f64 {
    fn to_f64(self) -> f64 {
        self
    }
}

/// `True` when the value is negative (the `-1` sentinel).
pub fn is_negative(value: impl PyNumber) -> bool {
    value.to_f64() < 0.0
}

/// `True` when the value is BYD's temperature sentinel `-129`.
pub fn is_temp_sentinel(value: impl PyNumber) -> bool {
    value.to_f64() == -129.0
}

/// One entry of a model's `_SENTINEL_RULES`: sets the field to `None` when the predicate holds.
pub fn clear_if<T: Copy>(field: &mut Option<T>, predicate: impl Fn(T) -> bool) {
    if field.is_some_and(predicate) {
        *field = None;
    }
}

// ---------------------------------------------------------------------------
// Units
// ---------------------------------------------------------------------------

const UNIT_QUANTITIES: &[(&str, &str)] = &[("kwh", "kWh"), ("度", "kWh"), ("l", "L"), ("升", "L")];
const UNIT_DISTANCES: &[(&str, &str)] = &[
    ("100km", "100km"),
    ("百公里", "100km"),
    ("100mi", "100miles"),
    ("100mile", "100miles"),
    ("100miles", "100miles"),
    ("百英里", "100miles"),
];

/// Python's `str.casefold()`, as far as it can turn text into one of the unit keys.
fn casefold(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            'ſ' => folded.push('s'),
            'ß' | 'ẞ' => folded.push_str("ss"),
            'ﬀ' => folded.push_str("ff"),
            'ﬁ' => folded.push_str("fi"),
            'ﬂ' => folded.push_str("fl"),
            'ﬃ' => folded.push_str("ffi"),
            'ﬄ' => folded.push_str("ffl"),
            'ﬅ' | 'ﬆ' => folded.push_str("st"),
            other => folded.extend(other.to_lowercase()),
        }
    }
    folded
}

fn lookup<'a>(table: &[(&str, &'a str)], key: &str) -> Option<&'a str> {
    table
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, canonical)| *canonical)
}

/// The canonical form of a BYD energy or consumption unit.
///
/// The quantity and the per-distance part are folded separately, ignoring
/// case, whitespace, the `·` / `.` inside `kW·h` and the Chinese spellings:
/// `kW·h/100km`, `KWH/100 KM` and `度/百公里` all become `kWh/100km`,
/// `升/百公里` becomes `L/100km`, and a per-100-miles distance becomes
/// `100miles`. A part it doesn't recognise is kept as sent, minus the `·`.
pub fn normalize_unit(text: Option<&str>) -> Option<String> {
    let text = text?.replace('·', "");
    let (quantity, separator, distance) = match text.split_once('/') {
        Some((quantity, distance)) => (quantity, "/", distance),
        None => (text.as_str(), "", ""),
    };
    let squeeze = |part: &str| {
        part.chars()
            .filter(|c| !py_is_space(*c))
            .collect::<String>()
    };
    let quantity_key = casefold(&squeeze(quantity).replace('.', ""));
    let quantity = lookup(UNIT_QUANTITIES, &quantity_key).unwrap_or(quantity);
    let distance = lookup(UNIT_DISTANCES, &casefold(&squeeze(distance))).unwrap_or(distance);
    Some(format!("{quantity}{separator}{distance}"))
}

// ---------------------------------------------------------------------------
// Datetimes
// ---------------------------------------------------------------------------

/// An aware Python `datetime`, as pyBYD's timestamp fields hold it.
///
/// Python keeps the wall-clock time and the offset separately, and an
/// offset may have seconds and microseconds; both survive here so the JSON
/// forms match exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PyDateTime {
    /// Date and time in the datetime's own offset.
    pub local: NaiveDateTime,
    /// `utcoffset()` in microseconds; `0` is UTC.
    pub offset_us: i64,
}

impl PyDateTime {
    /// A UTC datetime.
    pub fn utc(local: NaiveDateTime) -> Self {
        Self {
            local,
            offset_us: 0,
        }
    }

    /// The same instant as a chrono value. A sub-second offset is truncated to whole seconds.
    pub fn to_chrono(&self) -> chrono::DateTime<chrono::FixedOffset> {
        let offset = chrono::FixedOffset::east_opt((self.offset_us / 1_000_000) as i32)
            .expect("Python offsets are under 24 hours");
        let utc = self.local - Duration::microseconds(self.offset_us);
        chrono::DateTime::from_naive_utc_and_offset(utc, offset)
    }

    fn date_and_time(&self) -> String {
        let local = self.local;
        let mut text = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            local.year(),
            local.month(),
            local.day(),
            local.hour(),
            local.minute(),
            local.second()
        );
        let micros = local.nanosecond() / 1000;
        if micros != 0 {
            text.push_str(&format!(".{micros:06}"));
        }
        text
    }

    /// Python's `datetime.isoformat()`.
    pub fn isoformat(&self) -> String {
        let sign = if self.offset_us < 0 { '-' } else { '+' };
        let total = self.offset_us.unsigned_abs();
        let (seconds, micros) = (total / 1_000_000, total % 1_000_000);
        let mut text = format!(
            "{}{sign}{:02}:{:02}",
            self.date_and_time(),
            seconds / 3600,
            seconds % 3600 / 60
        );
        if seconds % 60 != 0 || micros != 0 {
            text.push_str(&format!(":{:02}", seconds % 60));
            if micros != 0 {
                text.push_str(&format!(".{micros:06}"));
            }
        }
        text
    }

    /// The text pydantic writes for this datetime in `model_dump(mode="json")`.
    ///
    /// The offset is rounded to whole seconds (half away from zero); zero is
    /// written `Z`, anything else `±HH:MM` with the seconds dropped, so
    /// `-00:00:30` becomes `-00:00`.
    pub fn to_json(&self) -> String {
        let seconds = (self.offset_us as f64 / 1e6).round() as i64;
        let offset = if seconds == 0 {
            "Z".to_owned()
        } else {
            let sign = if seconds < 0 { '-' } else { '+' };
            let total = seconds.unsigned_abs();
            format!("{sign}{:02}:{:02}", total / 3600, total % 3600 / 60)
        };
        format!("{}{offset}", self.date_and_time())
    }
}

/// Reads a byte as C does: past the end of the string is NUL.
fn byte_at(bytes: &[u8], index: usize) -> u8 {
    bytes.get(index).copied().unwrap_or(0)
}

/// CPython's `parse_digits`: exactly `count` ASCII digits.
fn parse_digits(bytes: &[u8], position: usize, count: usize) -> Option<(i64, usize)> {
    let mut value = 0;
    for index in position..position + count {
        let byte = byte_at(bytes, index);
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + i64::from(byte - b'0');
    }
    Some((value, position + count))
}

/// CPython's `_find_isoformat_datetime_separator`. `None` stands for its `-1`.
fn find_separator(bytes: &[u8]) -> Option<usize> {
    let length = bytes.len();
    if length == 7 {
        return Some(7);
    }
    if bytes[4] == b'-' {
        if bytes[5] != b'W' {
            return Some(10);
        }
        if length > 8 && bytes[8] == b'-' {
            if length == 9 {
                return None;
            }
            if length > 10 && bytes[10].is_ascii_digit() {
                return Some(8);
            }
            return Some(10);
        }
        return Some(8);
    }
    if bytes[4] == b'W' {
        let mut index = 7;
        while index < length && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index < 9 {
            return Some(index);
        }
        return Some(if index % 2 == 0 { 7 } else { 8 });
    }
    Some(8)
}

/// CPython's `iso_to_ymd`; the result may fall in year 10000, which the caller rejects.
fn iso_to_date(year: i64, week: i64, day: i64) -> Option<NaiveDate> {
    if !(1..=9999).contains(&year) || !(1..=7).contains(&day) {
        return None;
    }
    let weekday = chrono::Weekday::try_from((day - 1) as u8).ok()?;
    NaiveDate::from_isoywd_opt(year as i32, u32::try_from(week).ok()?, weekday)
}

/// CPython's `parse_isoformat_date`. `limit` is the separator position.
fn parse_iso_date(bytes: &[u8], limit: usize) -> Option<NaiveDate> {
    let (year, mut position) = parse_digits(bytes, 0, 4)?;
    let uses_separator = byte_at(bytes, position) == b'-';
    if uses_separator {
        position += 1;
    }
    if byte_at(bytes, position) == b'W' {
        let (week, after) = parse_digits(bytes, position + 1, 2)?;
        position = after;
        let mut day = 1;
        if position < limit {
            if uses_separator {
                if byte_at(bytes, position) != b'-' {
                    return None;
                }
                position += 1;
            }
            day = parse_digits(bytes, position, 1)?.0;
        }
        return iso_to_date(year, week, day);
    }
    let (month, after) = parse_digits(bytes, position, 2)?;
    position = after;
    if uses_separator {
        if byte_at(bytes, position) != b'-' {
            return None;
        }
        position += 1;
    }
    let (day, _) = parse_digits(bytes, position, 2)?;
    if !(1..=9999).contains(&year) {
        return None;
    }
    NaiveDate::from_ymd_opt(
        year as i32,
        u32::try_from(month).ok()?,
        u32::try_from(day).ok()?,
    )
}

/// Hours, minutes, seconds and microseconds, plus whether text was left over.
struct TimeParts {
    fields: [i64; 4],
    trailing: bool,
}

/// CPython's `parse_hh_mm_ss_ff` between `start` and `end`.
fn parse_hh_mm_ss_ff(bytes: &[u8], start: usize, end: usize) -> Option<TimeParts> {
    let mut fields = [0; 4];
    let mut position = start;
    let mut has_separator = true;
    for (index, field) in fields.iter_mut().take(3).enumerate() {
        let (value, after) = parse_digits(bytes, position, 2)?;
        *field = value;
        let c = byte_at(bytes, after);
        position = after + 1;
        if index == 0 {
            has_separator = c == b':';
        }
        if position >= end {
            return Some(TimeParts {
                fields,
                trailing: c != 0,
            });
        } else if has_separator && c == b':' {
            continue;
        } else if c == b'.' || c == b',' {
            break;
        } else if !has_separator {
            position -= 1;
        } else {
            return None;
        }
    }
    let count = (end - position).min(6);
    let (mut micros, mut after) = parse_digits(bytes, position, count)?;
    micros *= 10_i64.pow(6 - count as u32);
    while byte_at(bytes, after).is_ascii_digit() {
        after += 1;
    }
    fields[3] = micros;
    Some(TimeParts {
        fields,
        trailing: byte_at(bytes, after) != 0,
    })
}

/// CPython's `parse_isoformat_time`: the time and, if present, the offset in microseconds.
fn parse_iso_time(bytes: &[u8], start: usize) -> Option<([i64; 4], Option<i64>)> {
    let end = bytes.len();
    let mut zone = start;
    loop {
        if matches!(byte_at(bytes, zone), b'Z' | b'+' | b'-') {
            break;
        }
        zone += 1;
        if zone >= end {
            break;
        }
    }
    let time = parse_hh_mm_ss_ff(bytes, start, zone)?;
    if zone == end {
        return (!time.trailing).then_some((time.fields, None));
    }
    let marker = byte_at(bytes, zone);
    if marker == b'Z' {
        return (byte_at(bytes, zone + 1) == 0).then_some((time.fields, Some(0)));
    }
    let sign = if marker == b'-' { -1 } else { 1 };
    let offset = parse_hh_mm_ss_ff(bytes, zone + 1, end)?;
    if offset.trailing {
        return None;
    }
    let [hours, minutes, seconds, micros] = offset.fields;
    let whole = sign * (hours * 3600 + minutes * 60 + seconds);
    // CPython makes UTC from a zero whole-second offset, dropping any microseconds.
    let offset_us = if whole == 0 {
        0
    } else {
        whole * 1_000_000 + sign * micros
    };
    Some((time.fields, Some(offset_us)))
}

/// Python 3.12's `datetime.fromisoformat` (the C implementation, which is stricter
/// than the pure-Python one in some places and looser in others).
///
/// Returns the wall-clock time and the offset in microseconds, or `None` where
/// Python raises `ValueError`.
pub fn datetime_fromisoformat(text: &str) -> Option<(NaiveDateTime, Option<i64>)> {
    if text.chars().count() < 7 {
        return None;
    }
    let bytes = text.as_bytes();
    let separator = find_separator(bytes)?;
    let date = parse_iso_date(bytes, separator)?;
    let (fields, offset) = if bytes.len() > separator {
        let width = match bytes[separator] {
            byte if byte & 0x80 == 0 => 1,
            byte if byte & 0xF0 == 0xE0 => 3,
            byte if byte & 0xF0 == 0xF0 => 4,
            _ => 2,
        };
        parse_iso_time(bytes, separator + width)?
    } else {
        ([0; 4], None)
    };
    if offset.is_some_and(|offset| offset.abs() >= 86_400_000_000) || date.year() > 9999 {
        return None;
    }
    let [hour, minute, second, micros] = fields.map(|field| u32::try_from(field).ok());
    let time = NaiveTime::from_hms_micro_opt(hour?, minute?, second?, micros?)?;
    Some((date.and_time(time), offset))
}

/// Seconds since the epoch of 9999-12-31T23:59:59, the last second Python's `datetime` holds.
const MAX_DATETIME_SECONDS: i128 = 253_402_300_799;
/// The last second glibc's `gmtime_r` can express (its year is a C `int`).
const MAX_GMTIME_SECONDS: i128 = 67_768_036_191_676_799;
/// pyBYD reads epochs at or above this as milliseconds.
const MS_THRESHOLD: i128 = 1_000_000_000_000;

/// pyBYD's epoch handling after `int(value)`: zero and negatives mean "no
/// value", large values are milliseconds. Python on Linux is the reference:
/// `fromtimestamp` raises `ValueError` past year 9999, `OSError` when
/// `gmtime` overflows and `OverflowError` past `time_t`. (Windows raises
/// `OSError` from year 3001 on.)
fn from_epoch(timestamp: i128) -> Result<Option<PyDateTime>, ValidationError> {
    if timestamp <= 0 {
        return Ok(None);
    }
    let seconds = if timestamp >= MS_THRESHOLD {
        timestamp / 1000
    } else {
        timestamp
    };
    if seconds > i128::from(i64::MAX) {
        return Err(ValidationError::raised("OverflowError", "time_t"));
    }
    if seconds > MAX_GMTIME_SECONDS {
        return Err(ValidationError::raised("OSError", "gmtime"));
    }
    if seconds > MAX_DATETIME_SECONDS {
        return Err(ValidationError::raised("ValueError", "year_out_of_range"));
    }
    let local = chrono::DateTime::from_timestamp(seconds as i64, 0)
        .expect("checked against year 9999")
        .naive_utc();
    Ok(Some(PyDateTime::utc(local)))
}

/// Python's `int(text)` for text that `str.isdigit()` accepted after leading
/// dashes: one optional `-` and Unicode decimal digits. Saturates far outside
/// `i64`, which is enough to classify the epoch.
fn int_of_digit_text(text: &str) -> Option<i128> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut value: i128 = 0;
    for c in digits.chars() {
        value = value
            .saturating_mul(10)
            .saturating_add(i128::from(py_decimal(c)?));
    }
    Some(if negative { -value } else { value })
}

/// `parse_byd_timestamp`: a BYD timestamp to a UTC datetime.
///
/// Accepts epoch seconds or milliseconds (auto-detected) as a number or
/// numeric string, and ISO-8601 strings. `None` when the value is null or
/// unparseable. The error is the Python exception the function lets escape.
pub fn parse_byd_timestamp(value: &Value) -> Result<Option<PyDateTime>, ValidationError> {
    let timestamp = match value {
        Value::Null | Value::Array(_) | Value::Object(_) => return Ok(None),
        Value::Bool(flag) => i128::from(*flag),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                i128::from(int)
            } else if let Some(int) = number.as_u64() {
                i128::from(int)
            } else {
                let float = number.as_f64().unwrap_or(f64::NAN);
                if float.is_nan() {
                    return Ok(None);
                }
                if float.is_infinite() {
                    return Err(ValidationError::raised("OverflowError", "float_to_int"));
                }
                float.trunc() as i128
            }
        }
        Value::String(text) => {
            let text = py_strip(text);
            if text.is_empty() {
                return Ok(None);
            }
            let unsigned = text.trim_start_matches('-');
            if unsigned.is_empty() || !unsigned.chars().all(py_is_digit) {
                return Ok(datetime_fromisoformat(&text.replace('Z', "+00:00")).map(
                    |(local, offset)| PyDateTime {
                        local,
                        offset_us: offset.unwrap_or(0),
                    },
                ));
            }
            match int_of_digit_text(text) {
                Some(timestamp) => timestamp,
                None => return Ok(None),
            }
        }
    };
    from_epoch(timestamp)
}

/// A field declared `BydTimestamp`: [`parse_byd_timestamp`] as a pydantic
/// `BeforeValidator`, so its `ValueError` becomes a `ValidationError`.
pub fn byd_timestamp(value: &Value) -> Result<Option<PyDateTime>, ValidationError> {
    parse_byd_timestamp(value).map_err(|error| {
        if error.exception == "ValueError" {
            ValidationError::new("value_error")
        } else {
            error
        }
    })
}

// ---------------------------------------------------------------------------
// Serialisation as model_dump(mode="json")
// ---------------------------------------------------------------------------

/// `serialize_with` for an `f64` field: non-finite values as `"NaN"`, `"Infinity"`, `"-Infinity"`.
pub fn ser_float<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    coerce::float_json(*value).serialize(serializer)
}

/// `serialize_with` for an `Option<f64>` field.
pub fn ser_opt_float<S: Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
    value
        .map_or(Value::Null, coerce::float_json)
        .serialize(serializer)
}

/// `serialize_with` for a `Vec<f64>` field.
pub fn ser_float_list<S: Serializer>(value: &[f64], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(value.iter().map(|item| coerce::float_json(*item)))
}

/// `serialize_with` for a `PyDateTime` field: pydantic's datetime text.
pub fn ser_datetime<S: Serializer>(value: &PyDateTime, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_json())
}

/// `serialize_with` for an `Option<PyDateTime>` field (`BydTimestamp`).
pub fn ser_opt_datetime<S: Serializer>(
    value: &Option<PyDateTime>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(datetime) => serializer.serialize_str(&datetime.to_json()),
        None => serializer.serialize_none(),
    }
}

/// `serialize_with` for an `IntEnum` field: its integer value.
pub fn ser_int_enum<E: IntEnum, S: Serializer>(
    value: &E,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_i64(value.value())
}

/// `serialize_with` for an `Option<IntEnum>` field.
pub fn ser_opt_int_enum<E: IntEnum, S: Serializer>(
    value: &Option<E>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(member) => serializer.serialize_i64(member.value()),
        None => serializer.serialize_none(),
    }
}

// ---------------------------------------------------------------------------
// Keys and cleaning
// ---------------------------------------------------------------------------

/// pydantic's `to_camel` alias generator, for ASCII field names.
pub fn to_camel(snake: &str) -> String {
    let bytes = snake.as_bytes();
    let already_camel = bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes.iter().all(u8::is_ascii_alphanumeric)
        && !bytes
            .windows(2)
            .any(|pair| pair[0].is_ascii_digit() && pair[1].is_ascii_lowercase());
    if already_camel {
        return snake.to_owned();
    }
    // str.title(): a letter is upper case after a non-letter, lower case otherwise.
    let mut titled = Vec::with_capacity(bytes.len());
    let mut after_letter = false;
    for &byte in bytes {
        titled.push(if !byte.is_ascii_alphabetic() {
            byte
        } else if after_letter {
            byte.to_ascii_lowercase()
        } else {
            byte.to_ascii_uppercase()
        });
        after_letter = byte.is_ascii_alphabetic();
    }
    // re.sub('([0-9A-Za-z])_(?=[0-9A-Z])', r'\1', ...)
    let mut pascal = Vec::with_capacity(titled.len());
    let mut index = 0;
    while index < titled.len() {
        let byte = titled[index];
        pascal.push(byte);
        let next = titled.get(index + 1).copied();
        let after = titled.get(index + 2).copied();
        if byte.is_ascii_alphanumeric()
            && next == Some(b'_')
            && after.is_some_and(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
        {
            index += 2;
        } else {
            index += 1;
        }
    }
    // re.sub('(^_*[A-Z])', lowercase, ...)
    if let Some(first) = pascal.iter().position(|&byte| byte != b'_')
        && pascal[first].is_ascii_uppercase()
    {
        pascal[first] = pascal[first].to_ascii_lowercase();
    }
    String::from_utf8(pascal).expect("ASCII stays ASCII")
}

/// `BydBaseModel._clean_dict`: applies key aliases (old → new, only when the
/// new key is absent, in order) and drops null and sentinel values.
pub fn clean_dict(values: &Map<String, Value>, aliases: &[(&str, &str)]) -> Map<String, Value> {
    let mut working = values.clone();
    for &(old, new) in aliases {
        if working.contains_key(old)
            && !working.contains_key(new)
            && let Some(value) = working.shift_remove(old)
        {
            working.insert(new.to_owned(), value);
        }
    }
    working.retain(|_, value| match value {
        Value::Null => false,
        Value::String(text) => !SENTINELS.contains(&py_strip(text)),
        _ => true,
    });
    working
}

/// `BydBaseModel._clean_byd_values`, the base before-validator: cleans a
/// dict and stores the uncleaned dict under `raw` unless the input had its
/// own `raw` key. Anything that is not a dict passes through untouched.
pub fn clean_byd_values(values: &Value, aliases: &[(&str, &str)]) -> Value {
    let Value::Object(original) = values else {
        return values.clone();
    };
    let mut cleaned = clean_dict(original, aliases);
    if !original.contains_key("raw") {
        cleaned.insert("raw".to_owned(), values.clone());
    }
    Value::Object(cleaned)
}

// ---------------------------------------------------------------------------
// Reading fields
// ---------------------------------------------------------------------------

/// How a field is found in the input dict.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    /// The Python field name, which is also accepted as a key (`populate_by_name=True`).
    pub name: &'static str,
    /// Explicit `validation_alias` keys, tried in order (`AliasChoices`); when
    /// empty the alias is `to_camel(name)`.
    pub aliases: &'static [&'static str],
}

impl From<&'static str> for Field {
    fn from(name: &'static str) -> Self {
        Self { name, aliases: &[] }
    }
}

/// A field with `Field(validation_alias=...)` or `AliasChoices(...)`.
pub const fn aliased(name: &'static str, aliases: &'static [&'static str]) -> Field {
    Field { name, aliases }
}

/// Reads a model's fields out of the cleaned input dict, the way pydantic does.
///
/// Each read looks the field up (alias first, then the field name), returns
/// the default when it is absent and converts it otherwise. Errors do not
/// stop the reads, just as pydantic validates every field before raising;
/// call [`Fields::finish`] once all fields are read. An exception other than
/// `ValidationError` (which pydantic does not catch) stops the remaining
/// conversions and wins; see [`Failures`].
pub struct Fields<'a> {
    values: &'a Map<String, Value>,
    camel_case: bool,
    failures: Failures,
}

impl<'a> Fields<'a> {
    /// For a `BydBaseModel`: camelCase aliases and field names both accepted.
    /// Fails like pydantic (`model_type`) when the input is not a dict.
    pub fn new(values: &'a Value) -> Result<Self, ValidationError> {
        match values {
            Value::Object(values) => Ok(Self {
                values,
                camel_case: true,
                failures: Failures::default(),
            }),
            _ => Err(ValidationError::new("model_type")),
        }
    }

    /// For a plain pydantic `BaseModel` with no alias generator: the alias is the field name.
    pub fn plain(values: &'a Value) -> Result<Self, ValidationError> {
        let mut fields = Self::new(values)?;
        fields.camel_case = false;
        Ok(fields)
    }

    fn find(&self, field: Field) -> Option<&'a Value> {
        let from_alias = if field.aliases.is_empty() {
            if self.camel_case {
                self.values.get(&to_camel(field.name))
            } else {
                None
            }
        } else {
            field
                .aliases
                .iter()
                .find_map(|alias| self.values.get(*alias))
        };
        from_alias.or_else(|| self.values.get(field.name))
    }

    fn record(&mut self, error: ValidationError) {
        self.failures.record(error);
    }

    fn stopped(&self) -> bool {
        self.failures.stopped()
    }

    /// A field with a default: `name: T = default`.
    pub fn get<T>(
        &mut self,
        field: impl Into<Field>,
        default: T,
        convert: impl FnOnce(&Value) -> Result<T, ValidationError>,
    ) -> T {
        let field = field.into();
        if self.stopped() {
            return default;
        }
        let Some(value) = self.find(field) else {
            return default;
        };
        match convert(value) {
            Ok(converted) => converted,
            Err(error) => {
                self.record(error.at(field.name));
                default
            }
        }
    }

    /// An optional field: `name: T | None = None`.
    pub fn opt<T>(
        &mut self,
        field: impl Into<Field>,
        convert: impl FnOnce(&Value) -> Result<T, ValidationError>,
    ) -> Option<T> {
        self.get(field, None, |value| coerce::optional(value, convert))
    }

    /// A field without a default. A missing key is pydantic's `missing` error.
    pub fn required<T: Default>(
        &mut self,
        field: impl Into<Field>,
        convert: impl FnOnce(&Value) -> Result<T, ValidationError>,
    ) -> T {
        let field = field.into();
        if !self.stopped() && self.find(field).is_none() {
            self.record(ValidationError::new("missing").at(field.name));
            return T::default();
        }
        self.get(field, T::default(), convert)
    }

    /// The `raw: dict[str, Any]` field every `BydBaseModel` has.
    pub fn raw(&mut self) -> Map<String, Value> {
        self.get("raw", Map::new(), coerce::any_dict)
    }

    /// The first error, if any field failed.
    pub fn finish(self) -> Result<(), ValidationError> {
        self.failures.finish()
    }
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/// A pyBYD reply model.
///
/// `serde_json::to_value(&model)` is exactly `model.model_dump(mode="json")`.
pub trait BydModel: Serialize + Sized {
    /// `Model.model_validate(raw, context=context)`. `context` is JSON null when there is none.
    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError>;

    /// Every `@property` of the Python class, by name, as JSON.
    fn properties(&self) -> Map<String, Value> {
        Map::new()
    }

    /// `model_dump(mode="json")`.
    fn dump(&self) -> Value {
        serde_json::to_value(self).expect("models serialise to JSON")
    }
}

/// Builds the property map from `(name, value)` pairs.
pub fn properties<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
}

/// Answers a model's parity call: `{"raw", "context"}` in, `{"dump", "properties"}` out.
pub(crate) fn model_call<M: BydModel>(args: &Value) -> Outcome {
    let args = Args(args);
    let model = M::parse(args.value("raw"), args.value("context"))?;
    Ok(json!({"dump": model.dump(), "properties": model.properties()}))
}

/// `BydBaseModel` itself, which has only the `raw` field.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BydBaseModel {
    /// Original API response dict.
    pub raw: Map<String, Value>,
}

impl BydModel for BydBaseModel {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let values = clean_byd_values(raw, &[]);
        let mut fields = Fields::new(&values)?;
        let model = Self { raw: fields.raw() };
        fields.finish()?;
        Ok(model)
    }
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

fn number_arg(args: &Args<'_>) -> Result<f64, ParityError> {
    args.value("value")
        .as_f64()
        .ok_or_else(|| ParityError::bad_arguments("value must be a number"))
}

fn aliases_arg<'a>(args: &Args<'a>) -> Result<Vec<(&'a str, &'a str)>, ParityError> {
    let Some(aliases) = args.value("aliases").as_object() else {
        return Err(ParityError::bad_arguments("aliases must be an object"));
    };
    aliases
        .iter()
        .map(|(old, new)| {
            new.as_str()
                .map(|new| (old.as_str(), new))
                .ok_or_else(|| ParityError::bad_arguments("alias targets must be strings"))
        })
        .collect()
}

/// `BydEnum(value)` for a class whose members have these values, as the index
/// of the member `_missing_` and Python equality pick: the member equal to
/// `value`, else `UNKNOWN` (`-1`), else the first member. `None` if there are
/// no members (Python raises `StopIteration` there).
pub fn byd_enum_member(members: &[i64], value: &Value) -> Option<usize> {
    members
        .iter()
        .position(|member| py_equals_int(value, *member))
        .or_else(|| members.iter().position(|member| *member == -1))
        .or_else(|| (!members.is_empty()).then_some(0))
}

/// `models.base.BydEnum`: `members` are the values of a made-up subclass (the
/// one at `-1` is named `UNKNOWN`, the rest `M<index>`), `value` is what it is called with.
fn byd_enum_call(args: &Args<'_>) -> Outcome {
    let members: Vec<i64> = args
        .value("members")
        .as_array()
        .and_then(|items| items.iter().map(Value::as_i64).collect())
        .ok_or_else(|| ParityError::bad_arguments("members must be an array of integers"))?;
    let index = byd_enum_member(&members, args.value("value"))
        .ok_or_else(|| ParityError::bad_arguments("members must not be empty"))?;
    let name = if members[index] == -1 {
        "UNKNOWN".to_owned()
    } else {
        format!("M{index}")
    };
    Ok(json!({"name": name, "value": members[index]}))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "models.base.BydEnum" => byd_enum_call(&args),
        "models.base.normalize_unit" => {
            args.opt_str("text").map(|text| json!(normalize_unit(text)))
        }
        "models.base.is_negative" => number_arg(&args).map(|value| json!(is_negative(value))),
        "models.base.is_temp_sentinel" => {
            number_arg(&args).map(|value| json!(is_temp_sentinel(value)))
        }
        "models.base.to_camel" => args.str("name").map(|name| json!(to_camel(name))),
        "models.base.parse_byd_timestamp" => parse_byd_timestamp(args.value("value"))
            .map(|datetime| json!(datetime.map(|datetime| datetime.isoformat())))
            .map_err(ParityError::from),
        "models.base.BydTimestamp" => byd_timestamp(args.value("value"))
            .map(|datetime| json!(datetime.map(|datetime| datetime.to_json())))
            .map_err(ParityError::from),
        "models.base.clean_dict" => match (args.value("values").as_object(), aliases_arg(&args)) {
            (Some(values), Ok(aliases)) => Ok(Value::Object(clean_dict(values, &aliases))),
            (None, _) => Err(ParityError::bad_arguments("values must be an object")),
            (_, Err(error)) => Err(error),
        },
        "models.base.BydBaseModel" => model_call::<BydBaseModel>(args.0),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byd_enum_falls_back_to_unknown_then_the_first_member() {
        assert_eq!(byd_enum_member(&[-1, 0, 1], &json!(1.0)), Some(2));
        assert_eq!(byd_enum_member(&[-1, 0, 1], &json!("1")), Some(0));
        assert_eq!(byd_enum_member(&[3, 4], &json!(null)), Some(0));
        assert_eq!(byd_enum_member(&[3, -1, 4], &json!(true)), Some(1));
        assert_eq!(byd_enum_member(&[], &json!(1)), None);
        assert!(byd_enum_call(&Args(&json!({"members": [], "value": 1}))).is_err());
    }

    fn datetime(text: &str) -> PyDateTime {
        parse_byd_timestamp(&json!(text)).unwrap().unwrap()
    }

    #[test]
    fn epochs_and_their_limits() {
        assert_eq!(datetime("1700000000").to_json(), "2023-11-14T22:13:20Z");
        assert_eq!(
            datetime("1700000000123").isoformat(),
            "2023-11-14T22:13:20+00:00"
        );
        assert_eq!(parse_byd_timestamp(&json!(0)).unwrap(), None);
        assert_eq!(parse_byd_timestamp(&json!("--5")).unwrap(), None);
        assert_eq!(parse_byd_timestamp(&json!("1\u{b2}")).unwrap(), None);
        assert_eq!(
            parse_byd_timestamp(&json!(f64::MAX)).unwrap_err().exception,
            "OverflowError"
        );
        assert_eq!(
            parse_byd_timestamp(&json!("9".repeat(40)))
                .unwrap_err()
                .exception,
            "OverflowError"
        );
        assert_eq!(
            parse_byd_timestamp(&json!(1e20)).unwrap_err().exception,
            "OSError"
        );
        assert_eq!(
            parse_byd_timestamp(&json!(u64::MAX)).unwrap_err().exception,
            "ValueError"
        );
        assert_eq!(
            parse_byd_timestamp(&json!(253_402_300_800_i64))
                .unwrap_err()
                .exception,
            "ValueError"
        );
        assert_eq!(
            byd_timestamp(&json!(253_402_300_800_i64))
                .unwrap_err()
                .exception,
            "ValidationError"
        );
        assert_eq!(
            byd_timestamp(&json!(1e20)).unwrap_err().exception,
            "OSError"
        );
        assert_eq!(datetime("253402300799").to_json(), "9999-12-31T23:59:59Z");
    }

    #[test]
    fn offsets_print_like_python_and_pydantic() {
        let parsed = datetime("2024-01-01T10:00:00.5-00:30:15.25");
        assert_eq!(
            parsed.isoformat(),
            "2024-01-01T10:00:00.500000-00:30:15.250000"
        );
        assert_eq!(parsed.to_json(), "2024-01-01T10:00:00.500000-00:30");
        let tiny = PyDateTime {
            offset_us: -400_000,
            ..parsed
        };
        assert_eq!(tiny.to_json(), "2024-01-01T10:00:00.500000Z");
        let half = PyDateTime {
            offset_us: -500_000,
            ..parsed
        };
        assert_eq!(half.to_json(), "2024-01-01T10:00:00.500000-00:00");
        let near_day = PyDateTime {
            offset_us: 86_399_500_000,
            ..parsed
        };
        assert_eq!(near_day.to_json(), "2024-01-01T10:00:00.500000+24:00");
        assert_eq!(
            parsed.to_chrono().naive_utc().to_string(),
            "2024-01-01 10:30:15.750"
        );
    }

    #[test]
    fn fromisoformat_follows_cpython() {
        let ok = |text: &str| datetime_fromisoformat(text).is_some();
        assert!(ok("2024-01-01é10:00"));
        assert!(ok("2024-01-01比10:00"));
        assert!(ok("2024-01-01\u{1F697}10:00"));
        assert!(ok("2020-W53-7"));
        assert!(ok("2024W01110:00"));
        assert!(!ok("2024W0110:00"));
        assert!(ok("2024-01-01T100000123"));
        assert!(!ok("2024-W53"));
        assert!(!ok("2024-W01-"));
        assert!(!ok("2024-01-01T"));
        assert!(!ok("9999-W52-6"));
        assert!(!ok("2024-01-01T10:00+24:00"));
        assert!(!ok("2024-01-01T10:00:00.12a"));
        assert!(!ok("2024-01-01T10:00Zx"));
        assert!(!ok("0000-01-01"));
        assert!(!ok("2024-W00"));
        assert!(!ok("2024-W01-8"));
        assert_eq!(
            datetime_fromisoformat("2024-01-01T10:00+00:00:00.5")
                .unwrap()
                .1,
            Some(0)
        );
    }

    #[test]
    fn camel_case() {
        assert_eq!(to_camel("gps_timestamp"), "gpsTimestamp");
        assert_eq!(to_camel("lr_3rd_heat"), "lr3RdHeat");
        assert_eq!(to_camel("soc"), "soc");
        assert_eq!(to_camel("a__b"), "a__B");
        assert_eq!(to_camel("__x"), "__x");
        assert_eq!(to_camel("Already"), "already");
    }

    #[test]
    fn sentinel_rules() {
        let mut value = Some(-1_i64);
        clear_if(&mut value, is_negative);
        assert_eq!(value, None);
        let mut value = Some(-129.0);
        clear_if(&mut value, is_temp_sentinel);
        assert_eq!(value, None);
        let mut value = Some(3.0);
        clear_if(&mut value, is_temp_sentinel);
        assert_eq!(value, Some(3.0));
    }

    #[test]
    fn fields_report_the_first_error_and_let_escaping_errors_win() {
        let values = json!({"a": "x", "b": 1, "c": 1e300});
        let mut fields = Fields::new(&values).unwrap();
        assert_eq!(fields.get("a", 0, coerce::int), 0);
        assert_eq!(fields.required::<i64>("missing", coerce::int), 0);
        assert_eq!(fields.get("c", None, byd_timestamp), None);
        assert_eq!(
            fields.get("b", 0, coerce::int),
            0,
            "reads stop after an escaping error"
        );
        assert_eq!(fields.finish().unwrap_err().exception, "OverflowError");

        let mut fields = Fields::plain(&values).unwrap();
        assert_eq!(fields.required::<i64>("b", coerce::int), 1);
        assert_eq!(fields.required::<i64>("missing", coerce::int), 0);
        let error = fields.finish().unwrap_err();
        assert_eq!(
            (error.error_type, error.loc),
            ("missing", vec!["missing".to_owned()])
        );
        assert!(Fields::plain(&json!(1)).is_err());
    }

    #[test]
    fn serde_helpers() {
        #[derive(Serialize)]
        struct Sample {
            #[serde(serialize_with = "ser_float")]
            a: f64,
            #[serde(serialize_with = "ser_float_list")]
            b: Vec<f64>,
            #[serde(serialize_with = "ser_datetime")]
            c: PyDateTime,
            #[serde(serialize_with = "ser_opt_datetime")]
            d: Option<PyDateTime>,
            #[serde(serialize_with = "ser_int_enum")]
            e: Probe,
            #[serde(serialize_with = "ser_opt_int_enum")]
            f: Option<Probe>,
            #[serde(serialize_with = "ser_opt_int_enum")]
            g: Option<Probe>,
        }
        #[derive(Clone, Copy)]
        struct Probe;
        impl IntEnum for Probe {
            const MEMBERS: &'static [Self] = &[Probe];
            const MISSING: Option<Self> = None;
            fn value(self) -> i64 {
                7
            }
        }
        let when = datetime("1");
        let sample = Sample {
            a: f64::NAN,
            b: vec![1.0, f64::INFINITY],
            c: when,
            d: None,
            e: Probe,
            f: Some(Probe),
            g: None,
        };
        assert_eq!(
            serde_json::to_value(sample).unwrap(),
            json!({"a": "NaN", "b": [1.0, "Infinity"], "c": "1970-01-01T00:00:01Z", "d": null, "e": 7, "f": 7, "g": null})
        );
    }

    #[test]
    fn bad_parity_arguments() {
        let call = |name: &str, args: Value| parity_call(name, &args).unwrap().unwrap_err().kind;
        assert_eq!(
            call("models.base.is_negative", json!({"value": "x"})),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.base.clean_dict",
                json!({"values": 1, "aliases": {}})
            ),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.base.clean_dict",
                json!({"values": {}, "aliases": 1})
            ),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.base.clean_dict",
                json!({"values": {}, "aliases": {"a": 1}})
            ),
            "BadArguments"
        );
        assert!(parity_call("models.base.nothing", &json!({})).is_none());
    }
}
