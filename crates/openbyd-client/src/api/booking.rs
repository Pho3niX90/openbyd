//! Booking-list endpoint (`pybyd/_api/booking.py`).
//!
//! - `/control/getBookingList`: the scheduled-climate timers.

use openbyd_protocol::error::Error;
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::booking::BookingList;
use openbyd_protocol::parity::Outcome;
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_token_json,
};
use crate::api::vehicle::{parse_owned_model, run_trace};
use crate::env::Env;
use crate::scripted::Scenario;

const ENDPOINT: &str = "/control/getBookingList";

/// Fetches the vehicle's scheduled-climate timers. The endpoint sometimes
/// answers with an empty object even when a booking exists, so an empty list
/// means "none reported". A `vin` in the reply replaces the one asked for.
pub async fn fetch_booking_list<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
) -> Result<BookingList, Error> {
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
    // `{"vin": vin, **raw}`
    let mut merged = Map::new();
    merged.insert("vin".into(), json!(vin));
    if let Value::Object(raw) = decoded {
        merged.extend(raw);
    }
    parse_owned_model(Value::Object(merged))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.booking.fetch_booking_list" => Scenario::from_args(args).and_then(|scenario| {
            let vin = scenario.args().str("vin")?;
            run_trace(
                &scenario,
                fetch_booking_list(scenario.context(), vin),
                |bookings| bookings.dump(),
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
            "now_ms": 5, "seed": "x", "replies": [], "args": {},
        });
        let error = parity_call("api.booking.fetch_booking_list", &args)
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, "BadArguments");
        assert!(parity_call("api.booking.nothing", &args).is_none());
    }
}
