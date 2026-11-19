//! Async client for the BYD overseas app API.
//!
//! The endpoint functions of pyBYD's `_api` package are ported 1:1 in
//! [`api`], as `async fn`s generic over an [`Env`] that supplies the
//! network, the clock, randomness and sleeping. The same code runs against
//! the real service and, in the parity tests, against a script
//! ([`scripted`]) whose trace is compared with pyBYD's.
//!
//! The HTTP environment, the session manager and MQTT are not ported yet.

pub mod api;
pub mod client;
pub mod env;
pub mod http;
pub mod mqtt;
pub mod parity;
#[doc(hidden)]
pub mod scripted;

pub use env::Env;
