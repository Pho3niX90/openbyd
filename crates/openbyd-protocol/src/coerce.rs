//! Value conversions that match pydantic's lax mode, which pyBYD's models rely on.
//!
//! pyBYD validates every reply with pydantic v2 in its default (lax) mode, so
//! a field declared `int` also takes `1.0`, `"  12 "`, `"1_000"` or `True`.
//! Each function here takes the JSON value a model field receives and
//! returns what pydantic would store, or the [`ValidationError`] pydantic
//! would raise. The behaviour was measured against pydantic 2.14 (core
//! 2.50); `docs/porting-models.md` has the table.
//!
//! Integers are `i64`. pydantic keeps Python's unbounded integers, which
//! `serde_json::Value` cannot carry; a value outside `i64` that pydantic
//! would accept gives an error whose `exception` is `"Unsupported"`.
//!
//! Python's `float('nan')` and infinities have no JSON form. Wherever a
//! converted value is written as JSON, [`float_json`] writes them as the
//! strings `"NaN"`, `"Infinity"` and `"-Infinity"`; the parity oracles do the
//! same.

use std::fmt;

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Number, Value, json};

use crate::parity::{Args, Outcome, ParityError};

/// Why a value was rejected.
///
/// `exception` is the Python exception class pyBYD raises: almost always
/// pydantic's `"ValidationError"`, but a few pyBYD validators let other
/// exceptions (`"OverflowError"`, `"OSError"`, `"ValueError"`) escape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// The Python exception class name.
    pub exception: &'static str,
    /// pydantic's error type, such as `"int_parsing"`; informational.
    pub error_type: &'static str,
    /// Where the bad value was: field names and list indexes, outermost first.
    pub loc: Vec<String>,
}

impl ValidationError {
    /// A pydantic `ValidationError` of the given error type.
    pub fn new(error_type: &'static str) -> Self {
        Self::raised("ValidationError", error_type)
    }

    /// Another Python exception, which pydantic does not wrap and so escapes `model_validate`.
    pub fn raised(exception: &'static str, error_type: &'static str) -> Self {
        Self {
            exception,
            error_type,
            loc: Vec::new(),
        }
    }

    /// A value pydantic accepts but `i64` cannot hold.
    pub fn unsupported_int() -> Self {
        Self::raised("Unsupported", "int_outside_i64")
    }

    /// Whether this is a pydantic `ValidationError` (which pydantic collects) rather than an escaping exception.
    pub fn is_validation(&self) -> bool {
        self.exception == "ValidationError"
    }

    /// Whether this is an exception pydantic does not catch, which stops validation at once.
    pub fn is_escaping(&self) -> bool {
        !self.is_validation() && self.exception != "Unsupported"
    }

    /// Which error wins when several fields fail: an escaping exception
    /// (pydantic never got to the rest), then a `ValidationError`, then
    /// `Unsupported` (pydantic would have accepted that value).
    fn rank(&self) -> u8 {
        if self.is_escaping() {
            2
        } else if self.is_validation() {
            1
        } else {
            0
        }
    }

