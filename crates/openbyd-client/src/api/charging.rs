//! Smart charging status endpoint (`pybyd/_api/charging.py`).
//!
//! - `/control/smartCharge/homePage`: live state and the configured schedule in one reply.

use openbyd_protocol::error::Error;
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::charging::ChargingStatus;
use openbyd_protocol::parity::Outcome;
use serde_json::{Map, Value};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_token_json,
};
use crate::api::vehicle::{parse_model, run_trace};
use crate::env::Env;
use crate::scripted::Scenario;

const ENDPOINT: &str = "/control/smartCharge/homePage";

/// Posts `/control/smartCharge/homePage` and returns the reply's data as it
/// is, or an empty object when it is not an object. Both
/// [`fetch_charging_status`] and the smart charging schedule build on it.
pub async fn fetch_charging_homepage<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
) -> Result<Map<String, Value>, Error> {
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
    let decoded = post_token_json(cx, ENDPOINT, &inner, None, Some(vin), codes, None).await?;
    Ok(match decoded {
        Value::Object(entries) => entries,
        _ => Map::new(),
    })
}

/// Fetches the smart charging status (SOC, charge state, time to full).
pub async fn fetch_charging_status<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
) -> Result<ChargingStatus, Error> {
    let raw = fetch_charging_homepage(cx, vin).await?;
    parse_model(&Value::Object(raw))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.charging.fetch_charging_homepage" => Scenario::from_args(args).and_then(|scenario| {
            let vin = scenario.args().str("vin")?;
            run_trace(
                &scenario,
                fetch_charging_homepage(scenario.context(), vin),
                Value::Object,
            )
        }),
        "api.charging.fetch_charging_status" => Scenario::from_args(args).and_then(|scenario| {
            let vin = scenario.args().str("vin")?;
            run_trace(
                &scenario,
                fetch_charging_status(scenario.context(), vin),
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
        for name in [
            "api.charging.fetch_charging_homepage",
            "api.charging.fetch_charging_status",
        ] {
            assert_eq!(
                parity_call(name, &args).unwrap().unwrap_err().kind,
                "BadArguments"
            );
        }
        assert!(parity_call("api.charging.nothing", &args).is_none());
    }
}
