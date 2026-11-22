//! Shared constants and the climate scale helpers (`pybyd/_constants.py`).

use std::cmp::Ordering;

use serde_json::{Value, json};

use crate::error::{Error, ErrorKind};
use crate::parity::{Args, Outcome, ParityError};
use crate::pyjson::py_float_from_str;

/// The `user-agent` header pyBYD sends.
pub const USER_AGENT: &str = "okhttp/4.12.0";

/// API codes that mean the session token is no longer valid.
pub const SESSION_EXPIRED_CODES: [&str; 4] = ["1002", "1011", "3001", "3003"];

/// API codes that mean the session token needs a refresh (or a new login).
pub const TOKEN_REFRESH_CODES: [&str; 1] = ["1030"];

/// API codes that mean the client's clock is too far from BYD's.
pub const SERVER_TIME_DIFF_CODES: [&str; 1] = ["1003"];

/// How far BYD's clock (from the HTTP `Date` header) may drift from ours, in
/// milliseconds, before requests are stamped with BYD's time instead.
pub const SERVER_TIME_DRIFT_THRESHOLD_MS: i64 = 30_000;

/// The official app's 10-step command polling ladder (33 s in all), in seconds.
pub const COMMAND_POLL_DELAYS_S: [f64; 10] = [1.0, 0.5, 0.5, 2.0, 2.0, 2.0, 5.0, 5.0, 5.0, 10.0];

/// Why [`resolve_poll_delays`] has no list to give, where pyBYD raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollDelaysError {
    /// Python's `MemoryError`: the list does not fit in memory.
    MemoryError,
    /// Python's `IndexError`: more attempts than steps on an empty ladder.
    IndexError,
}

/// The sleep before each poll attempt, one at a time: what
/// [`resolve_poll_delays`] lists, without a list, so the loops that poll
/// allocate nothing however many attempts they are given.
///
/// With a fixed `poll_interval`, that interval `poll_attempts` times;
/// without one, the steps of `default_ladder`, repeating its last step
/// when there are more attempts than steps. An empty ladder gives nothing
/// (pyBYD raises `IndexError` there).
pub fn poll_delays(
    poll_attempts: i64,
    poll_interval: Option<f64>,
    default_ladder: &[f64],
) -> impl Iterator<Item = f64> + '_ {
    let attempts = usize::try_from(poll_attempts).unwrap_or(0);
    let last = default_ladder.last().copied();
    (0..attempts).map_while(move |index| match poll_interval {
        Some(interval) => Some(interval),
        None => default_ladder.get(index).copied().or(last),
    })
}

/// The sleep before each poll attempt, as a list (pyBYD's
/// `resolve_poll_delays`): [`poll_delays`] collected, failing where Python
/// does instead of aborting on an allocation it cannot make.
pub fn resolve_poll_delays(
    poll_attempts: i64,
    poll_interval: Option<f64>,
    default_ladder: &[f64],
) -> Result<Vec<f64>, PollDelaysError> {
    let attempts = usize::try_from(poll_attempts).unwrap_or(0);
    if attempts > default_ladder.len() && poll_interval.is_none() && default_ladder.is_empty() {
        return Err(PollDelaysError::IndexError);
    }
    let mut delays = Vec::new();
    delays
        .try_reserve_exact(attempts)
        .map_err(|_| PollDelaysError::MemoryError)?;
    delays.extend(poll_delays(poll_attempts, poll_interval, default_ladder));
    Ok(delays)
}

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

/// Converts a BYD `time_span` code (1-5) back to minutes; `None` for any other code.
pub fn time_span_to_minutes(code: i64) -> Option<i64> {
    usize::try_from(code.checked_sub(1)?)
        .ok()
        .and_then(|index| VALID_CLIMATE_DURATIONS.get(index))
        .copied()
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
            "TOKEN_REFRESH_CODES": TOKEN_REFRESH_CODES,
            "SERVER_TIME_DIFF_CODES": SERVER_TIME_DIFF_CODES,
            "SERVER_TIME_DRIFT_THRESHOLD_MS": SERVER_TIME_DRIFT_THRESHOLD_MS,
            "COMMAND_POLL_DELAYS_S": COMMAND_POLL_DELAYS_S,
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
        // pyBYD compares each code with `==`, so `1.0` and `True` find 10 minutes.
        "constants.time_span_to_minutes" => {
            let code = PyNum::from_value(args.value("code"));
            let found = (1..=VALID_CLIMATE_DURATIONS.len() as i64).find(|&valid| {
                code.and_then(|number| number.compare(PyNum::Int(valid.into())))
                    == Some(Ordering::Equal)
            });
            Ok(json!(found.and_then(time_span_to_minutes)))
        }
        "constants.resolve_poll_delays" => poll_delays_call(&args),
        _ => return None,
    })
}