    /// The same error, located one level further in.
    pub fn at(mut self, loc: impl Into<String>) -> Self {
        self.loc.insert(0, loc.into());
        self
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.exception, self.error_type)?;
        if !self.loc.is_empty() {
            write!(f, " at {}", self.loc.join("."))?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationError {}

impl From<ValidationError> for ParityError {
    fn from(error: ValidationError) -> Self {
        ParityError::new(error.exception, error.to_string())
    }
}

/// Collects the errors of several values the way pydantic reports them: all
/// values are validated, one error is raised, and an escaping exception stops
/// the rest.
#[derive(Debug, Default)]
pub struct Failures(Option<ValidationError>);

impl Failures {
    /// Notes an error; the first of the highest rank is kept.
    pub fn record(&mut self, error: ValidationError) {
        if self
            .0
            .as_ref()
            .is_none_or(|kept| error.rank() > kept.rank())
        {
            self.0 = Some(error);
        }
    }

    /// Whether an escaping exception was seen, so nothing more should run.
    pub fn stopped(&self) -> bool {
        self.0.as_ref().is_some_and(ValidationError::is_escaping)
    }

    /// The error to raise, if any.
    pub fn finish(self) -> Result<(), ValidationError> {
        self.0.map_or(Ok(()), Err)
    }
}

/// Converts every item, then fails with the winning error, if any.
fn collect_all<K, T>(
    items: impl Iterator<Item = (K, Result<T, ValidationError>)>,
    loc: impl Fn(&K) -> String,
) -> Result<Vec<(K, T)>, ValidationError> {
    let mut failures = Failures::default();
    let mut converted = Vec::new();
    for (key, result) in items {
        match result {
            Ok(value) => converted.push((key, value)),
            Err(error) => {
                let location = loc(&key);
                failures.record(error.at(location));
                if failures.stopped() {
                    break;
                }
            }
        }
    }
    failures.finish().map(|()| converted)
}

// ---------------------------------------------------------------------------
// Python string semantics
// ---------------------------------------------------------------------------

/// Python's `str.isspace()`: Unicode White_Space plus the four ASCII separators U+001C to U+001F.
pub fn py_is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python's `str.strip()` with no argument.
pub fn py_strip(text: &str) -> &str {
    text.trim_matches(py_is_space)
}

/// The value of a Unicode decimal digit (category Nd), as Python's `int()` reads it.
pub fn py_decimal(c: char) -> Option<u32> {
    let code = u32::from(c);
    DECIMAL_ZEROS
        .iter()
        .find(|&&zero| (zero..zero + 10).contains(&code))
        .map(|zero| code - zero)
}

/// Python's `str.isdigit()` for one character: decimal digits plus superscripts, circled digits and the like.
pub fn py_is_digit(c: char) -> bool {
    let code = u32::from(c);
    py_decimal(c).is_some()
        || DIGIT_ONLY
            .iter()
            .any(|&(first, last)| (first..=last).contains(&code))
}

// Generated from Python 3.12 (Unicode 15.0): the zero of every run of ten
// decimal digits, and the characters that are digits but not decimals.
const DECIMAL_ZEROS: &[u32] = &[
    0x30, 0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66,
    0xDE6, 0xE50, 0xED0, 0xF20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90,
    0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10,
    0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0, 0x11650,
    0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0, 0x11F50, 0x16A60, 0x16AC0,
    0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E950,
    0x1FBF0,
];
const DIGIT_ONLY: &[(u32, u32)] = &[
    (0xB2, 0xB3),
    (0xB9, 0xB9),
    (0x1369, 0x1371),
    (0x19DA, 0x19DA),
    (0x2070, 0x2070),
    (0x2074, 0x2079),
    (0x2080, 0x2089),
    (0x2460, 0x2468),
    (0x2474, 0x247C),
    (0x2488, 0x2490),
    (0x24EA, 0x24EA),
    (0x24F5, 0x24FD),
    (0x24FF, 0x24FF),
    (0x2776, 0x277E),
    (0x2780, 0x2788),
    (0x278A, 0x2792),
    (0x10A40, 0x10A43),
    (0x10E60, 0x10E68),
    (0x11052, 0x1105A),
    (0x1F100, 0x1F10A),
];

// ---------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------

/// A JSON number as Python sees it after `json.loads`.
enum PyNumber {
    Int(i64),
    /// An integer above `i64::MAX` (serde_json keeps up to `u64::MAX`).
    BigInt,
    Float(f64),
}

fn py_number(number: &Number) -> PyNumber {
    if let Some(int) = number.as_i64() {
        PyNumber::Int(int)
    } else if number.is_u64() {
        PyNumber::BigInt
    } else {
        PyNumber::Float(number.as_f64().unwrap_or(f64::NAN))
    }
}

/// Integer text as pydantic reads it.
#[derive(Debug, PartialEq, Eq)]
enum IntText {
    Int(i64),
    /// Valid, but outside `i64`.
    Big {
        negative: bool,
    },
    Invalid,
}

/// Plain integer text, `[+-]?[0-9]+`, as Rust's `i64::from_str` (or a big integer) reads it.
fn parse_plain_int(text: &str) -> IntText {
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return IntText::Invalid;
    }
    let mut magnitude: i128 = 0;
    for byte in digits.bytes() {
        magnitude = magnitude
            .saturating_mul(10)
            .saturating_add(i128::from(byte - b'0'));
    }
    let value = if negative { -magnitude } else { magnitude };
    i64::try_from(value).map_or(IntText::Big { negative }, IntText::Int)
}

/// pydantic-core's `strip_leading_zeros`: skips leading zeros and
/// underscores and returns the rest from the first other digit or a `-`.
/// All zeros and underscores leave the last character.
fn strip_leading_zeros(text: &str) -> Option<&str> {
    let mut chars = text.char_indices();
    match chars.next() {
        Some((_, '0')) => {}
        Some((_, '1'..='9')) => return Some(text),
        _ => return None,
    }
    for (index, c) in chars {
        match c {
            '0' | '_' => {}
            '1'..='9' | '-' => return Some(&text[index..]),
            _ => return None,
        }
    }
    Some(&text[text.len() - 1..])
}

/// A JSON integer, `-?(0|[1-9][0-9]*)`: no `+` and no leading zeros.
fn parse_json_int(text: &str) -> IntText {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.len() > 1 && digits.starts_with('0') || text.starts_with('+') {
        return IntText::Invalid;
    }
    parse_plain_int(text)
}

/// pydantic-core's `_parse_str`: plain text first (Rust's `i64::from_str`,
/// which takes `+` and leading zeros); failing that, one sign is taken off,
/// leading zeros and single underscores are removed, and what is left must
/// be a JSON integer. So `"0__1"` is 1 and `"0_-5"` is -5, but `"0-05"` fails.
fn parse_int_body(text: &str) -> IntText {
    let plain = parse_plain_int(text);
    if plain != IntText::Invalid {
        return plain;
    }
    let (sign, rest) = match text.as_bytes().first() {
        Some(b'-') => ("-", &text[1..]),
        Some(b'+') => ("", &text[1..]),
        _ => ("", text),
    };
    strip_leading_zeros(rest)
        .and_then(strip_underscores)
        .map_or(IntText::Invalid, |digits| {
            parse_json_int(&format!("{sign}{digits}"))
        })
}

/// pydantic's str → int: the trimmed text, or failing that the part before
/// a `.` followed only by zeros.
fn parse_int_text(text: &str) -> IntText {
    let text = text.trim();
    match parse_int_body(text) {
        IntText::Invalid => match text.split_once('.') {
            Some((whole, tail)) if !tail.is_empty() && tail.bytes().all(|byte| byte == b'0') => {
                parse_int_body(whole)
            }
            _ => IntText::Invalid,
        },
        parsed => parsed,
    }
}

/// pydantic's float → int: whole numbers strictly inside the `i64` range.
fn float_to_int(value: f64) -> Result<i64, ValidationError> {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0; // 2^63
    if !value.is_finite() {
        Err(ValidationError::new("finite_number"))
    } else if value.fract() != 0.0 {
        Err(ValidationError::new("int_from_float"))
    } else if value <= -LIMIT || value >= LIMIT {
        Err(ValidationError::new("int_parsing_size"))
    } else {
        Ok(value as i64)
    }
}

/// Removes single underscores the way pydantic does before parsing a float:
/// none at either end and never two in a row.
fn strip_underscores(text: &str) -> Option<String> {
    if text.starts_with('_') || text.ends_with('_') || text.contains("__") {
        None
    } else {
        Some(text.replace('_', ""))
    }
}

/// pydantic's str → float. The trimmed text is tried first; failing that, the
/// untrimmed text without underscores (so `" 1_0 "` is rejected).
fn parse_float_text(text: &str) -> Option<f64> {
    text.trim()
        .parse()
        .ok()
        .or_else(|| strip_underscores(text)?.parse().ok())
}

/// A float as JSON: non-finite values become `"NaN"`, `"Infinity"` or `"-Infinity"`.
pub fn float_json(value: f64) -> Value {
    if value.is_nan() {
        json!("NaN")
    } else if value.is_infinite() {
        json!(if value > 0.0 { "Infinity" } else { "-Infinity" })
    } else {
        json!(value)
    }
}

// ---------------------------------------------------------------------------
// Scalar fields
// ---------------------------------------------------------------------------

/// An `int` field's value: an `i64`, or the sign of a valid integer outside it.
enum PyInt {
    Small(i64),
    Big { negative: bool },
}

fn py_int(value: &Value) -> Result<PyInt, ValidationError> {
    match value {
        Value::Bool(flag) => Ok(PyInt::Small(i64::from(*flag))),
        Value::Number(number) => match py_number(number) {
            PyNumber::Int(int) => Ok(PyInt::Small(int)),
            PyNumber::BigInt => Ok(PyInt::Big { negative: false }),
            PyNumber::Float(float) => float_to_int(float).map(PyInt::Small),
        },
        Value::String(text) => match parse_int_text(text) {
            IntText::Int(int) => Ok(PyInt::Small(int)),
            IntText::Big { negative } => Ok(PyInt::Big { negative }),
            IntText::Invalid => Err(ValidationError::new("int_parsing")),
        },
        _ => Err(ValidationError::new("int_type")),
    }
}

/// A field declared `int`.
pub fn int(value: &Value) -> Result<i64, ValidationError> {
    match py_int(value)? {
        PyInt::Small(int) => Ok(int),
        PyInt::Big { .. } => Err(ValidationError::unsupported_int()),
    }
}

/// A field declared `float`. Strings such as `"nan"` and `"-Infinity"` give non-finite values.
pub fn float(value: &Value) -> Result<f64, ValidationError> {
    match value {
        Value::Bool(flag) => Ok(f64::from(u8::from(*flag))),
        Value::Number(number) => Ok(match py_number(number) {
            PyNumber::Int(int) => int as f64,
            PyNumber::BigInt | PyNumber::Float(_) => number.as_f64().unwrap_or(f64::NAN),
        }),
        Value::String(text) => {
            parse_float_text(text).ok_or_else(|| ValidationError::new("float_parsing"))
        }
        _ => Err(ValidationError::new("float_type")),
    }
}

/// A field declared `str`. Only strings pass; numbers are not turned into text.
pub fn string(value: &Value) -> Result<String, ValidationError> {
    match value {
        Value::String(text) => Ok(text.clone()),
        _ => Err(ValidationError::new("string_type")),
    }
}

/// A field declared `bool`: `0`/`1`, `0.0`/`1.0` and a fixed set of words, without trimming.
pub fn boolean(value: &Value) -> Result<bool, ValidationError> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        Value::Number(number) => match py_number(number) {
            PyNumber::Int(0) => Ok(false),
            PyNumber::Int(1) => Ok(true),
            PyNumber::Int(_) | PyNumber::BigInt => Err(ValidationError::new("bool_parsing")),
            PyNumber::Float(0.0) => Ok(false), // also -0.0
            PyNumber::Float(1.0) => Ok(true),
            PyNumber::Float(_) => Err(ValidationError::new("bool_type")),
        },
        Value::String(text) => match text.to_ascii_lowercase().as_str() {
            "0" | "off" | "f" | "false" | "n" | "no" => Ok(false),
            "1" | "on" | "t" | "true" | "y" | "yes" => Ok(true),
            _ => Err(ValidationError::new("bool_parsing")),
        },
        _ => Err(ValidationError::new("bool_type")),
    }
}

