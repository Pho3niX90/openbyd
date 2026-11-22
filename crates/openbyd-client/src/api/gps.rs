//! GPS info endpoints (`pybyd/_api/gps.py`).
//!
//! - `/control/getGpsInfo` (trigger)
//! - `/control/getGpsInfoResult` (poll)

use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::parity::{Args, Outcome, ParityError};
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_vehicle_json,
};
use crate::env::Env;
use crate::scripted::Scenario;

/// API codes meaning GPS data is temporarily unavailable (for example no
/// satellite fix while parked in a garage).
pub const GPS_DATA_UNAVAILABLE_CODES: [&str; 1] = ["6051"];

/// API codes meaning a shared ("Family and friends") guest has no location
/// permission (code `"31"`) granted.
pub const GPS_PERMISSION_DENIED_CODES: [&str; 1] = ["8002"];

/// Whether a GPS reply has meaningful content: not empty, and more than a
/// bare `requestSerial`.
pub fn is_gps_info_ready(gps_info: &Map<String, Value>) -> bool {
    !gps_info.is_empty() && !(gps_info.len() == 1 && gps_info.contains_key("requestSerial"))
}

/// Fetches one GPS endpoint and returns the reply's data with the serial to
/// poll with next: the reply's `requestSerial` when it is a string, else
/// `request_serial`. A reply whose data is not an object gives an empty
/// object. Codes 1001 and 8002 are `BydEndpointNotSupportedError`, 6051
/// `BydDataUnavailableError`.
pub async fn fetch_gps_endpoint<E: Env>(
    cx: Context<'_, E>,
    endpoint: &str,
    vin: &str,
    request_serial: Option<&str>,
) -> Result<(Value, Option<String>), Error> {
    let inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        request_serial,
    );
    let codes = ErrorCodes {
        not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
        extra_code_map: &[
            (
                &GPS_DATA_UNAVAILABLE_CODES,
                ErrorKind::BydDataUnavailableError,
            ),
            (
                &GPS_PERMISSION_DENIED_CODES,
                ErrorKind::BydEndpointNotSupportedError,
            ),
        ],
    };
    let decoded = post_vehicle_json(cx, endpoint, &inner, vin, None, codes).await?;
    let request_serial = request_serial.map(str::to_owned);
    if !decoded.is_object() {
        return Ok((json!({}), request_serial));
    }
    let next_serial = match decoded.get("requestSerial") {
        Some(Value::String(serial)) => Some(serial.clone()),
        _ => request_serial,
    };
    Ok((decoded, next_serial))
}

fn is_ready_call(args: &Args<'_>) -> Outcome {
    match args.value("gps_info") {
        Value::Object(info) => Ok(json!(is_gps_info_ready(info))),
        // pyBYD's `bool(gps_info) and gps_info.keys()`: anything falsy is
        // not ready, anything else has no `keys`.
        Value::Null | Value::Bool(false) => Ok(json!(false)),
        Value::String(text) if text.is_empty() => Ok(json!(false)),
        Value::Array(items) if items.is_empty() => Ok(json!(false)),
        Value::Number(number) if number.as_f64() == Some(0.0) => Ok(json!(false)),
        _ => Err(ParityError::new(
            "AttributeError",
            "object has no attribute 'keys'",
        )),
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.gps.is_gps_info_ready" => is_ready_call(&Args(args)),
        "api.gps.fetch_gps_endpoint" => Scenario::from_args(args).and_then(|scenario| {
            let call = scenario.args();
            scenario.run(
                fetch_gps_endpoint(
                    scenario.context(),
                    call.str("endpoint")?,
                    call.str("vin")?,
                    call.opt_str("request_serial")?,
                ),
                |(info, serial)| json!([info, serial]),
            )
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness() {
        let ready = |value: Value| is_ready_call(&Args(&json!({ "gps_info": value })));
        assert_eq!(ready(json!({})).unwrap(), json!(false));
        assert_eq!(ready(json!({"requestSerial": "S"})).unwrap(), json!(false));
        assert_eq!(ready(json!({"latitude": 52.0})).unwrap(), json!(true));
        for falsy in [json!(null), json!(false), json!(""), json!([]), json!(0)] {
            assert_eq!(ready(falsy).unwrap(), json!(false));
        }
        for truthy in [json!(true), json!("x"), json!([1]), json!(1.5)] {
            assert_eq!(ready(truthy).unwrap_err().kind, "AttributeError");
        }
    }

    #[test]
    fn arguments_are_checked() {
        let args = json!({
            "config": {"username": "user@example.com", "password": "p"},
            "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
            "now_ms": 5, "seed": "x", "replies": [], "args": {"endpoint": "/e"},
        });
        let error = parity_call("api.gps.fetch_gps_endpoint", &args)
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, "BadArguments");
        assert!(parity_call("api.gps.nothing", &args).is_none());
    }
}
