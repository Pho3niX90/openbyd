//! Shared constants and the climate scale helpers (`pybyd/_constants.py`).

use std::cmp::Ordering;

use serde_json::{Value, json};

use crate::error::{Error, ErrorKind};
use crate::parity::{Args, Outcome, ParityError};
use crate::pyjson::py_float_from_str;

/// The `user-agent` header pyBYD sends.
pub const USER_AGENT: &str = "okhttp/4.12.0";

/// API codes that mean the session token is no longer valid.
pub const SESSION_EXPIRED_CODES: [&str; 3] = ["1002", "1005", "1010"];

const SCALE_MIN: i64 = 1;
const SCALE_MAX: i64 = 17;

/// The offset between °C and BYD's 1-17 climate scale for the usual 15-31 °C range.
pub const DEFAULT_TEMP_OFFSET_C: f64 = 14.0;

/// The climate durations BYD accepts, in minutes.
pub const VALID_CLIMATE_DURATIONS: [i64; 5] = [10, 15, 20, 25, 30];

/// The scale offset for a vehicle that reports its own °C range.
///
/// Most models use 15-31 °C, but some (the HAN) report 17-33 °C on the
/// same 1-17 scale. Falls back to [`DEFAULT_TEMP_OFFSET_C`] when the range
/// is missing or does not fit the scale.
pub fn temp_offset_from_range(range_min: Option<f64>, range_max: Option<f64>) -> f64 {
    match (range_min, range_max) {
        (Some(min), Some(max)) => offset_from(PyNum::Float(min), PyNum::Float(max)),
        _ => DEFAULT_TEMP_OFFSET_C,
    }
}

/// Converts a °C temperature to BYD's climate scale (1-17), given the
/// offset from [`temp_offset_from_range`].
///
/// Fails with [`ErrorKind::ValueError`] when the temperature is outside
/// the range the offset covers.
pub fn celsius_to_scale(temp_c: f64, offset: f64) -> Result<i64, Error> {
    scale_from(temp_c, PyNum::Float(offset))
}

/// Converts a climate duration in minutes (10, 15, 20, 25 or 30) to BYD's
/// `time_span` code 1-5.
///
/// Fails with [`ErrorKind::ValueError`] for any other duration.
pub fn minutes_to_time_span(minutes: i64) -> Result<i64, Error> {
    VALID_CLIMATE_DURATIONS
        .iter()
        .position(|&valid| valid == minutes)
        .map(|index| index as i64 + 1)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::ValueError,
                format!("duration must be one of (10, 15, 20, 25, 30) minutes, got {minutes}"),
            )
        })
}

/// A Python number: integers stay exact, as they do in Python.
#[derive(Debug, Clone, Copy)]
enum PyNum {
    Int(i128),
    Float(f64),
}

impl PyNum {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Bool(flag) => Some(PyNum::Int(i128::from(*flag))),
            Value::Number(number) => Some(if let Some(int) = number.as_i64() {
                PyNum::Int(int.into())
            } else if let Some(int) = number.as_u64() {
                PyNum::Int(int.into())
            } else {
                PyNum::Float(number.as_f64()?)
            }),
            _ => None,
        }
    }

    fn as_f64(self) -> f64 {
        match self {
            PyNum::Int(int) => int as f64,
            PyNum::Float(float) => float,
        }
    }

    fn add(self, other: i64) -> PyNum {
        match self {
            PyNum::Int(int) => PyNum::Int(int + i128::from(other)),
            PyNum::Float(float) => PyNum::Float(float + other as f64),
        }
    }

    fn sub(self, other: PyNum) -> PyNum {
        match (self, other) {
            (PyNum::Int(a), PyNum::Int(b)) => PyNum::Int(a - b),
            (a, b) => PyNum::Float(a.as_f64() - b.as_f64()),
        }
    }

    /// Python's exact comparison of mixed ints and floats; `None` for NaN.
    fn compare(self, other: PyNum) -> Option<Ordering> {
        match (self, other) {
            (PyNum::Int(a), PyNum::Int(b)) => Some(a.cmp(&b)),
            (PyNum::Float(a), PyNum::Float(b)) => a.partial_cmp(&b),
            (PyNum::Float(a), PyNum::Int(b)) => compare_float_int(a, b),
            (PyNum::Int(a), PyNum::Float(b)) => compare_float_int(b, a).map(Ordering::reverse),
        }
    }
}

fn compare_float_int(float: f64, int: i128) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    // Every i128 lies strictly inside ±2**127.
    const LIMIT: f64 = 1.7014118346046923e38;
    if float >= LIMIT {
        return Some(Ordering::Greater);
    }
    if float < -LIMIT {
        return Some(Ordering::Less);
    }
    let whole = float.trunc();
    Some((whole as i128).cmp(&int).then_with(|| {
        let fraction = float - whole;
        fraction.partial_cmp(&0.0).unwrap_or(Ordering::Equal)
    }))
}

fn offset_from(range_min: PyNum, range_max: PyNum) -> f64 {
    let width = range_max.sub(range_min);
    let fits = width.compare(PyNum::Int(0)) == Some(Ordering::Greater)
        && matches!(
            width.compare(PyNum::Int((SCALE_MAX - SCALE_MIN).into())),
            Some(Ordering::Less | Ordering::Equal)
        );
    if fits {
        range_min.as_f64() - SCALE_MIN as f64
    } else {
        DEFAULT_TEMP_OFFSET_C
    }
}