/// A field declared `int` with `Field(ge=..., le=...)`.
pub fn int_bounded(
    value: &Value,
    ge: Option<i64>,
    le: Option<i64>,
) -> Result<i64, ValidationError> {
    let number = match py_int(value)? {
        PyInt::Small(number) => number,
        // Outside i64 a bound still decides, as long as it is on the right side.
        PyInt::Big { negative: true } if ge.is_some() => {
            return Err(ValidationError::new("greater_than_equal"));
        }
        PyInt::Big { negative: false } if le.is_some() => {
            return Err(ValidationError::new("less_than_equal"));
        }
        PyInt::Big { .. } => return Err(ValidationError::unsupported_int()),
    };
    if ge.is_some_and(|ge| number < ge) {
        Err(ValidationError::new("greater_than_equal"))
    } else if le.is_some_and(|le| number > le) {
        Err(ValidationError::new("less_than_equal"))
    } else {
        Ok(number)
    }
}

/// A field declared `Any`: the value as it came.
pub fn any(value: &Value) -> Result<Value, ValidationError> {
    Ok(value.clone())
}

/// `int | str` in pydantic's smart union mode: a string stays a string, anything else is tried as `int`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum IntOrStr {
    /// The value was a number or a boolean.
    Int(i64),
    /// The value was a string, kept as sent.
    Str(String),
}

