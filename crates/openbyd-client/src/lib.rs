//! Async client for the BYD overseas app API: a Rust port of pyBYD.
//!
//! - [`client`]: [`client::BydClient`], pyBYD's `BydClient`: sessions,
//!   reads, remote commands and the MQTT push channel, with the options
//!   applications used to monkeypatch (gate policy, retries, PIN resolver,
//!   re-login policy, request observer) and session export and import.
//! - [`car`]: [`car::BydCar`], pyBYD's `BydCar`: one vehicle's merged state,
//!   its capability commands with optimistic projections, and its updates.
//!   [`client::BydClient::get_car`] builds and keeps one per VIN.
//! - [`api`]: the endpoint functions of pyBYD's `_api` package, ported 1:1
//!   as `async fn`s generic over an [`Env`] that supplies the network, the
//!   clock, randomness and sleeping.
//! - [`http`]: the production HTTP environment and the connection pool
//!   ([`http::HttpPool`]) that every client in a process shares.
//! - [`mqtt`]: the broker lookup and the MQTT runtime.
//! - [`testing`] (feature `testing`): a fake BYD server and a TLS MQTT test
//!   broker, for tests, bindings and benchmarks.
//!
//! The same code runs against the real service and, in the parity tests,
//! against a script ([`scripted`]) whose trace is compared with pyBYD's
//! (see [`parity`]).

pub mod api;
pub mod car;
pub mod client;
pub mod env;
pub mod http;
pub mod mqtt;
pub mod parity;
#[doc(hidden)]
pub mod scripted;
pub mod testing;

pub use env::Env;