fn scale_from(value: f64, offset: PyNum) -> Result<i64, Error> {
    let low = offset.add(SCALE_MIN);
    let high = offset.add(SCALE_MAX);
    let within = matches!(
        low.compare(PyNum::Float(value)),
        Some(Ordering::Less | Ordering::Equal)
    ) && matches!(
        PyNum::Float(value).compare(high),
        Some(Ordering::Less | Ordering::Equal)
    );
    if !within {
        return Err(Error::new(
            ErrorKind::ValueError,
            format!(
                "temperature must be between {} and {} °C, got {value}",
                low.as_f64(),
                high.as_f64()
            ),
        ));
    }
    let rounded = (value - offset.as_f64()).round_ties_even();
    Ok(rounded.clamp(SCALE_MIN as f64, SCALE_MAX as f64) as i64)
}

fn type_error(message: &str) -> ParityError {
    ParityError::new("TypeError", message)
}

/// Python's `float(value)` for a JSON value.
fn py_float(value: &Value) -> Result<f64, ParityError> {
    match value {
        Value::String(text) => py_float_from_str(text)
            .ok_or_else(|| ParityError::new("ValueError", "could not convert string to float")),
        other => PyNum::from_value(other)
            .map(PyNum::as_f64)
            .ok_or_else(|| type_error("float() argument must be a string or a real number")),
    }
}

fn number_or_type_error(value: &Value) -> Result<PyNum, ParityError> {
    PyNum::from_value(value).ok_or_else(|| type_error("unsupported operand type"))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "constants.values" => Ok(json!({
            "USER_AGENT": USER_AGENT,
            "SESSION_EXPIRED_CODES": SESSION_EXPIRED_CODES,
            "DEFAULT_TEMP_OFFSET_C": DEFAULT_TEMP_OFFSET_C,
            "VALID_CLIMATE_DURATIONS": VALID_CLIMATE_DURATIONS,
        })),
        "constants.temp_offset_from_range" => {
            match (args.value("range_min"), args.value("range_max")) {
                (Value::Null, _) | (_, Value::Null) => Ok(json!(DEFAULT_TEMP_OFFSET_C)),
                (min, max) => (|| {
                    let (min, max) = (number_or_type_error(min)?, number_or_type_error(max)?);
                    Ok(json!(offset_from(min, max)))
                })(),
            }
        }
        "constants.celsius_to_scale" => (|| {
            let value = py_float(args.value("temp_c"))?;
            let offset = match args.0.get("offset") {
                None => PyNum::Float(DEFAULT_TEMP_OFFSET_C),
                Some(offset) => number_or_type_error(offset)?,
            };
            Ok(json!(scale_from(value, offset)?))
        })(),
        "constants.minutes_to_time_span" => match args.value("minutes") {
            Value::Array(_) | Value::Object(_) => Err(type_error("unhashable type")),
            minutes => {
                let whole = PyNum::from_value(minutes).filter(|_| !minutes.is_boolean());
                let found = VALID_CLIMATE_DURATIONS.iter().find(|&&valid| {
                    whole.and_then(|number| number.compare(PyNum::Int(valid.into())))
                        == Some(Ordering::Equal)
                });
                match found {
                    Some(&valid) => minutes_to_time_span(valid).map(|code| json!(code)),
                    None => minutes_to_time_span(0).map(|code| json!(code)),
                }
                .map_err(ParityError::from)
            }
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn climate_helpers() {
        assert_eq!(temp_offset_from_range(None, Some(30.0)), 14.0);
        assert_eq!(temp_offset_from_range(Some(17.0), Some(33.0)), 16.0);
        assert_eq!(temp_offset_from_range(Some(17.0), Some(40.0)), 14.0);
        assert_eq!(celsius_to_scale(21.0, DEFAULT_TEMP_OFFSET_C).unwrap(), 7);
        assert_eq!(celsius_to_scale(15.5, DEFAULT_TEMP_OFFSET_C).unwrap(), 2);
        assert_eq!(
            celsius_to_scale(14.9, 14.0).unwrap_err().kind(),
            ErrorKind::ValueError
        );
        assert_eq!(
            celsius_to_scale(f64::NAN, 14.0).unwrap_err().kind(),
            ErrorKind::ValueError
        );
        assert_eq!(minutes_to_time_span(25).unwrap(), 4);
        assert!(minutes_to_time_span(11).is_err());
    }

    #[test]
    fn mixed_comparisons_are_exact() {
        assert_eq!(compare_float_int(1e39, i128::MAX), Some(Ordering::Greater));
        assert_eq!(compare_float_int(-1e39, i128::MIN), Some(Ordering::Less));
        assert_eq!(compare_float_int(2.5, 2), Some(Ordering::Greater));
        assert_eq!(compare_float_int(-2.5, -2), Some(Ordering::Less));
        assert_eq!(compare_float_int(f64::NAN, 0), None);
        assert_eq!(
            PyNum::Int(3).compare(PyNum::Float(3.0)),
            Some(Ordering::Equal)
        );
        assert!(PyNum::from_value(&json!("x")).is_none());
    }
}