/// A field declared `int | str`.
pub fn int_or_str(value: &Value) -> Result<IntOrStr, ValidationError> {
    match value {
        Value::String(text) => Ok(IntOrStr::Str(text.clone())),
        other => int(other).map(IntOrStr::Int).map_err(|error| {
            if error.is_validation() {
                ValidationError::new("union")
            } else {
                error
            }
        }),
    }
}

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

/// `T | None`: JSON null is `None`, anything else goes through `convert`.
pub fn optional<T>(
    value: &Value,
    convert: impl FnOnce(&Value) -> Result<T, ValidationError>,
) -> Result<Option<T>, ValidationError> {
    match value {
        Value::Null => Ok(None),
        other => convert(other).map(Some),
    }
}

/// `list[T]`. Only a JSON array passes (a string is not a list of characters).
pub fn list<T>(
    value: &Value,
    item: impl Fn(&Value) -> Result<T, ValidationError>,
) -> Result<Vec<T>, ValidationError> {
    let Value::Array(items) = value else {
        return Err(ValidationError::new("list_type"));
    };
    let converted = collect_all(
        items
            .iter()
            .enumerate()
            .map(|(index, element)| (index, item(element))),
        usize::to_string,
    )?;
    Ok(converted.into_iter().map(|(_, value)| value).collect())
}

