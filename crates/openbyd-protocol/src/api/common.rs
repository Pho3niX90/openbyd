//! Fields every request carries, and the API code sets (`pybyd/_api/_common.py`).
//!
//! `_raise_for_code` is [`crate::error::raise_for_code`] and
//! `decode_respond_data` is [`crate::envelope::decode_respond_data`]; both
//! are re-exported here under pyBYD's module.

use serde_json::{Map, Value, json};

pub use crate::envelope::decode_respond_data;
pub use crate::error::raise_for_code;

use crate::config::{BydConfig, config_argument};
use crate::error::{ErrorKind, describe, kind_argument};
use crate::parity::{Args, Outcome, ParityError};

/// API codes meaning the endpoint is not supported for this vehicle.
pub const ENDPOINT_NOT_SUPPORTED_CODES: [&str; 1] = ["1001"];
/// Backend busy / soft rate limit: recoverable, back off.
pub const SERVICE_BUSY_CODES: [&str; 1] = ["1008"];
/// Vehicle temporarily unreachable (deep sleep, offline): keep the last value.
pub const VEHICLE_UNREACHABLE_CODES: [&str; 1] = ["6002"];
/// The outer `userType` of an account the car was shared with. Owners leave the field out.
pub const USER_TYPE_GUEST: &str = "1";
/// What a guest gets when it does not declare itself.
pub const GUEST_NOT_DECLARED_CODE: &str = "1001";
/// What an owner gets when it claims to be a guest.
pub const NOT_A_GUEST_CODE: &str = "8003";

/// Uppercase hex of a request's 16 random bytes (pyBYD's `secrets.token_hex(16).upper()`).
pub fn random_hex(random: &[u8; 16]) -> String {
    crate::hashing::hex(random, true)
}

/// The inner payload fields most endpoints share, for a request made at
/// `now_ms` (milliseconds since the epoch) with 16 fresh random bytes.
/// `vin` and `request_serial` are added when given and not empty.
pub fn build_inner_base(
    config: &BydConfig,
    now_ms: i64,
    random: &[u8; 16],
    vin: Option<&str>,
    request_serial: Option<&str>,
) -> Map<String, Value> {
    let mut inner = Map::new();
    inner.insert("deviceType".into(), json!(config.device.device_type));
    inner.insert("imeiMD5".into(), json!(config.device.imei_md5));
    inner.insert("networkType".into(), json!(config.device.network_type));
    inner.insert("random".into(), json!(random_hex(random)));
    inner.insert("timeStamp".into(), json!(now_ms.to_string()));
    inner.insert("version".into(), json!(config.app_inner_version));
    if let Some(vin) = vin.filter(|vin| !vin.is_empty()) {
        inner.insert("vin".into(), json!(vin));
    }
    if let Some(serial) = request_serial.filter(|serial| !serial.is_empty()) {
        inner.insert("requestSerial".into(), json!(serial));
    }
    inner
}

/// The 16 random bytes a parity call gives as 32 hex characters in `random`.
pub(crate) fn random_argument(args: &Args<'_>) -> Result<[u8; 16], ParityError> {
    let text = args.str("random")?;
    let mut bytes = [0u8; 16];
    if text.len() != 32 || !text.is_ascii() {
        return Err(ParityError::bad_arguments(
            "random must be 32 hex characters",
        ));
    }
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|_| ParityError::bad_arguments("random must be hex"))?;
    }
    Ok(bytes)
}

fn string_list(value: &Value) -> Result<Vec<&str>, ParityError> {
    value
        .as_array()
        .and_then(|items| items.iter().map(Value::as_str).collect())
        .ok_or_else(|| ParityError::bad_arguments("expected a list of strings"))
}

fn raise_for_code_call(args: &Args<'_>) -> Outcome {
    let not_supported = match args.value("not_supported_codes") {
        Value::Null => None,
        codes => Some(string_list(codes)?),
    };
    let mut extra: Vec<(Vec<&str>, ErrorKind)> = Vec::new();
    if let Some(entries) = args.value("extra_code_map").as_array() {
        for entry in entries {
            let codes = string_list(&entry[0])?;
            let kind = kind_argument(entry[1].as_str().unwrap_or_default())?;
            // pyBYD's map is a dict keyed by frozenset: an equal set keeps
            // its first position and takes the later class.
            let same_set = |other: &Vec<&str>| {
                codes.iter().all(|code| other.contains(code))
                    && other.iter().all(|code| codes.contains(code))
            };
            match extra.iter_mut().find(|(other, _)| same_set(other)) {
                Some(existing) => existing.1 = kind,
                None => extra.push((codes, kind)),
            }
        }
    }
    let code = args.str("code")?;
    // pyBYD constructs the class with `code=` and `endpoint=`, which only
    // BydApiError and its subclasses accept.
    if let Some((_, kind)) = extra.iter().find(|(codes, _)| codes.contains(&code))
        && !kind.is_a(ErrorKind::BydApiError)
        && !crate::constants::SESSION_EXPIRED_CODES.contains(&code)
        && !not_supported
            .as_ref()
            .is_some_and(|codes| codes.contains(&code))
    {
        return Err(ParityError::new(
            "TypeError",
            "unexpected keyword argument 'code'",
        ));
    }
    let extra: Vec<(&[&str], ErrorKind)> = extra
        .iter()
        .map(|(codes, kind)| (codes.as_slice(), *kind))
        .collect();
    let error = raise_for_code(
        args.str("endpoint")?,
        code,
        args.str("message")?,
        args.opt_str("vin")?,
        not_supported.as_deref(),
        &extra,
    );
    Ok(describe(&error))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "api.common.constants" => Ok(json!({
            "ENDPOINT_NOT_SUPPORTED_CODES": ENDPOINT_NOT_SUPPORTED_CODES,
            "SERVICE_BUSY_CODES": SERVICE_BUSY_CODES,
            "VEHICLE_UNREACHABLE_CODES": VEHICLE_UNREACHABLE_CODES,
            "USER_TYPE_GUEST": USER_TYPE_GUEST,
            "GUEST_NOT_DECLARED_CODE": GUEST_NOT_DECLARED_CODE,
            "NOT_A_GUEST_CODE": NOT_A_GUEST_CODE,
        })),
        "api.common.build_inner_base" => (|| {
            let config = config_argument(&args)?;
            let inner = build_inner_base(
                &config,
                args.i64("now_ms")?,
                &random_argument(&args)?,
                args.opt_str("vin")?,
                args.opt_str("request_serial")?,
            );
            Ok(Value::Object(inner))
        })(),
        "api.common.raise_for_code" => raise_for_code_call(&args),
        "api.common.decode_respond_data" => (|| {
            let response = args.value("response");
            let decoded =
                decode_respond_data(args.str("endpoint")?, response, args.str("content_key")?)?;
            Ok(decoded)
        })(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inner_base_fields() {
        let config = BydConfig::new("user@example.com", "p");
        let inner = build_inner_base(&config, 5, &[0xab; 16], Some(""), Some("S1"));
        assert_eq!(inner["random"], "AB".repeat(16));
        assert_eq!(inner["timeStamp"], "5");
        assert!(!inner.contains_key("vin"));
        assert_eq!(inner["requestSerial"], "S1");
        assert!(random_argument(&Args(&json!({"random": "zz"}))).is_err());
        assert!(random_argument(&Args(&json!({"random": "z".repeat(32)}))).is_err());
        assert!(string_list(&json!([1])).is_err());
    }
}
