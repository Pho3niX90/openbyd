//! Vehicle realtime data endpoints (`pybyd/_api/realtime.py`).
//!
//! - `/vehicleInfo/vehicle/vehicleRealTimeRequest` (trigger)
//! - `/vehicleInfo/vehicle/vehicleRealTimeResult` (poll)

use openbyd_protocol::coerce::IntEnum;
use openbyd_protocol::error::Error;
use openbyd_protocol::models::vehicle::EnergyType;
use openbyd_protocol::parity::Outcome;
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_vehicle_json,
};
use crate::api::vehicle::{energy_type_argument, py_truthy, run_trace};
use crate::env::Env;
use crate::scripted::Scenario;

/// Fetches one realtime endpoint and returns the reply's data (an empty
/// object when it is not an object) with the serial to poll with next.
///
/// The next serial is the reply's `requestSerial` when that is truthy,
/// whatever its type (pyBYD does not check), else `request_serial`.
/// `energy_type` is the vehicle's own: a hybrid asked as `EnergyType::Ev`
/// has its ICE-side fields zeroed by the cloud. Code 1001 is
/// `BydEndpointNotSupportedError`.
pub async fn fetch_realtime_endpoint<E: Env>(
    cx: Context<'_, E>,
    endpoint: &str,
    vin: &str,
    request_serial: Option<&str>,
    energy_type: EnergyType,
) -> Result<(Map<String, Value>, Option<Value>), Error> {
    let now_ms = cx.env.now_ms();
    let mut inner = build_inner_base(
        cx.config,
        now_ms,
        &cx.env.random_bytes(),
        Some(vin),
        request_serial,
    );
    inner.insert("energyType".into(), json!(energy_type.value().to_string()));
    inner.insert("tboxVersion".into(), json!(cx.config.tbox_version));
    let codes = ErrorCodes {
        not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
        ..ErrorCodes::default()
    };
    let decoded = post_vehicle_json(cx, endpoint, &inner, vin, Some(now_ms), codes).await?;
    let vehicle_info = match decoded {
        Value::Object(info) => info,
        _ => Map::new(),
    };
    let next_serial = match vehicle_info.get("requestSerial") {
        Some(serial) if py_truthy(serial) => Some(serial.clone()),
        _ => request_serial.map(|serial| json!(serial)),
    };
    Ok((vehicle_info, next_serial))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.realtime.fetch_realtime_endpoint" => Scenario::from_args(args).and_then(|scenario| {
            let call = scenario.args();
            let (endpoint, vin) = (call.str("endpoint")?, call.str("vin")?);
            let request_serial = call.opt_str("request_serial")?;
            let energy_type = energy_type_argument(&scenario, "energy_type", EnergyType::Ev)?;
            run_trace(
                &scenario,
                fetch_realtime_endpoint(
                    scenario.context(),
                    endpoint,
                    vin,
                    request_serial,
                    energy_type,
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
    fn arguments_are_checked() {
        let mut args = json!({
            "config": {"username": "user@example.com", "password": "p"},
            "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
            "now_ms": 5, "seed": "x", "replies": [], "args": {"endpoint": "/e"},
        });
        let kind = |args: &Value| {
            parity_call("api.realtime.fetch_realtime_endpoint", args)
                .unwrap()
                .unwrap_err()
                .kind
        };
        assert_eq!(kind(&args), "BadArguments");
        args["args"] = json!({"endpoint": "/e", "vin": "V", "energy_type": "x"});
        assert_eq!(kind(&args), "BadArguments");
        args["args"] = json!({"endpoint": "/e", "vin": "V", "request_serial": 4});
        assert_eq!(kind(&args), "BadArguments");
        assert!(parity_call("api.realtime.nothing", &args).is_none());
    }
}
