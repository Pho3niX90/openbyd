//! Signing, envelopes, models and error codes of the BYD overseas app API.
//!
//! Nothing in this crate touches the network or the clock: every function
//! takes the time and any random value as an argument, so the same inputs
//! always give the same bytes. `openbyd-client` adds the I/O.
//!
//! The crate is a port of [pyBYD](https://github.com/jkaberg/pyBYD) and is
//! tested against it. See `AGENTS.md` at the repository root for how.

pub mod aes;
pub mod api;
pub mod capabilities;
pub mod coerce;
pub mod config;
pub mod constants;
pub mod envelope;
pub mod error;
pub mod hashing;
pub mod models;
pub mod mqtt;
pub mod parity;
pub mod pyjson;
pub mod redact;
pub mod session;
pub mod signing;
pub mod state;
