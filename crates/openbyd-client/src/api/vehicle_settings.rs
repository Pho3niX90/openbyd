//! Vehicle settings endpoints (`pybyd/_api/vehicle_settings.py`).
//!
//! - `/control/vehicle/modifyAutoAlias`: rename a vehicle.

use openbyd_protocol::error::Error;
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::control::CommandAck;
use openbyd_protocol::parity::Outcome;
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_token_json,
};
use crate::api::vehicle::{parse_model, run_trace};
use crate::env::Env;
use crate::scripted::Scenario;

const RENAME_ENDPOINT: &str = "/control/vehicle/modifyAutoAlias";

/// The acknowledgement model of a reply: `{"vin": vin, **raw, "raw": raw}`,
/// where `raw` is the reply's data or an empty object when it is not one.
/// Shared with the push switch setter.
pub(crate) fn ack_from_reply(vin: &str, decoded: Value) -> Result<CommandAck, Error> {
    let raw = match decoded {
        Value::Object(raw) => raw,
        _ => Map::new(),
    };
    let mut merged = Map::new();
    merged.insert("vin".into(), json!(vin));
    merged.extend(raw.clone());
    merged.insert("raw".into(), Value::Object(raw));
    parse_model(&Value::Object(merged))
}

/// Renames a vehicle (sets its alias).
pub async fn rename_vehicle<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    name: &str,
) -> Result<CommandAck, Error> {
    let mut inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        None,
    );
    inner.insert("autoAlias".into(), json!(name));
    let codes = ErrorCodes {
        not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
        ..ErrorCodes::default()
    };
    let decoded =
        post_token_json(cx, RENAME_ENDPOINT, &inner, None, Some(vin), codes, None).await?;
    ack_from_reply(vin, decoded)
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.vehicle_settings.rename_vehicle" => Scenario::from_args(args).and_then(|scenario| {
            let call = scenario.args();
            let (vin, name) = (call.str("vin")?, call.str("name")?);
            run_trace(
                &scenario,
                rename_vehicle(scenario.context(), vin, name),
                |ack| ack.dump(),
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
        let args = json!({
            "config": {"username": "user@example.com", "password": "p"},
            "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
            "now_ms": 5, "seed": "x", "replies": [], "args": {"vin": "V"},
        });
        let error = parity_call("api.vehicle_settings.rename_vehicle", &args)
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, "BadArguments");
        assert!(parity_call("api.vehicle_settings.nothing", &args).is_none());
    }

    #[test]
    fn an_ack_keeps_the_reply_as_raw() {
        let ack = ack_from_reply("V", json!({"result": "ok", "vin": "W", "raw": 5})).unwrap();
        assert_eq!(ack.vin, "W");
        assert_eq!(ack.raw["raw"], json!(5));
        let empty = ack_from_reply("V", json!([1])).unwrap();
        assert_eq!(empty.vin, "V");
        assert!(empty.raw.is_empty());
    }
}
