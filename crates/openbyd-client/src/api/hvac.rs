//! HVAC status endpoint (`pybyd/_api/hvac.py`).
//!
//! - `/control/getStatusNow`

use openbyd_protocol::error::Error;
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::hvac::HvacStatus;
use openbyd_protocol::parity::Outcome;
use serde_json::Value;

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_vehicle_json,
};
use crate::api::vehicle::{parse_model, run_trace};
use crate::env::Env;
use crate::scripted::Scenario;

const ENDPOINT: &str = "/control/getStatusNow";

/// Fetches the current HVAC and climate control status of a vehicle. It
/// reaches the car, so a guest account declares itself (see
/// [`post_vehicle_json`]).
pub async fn fetch_hvac_status<E: Env>(cx: Context<'_, E>, vin: &str) -> Result<HvacStatus, Error> {
    let inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        None,
    );
    let codes = ErrorCodes {
        not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
        ..ErrorCodes::default()
    };
    let decoded = post_vehicle_json(cx, ENDPOINT, &inner, vin, None, codes).await?;
    parse_model(&decoded)
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.hvac.fetch_hvac_status" => Scenario::from_args(args).and_then(|scenario| {
            let vin = scenario.args().str("vin")?;
            run_trace(
                &scenario,
                fetch_hvac_status(scenario.context(), vin),
                |status| status.dump(),
            )
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn arguments_are_checked() {
        let args = json!({
            "config": {"username": "user@example.com", "password": "p"},
            "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
            "now_ms": 5, "seed": "x", "replies": [], "args": {},
        });
        let error = parity_call("api.hvac.fetch_hvac_status", &args)
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, "BadArguments");
        assert!(parity_call("api.hvac.nothing", &args).is_none());
    }
}