/// pyBYD's `resolve_poll_delays` on JSON arguments: `poll_attempts` is
/// Python's operand of `n <= 0`, `[x] * n` and `ladder[:n]` (an int or a
/// bool; a float only gets past the comparison), and the interval and the
/// ladder's steps come back as given.
fn poll_delays_call(args: &Args<'_>) -> Outcome {
    let attempts = args.value("poll_attempts");
    let interval = args.value("poll_interval");
    let ladder: Vec<Value> = match args.0.get("default_ladder") {
        None => COMMAND_POLL_DELAYS_S
            .iter()
            .map(|step| json!(step))
            .collect(),
        Some(Value::Array(steps)) => steps.clone(),
        Some(_) => return Err(type_error("the ladder must be a list")),
    };
    let count = match attempts {
        Value::Bool(flag) => i128::from(*flag),
        Value::Number(number) => match number
            .as_i64()
            .map(i128::from)
            .or_else(|| number.as_u64().map(i128::from))
        {
            Some(whole) => whole,
            None => {
                // A float passes `<= 0`, then fails at `[x] * n`, at
                // `ladder[:n]` or, past an empty ladder, at `ladder[-1]`.
                let float = number.as_f64().unwrap_or(f64::NAN);
                if float <= 0.0 {
                    return Ok(json!([]));
                }
                if interval.is_null() && ladder.is_empty() {
                    return Err(ParityError::new("IndexError", "tuple index out of range"));
                }
                return Err(type_error(
                    "can't multiply sequence by non-int of type 'float'",
                ));
            }
        },
        _ => return Err(type_error("'<=' not supported")),
    };
    if count <= 0 {
        return Ok(json!([]));
    }
    // `[x] * n` takes an index-sized n: beyond it, OverflowError; within it
    // but too large to allocate, MemoryError (as `try_reserve` finds).
    let overflow = || {
        ParityError::new(
            "OverflowError",
            "cannot fit 'int' into an index-sized integer",
        )
    };
    let memory = || ParityError::new("MemoryError", "");
    let filled = |value: &Value, n: i128| -> Result<Vec<Value>, ParityError> {
        if n > i128::from(i64::MAX) {
            return Err(overflow());
        }
        let n = usize::try_from(n).map_err(|_| memory())?;
        let mut items = Vec::new();
        items.try_reserve_exact(n).map_err(|_| memory())?;
        items.resize(n, value.clone());
        Ok(items)
    };
    if !interval.is_null() {
        return filled(interval, count).map(Value::Array);
    }
    let length = ladder.len() as i128;
    if count <= length {
        return Ok(Value::Array(ladder[..count as usize].to_vec()));
    }
    let Some(last) = ladder.last().cloned() else {
        return Err(ParityError::new("IndexError", "tuple index out of range"));
    };
    let tail = filled(&last, count - length)?;
    let mut delays = ladder;
    delays.try_reserve_exact(tail.len()).map_err(|_| memory())?;
    delays.extend(tail);
    Ok(Value::Array(delays))
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
        assert_eq!(time_span_to_minutes(4), Some(25));
        assert_eq!(time_span_to_minutes(0), None);
        assert_eq!(time_span_to_minutes(6), None);
        assert_eq!(time_span_to_minutes(i64::MIN), None);
    }

    #[test]
    fn poll_delays_follow_the_ladder() {
        let ladder = COMMAND_POLL_DELAYS_S;
        let resolve = |n, interval| resolve_poll_delays(n, interval, &ladder).unwrap();
        assert_eq!(resolve(0, None), Vec::<f64>::new());
        assert_eq!(resolve(-3, Some(1.5)), Vec::<f64>::new());
        assert_eq!(resolve(3, None), vec![1.0, 0.5, 0.5]);
        assert_eq!(resolve(2, Some(1.5)), vec![1.5, 1.5]);
        assert_eq!(resolve(10, None).iter().sum::<f64>(), 33.0);
        assert_eq!(resolve(12, None)[10..], [10.0, 10.0]);
        // Lazily, any number of attempts costs nothing up front.
        let mut lazy = poll_delays(i64::MAX, None, &ladder);
        assert_eq!(lazy.by_ref().take(12).sum::<f64>(), 53.0);
        assert_eq!(lazy.next(), Some(10.0));
        assert_eq!(
            poll_delays(i64::MAX, Some(0.5), &ladder).nth(1 << 20),
            Some(0.5)
        );
        assert_eq!(poll_delays(3, None, &[]).count(), 0);
        // A list that cannot be allocated is Python's MemoryError, not an abort.
        assert_eq!(
            resolve_poll_delays(i64::MAX, None, &ladder),
            Err(PollDelaysError::MemoryError)
        );
        assert_eq!(
            resolve_poll_delays(1 << 62, Some(1.0), &ladder),
            Err(PollDelaysError::MemoryError)
        );
        assert_eq!(
            resolve_poll_delays(1, None, &[]),
            Err(PollDelaysError::IndexError)
        );
        let call = |args: Value| poll_delays_call(&Args(&args));
        let kind = |args: Value| call(args).unwrap_err().kind;
        assert_eq!(
            kind(json!({"poll_attempts": 2.5, "poll_interval": null})),
            "TypeError"
        );
        assert_eq!(
            call(json!({"poll_attempts": -2.5, "poll_interval": null})).unwrap(),
            json!([])
        );
        assert_eq!(
            kind(json!({"poll_attempts": "2", "poll_interval": null})),
            "TypeError"
        );
        assert_eq!(
            kind(json!({"poll_attempts": 2, "poll_interval": null, "default_ladder": []})),
            "IndexError"
        );
        assert_eq!(
            kind(json!({"poll_attempts": 2, "poll_interval": null, "default_ladder": 5})),
            "TypeError"
        );
        assert_eq!(
            kind(json!({"poll_attempts": 1_u64 << 62, "poll_interval": 1})),
            "MemoryError"
        );
        assert_eq!(
            kind(json!({"poll_attempts": i64::MAX, "poll_interval": null})),
            "MemoryError"
        );
        assert_eq!(
            kind(json!({"poll_attempts": 1_u64 << 63, "poll_interval": 1})),
            "OverflowError"
        );
        assert_eq!(
            kind(json!({"poll_attempts": 1_u64 << 63, "poll_interval": null})),
            "MemoryError"
        );
        assert_eq!(
            kind(json!({"poll_attempts": u64::MAX, "poll_interval": null})),
            "OverflowError"
        );
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
