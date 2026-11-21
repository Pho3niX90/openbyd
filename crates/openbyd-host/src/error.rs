//! A failure as every binding receives it: the Python exception class pyBYD
//! would raise, its parent classes, and what that exception carries.
//!
//! A binding maps `class` to its own exception class, trying each of
//! `bases` in turn when it has none of that name, so a class a newer core
//! adds still lands on the nearest one the binding knows.

use std::fmt;

use openbyd_protocol::coerce::ValidationError;
use openbyd_protocol::error::{Error, ErrorKind};
use serde_json::{Value, json};

/// A failure of a call, ready to cross into another language. Its fields
/// are [`Details`], reached through it directly: `error.class`.
#[derive(Debug, Clone, PartialEq)]
pub struct HostError(Box<Details>);

/// What a [`HostError`] carries. Boxed inside it, so a `Result` stays small.
#[derive(Debug, Clone, PartialEq)]
pub struct Details {
    /// The Python class name, such as `"BydSessionExpiredError"`.
    pub class: String,
    /// The parent classes, nearest first.
    pub bases: Vec<String>,
    /// The message. Wording is openbyd's, not pyBYD's.
    pub message: String,
    /// `BydApiError.code`.
    pub code: Option<String>,
    /// The endpoint of a `BydApiError` or `BydTransportError`.
    pub endpoint: Option<String>,
    /// `BydTransportError.status_code`.
    pub status_code: Option<u16>,
    /// pydantic's error type of a `ValidationError`, such as `"int_parsing"`.
    pub error_type: Option<String>,
    /// Where a `ValidationError`'s bad value was, outermost first.
    pub loc: Option<Vec<String>>,
}

impl std::ops::Deref for HostError {
    type Target = Details;

    fn deref(&self) -> &Details {
        &self.0
    }
}

impl std::ops::DerefMut for HostError {
    fn deref_mut(&mut self) -> &mut Details {
        &mut self.0
    }
}

impl HostError {
    /// An error of `kind` with only a message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::from(Error::new(kind, message))
    }

    /// Python's `ValueError`: an argument has a value that cannot be used.
    pub fn value(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ValueError, message)
    }

    /// Python's `TypeError`: an argument is missing or of the wrong type.
    pub fn type_error(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::TypeError, message)
    }

    /// A Python built-in exception the core has no kind for (`OverflowError`,
    /// say), under its own name. It is reported as a kind of `ValueError`:
    /// it is always about a value that could not be used, and a binding
    /// that knows no class of that name then lands on the one for `ValueError`.
    pub fn builtin(class: impl Into<String>, message: impl Into<String>) -> Self {
        let mut error = Self::value(message);
        error.bases = vec![std::mem::replace(&mut error.class, class.into())];
        error
    }

    /// A failure inside the binding itself (a panic, a lost task), as a `BydError`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::BydError, message)
    }

    /// The error as the JSON object bindings receive.
    pub fn to_json(&self) -> Value {
        json!({
            "class": self.class,
            "bases": self.bases,
            "message": self.message,
            "code": self.code,
            "endpoint": self.endpoint,
            "status_code": self.status_code,
            "error_type": self.error_type,
            "loc": self.loc,
        })
    }

    /// The error a binding hands back (an answer to `relogin`): an object
    /// with `class` and `message`, and optionally `code`, `endpoint` and
    /// `status_code`. An unknown class becomes a `BydError` naming it.
    pub fn to_error(value: &Value) -> Error {
        let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default();
        let class = text("class");
        let message = text("message");
        match ErrorKind::from_class_name(class) {
            Some(kind) if kind.is_a(ErrorKind::BydTransportError) => Error::transport(
                message,
                value
                    .get("status_code")
                    .and_then(Value::as_u64)
                    .and_then(|status| u16::try_from(status).ok()),
                text("endpoint"),
            ),
            Some(kind) if kind.is_a(ErrorKind::BydApiError) => {
                Error::api(kind, message, text("code"), text("endpoint"))
            }
            Some(kind) => Error::new(kind, message),
            None if class.is_empty() => Error::new(ErrorKind::BydError, message),
            None => Error::new(ErrorKind::BydError, format!("{class}: {message}")),
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.class, self.message)
    }
}

impl std::error::Error for HostError {}

fn bases(kind: ErrorKind) -> Vec<String> {
    kind.lineage()
        .into_iter()
        .skip(1)
        .map(|parent| parent.class_name().to_owned())
        .collect()
}

impl From<Error> for HostError {
    fn from(error: Error) -> Self {
        let kind = error.kind();
        let api = kind.is_a(ErrorKind::BydApiError);
        let transport = kind.is_a(ErrorKind::BydTransportError);
        Self(Box::new(Details {
            class: kind.class_name().to_owned(),
            bases: bases(kind),
            message: error.message().to_owned(),
            code: api.then(|| error.code().to_owned()),
            endpoint: (api || transport).then(|| error.endpoint().to_owned()),
            status_code: error.status_code().filter(|_| transport),
            error_type: None,
            loc: None,
        }))
    }
}

impl From<ValidationError> for HostError {
    /// A pydantic `ValidationError` keeps its error type and location; an
    /// exception that escapes pydantic in pyBYD (`TypeError`, ...) is that
    /// class, and one openbyd cannot name is a `ValueError`.
    fn from(error: ValidationError) -> Self {
        let message = error.to_string();
        if error.exception == "ValidationError" {
            let mut invalid = Self::new(ErrorKind::ValidationError, message);
            invalid.error_type = Some(error.error_type.to_owned());
            invalid.loc = Some(error.loc);
            return invalid;
        }
        let kind = ErrorKind::from_class_name(error.exception).unwrap_or(ErrorKind::ValueError);
        Self::new(kind, message)
    }
}