/// `dict[str, T]`, keeping the keys in the order they came.
pub fn dict<T>(
    value: &Value,
    item: impl Fn(&Value) -> Result<T, ValidationError>,
) -> Result<IndexMap<String, T>, ValidationError> {
    let Value::Object(entries) = value else {
        return Err(ValidationError::new("dict_type"));
    };
    let converted = collect_all(
        entries
            .iter()
            .map(|(key, element)| (key.clone(), item(element))),
        String::clone,
    )?;
    Ok(converted.into_iter().collect())
}

/// `dict[str, Any]`, the type of every model's `raw` field.
pub fn any_dict(value: &Value) -> Result<serde_json::Map<String, Value>, ValidationError> {
    match value {
        Value::Object(entries) => Ok(entries.clone()),
        _ => Err(ValidationError::new("dict_type")),
    }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// A Python `IntEnum`, including pyBYD's `BydEnum` subclasses.
///
/// ```
/// use openbyd_protocol::coerce::IntEnum;
///
/// #[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// pub enum DoorOpenState { Unknown = -1, Closed = 0, Open = 1 }
///
/// impl IntEnum for DoorOpenState {
///     const MEMBERS: &'static [Self] = &[Self::Unknown, Self::Closed, Self::Open];
///     const MISSING: Option<Self> = Some(Self::Unknown);
///     fn value(self) -> i64 { self as i64 }
/// }
/// ```
pub trait IntEnum: Sized + Copy + 'static {
    /// Every member, in definition order.
    const MEMBERS: &'static [Self];
    /// What `_missing_` gives for a value with no member: `UNKNOWN` for a
    /// `BydEnum` (or its first member if it has no `UNKNOWN`), `None` for a
    /// plain `IntEnum`, which pydantic then rejects.
    const MISSING: Option<Self>;
    /// The member's integer value.
    fn value(self) -> i64;
}

