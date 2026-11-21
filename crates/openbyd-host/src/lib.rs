//! The language-neutral surface of openbyd.
//!
//! Every binding other than Python's sits on this crate: the Node add-on
//! (`openbyd-node`) and the C library (`openbyd-capi`) the JVM and .NET
//! packages load. It offers the client by method name, with JSON arguments
//! and JSON results, so a binding is a thin typed layer generated from one
//! description (`bindings/spec.py`) instead of a hand-written copy of the
//! client in each language.
//!
//! - [`Client`]: pyBYD's `BydClient` and `BydCar` by method name.
//! - [`Host`]: where a client sends its events and asks its questions.
//! - [`HostError`]: a failure as the Python exception class pyBYD raises.
//! - [`call`]: the functions that need no client (the envelope cipher, the
//!   parity hook, the runtime's settings).
//! - [`runtime`]: the tokio runtime and HTTP pool all clients share.
//! - `testing` (feature `testing`): the fake server and test broker by call name.
//!
//! What crosses is exactly what `openbyd-py` hands Python: a model is
//! pyBYD's `model_dump(mode="json")`. A float JSON cannot hold is the
//! string `"NaN"`, `"Infinity"` or `"-Infinity"`; a typed binding reads
//! those back as numbers where the field is a float.

mod args;
mod client;
mod error;
mod host;
pub mod runtime;
#[cfg(feature = "testing")]
pub mod testing;

use serde_json::{Value, json};

pub use args::Args;
pub use client::{ASYNC_METHODS, Client, EVENTS, SYNC_METHODS};
pub use error::{Details, HostError};
pub use host::{DECISION_TIMEOUT, Host, NoHost};

/// The functions [`call`] runs.
pub const FUNCTIONS: [&str; 8] = [
    "version",
    "has_testing",
    "encode_envelope",
    "decode_envelope",
    "parity_call",
    "parse_model",
    "configure",
    "runtime",
];

/// This crate's version, which is the workspace's.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn unhex(text: &str) -> Result<Vec<u8>, HostError> {
    let invalid = || HostError::value("hex must be pairs of hexadecimal digits");
    if !text.is_ascii() || !text.len().is_multiple_of(2) {
        return Err(invalid());
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).map_err(|_| invalid()))
        .collect()
}

/// A parity call's failure as a [`HostError`]: `kind` is the Python class name.
fn parity_error(error: openbyd_protocol::parity::ParityError) -> HostError {
    use openbyd_protocol::error::ErrorKind;
    match ErrorKind::from_class_name(&error.kind) {
        Some(kind) => HostError::new(kind, error.message),
        None => HostError::builtin(error.kind, error.message),
    }
}

fn parity(name: &str, args: &Value) -> openbyd_protocol::parity::Outcome {
    match openbyd_protocol::parity::call(name, args) {
        Err(error) if error.kind == "UnknownCall" => openbyd_client::parity::call(name, args),
        answer => answer,
    }
}