impl From<openbyd_crypto::Error> for HostError {
    fn from(error: openbyd_crypto::Error) -> Self {
        Self::new(ErrorKind::BangcleError, error.to_string())
    }
}

impl From<wallclock::Error> for HostError {
    fn from(error: wallclock::Error) -> Self {
        let class = error.python_exception();
        let message = error.to_string();
        match ErrorKind::from_class_name(class) {
            Some(kind) => Self::new(kind, message),
            None if class == "ZoneInfoNotFoundError" => {
                // CPython's ZoneInfoNotFoundError inherits from KeyError and LookupError.
                let mut host = Self::value(message);
                host.class = class.to_owned();
                host.bases = vec!["KeyError".to_owned(), "LookupError".to_owned()];
                host
            }
            None => Self::builtin(class, message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_api_error_carries_its_code_endpoint_and_bases() {
        let error = HostError::from(Error::api(
            ErrorKind::BydSessionExpiredError,
            "expired",
            "1005",
            "/x",
        ));
        assert_eq!(error.class, "BydSessionExpiredError");
        assert_eq!(
            error.bases,
            ["BydAuthenticationError", "BydApiError", "BydError"]
        );
        assert_eq!(error.code.as_deref(), Some("1005"));
        assert_eq!(error.endpoint.as_deref(), Some("/x"));
        assert_eq!(error.status_code, None);
        assert_eq!(error.to_json()["bases"][2], "BydError");
        assert_eq!(error.to_string(), "BydSessionExpiredError: expired");
    }

    #[test]
    fn a_transport_error_carries_its_status() {
        let error = HostError::from(Error::transport("down", Some(503), "/y"));
        assert_eq!(error.class, "BydTransportError");
        assert_eq!(error.status_code, Some(503));
        assert_eq!(error.code, None);
        assert_eq!(error.endpoint.as_deref(), Some("/y"));
    }

    #[test]
    fn validation_errors_keep_their_type_and_location() {
        let error = HostError::from(ValidationError::new("int_parsing").at("speed"));
        assert_eq!(error.class, "ValidationError");
        assert_eq!(error.bases, ["ValueError"]);
        assert_eq!(error.error_type.as_deref(), Some("int_parsing"));
        assert_eq!(error.loc, Some(vec!["speed".to_owned()]));

        let escaped = HostError::from(ValidationError::raised("TypeError", "x"));
        assert_eq!(escaped.class, "TypeError");
        assert_eq!(escaped.error_type, None);

        let unknown = HostError::from(ValidationError::raised("Unsupported", "x"));
        assert_eq!(unknown.class, "ValueError");
    }

    #[test]
    fn errors_come_back_from_a_binding() {
        let api = HostError::to_error(&json!({"class": "BydRateLimitError", "message": "m",
            "code": "6024", "endpoint": "/e"}));
        assert_eq!(api.kind(), ErrorKind::BydRateLimitError);
        assert_eq!(api.code(), "6024");
        let transport = HostError::to_error(
            &json!({"class": "BydTransportError", "message": "m", "status_code": 502}),
        );
        assert_eq!(transport.status_code(), Some(502));
        let plain = HostError::to_error(&json!({"class": "BydError", "message": "m"}));
        assert_eq!(plain.kind(), ErrorKind::BydError);
        let foreign = HostError::to_error(&json!({"class": "Oops", "message": "m"}));
        assert_eq!(foreign.message(), "Oops: m");
        let bare = HostError::to_error(&json!({"message": "m"}));
        assert_eq!(bare.message(), "m");
    }

    #[test]
    fn an_unknown_builtin_keeps_its_name() {
        let error = HostError::builtin("OverflowError", "too wide");
        assert_eq!(error.class, "OverflowError");
        assert_eq!(error.bases, ["ValueError"]);
        assert_eq!(error.message, "too wide");
    }

    #[test]
    fn crypto_errors_are_bangcle_errors() {
        let error = HostError::from(openbyd_crypto::decode_envelope("").unwrap_err());
        assert_eq!(error.class, "BangcleError");
        assert_eq!(error.bases, ["BydCryptoError", "BydError"]);
    }

    #[test]
    fn wallclock_errors_carry_python_class_and_bases() {
        let missing = HostError::from(wallclock::Tz::zone("Mars/Olympus_Mons").unwrap_err());
        assert_eq!(missing.class, "ZoneInfoNotFoundError");
        assert_eq!(missing.bases, ["KeyError", "LookupError"]);

        let bad_key = HostError::from(wallclock::Tz::zone("/Europe/Madrid").unwrap_err());
        assert_eq!(bad_key.class, "ValueError");
        assert!(bad_key.bases.is_empty());

        let madrid = wallclock::Tz::zone("Europe/Madrid").unwrap();
        let edge = wallclock::DateTime::new(9999, 12, 31, &wallclock::Tz::UTC)
            .unwrap()
            .with_time(23, 30, 0, 0)
            .unwrap();
        let overflow = HostError::from(edge.astimezone(&madrid).unwrap_err());
        assert_eq!(overflow.class, "OverflowError");
        assert_eq!(overflow.bases, ["ValueError"]);
    }
}