/// A field whose type is an `IntEnum`.
pub fn int_enum<E: IntEnum>(value: &Value) -> Result<E, ValidationError> {
    let values: Vec<i64> = E::MEMBERS.iter().map(|member| member.value()).collect();
    let missing = E::MISSING.map(IntEnum::value);
    let found = int_enum_value(value, &values, missing)?;
    Ok(*E::MEMBERS
        .iter()
        .find(|member| member.value() == found)
        .expect("int_enum_value returns a member value"))
}

/// The value pydantic picks for an `IntEnum` with the given member values.
///
/// The input is read as an integer the way an `int` field reads it; if that
/// fails or no member has the value, `_missing_` decides (`missing`). An
/// integer outside `i64` is an error even when `_missing_` exists.
pub fn int_enum_value(
    value: &Value,
    members: &[i64],
    missing: Option<i64>,
) -> Result<i64, ValidationError> {
    let candidate = match value {
        Value::Bool(flag) => Some(i64::from(*flag)),
        Value::Number(number) => match py_number(number) {
            PyNumber::Int(int) => Some(int),
            PyNumber::BigInt => return Err(ValidationError::new("int_parsing_size")),
            PyNumber::Float(float) => float_to_int(float).ok(),
        },
        Value::String(text) => match parse_int_text(text) {
            IntText::Int(int) => Some(int),
            IntText::Big { .. } => return Err(ValidationError::new("int_parsing_size")),
            IntText::Invalid => None,
        },
        _ => None,
    };
    match candidate {
        Some(found) if members.contains(&found) => Ok(found),
        _ => missing.ok_or_else(|| ValidationError::new("enum")),
    }
}

/// A Python `StrEnum`.
pub trait StrEnum: Sized + Copy + 'static {
    /// Every member, in definition order.
    const MEMBERS: &'static [Self];
    /// The member's string value.
    fn value(self) -> &'static str;
}

/// A field whose type is a `StrEnum`: only the exact value string passes.
pub fn str_enum<E: StrEnum>(value: &Value) -> Result<E, ValidationError> {
    E::MEMBERS
        .iter()
        .copied()
        .find(|member| value.as_str() == Some(member.value()))
        .ok_or_else(|| ValidationError::new("enum"))
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

fn to_json<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("converted values serialise")
}

fn float_list_json(values: &[f64]) -> Value {
    Value::Array(values.iter().copied().map(float_json).collect())
}

fn i64_list(args: &Args<'_>, key: &str) -> Result<Vec<i64>, ParityError> {
    args.value(key)
        .as_array()
        .and_then(|items| items.iter().map(Value::as_i64).collect())
        .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be a list of integers")))
}

