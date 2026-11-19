//! Error types and the mapping from API codes to them (`pybyd/exceptions.py`, `_common._raise_for_code`).
//!
//! pyBYD reports failures with a hierarchy of exception classes. Here they
//! are one [`Error`] struct whose [`ErrorKind`] names the Python class and
//! knows its parents, so callers can ask "is this a `BydApiError`?" the way
//! Python code writes `except BydApiError`.

use std::fmt;

use serde_json::{Value, json};

use crate::api::common::{SERVICE_BUSY_CODES, VEHICLE_UNREACHABLE_CODES};
use crate::constants::SESSION_EXPIRED_CODES;
use crate::parity::{Args, Outcome, ParityError};

/// The Python exception class an [`Error`] stands for.
///
/// The first group mirrors `pybyd/exceptions.py`. The second names the
/// Python built-in exceptions that pyBYD lets through unchanged for some
/// inputs (a reply that is not JSON, text that is not UTF-8, a config value
/// of the wrong type), so the client can still tell them apart.
///
/// [`ErrorKind::ScriptExhausted`] is the parity tests' own: a scripted
/// environment raises it when a flow asks for more replies than the script
/// holds. Production code never sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// Base of every pyBYD error.
    BydError,
    /// Encryption or decryption failed.
    BydCryptoError,
    /// A Bangcle envelope could not be encoded or decoded.
    BangcleError,
    /// The Bangcle tables could not be loaded.
    BangcleTableLoadError,
    /// HTTP-level failure: network, non-200 status, a body that is not JSON.
    BydTransportError,
    /// The API answered with a non-zero code.
    BydApiError,
    /// Login failed or the session expired.
    BydAuthenticationError,
    /// The server rejected the session token.
    BydSessionExpiredError,
    /// A remote control command failed after the cloud accepted it.
    BydRemoteControlError,
    /// A remote control command was rejected for its operation password.
    BydControlPasswordError,
    /// The endpoint is not supported for this vehicle or region.
    BydEndpointNotSupportedError,
    /// The vehicle cannot provide the data right now.
    BydDataUnavailableError,
    /// A previous command is still in progress (code 6024).
    BydRateLimitError,
    /// The backend is busy (code 1008); back off.
    BydServiceBusyError,
    /// Python's `ValueError`.
    ValueError,
    /// Python's `json.JSONDecodeError`, a `ValueError`.
    JsonDecodeError,
    /// Python's `UnicodeDecodeError`, a `ValueError`.
    UnicodeDecodeError,
    /// pydantic's `ValidationError`, a `ValueError`.
    ValidationError,
    /// Python's `TypeError`.
    TypeError,
    /// Python's `AttributeError`.
    AttributeError,
    /// A scripted test environment ran out of replies (`parity/api_support.py`).
    ScriptExhausted,
}

impl ErrorKind {
    /// Every kind, parents before children.
    pub const ALL: [ErrorKind; 21] = [
        ErrorKind::BydError,
        ErrorKind::BydCryptoError,
        ErrorKind::BangcleError,
        ErrorKind::BangcleTableLoadError,
        ErrorKind::BydTransportError,
        ErrorKind::BydApiError,
        ErrorKind::BydAuthenticationError,
        ErrorKind::BydSessionExpiredError,
        ErrorKind::BydRemoteControlError,
        ErrorKind::BydControlPasswordError,
        ErrorKind::BydEndpointNotSupportedError,
        ErrorKind::BydDataUnavailableError,
        ErrorKind::BydRateLimitError,
        ErrorKind::BydServiceBusyError,
        ErrorKind::ValueError,
        ErrorKind::JsonDecodeError,
        ErrorKind::UnicodeDecodeError,
        ErrorKind::ValidationError,
        ErrorKind::TypeError,
        ErrorKind::AttributeError,
        ErrorKind::ScriptExhausted,
    ];

