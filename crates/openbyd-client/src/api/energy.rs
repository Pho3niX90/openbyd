//! Energy consumption endpoint (`pybyd/_api/energy.py`).
//!
//! - `/vehicleInfo/vehicle/getEnergyConsumption`

use openbyd_protocol::coerce::IntEnum;
use openbyd_protocol::error::Error;
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::energy::EnergyConsumption;
use openbyd_protocol::models::vehicle::EnergyType;
use openbyd_protocol::parity::Outcome;
use serde_json::{Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_token_json,
};
use crate::api::vehicle::{energy_type_argument, parse_owned_model, run_trace};
use crate::env::Env;
use crate::scripted::Scenario;

const ENDPOINT: &str = "/vehicleInfo/vehicle/getEnergyConsumption";

/// Fetches the energy consumption data of a vehicle.
///
/// Beyond the base fields the request carries `powerType` (the energy type
/// as a string of its integer value: `"0"` EV view, `"1"` ICE view, `"2"`
/// hybrid combined), `requestType` (`0`, purpose unknown but the app sends
/// it) and, when `auto_model_name` is not empty, `autoModelNameOut`, without
/// which the `autoModelGraph` section zero-fills.
pub async fn fetch_energy_consumption<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    power_type: EnergyType,
    auto_model_name: Option<&str>,
) -> Result<EnergyConsumption, Error> {
    let mut inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        None,
    );
    inner.insert("powerType".into(), json!(power_type.value().to_string()));
    inner.insert("requestType".into(), json!(0));
    if let Some(name) = auto_model_name.filter(|name| !name.is_empty()) {
        inner.insert("autoModelNameOut".into(), json!(name));
    }
    let codes = ErrorCodes {
        not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
        ..ErrorCodes::default()
    };
    let decoded = post_token_json(cx, ENDPOINT, &inner, None, Some(vin), codes, None).await?;
    parse_owned_model(decoded)
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.energy.fetch_energy_consumption" => Scenario::from_args(args).and_then(|scenario| {
            let call = scenario.args();
            let vin = call.str("vin")?;
            let power_type = energy_type_argument(&scenario, "power_type", EnergyType::Ev)?;
            let auto_model_name = call.opt_str("auto_model_name")?;
            run_trace(
                &scenario,
                fetch_energy_consumption(scenario.context(), vin, power_type, auto_model_name),
                |energy| energy.dump(),
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
            "now_ms": 5, "seed": "x", "replies": [], "args": {},
        });
        let kind = |args: &Value| {
            parity_call("api.energy.fetch_energy_consumption", args)
                .unwrap()
                .unwrap_err()
                .kind
        };
        assert_eq!(kind(&args), "BadArguments");
        args["args"] = json!({"vin": "V", "power_type": 9});
        assert_eq!(kind(&args), "BadArguments");
        args["args"] = json!({"vin": "V", "auto_model_name": 5});
        assert_eq!(kind(&args), "BadArguments");
        assert!(parity_call("api.energy.nothing", &args).is_none());
    }
}