/// Runs a function that needs no client. `args` is an object or `null`.
///
/// | Function | Arguments | Result |
/// |---|---|---|
/// | `version` | | the version of openbyd |
/// | `has_testing` | | whether the build has the feature `testing` |
/// | `encode_envelope` | `text` or `hex`: the plaintext | the envelope |
/// | `decode_envelope` | `envelope` | `{"hex"}`: the plaintext bytes |
/// | `parity_call` | `name`, `args` | `{"ok": ...}` or `{"error": {"kind", "message"}}` |
/// | `parse_model` | `name` (`models.gps.GpsInfo`), `raw`, `context` | `{"dump", "properties"}` |
/// | `configure` | `worker_threads` | |
/// | `runtime` | | `{"worker_threads", "started"}` |
pub fn call(function: &str, args: Value) -> Result<Value, HostError> {
    let args = Args::new(args)?;
    Ok(match function {
        "version" => json!(VERSION),
        "has_testing" => json!(cfg!(feature = "testing")),
        "encode_envelope" => {
            let plaintext = match args.opt_str("hex")? {
                Some(hex) => unhex(&hex)?,
                None => args.str("text")?.into_bytes(),
            };
            json!(openbyd_crypto::encode_envelope(&plaintext))
        }
        "decode_envelope" => {
            let plaintext = openbyd_crypto::decode_envelope(&args.str("envelope")?)?;
            json!({ "hex": hex(&plaintext) })
        }
        "parity_call" => {
            let nothing = Value::Null;
            match parity(&args.str("name")?, args.get("args").unwrap_or(&nothing)) {
                Ok(value) => json!({ "ok": value }),
                Err(error) => json!({"error": {"kind": error.kind, "message": error.message}}),
            }
        }
        "parse_model" => {
            let name = args.str("name")?;
            if !name.starts_with("models.") {
                return Err(HostError::value(format!("{name:?} is not a model")));
            }
            let call = json!({
                "raw": args.get("raw").cloned().unwrap_or(Value::Null),
                "context": args.get("context").cloned().unwrap_or(Value::Null),
            });
            openbyd_protocol::parity::call(&name, &call).map_err(parity_error)?
        }
        "configure" => {
            let threads = usize::try_from(args.int("worker_threads")?)
                .map_err(|_| HostError::value("worker_threads must be positive"))?;
            runtime::configure(threads)?;
            Value::Null
        }
        "runtime" => json!({
            "worker_threads": runtime::worker_threads(),
            "started": runtime::started(),
        }),
        other => return Err(HostError::value(format!("no function {other:?}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cipher_round_trips_text_and_bytes() {
        let envelope = call("encode_envelope", json!({"text": "{\"a\":1}"})).unwrap();
        let plain = call("decode_envelope", json!({ "envelope": envelope })).unwrap();
        assert_eq!(plain["hex"], hex(b"{\"a\":1}"));

        let envelope = call("encode_envelope", json!({"hex": "00ff10"})).unwrap();
        let plain = call("decode_envelope", json!({ "envelope": envelope })).unwrap();
        assert_eq!(plain["hex"], "00ff10");
    }

    #[test]
    fn bad_cipher_input_fails_as_python_would() {
        let error = call("decode_envelope", json!({"envelope": ""})).unwrap_err();
        assert_eq!(error.class, "BangcleError");
        for bad in ["0", "zz", "é0"] {
            let error = call("encode_envelope", json!({ "hex": bad })).unwrap_err();
            assert_eq!(error.class, "ValueError", "{bad}");
        }
        assert_eq!(
            call("encode_envelope", json!({})).unwrap_err().class,
            "TypeError"
        );
    }

    #[test]
    fn parity_calls_report_errors_as_data() {
        let ok = call(
            "parity_call",
            json!({"name": "hashing.md5_hex", "args": {"value": "abc"}}),
        )
        .unwrap();
        assert_eq!(ok["ok"], "900150983CD24FB0D6963F7D28E17F72");
        let unknown = call("parity_call", json!({"name": "no.such", "args": {}})).unwrap();
        assert_eq!(unknown["error"]["kind"], "UnknownCall");
    }

    #[test]
    fn models_parse_by_call_name() {
        let parsed = call(
            "parse_model",
            json!({"name": "models.gps.GpsInfo", "raw": {"latitude": 52.0, "longitude": 4.0}}),
        )
        .unwrap();
        assert_eq!(parsed["dump"]["latitude"], 52.0);
        assert!(parsed["properties"].is_object());

        let not_a_model = call("parse_model", json!({"name": "hashing.md5_hex"})).unwrap_err();
        assert_eq!(not_a_model.class, "ValueError");
        let unknown = call("parse_model", json!({"name": "models.gps.Nope"})).unwrap_err();
        assert_eq!(unknown.class, "UnknownCall");
        assert_eq!(unknown.bases, ["ValueError"]);
        let invalid = call(
            "parse_model",
            json!({"name": "models.charging.ChargingStatus", "raw": {"elecPercent": "lots"}}),
        )
        .unwrap_err();
        assert_eq!(invalid.class, "ValidationError");
    }

    #[test]
    fn the_runtime_reports_and_refuses_bad_settings() {
        assert_eq!(call("version", Value::Null).unwrap(), VERSION);
        let state = call("runtime", Value::Null).unwrap();
        assert!(state["worker_threads"].as_u64().unwrap() >= 1);
        assert_eq!(
            call("configure", json!({"worker_threads": 0}))
                .unwrap_err()
                .class,
            "ValueError"
        );
        assert_eq!(
            call("configure", json!({"worker_threads": -1}))
                .unwrap_err()
                .class,
            "ValueError"
        );
        assert_eq!(call("nope", Value::Null).unwrap_err().class, "ValueError");
    }

    #[test]
    fn every_listed_function_exists() {
        for name in FUNCTIONS {
            let outcome = call(name, Value::Null);
            if let Err(error) = outcome {
                assert!(!error.message.starts_with("no function"), "{name}");
            }
        }
    }
}
