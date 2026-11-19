//! Push notification endpoints (`pybyd/_api/push_notifications.py`).
//!
//! The EU host uses the `/vehicle/vehicleswitch/` namespace, as for
//! getLatestConfig and verifyControlPassword; the `/app/push/` paths answer
//! 404 there.
//!
//! - `/vehicle/vehicleswitch/getPushSwitchState` (current state)
//! - `/vehicle/vehicleswitch/setPushSwitchState` (toggle on or off)

use openbyd_protocol::error::Error;
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::control::CommandAck;
use openbyd_protocol::models::push_notification::{
    PushNotificationState, PushSwitch, VEHICLE_STATUS_PUSH_TYPE,
};
use openbyd_protocol::parity::Outcome;
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_token_json,
};
use crate::api::vehicle::{parse_model, run_trace};
use crate::api::vehicle_settings::ack_from_reply;
use crate::env::Env;
use crate::scripted::Scenario;

const GET_ENDPOINT: &str = "/vehicle/vehicleswitch/getPushSwitchState";
const SET_ENDPOINT: &str = "/vehicle/vehicleswitch/setPushSwitchState";

fn codes() -> ErrorCodes<'static> {
    ErrorCodes {
        not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
        ..ErrorCodes::default()
    }
}

/// Fetches the push notification switches of a vehicle. The reply is a list
/// of `{type, state}`; anything else is no switches, and items that are not
/// objects are skipped.
///
/// pyBYD builds each switch from the item's `type` and `state` only, and the
/// state from the switches themselves, so `raw` of the state holds the VIN
/// and the switches as dumped.
pub async fn fetch_push_state<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
) -> Result<PushNotificationState, Error> {
    let inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        None,
    );
    let decoded = post_token_json(cx, GET_ENDPOINT, &inner, None, Some(vin), codes(), None).await?;
    let items: &[Value] = decoded.as_array().map_or(&[], Vec::as_slice);
    let mut switches = Vec::new();
    for item in items.iter().filter_map(Value::as_object) {
        // `PushSwitch(type=item.get("type"), state=item.get("state"))`
        let mut fields = Map::new();
        for key in ["type", "state"] {
            fields.insert(key.into(), item.get(key).cloned().unwrap_or(Value::Null));
        }
        let switch: PushSwitch = parse_model(&Value::Object(fields))?;
        switches.push(switch.dump());
    }
    // `PushNotificationState(vin=vin, switches=switches)`: the switches are
    // already models, which a dump parses back to the same thing.
    parse_model(&json!({"vin": vin, "switches": switches}))
}

/// Enables or disables the push switch of one notification type
/// (pyBYD's default `push_type` is [`VEHICLE_STATUS_PUSH_TYPE`], 701).
///
/// The EU endpoint is keyed by `type`. On a Sealion 7 EU the write answers
/// code 1001 for the status push type: the car does not allow enabling it
/// remotely. The body `{type, state}` mirrors the read keys and is not
/// verified against a successful write.
pub async fn set_push_state<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    enable: bool,
    push_type: i64,
) -> Result<CommandAck, Error> {
    let mut inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        None,
    );
    inner.insert("type".into(), json!(push_type.to_string()));
    inner.insert("state".into(), json!(if enable { "1" } else { "0" }));
    let decoded = post_token_json(cx, SET_ENDPOINT, &inner, None, Some(vin), codes(), None).await?;
    ack_from_reply(vin, decoded)
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.push_notifications.fetch_push_state" => {
            Scenario::from_args(args).and_then(|scenario| {
                let vin = scenario.args().str("vin")?;
                run_trace(
                    &scenario,
                    fetch_push_state(scenario.context(), vin),
                    |state| state.dump(),
                )
            })
        }
        "api.push_notifications.set_push_state" => Scenario::from_args(args).and_then(|scenario| {
            let call = scenario.args();
            let vin = call.str("vin")?;
            let enable = call.bool("enable")?;
            let push_type = match call.value("push_type") {
                Value::Null => VEHICLE_STATUS_PUSH_TYPE,
                _ => call.i64("push_type")?,
            };
            run_trace(
                &scenario,
                set_push_state(scenario.context(), vin, enable, push_type),
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
        let mut args = json!({
            "config": {"username": "user@example.com", "password": "p"},
            "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
            "now_ms": 5, "seed": "x", "replies": [], "args": {"vin": "V"},
        });
        let kind = |name: &str, args: &Value| parity_call(name, args).unwrap().unwrap_err().kind;
        assert_eq!(
            kind("api.push_notifications.set_push_state", &args),
            "BadArguments"
        );
        args["args"] = json!({"vin": "V", "enable": true, "push_type": "x"});
        assert_eq!(
            kind("api.push_notifications.set_push_state", &args),
            "BadArguments"
        );
        args["args"] = json!({});
        assert_eq!(
            kind("api.push_notifications.fetch_push_state", &args),
            "BadArguments"
        );
        assert!(parity_call("api.push_notifications.nothing", &args).is_none());
    }
}
