//! Entry point for the parity tests.
//!
//! Every function ported from pyBYD is reachable here under a name such as
//! `"hashing.md5_hex"`, taking and returning JSON. The test suites feed the
//! same arguments to pyBYD and compare. Nothing else should use this module,
//! except `openbyd-client`, whose own `parity` module answers the calls for
//! the flows it ports with the same helpers ([`Args`], [`first_of`],
//! [`no_calls!`](crate::no_calls)).

use std::fmt;

use serde_json::Value;

/// A failure, named after the exception pyBYD raises for the same input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityError {
    /// The Python exception class name, for example `"BydCryptoError"`.
    pub kind: String,
    /// What went wrong. Not compared with pyBYD's wording.
    pub message: String,
}

impl ParityError {
    /// Builds an error of the given kind.
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }

    /// The arguments did not fit the call. A bug in the test, not a parity result.
    pub fn bad_arguments(message: impl Into<String>) -> Self {
        Self::new("BadArguments", message)
    }
}

impl fmt::Display for ParityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ParityError {}

/// What a parity call gives back.
pub type Outcome = Result<Value, ParityError>;

/// A module's parity entry point: `None` when the name is not one of its calls.
#[doc(hidden)]
pub type Handler = fn(&str, &Value) -> Option<Outcome>;

const MODULES: &[Handler] = &[
    crate::aes::parity_call,
    crate::api::parity_call,
    crate::capabilities::parity_call,
    crate::coerce::parity_call,
    crate::config::parity_call,
    crate::constants::parity_call,
    crate::envelope::parity_call,
    crate::error::parity_call,
    crate::hashing::parity_call,
    crate::models::parity_call,
    crate::mqtt::parity_call,
    crate::pyjson::parity_call,
    crate::redact::parity_call,
    crate::session::parity_call,
    crate::signing::parity_call,
    crate::state::parity_call,
];

/// Runs the call named `name` with JSON `args`.
pub fn call(name: &str, args: &Value) -> Outcome {
    first_of(MODULES, name, args).unwrap_or_else(|| {
        Err(ParityError::new(
            "UnknownCall",
            format!("no parity call named {name:?}"),
        ))
    })
}

/// The answer of the first handler that knows `name`.
#[doc(hidden)]
pub fn first_of(handlers: &[Handler], name: &str, args: &Value) -> Option<Outcome> {
    handlers.iter().find_map(|handler| handler(name, args))
}

/// Defines `parity_call` for a module that has nothing to call yet.
#[doc(hidden)]
#[macro_export]
macro_rules! no_calls {
    () => {
        pub(crate) fn parity_call(
            _name: &str,
            _args: &serde_json::Value,
        ) -> Option<$crate::parity::Outcome> {
            None
        }
    };
}
#[doc(hidden)]
pub use no_calls;

/// Reads named arguments out of the JSON object a call was given.
#[doc(hidden)]
pub struct Args<'a>(pub &'a Value);

impl<'a> Args<'a> {
    /// The argument, or JSON null when it was left out.
    pub fn value(&self, key: &str) -> &'a Value {
        self.0.get(key).unwrap_or(&Value::Null)
    }

    /// A required string argument.
    pub fn str(&self, key: &str) -> Result<&'a str, ParityError> {
        self.value(key)
            .as_str()
            .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be a string")))
    }

    /// A string argument that may be null or left out.
    pub fn opt_str(&self, key: &str) -> Result<Option<&'a str>, ParityError> {
        match self.value(key) {
            Value::Null => Ok(None),
            Value::String(text) => Ok(Some(text)),
            _ => Err(ParityError::bad_arguments(format!(
                "{key} must be a string or null"
            ))),
        }
    }

    /// A required integer argument.
    pub fn i64(&self, key: &str) -> Result<i64, ParityError> {
        self.value(key)
            .as_i64()
            .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be an integer")))
    }

    /// A required boolean argument.
    pub fn bool(&self, key: &str) -> Result<bool, ParityError> {
        self.value(key)
            .as_bool()
            .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be true or false")))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn unknown_calls_are_reported() {
        let error = call("nothing.here", &json!({})).unwrap_err();
        assert_eq!(error.kind, "UnknownCall");
        assert!(error.to_string().contains("nothing.here"));
    }

    #[test]
    fn arguments_are_checked() {
        let args = json!({"text": "a", "count": 3, "flag": true, "nothing": null});
        let args = Args(&args);
        assert_eq!(args.str("text").unwrap(), "a");
        assert_eq!(args.opt_str("text").unwrap(), Some("a"));
        assert_eq!(args.opt_str("nothing").unwrap(), None);
        assert_eq!(args.opt_str("missing").unwrap(), None);
        assert_eq!(args.i64("count").unwrap(), 3);
        assert!(args.bool("flag").unwrap());
        assert!(args.value("missing").is_null());
        assert_eq!(args.str("count").unwrap_err().kind, "BadArguments");
        assert_eq!(args.opt_str("count").unwrap_err().kind, "BadArguments");
        assert_eq!(args.i64("text").unwrap_err().kind, "BadArguments");
        assert_eq!(args.bool("text").unwrap_err().kind, "BadArguments");
    }
}