    /// The Python class name, for example `"BydSessionExpiredError"`.
    pub fn class_name(self) -> &'static str {
        match self {
            ErrorKind::BydError => "BydError",
            ErrorKind::BydCryptoError => "BydCryptoError",
            ErrorKind::BangcleError => "BangcleError",
            ErrorKind::BangcleTableLoadError => "BangcleTableLoadError",
            ErrorKind::BydTransportError => "BydTransportError",
            ErrorKind::BydApiError => "BydApiError",
            ErrorKind::BydAuthenticationError => "BydAuthenticationError",
            ErrorKind::BydSessionExpiredError => "BydSessionExpiredError",
            ErrorKind::BydRemoteControlError => "BydRemoteControlError",
            ErrorKind::BydControlPasswordError => "BydControlPasswordError",
            ErrorKind::BydEndpointNotSupportedError => "BydEndpointNotSupportedError",
            ErrorKind::BydDataUnavailableError => "BydDataUnavailableError",
            ErrorKind::BydRateLimitError => "BydRateLimitError",
            ErrorKind::BydServiceBusyError => "BydServiceBusyError",
            ErrorKind::ValueError => "ValueError",
            ErrorKind::JsonDecodeError => "JSONDecodeError",
            ErrorKind::UnicodeDecodeError => "UnicodeDecodeError",
            ErrorKind::ValidationError => "ValidationError",
            ErrorKind::TypeError => "TypeError",
            ErrorKind::AttributeError => "AttributeError",
            ErrorKind::ScriptExhausted => "ScriptExhausted",
        }
    }

    /// The kind whose Python class name is `name`.
    pub fn from_class_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.class_name() == name)
    }

    /// The direct parent class, or `None` at the root of a hierarchy.
    pub fn parent(self) -> Option<Self> {
        use ErrorKind::*;
        match self {
            BydError | ValueError | TypeError | AttributeError | ScriptExhausted => None,
            BydCryptoError | BydTransportError | BydApiError => Some(BydError),
            BangcleError => Some(BydCryptoError),
            BangcleTableLoadError => Some(BangcleError),
            BydSessionExpiredError => Some(BydAuthenticationError),
            BydAuthenticationError
            | BydRemoteControlError
            | BydControlPasswordError
            | BydEndpointNotSupportedError
            | BydDataUnavailableError
            | BydRateLimitError
            | BydServiceBusyError => Some(BydApiError),
            JsonDecodeError | UnicodeDecodeError | ValidationError => Some(ValueError),
        }
    }

    /// Whether this class is `other` or a subclass of it, as Python's `issubclass`.
    pub fn is_a(self, other: ErrorKind) -> bool {
        let mut current = Some(self);
        while let Some(kind) = current {
            if kind == other {
                return true;
            }
            current = kind.parent();
        }
        false
    }

    /// This class and its parents, nearest first (Python's MRO without `Exception`).
    pub fn lineage(self) -> Vec<ErrorKind> {
        std::iter::successors(Some(self), |kind| kind.parent()).collect()
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.class_name())
    }
}

/// A failure, carrying what the matching pyBYD exception carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    code: String,
    endpoint: String,
    status_code: Option<u16>,
}

