//! What a flow needs from the outside world (pyBYD's `Transport`, plus the
//! clock, randomness and sleeping that pyBYD reads from `time`, `secrets`
//! and `asyncio`).
//!
//! Every function in [`crate::api`] is generic over [`Env`] and reaches the
//! network, the clock and randomness only through it. Production code
//! implements it over HTTP and the system clock; the parity tests implement
//! it with [`crate::scripted::ScriptedEnv`], which replays a script and
//! records what the flow did.

use std::future::Future;

use openbyd_protocol::error::Error;
use serde_json::{Map, Value};

/// The environment a flow runs in.
///
/// The futures are `Send`, so a flow can run on a multi-threaded runtime,
/// and the methods take `&self`, so several flows can share one environment.
///
/// Waiting for MQTT events will be added here when the flows that need it
/// (`_api/control.py`, the realtime and GPS pollers) are ported.
pub trait Env: Send + Sync {
    /// The wall clock in milliseconds since the Unix epoch: pyBYD's
    /// `int(time.time() * 1000)`.
    fn now_ms(&self) -> i64;

    /// A monotonic clock in seconds: pyBYD's `time.monotonic()`.
    fn monotonic(&self) -> f64;

    /// 16 fresh random bytes: pyBYD's `secrets.token_hex(16)`, before it is hex-encoded.
    fn random_bytes(&self) -> [u8; 16];

    /// Waits `seconds`: pyBYD's `asyncio.sleep(seconds)`.
    fn sleep(&self, seconds: f64) -> impl Future<Output = ()> + Send;

    /// Sends a signed outer payload to `endpoint` and returns the decoded
    /// outer reply: pyBYD's `SecureTransport.post_secure`.
    ///
    /// A real implementation encodes the body with
    /// [`openbyd_protocol::envelope::encode_request_body`], posts it to the
    /// config's `base_url` followed by `endpoint` with
    /// [`openbyd_protocol::envelope::REQUEST_HEADERS`], and decodes the reply
    /// with [`openbyd_protocol::envelope::decode_response_body`]. Network
    /// failures are `BydTransportError`s.
    fn post_secure(
        &self,
        endpoint: &str,
        outer: &Map<String, Value>,
    ) -> impl Future<Output = Result<Value, Error>> + Send;
}
