//! Per-endpoint request building and reply handling. Nothing here touches the network.

pub mod booking;
pub mod charging;
pub mod common;
pub mod control;
pub mod energy;
pub mod gps;
pub mod hvac;
pub mod latest_config;
pub mod message_center;
pub mod push_notifications;
pub mod realtime;
pub mod smart_charging;
pub mod vehicle;
pub mod vehicle_settings;

use serde_json::Value;

use crate::parity::{Outcome, first_of};

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    first_of(
        &[
            booking::parity_call,
            charging::parity_call,
            common::parity_call,
            control::parity_call,
            energy::parity_call,
            gps::parity_call,
            hvac::parity_call,
            latest_config::parity_call,
            message_center::parity_call,
            push_notifications::parity_call,
            realtime::parity_call,
            smart_charging::parity_call,
            vehicle::parity_call,
            vehicle_settings::parity_call,
        ],
        name,
        args,
    )
}
