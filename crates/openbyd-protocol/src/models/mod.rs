//! Typed forms of the API's replies. Each one serialises to what pyBYD's `model_dump(mode="json")` gives.

pub mod base;
pub mod booking;
pub mod charging;
pub mod command_gating;
pub mod control;
pub mod energy;
pub mod gps;
pub mod hvac;
pub mod latest_config;
pub mod message_center;
pub mod push_notification;
pub mod realtime;
pub mod smart_charging;
pub mod token;
pub mod vehicle;

use serde_json::Value;

use crate::parity::{Outcome, first_of};

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    first_of(
        &[
            base::parity_call,
            booking::parity_call,
            charging::parity_call,
            command_gating::parity_call,
            control::parity_call,
            energy::parity_call,
            gps::parity_call,
            hvac::parity_call,
            latest_config::parity_call,
            message_center::parity_call,
            push_notification::parity_call,
            realtime::parity_call,
            smart_charging::parity_call,
            token::parity_call,
            vehicle::parity_call,
        ],
        name,
        args,
    )
}
