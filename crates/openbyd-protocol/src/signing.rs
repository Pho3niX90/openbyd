//! The request sign string (`pybyd/_crypto/signing.py`).

use serde_json::{Map, Value, json};

use crate::parity::{Args, Outcome, ParityError};
use crate::pyjson::py_str;

/// The text BYD signs: `key=value` pairs sorted by key and joined with `&`,
/// then `&password=` and `password`.
///
/// Values are written as Python's `str()` writes them, with `null` for a
/// JSON null. pyBYD only signs strings in practice.
pub fn build_sign_string(fields: &Map<String, Value>, password: &str) -> String {
    let mut keys: Vec<&String> = fields.keys().collect();
    keys.sort();
    let joined: Vec<String> = keys
        .into_iter()
        .map(|key| match &fields[key] {
            Value::Null => format!("{key}=null"),
            value => format!("{key}={}", py_str(value)),
        })
        .collect();
    // An empty field list still gives the leading `&`, as in pyBYD.
    format!("{}&password={password}", joined.join("&"))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "signing.build_sign_string" => (|| {
            let fields = args
                .value("fields")
                .as_object()
                .ok_or_else(|| ParityError::bad_arguments("fields must be an object"))?;
            Ok(json!(build_sign_string(fields, args.str("password")?)))
        })(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_and_appends_the_password() {
        let fields = json!({"b": "2", "a": null, "c": true});
        assert_eq!(
            build_sign_string(fields.as_object().unwrap(), "P"),
            "a=null&b=2&c=True&password=P"
        );
        assert_eq!(build_sign_string(&Map::new(), "P"), "&password=P");
        assert!(
            parity_call("signing.build_sign_string", &json!({"fields": 1}))
                .unwrap()
                .is_err()
        );
    }
}
