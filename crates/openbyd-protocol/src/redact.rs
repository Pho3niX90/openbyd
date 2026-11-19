//! Hides secrets in values before they are logged (`pybyd/_redact.py`).

use serde_json::{Map, Value, json};

use crate::parity::{Args, Outcome, ParityError};

/// Keys whose values are always hidden (compared in lower case).
pub const SENSITIVE_VALUE_KEYS: [&str; 9] = [
    "password",
    "signkey",
    "signtoken",
    "encrytoken",
    "refreshtoken",
    "accesstoken",
    "token",
    "authorization",
    "cookie",
];

/// Keys of encrypted blobs: hidden while they are still a string, walked
/// into once decrypted.
pub const ENCODED_BLOB_KEYS: [&str; 4] = ["encrydata", "responddata", "request", "response"];

/// The longest string kept whole by default.
pub const DEFAULT_MAX_STRING: i64 = 512;

const REDACTED: &str = "<redacted>";

/// A copy of `value` that is safe for debug logs: secrets replaced by
/// `<redacted>` and strings cut at [`DEFAULT_MAX_STRING`] characters.
pub fn redact_for_log(value: &Value) -> Value {
    redact_for_log_with(value, DEFAULT_MAX_STRING)
}

/// [`redact_for_log`] with another string limit. A negative limit behaves
/// as Python's `value[:max_string]` does: it cuts from the end.
pub fn redact_for_log_with(value: &Value, max_string: i64) -> Value {
    redact(value, max_string, 0)
}

fn redact(value: &Value, max_string: i64, depth: usize) -> Value {
    if depth > 20 {
        return json!("<max-depth>");
    }
    match value {
        Value::String(text) => {
            let length = text.chars().count() as i64;
            if length > max_string {
                let keep = if max_string >= 0 {
                    max_string
                } else {
                    (length + max_string).max(0)
                } as usize;
                let kept: String = text.chars().take(keep).collect();
                json!(format!("{kept}…<truncated>"))
            } else {
                value.clone()
            }
        }
        Value::Object(map) => {
            let mut redacted = Map::new();
            for (key, item) in map {
                let lower = key.to_lowercase();
                let hidden = SENSITIVE_VALUE_KEYS.contains(&lower.as_str())
                    || (ENCODED_BLOB_KEYS.contains(&lower.as_str()) && item.is_string());
                let item = if hidden {
                    json!(REDACTED)
                } else {
                    redact(item, max_string, depth + 1)
                };
                redacted.insert(key.clone(), item);
            }
            Value::Object(redacted)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact(item, max_string, depth + 1))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "redact.redact_for_log" => match args.0.get("max_string") {
            None => Ok(redact_for_log(args.value("value"))),
            Some(limit) => limit
                .as_i64()
                .ok_or_else(|| ParityError::bad_arguments("max_string must be an integer"))
                .map(|limit| redact_for_log_with(args.value("value"), limit)),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_secrets_and_cuts_strings() {
        let value = json!({"Password": "x", "encryData": "AB", "respondData": {"a": 1}, "n": [1, "abcdef"]});
        assert_eq!(
            redact_for_log_with(&value, 3),
            json!({"Password": "<redacted>", "encryData": "<redacted>", "respondData": {"a": 1}, "n": [1, "abc…<truncated>"]})
        );
        assert_eq!(
            redact_for_log_with(&json!("abcdef"), -2),
            json!("abcd…<truncated>")
        );
        assert_eq!(redact_for_log_with(&json!("ab"), -5), json!("…<truncated>"));
        let mut deep = json!(1);
        for _ in 0..25 {
            deep = json!([deep]);
        }
        assert!(redact_for_log(&deep).to_string().contains("<max-depth>"));
        assert!(
            parity_call("redact.redact_for_log", &json!({"max_string": "x"}))
                .unwrap()
                .is_err()
        );
    }
}
