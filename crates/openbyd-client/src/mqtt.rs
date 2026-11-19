//! The MQTT push channel (`pybyd/_mqtt.py`).
//!
//! BYD pushes vehicle events (realtime data, command results, charging
//! state) over MQTT. Connecting takes two steps:
//!
//! 1. [`fetch_mqtt_bootstrap`] asks the API for the broker and builds the
//!    credentials from the session (pyBYD's `fetch_mqtt_bootstrap`).
//! 2. An [`MqttRuntime`] keeps one connection to that broker open, decrypts
//!    each message with the session's content key and hands the events to
//!    its owner (pyBYD's `BydMqttRuntime`). See the [`runtime`] module for
//!    how events and notifications reach the owner.
//!
//! The pure parts (broker parsing, credentials, payload decoding, the two
//! models and the per-message decisions) live in
//! [`openbyd_protocol::mqtt`] and are re-exported here.

pub mod runtime;

use openbyd_protocol::error::Error;
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::parity::Outcome;
use serde_json::Value;

pub use openbyd_protocol::mqtt::{
    EMQ_BROKER_ENDPOINT, MessageDecoder, MessageOutcome, MqttBootstrap, MqttEvent,
    broker_from_reply, build_client_id, build_mqtt_bootstrap, build_mqtt_password,
    decode_mqtt_payload, parse_broker, unix_seconds,
};
pub use runtime::{
    EventFilter, EventWaiter, MqttNotice, MqttRuntime, MqttRuntimeOptions, MqttStartError,
    MqttStatus, WaitError, client_config,
};
/// The TLS library the runtime uses, for building a custom [`MqttRuntimeOptions::tls`].
pub use rustls;

use crate::api::Context;
use crate::api::common::{ErrorCodes, build_inner_base, post_token_json};
use crate::env::Env;
use crate::scripted::Scenario;

/// Asks the API which broker serves this account
/// (`/app/emqAuth/getEmqBrokerIp`) and returns its address, as pyBYD's
/// `_fetch_emq_broker`.
async fn fetch_emq_broker<E: Env>(cx: Context<'_, E>) -> Result<String, Error> {
    let inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        None,
        None,
    );
    let decoded = post_token_json(
        cx,
        EMQ_BROKER_ENDPOINT,
        &inner,
        None,
        None,
        ErrorCodes::default(),
        None,
    )
    .await?;
    broker_from_reply(&decoded)
}

/// Builds the MQTT connection details for the logged-in session: the
/// broker the API names, the client id, and a password derived from the
/// session and the current time.
///
/// Fails as the broker lookup does (any API or transport error), with
/// `BydError` when the reply names no broker, and with `ValueError` when
/// the broker's port cannot be read.
pub async fn fetch_mqtt_bootstrap<E: Env>(cx: Context<'_, E>) -> Result<MqttBootstrap, Error> {
    let broker = fetch_emq_broker(cx).await?;
    build_mqtt_bootstrap(
        cx.config,
        cx.session,
        &broker,
        unix_seconds(cx.env.now_ms()),
    )
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "mqtt.fetch_mqtt_bootstrap" => Scenario::from_args(args).and_then(|scenario| {
            scenario.run(fetch_mqtt_bootstrap(scenario.context()), |bootstrap| {
                bootstrap.dump()
            })
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
        let error = parity_call("mqtt.fetch_mqtt_bootstrap", &json!({}))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, "BadArguments");
        assert!(parity_call("mqtt.nothing", &json!({})).is_none());
    }
}