impl Error {
    /// An error with only a kind and a message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            code: String::new(),
            endpoint: String::new(),
            status_code: None,
        }
    }

    /// A `BydApiError` or one of its subclasses, with the API's code and the endpoint.
    pub fn api(
        kind: ErrorKind,
        message: impl Into<String>,
        code: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            endpoint: endpoint.into(),
            ..Self::new(kind, message)
        }
    }

    /// A `BydTransportError` for `endpoint`, with the HTTP status when there was one.
    pub fn transport(
        message: impl Into<String>,
        status_code: Option<u16>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            status_code,
            ..Self::new(ErrorKind::BydTransportError, message)
        }
    }

    /// Which pyBYD exception this is.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Whether this error is `kind` or a subclass of it.
    pub fn is_a(&self, kind: ErrorKind) -> bool {
        self.kind.is_a(kind)
    }

    /// The Python class name of this error.
    pub fn class_name(&self) -> &'static str {
        self.kind.class_name()
    }

    /// The human-readable message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The API's error code (`BydApiError.code`); empty when there is none.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The endpoint that failed (`.endpoint`); empty when unknown.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The HTTP status (`BydTransportError.status_code`).
    pub fn status_code(&self) -> Option<u16> {
        self.status_code
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {}

impl From<Error> for ParityError {
    fn from(error: Error) -> Self {
        ParityError::new(error.kind.class_name(), error.message)
    }
}

impl From<openbyd_crypto::Error> for Error {
    fn from(error: openbyd_crypto::Error) -> Self {
        Error::new(ErrorKind::BangcleError, error.to_string())
    }
}

/// The error pyBYD's `_common._raise_for_code` raises for a non-zero API code.
///
/// The checks run in pyBYD's order: session expired, then
/// `not_supported_codes`, then each entry of `extra_code_map` in order, then
/// the defaults every endpoint shares (1008 busy, 6002 unreachable), then a
/// plain `BydApiError`. The map's kinds should be `BydApiError` or a
/// subclass, as in pyBYD.
pub fn raise_for_code(
    endpoint: &str,
    code: &str,
    message: &str,
    vin: Option<&str>,
    not_supported_codes: Option<&[&str]>,
    extra_code_map: &[(&[&str], ErrorKind)],
) -> Error {
    let failed = || format!("{endpoint} failed: code={code} message={message}");
    if SESSION_EXPIRED_CODES.contains(&code) {
        return Error::api(ErrorKind::BydSessionExpiredError, failed(), code, endpoint);
    }
    if not_supported_codes.is_some_and(|codes| codes.contains(&code)) {
        let suffix = match vin {
            Some(vin) if !vin.is_empty() => format!(" for VIN {vin}"),
            _ => String::new(),
        };
        return Error::api(
            ErrorKind::BydEndpointNotSupportedError,
            format!("{endpoint} not supported{suffix} (code={code})"),
            code,
            endpoint,
        );
    }
    for (codes, kind) in extra_code_map {
        if codes.contains(&code) {
            return Error::api(*kind, failed(), code, endpoint);
        }
    }
    let kind = if SERVICE_BUSY_CODES.contains(&code) {
        ErrorKind::BydServiceBusyError
    } else if VEHICLE_UNREACHABLE_CODES.contains(&code) {
        ErrorKind::BydDataUnavailableError
    } else {
        ErrorKind::BydApiError
    };
    Error::api(kind, failed(), code, endpoint)
}

/// The parity form of an error: everything pyBYD's exception carries.
pub(crate) fn describe(error: &Error) -> Value {
    json!({
        "class": error.class_name(),
        "lineage": error.kind.lineage().iter().map(|kind| kind.class_name()).collect::<Vec<_>>(),
        "message": error.message,
        "code": error.code,
        "endpoint": error.endpoint,
    })
}

pub(crate) fn kind_argument(name: &str) -> Result<ErrorKind, ParityError> {
    ErrorKind::from_class_name(name)
        .ok_or_else(|| ParityError::bad_arguments(format!("unknown error class {name}")))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "error.issubclass" => (|| {
            let child = kind_argument(args.str("child")?)?;
            let parent = kind_argument(args.str("parent")?)?;
            Ok(json!(child.is_a(parent)))
        })(),
        "error.lineage" => args.str("class").and_then(kind_argument).map(|kind| {
            json!(
                kind.lineage()
                    .iter()
                    .map(|kind| kind.class_name())
                    .collect::<Vec<_>>()
            )
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchy_matches_pybyd() {
        assert!(ErrorKind::BydSessionExpiredError.is_a(ErrorKind::BydAuthenticationError));
        assert!(ErrorKind::BydSessionExpiredError.is_a(ErrorKind::BydApiError));
        assert!(ErrorKind::BydSessionExpiredError.is_a(ErrorKind::BydError));
        assert!(!ErrorKind::BydApiError.is_a(ErrorKind::BydSessionExpiredError));
        assert!(ErrorKind::BangcleTableLoadError.is_a(ErrorKind::BydCryptoError));
        assert!(ErrorKind::JsonDecodeError.is_a(ErrorKind::ValueError));
        assert!(!ErrorKind::JsonDecodeError.is_a(ErrorKind::BydError));
        for kind in ErrorKind::ALL {
            assert_eq!(ErrorKind::from_class_name(kind.class_name()), Some(kind));
            assert_eq!(kind.to_string(), kind.class_name());
        }
        assert_eq!(ErrorKind::from_class_name("Nope"), None);
    }

    #[test]
    fn errors_carry_their_fields() {
        let error = Error::transport("HTTP 500", Some(500), "/x");
        assert_eq!(error.status_code(), Some(500));
        assert_eq!(error.endpoint(), "/x");
        assert_eq!(error.code(), "");
        assert_eq!(error.class_name(), "BydTransportError");
        assert!(error.is_a(ErrorKind::BydError));
        assert_eq!(error.to_string(), "BydTransportError: HTTP 500");
        assert_eq!(error.message(), "HTTP 500");
        let parity: ParityError = error.into();
        assert_eq!(parity.kind, "BydTransportError");
        let crypto: Error = openbyd_crypto::Error::EmptyInput.into();
        assert_eq!(crypto.kind(), ErrorKind::BangcleError);
    }

    #[test]
    fn codes_map_in_pybyd_order() {
        let map: &[(&[&str], ErrorKind)] =
            &[(&["1001", "5005"], ErrorKind::BydControlPasswordError)];
        let kind = |code, not_supported: Option<&[&str]>| {
            raise_for_code("/e", code, "m", Some("V"), not_supported, map).kind()
        };
        assert_eq!(
            kind("1005", Some(&["1005"])),
            ErrorKind::BydSessionExpiredError
        );
        assert_eq!(
            kind("1001", Some(&["1001"])),
            ErrorKind::BydEndpointNotSupportedError
        );
        assert_eq!(kind("1001", None), ErrorKind::BydControlPasswordError);
        assert_eq!(kind("1008", None), ErrorKind::BydServiceBusyError);
        assert_eq!(kind("6002", None), ErrorKind::BydDataUnavailableError);
        assert_eq!(kind("9", None), ErrorKind::BydApiError);
        let error = raise_for_code("/e", "1001", "m", Some(""), Some(&["1001"]), &[]);
        assert_eq!(error.message(), "/e not supported (code=1001)");
        assert!(kind_argument("Nope").is_err());
    }
}
