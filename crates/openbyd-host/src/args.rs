//! The arguments of a call: a JSON object read by key, with Python's errors.
//!
//! A missing key and a JSON `null` both mean "not given". A value of the
//! wrong type is a `TypeError`, as passing it to pyBYD would be.

use serde_json::{Map, Value};

use crate::error::HostError;

/// The arguments of one call.
#[derive(Debug, Default)]
pub struct Args(Map<String, Value>);

impl Args {
    /// The arguments in `value`: an object, or `null` for none.
    pub fn new(value: Value) -> Result<Self, HostError> {
        match value {
            Value::Object(map) => Ok(Self(map)),
            Value::Null => Ok(Self::default()),
            _ => Err(HostError::type_error("arguments must be an object")),
        }
    }

    /// The value of `key`, unless it is missing or `null`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key).filter(|value| !value.is_null())
    }

    fn required(&self, key: &str) -> Result<&Value, HostError> {
        self.get(key)
            .ok_or_else(|| HostError::type_error(format!("missing argument: {key}")))
    }

    /// A required string.
    pub fn str(&self, key: &str) -> Result<String, HostError> {
        as_str(key, self.required(key)?)
    }

    /// An optional string.
    pub fn opt_str(&self, key: &str) -> Result<Option<String>, HostError> {
        self.get(key).map(|value| as_str(key, value)).transpose()
    }

    /// A required boolean.
    pub fn bool(&self, key: &str) -> Result<bool, HostError> {
        as_bool(key, self.required(key)?)
    }

    /// A boolean with a default.
    pub fn bool_or(&self, key: &str, default: bool) -> Result<bool, HostError> {
        self.get(key)
            .map_or(Ok(default), |value| as_bool(key, value))
    }

    /// A required integer.
    pub fn int(&self, key: &str) -> Result<i64, HostError> {
        as_int(key, self.required(key)?)
    }

    /// An optional integer.
    pub fn opt_int(&self, key: &str) -> Result<Option<i64>, HostError> {
        self.get(key).map(|value| as_int(key, value)).transpose()
    }

    /// An integer with a default.
    pub fn int_or(&self, key: &str, default: i64) -> Result<i64, HostError> {
        Ok(self.opt_int(key)?.unwrap_or(default))
    }

    /// An optional number.
    pub fn opt_float(&self, key: &str) -> Result<Option<f64>, HostError> {
        self.get(key).map(|value| as_float(key, value)).transpose()
    }

    /// A number with a default.
    pub fn float_or(&self, key: &str, default: f64) -> Result<f64, HostError> {
        Ok(self.opt_float(key)?.unwrap_or(default))
    }

    /// A required object.
    pub fn object(&self, key: &str) -> Result<&Map<String, Value>, HostError> {
        as_object(key, self.required(key)?)
    }

    /// An optional object.
    pub fn opt_object(&self, key: &str) -> Result<Option<&Map<String, Value>>, HostError> {
        self.get(key).map(|value| as_object(key, value)).transpose()
    }

    /// A required list of strings.
    pub fn strings(&self, key: &str) -> Result<Vec<String>, HostError> {
        match self.required(key)? {
            Value::Array(items) => items.iter().map(|item| as_str(key, item)).collect(),
            _ => Err(HostError::type_error(format!("{key} must be a list"))),
        }
    }
}

fn as_str(key: &str, value: &Value) -> Result<String, HostError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| HostError::type_error(format!("{key} must be a string")))
}

fn as_bool(key: &str, value: &Value) -> Result<bool, HostError> {
    value
        .as_bool()
        .ok_or_else(|| HostError::type_error(format!("{key} must be a boolean")))
}

/// An integer, also when a language without integers wrote it as `3.0`.
fn as_int(key: &str, value: &Value) -> Result<i64, HostError> {
    let whole = value.as_i64().or_else(|| {
        value
            .as_f64()
            .filter(|number| number.fract() == 0.0 && number.abs() < 9.0e15)
            .map(|number| number as i64)
    });
    whole.ok_or_else(|| HostError::type_error(format!("{key} must be an integer")))
}

fn as_float(key: &str, value: &Value) -> Result<f64, HostError> {
    value
        .as_f64()
        .ok_or_else(|| HostError::type_error(format!("{key} must be a number")))
}

fn as_object<'a>(key: &str, value: &'a Value) -> Result<&'a Map<String, Value>, HostError> {
    value
        .as_object()
        .ok_or_else(|| HostError::type_error(format!("{key} must be an object")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn args(value: Value) -> Args {
        Args::new(value).unwrap()
    }

    #[test]
    fn arguments_are_an_object_or_nothing() {
        assert!(Args::new(Value::Null).unwrap().get("x").is_none());
        assert_eq!(Args::new(json!([1])).unwrap_err().class, "TypeError");
    }

    #[test]
    fn null_and_missing_mean_not_given() {
        let given = args(json!({"a": null}));
        assert_eq!(given.opt_str("a").unwrap(), None);
        assert_eq!(given.opt_str("b").unwrap(), None);
        assert!(given.bool_or("a", true).unwrap());
        assert_eq!(given.int_or("a", 7).unwrap(), 7);
        assert_eq!(given.float_or("a", 1.5).unwrap(), 1.5);
        assert!(given.opt_object("a").unwrap().is_none());
        assert_eq!(given.str("a").unwrap_err().class, "TypeError");
        assert_eq!(given.bool("a").unwrap_err().message, "missing argument: a");
    }

    #[test]
    fn values_are_read_by_type() {
        let given = args(json!({"s": "x", "b": false, "i": 3, "w": 4.0, "f": 2.5,
            "o": {"k": 1}, "l": ["a", "b"]}));
        assert_eq!(given.str("s").unwrap(), "x");
        assert!(!given.bool("b").unwrap());
        assert_eq!(given.int("i").unwrap(), 3);
        assert_eq!(given.int("w").unwrap(), 4);
        assert_eq!(given.opt_float("i").unwrap(), Some(3.0));
        assert_eq!(given.opt_float("f").unwrap(), Some(2.5));
        assert_eq!(given.object("o").unwrap()["k"], 1);
        assert_eq!(given.strings("l").unwrap(), ["a", "b"]);
    }

    #[test]
    fn wrong_types_are_type_errors() {
        let given = args(
            json!({"s": 1, "b": "x", "i": 2.5, "f": "x", "o": [], "l": [1],
            "n": "x"}),
        );
        for error in [
            given.str("s").unwrap_err(),
            given.bool("b").unwrap_err(),
            given.int("i").unwrap_err(),
            given.opt_float("f").unwrap_err(),
            given.object("o").unwrap_err(),
            given.strings("l").unwrap_err(),
            given.strings("n").unwrap_err(),
        ] {
            assert_eq!(error.class, "TypeError", "{error}");
        }
    }
}
