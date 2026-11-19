//! Vehicle capability config endpoint (`pybyd/_api/latest_config.py`).
//!
//! - `/vehicle/vehicleswitch/getLatestConfig`

use openbyd_protocol::error::Error;
use openbyd_protocol::models::latest_config::VehicleLatestConfig;
use openbyd_protocol::parity::{Outcome, ParityError};
use openbyd_protocol::pyjson::{DumpOptions, Separators, dumps};
use serde_json::{Value, json};

use crate::api::Context;
use crate::api::common::{ErrorCodes, build_inner_base, post_token_json};
use crate::api::vehicle::{dump_map, parse_owned_model, run_trace};
use crate::env::Env;
use crate::scripted::Scenario;

const ENDPOINT: &str = "/vehicle/vehicleswitch/getLatestConfig";

/// `json.dumps(vin_list, ensure_ascii=False)`: Python's default separators.
fn vin_list_text(vin_list: &[String]) -> String {
    let list = Value::Array(vin_list.iter().map(|vin| json!(vin)).collect());
    dumps(
        &list,
        DumpOptions {
            ensure_ascii: false,
            separators: Separators::Default,
            sort_keys: false,
        },
    )
}

/// Fetches the latest capability configuration of each VIN in `vin_list`.
/// The reply is an object of VIN to configuration; entries that are not
/// objects are skipped, in the reply's order.
pub async fn fetch_latest_config<E: Env>(
    cx: Context<'_, E>,
    vin_list: &[String],
) -> Result<Vec<(String, VehicleLatestConfig)>, Error> {
    let mut inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        None,
        None,
    );
    inner.insert("appConfigVersion".into(), json!("2"));
    inner.insert("terminalType".into(), json!("0"));
    inner.insert("vinList".into(), json!(vin_list_text(vin_list)));
    let decoded = post_token_json(
        cx,
        ENDPOINT,
        &inner,
        None,
        None,
        ErrorCodes::default(),
        None,
    )
    .await?;
    let Value::Object(payload) = decoded else {
        return Ok(Vec::new());
    };
    payload
        .into_iter()
        .filter(|(_, raw)| raw.is_object())
        .map(|(vin, raw)| Ok((vin, parse_owned_model(raw)?)))
        .collect()
}

fn vin_list_argument(scenario: &Scenario) -> Result<Vec<String>, ParityError> {
    scenario
        .args()
        .value("vin_list")
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .ok_or_else(|| ParityError::bad_arguments("vin_list must be a list of strings"))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.latest_config.fetch_latest_config" => Scenario::from_args(args).and_then(|scenario| {
            let vin_list = vin_list_argument(&scenario)?;
            run_trace(
                &scenario,
                fetch_latest_config(scenario.context(), &vin_list),
                |configs| dump_map(&configs),
            )
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vin_list_is_python_json() {
        assert_eq!(vin_list_text(&[]), "[]");
        assert_eq!(
            vin_list_text(&["A".into(), "Zoë \"1\"\n".into()]),
            "[\"A\", \"Zoë \\\"1\\\"\\n\"]"
        );
    }

    #[test]
    fn arguments_are_checked() {
        let mut args = json!({
            "config": {"username": "user@example.com", "password": "p"},
            "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
            "now_ms": 5, "seed": "x", "replies": [], "args": {},
        });
        let kind = |args: &Value| {
            parity_call("api.latest_config.fetch_latest_config", args)
                .unwrap()
                .unwrap_err()
                .kind
        };
        assert_eq!(kind(&args), "BadArguments");
        args["args"] = json!({"vin_list": [1]});
        assert_eq!(kind(&args), "BadArguments");
        assert!(parity_call("api.latest_config.nothing", &args).is_none());
    }
}