fn opt_i64(args: &Args<'_>, key: &str) -> Result<Option<i64>, ParityError> {
    match args.value(key) {
        Value::Null => Ok(None),
        other => other
            .as_i64()
            .map(Some)
            .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be an integer"))),
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    let value = args.value("value");
    let result: Result<Value, ValidationError> = match name {
        "coerce.int" => int(value).map(to_json),
        "coerce.float" => float(value).map(float_json),
        "coerce.str" => string(value).map(to_json),
        "coerce.bool" => boolean(value).map(to_json),
        "coerce.any" => any(value),
        "coerce.optional_int" => optional(value, int).map(to_json),
        "coerce.optional_float" => {
            optional(value, float).map(|v| v.map_or(Value::Null, float_json))
        }
        "coerce.optional_str" => optional(value, string).map(to_json),
        "coerce.optional_bool" => optional(value, boolean).map(to_json),
        "coerce.list_int" => list(value, int).map(to_json),
        "coerce.list_float" => list(value, float).map(|v| float_list_json(&v)),
        "coerce.list_str" => list(value, string).map(to_json),
        "coerce.dict_any" => any_dict(value).map(Value::Object),
        "coerce.dict_str_int" => dict(value, int).map(to_json),
        "coerce.dict_str_bool" => dict(value, boolean).map(to_json),
        "coerce.int_or_str" => int_or_str(value).map(to_json),
        "coerce.int_bounded" => {
            let bounds = opt_i64(&args, "ge").and_then(|ge| Ok((ge, opt_i64(&args, "le")?)));
            match bounds {
                Ok((ge, le)) => int_bounded(value, ge, le).map(to_json),
                Err(error) => return Some(Err(error)),
            }
        }
        "coerce.int_enum" => {
            let members = match i64_list(&args, "members") {
                Ok(members) => members,
                Err(error) => return Some(Err(error)),
            };
            let missing = match args.bool("byd") {
                // BydEnum._missing_: UNKNOWN (-1) if defined, else the first member.
                Ok(true) => Some(if members.contains(&-1) {
                    -1
                } else {
                    members[0]
                }),
                Ok(false) => None,
                Err(error) => return Some(Err(error)),
            };
            int_enum_value(value, &members, missing).map(to_json)
        }
        "coerce.str_enum" => {
            let members: Option<Vec<&str>> = args
                .value("members")
                .as_array()
                .and_then(|items| items.iter().map(Value::as_str).collect());
            let Some(members) = members else {
                return Some(Err(ParityError::bad_arguments(
                    "members must be a list of strings",
                )));
            };
            match value.as_str().filter(|text| members.contains(text)) {
                Some(text) => Ok(json!(text)),
                None => Err(ValidationError::new("enum")),
            }
        }
        _ => return None,
    };
    Some(result.map_err(ParityError::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_whitespace_includes_the_ascii_separators() {
        assert_eq!(py_strip("\u{1c} x\u{3000}\u{85}"), "x");
        assert_eq!(py_strip("\u{200b}x"), "\u{200b}x");
    }

    #[test]
    fn unicode_digits() {
        assert_eq!(py_decimal('7'), Some(7));
        assert_eq!(py_decimal('\u{663}'), Some(3));
        assert_eq!(py_decimal('\u{ff11}'), Some(1));
        assert_eq!(py_decimal('\u{b2}'), None);
        assert!(py_is_digit('\u{b2}'));
        assert!(py_is_digit('\u{2460}'));
        assert!(!py_is_digit('a'));
    }

    #[test]
    fn int_text() {
        assert_eq!(parse_int_text(" +1_000.00 "), IntText::Int(1000));
        assert_eq!(parse_int_text("1."), IntText::Invalid);
        assert_eq!(parse_int_text("1__0"), IntText::Invalid);
        assert_eq!(parse_int_text("0__1"), IntText::Int(1));
        assert_eq!(parse_int_text("0__-1_0"), IntText::Int(-10));
        assert_eq!(parse_int_text("-0-1"), IntText::Invalid);
        assert_eq!(parse_int_text("0-05"), IntText::Invalid);
        assert_eq!(parse_int_text("0-0"), IntText::Int(0));
        assert_eq!(parse_int_text("0-+1"), IntText::Invalid);
        assert_eq!(parse_int_text("0__"), IntText::Invalid);
        assert_eq!(parse_int_text("0__0"), IntText::Int(0));
        assert_eq!(parse_int_text("-_1"), IntText::Invalid);
        assert_eq!(
            parse_int_text("9223372036854775808"),
            IntText::Big { negative: false }
        );
        assert_eq!(
            parse_int_text("-9223372036854775808"),
            IntText::Int(i64::MIN)
        );
        assert_eq!(
            parse_int_text("0__-99999999999999999999"),
            IntText::Big { negative: true }
        );
        assert_eq!(
            parse_int_text(&"9".repeat(60)),
            IntText::Big { negative: false }
        );
    }

    #[test]
    fn integers_outside_i64_are_reported_not_wrapped() {
        let big = json!(u64::MAX);
        assert_eq!(int(&big).unwrap_err().exception, "Unsupported");
        assert_eq!(
            int(&json!("9223372036854775808")).unwrap_err().exception,
            "Unsupported"
        );
        assert_eq!(boolean(&big).unwrap_err().error_type, "bool_parsing");
        assert_eq!(float(&big).unwrap(), u64::MAX as f64);
        assert_eq!(
            int_or_str(&big).unwrap_err().exception,
            "Unsupported",
            "an escaping error is not folded into the union error"
        );
    }

    #[test]
    fn floats_and_their_json() {
        assert_eq!(float(&json!("+_1")).unwrap(), 1.0);
        assert!(float(&json!(" 1_0")).is_err());
        assert!(float(&json!(" 1_0 ")).is_err());
        assert!(float(&json!("NAN")).unwrap().is_nan());
        assert_eq!(float_json(f64::NAN), json!("NaN"));
        assert_eq!(float_json(f64::NEG_INFINITY), json!("-Infinity"));
        assert_eq!(float_json(f64::INFINITY), json!("Infinity"));
        assert_eq!(float_json(-0.0).to_string(), "-0.0");
    }

    #[test]
    fn a_validation_error_beats_an_unsupported_value_and_an_escaping_one_beats_both() {
        let error = list(&json!([u64::MAX, "x"]), int).unwrap_err();
        assert_eq!(
            (error.exception, error.loc),
            ("ValidationError", vec!["1".to_owned()])
        );
        let mut failures = Failures::default();
        failures.record(ValidationError::unsupported_int());
        failures.record(ValidationError::new("a"));
        failures.record(ValidationError::raised("OSError", "b"));
        assert!(failures.stopped());
        failures.record(ValidationError::new("c"));
        assert_eq!(failures.finish().unwrap_err().exception, "OSError");
        assert!(Failures::default().finish().is_ok());
    }

    #[test]
    fn errors_carry_their_location() {
        let error = list(&json!([1, "x"]), int).unwrap_err();
        assert_eq!(error.loc, vec!["1"]);
        assert_eq!(error.to_string(), "ValidationError (int_parsing) at 1");
        let error = dict(&json!({"a": []}), int).unwrap_err();
        assert_eq!(error.loc, vec!["a"]);
        assert_eq!(ValidationError::new("x").to_string(), "ValidationError (x)");
        let parity: ParityError = ValidationError::raised("OSError", "x").into();
        assert_eq!(parity.kind, "OSError");
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Door {
        Unknown = -1,
        Closed = 0,
        Open = 1,
    }

    impl IntEnum for Door {
        const MEMBERS: &'static [Self] = &[Self::Unknown, Self::Closed, Self::Open];
        const MISSING: Option<Self> = Some(Self::Unknown);
        fn value(self) -> i64 {
            self as i64
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Command {
        Lock,
    }

    impl StrEnum for Command {
        const MEMBERS: &'static [Self] = &[Self::Lock];
        fn value(self) -> &'static str {
            "LOCKDOOR"
        }
    }

    #[test]
    fn enums() {
        assert_eq!(int_enum::<Door>(&json!(" 1 ")).unwrap(), Door::Open);
        assert_eq!(int_enum::<Door>(&json!(7)).unwrap(), Door::Unknown);
        assert_eq!(int_enum::<Door>(&json!(false)).unwrap(), Door::Closed);
        assert!(int_enum::<Door>(&json!(u64::MAX)).is_err());
        assert_eq!(
            int_enum_value(&json!(1.5), &[0], None)
                .unwrap_err()
                .error_type,
            "enum"
        );
        assert_eq!(
            str_enum::<Command>(&json!("LOCKDOOR")).unwrap(),
            Command::Lock
        );
        assert!(str_enum::<Command>(&json!("lockdoor")).is_err());
    }

    #[test]
    fn bad_parity_arguments() {
        let call = |args: Value| parity_call("coerce.int_enum", &args).unwrap().unwrap_err();
        assert_eq!(
            call(json!({"value": 1, "members": "x"})).kind,
            "BadArguments"
        );
        assert_eq!(
            call(json!({"value": 1, "members": [1]})).kind,
            "BadArguments"
        );
        let error = parity_call("coerce.str_enum", &json!({"members": [1]}))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, "BadArguments");
        let error = parity_call("coerce.int_bounded", &json!({"value": 1, "le": "x"}))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, "BadArguments");
        assert!(parity_call("coerce.nothing", &json!({})).is_none());
    }
}
