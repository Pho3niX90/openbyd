//! pyBYD's `_api` package: one module per file, with the same function names.
//!
//! Each function builds a request, posts it through the [`Env`] and
//! interprets the reply, exactly as the pyBYD coroutine of the same name.
//! pyBYD passes `config`, `session` and `transport` to every one of them;
//! here they travel together as a [`Context`].

pub mod booking;
pub mod charging;
pub mod common;
pub mod control;
pub mod energy;
pub mod gps;
pub mod hvac;
pub mod latest_config;
pub mod login;
pub mod maintenance;
pub mod message_center;
pub mod ota;
pub mod push_notifications;
pub mod realtime;
pub mod smart_charging;
pub mod standby;
pub mod vehicle;
pub mod vehicle_settings;

use openbyd_protocol::config::BydConfig;
use openbyd_protocol::parity::{Outcome, first_of};
use openbyd_protocol::session::Session;
use serde_json::Value;

use crate::env::Env;

/// What every flow works with: the environment (pyBYD's `transport`, clock
/// and randomness), the configuration and the logged-in session.
///
/// The session is shared: flows running at the same time on one session
/// see each other's corrections to its guest state.
pub struct Context<'a, E> {
    /// The network, clock, randomness and sleeping.
    pub env: &'a E,
    /// The client configuration.
    pub config: &'a BydConfig,
    /// The logged-in session.
    pub session: &'a Session,
}

impl<E> Clone for Context<'_, E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for Context<'_, E> {}

impl<'a, E: Env> Context<'a, E> {
    /// Bundles the three for passing to flows.
    pub fn new(env: &'a E, config: &'a BydConfig, session: &'a Session) -> Self {
        Self {
            env,
            config,
            session,
        }
    }
}

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
            login::parity_call,
            maintenance::parity_call,
            message_center::parity_call,
            ota::parity_call,
            push_notifications::parity_call,
            realtime::parity_call,
            smart_charging::parity_call,
            standby::parity_call,
            vehicle::parity_call,
            vehicle_settings::parity_call,
        ],
        name,
        args,
    )
}
