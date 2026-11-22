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
use std::pin::Pin;
use std::sync::{Arc, Weak};

use openbyd_protocol::error::Error;
use openbyd_protocol::mqtt::{MqttBootstrap, MqttEvent};
use serde_json::{Map, Value};

/// The environment a flow runs in.
///
/// The futures are `Send`, so a flow can run on a multi-threaded runtime,
/// and the methods take `&self`, so several flows can share one environment.
///
/// What [`crate::client::BydClient`] needs on top of this (waiting with a
/// timeout, background tasks and the MQTT channel) is [`ClientEnv`].
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

    /// How far BYD's clock is ahead of ours, in milliseconds, as the last
    /// reply's `Date` header showed (pyBYD's
    /// `SecureTransport.server_time_offset_ms`): 0 until a drift beyond
    /// [`openbyd_protocol::constants::SERVER_TIME_DRIFT_THRESHOLD_MS`] is
    /// seen. Requests are stamped with our clock plus this. Every
    /// environment must report it: a wrapper that forgot to forward it would
    /// sign with the local clock again.
    fn server_time_offset_ms(&self) -> i64;
}

/// A boxed future, as [`ClientEnv`] passes them around.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where the MQTT channel delivers what it receives: pyBYD's
/// `BydMqttRuntime` callbacks (`on_event`, `on_decrypt_error`,
/// `on_connack_refused`, `on_connected`). [`crate::client::BydClient`]
/// implements it; the environment holds it weakly, so a forgotten channel
/// never keeps a client alive.
pub trait MqttSink: Send + Sync {
    /// A message decrypted into an event.
    fn on_event(&self, event: &MqttEvent);
    /// A message did not decrypt with a key no message has confirmed yet.
    fn on_decrypt_error(&self);
    /// The broker refused the connection, `consecutive` times in a row.
    fn on_connack_refused(&self, consecutive: u32);
    /// The broker accepted the connection.
    fn on_connected(&self);
    /// The connection dropped (it reconnects by itself). openbyd's addition.
    fn on_disconnected(&self, _reason: &str) {}
}

/// The environment of a whole client: a flow [`Env`] plus what pyBYD's
/// `BydClient` takes from asyncio and paho.
///
/// [`crate::client::ProductionEnv`] implements it with tokio and an
/// [`crate::mqtt::MqttRuntime`]; [`crate::scripted::ScriptedEnv`] with a
/// script of MQTT events arriving at given times, for the client parity tests.
pub trait ClientEnv: Env + 'static {
    /// Waits for `future` at most `timeout` seconds (above 0): pyBYD's
    /// `asyncio.wait_for`. `None` when the time ran out.
    ///
    /// While it waits, the MQTT channel goes on delivering to its sink.
    fn wait_for<'a, T: Send + 'a>(
        &'a self,
        future: BoxFuture<'a, T>,
        timeout: f64,
    ) -> impl Future<Output = Option<T>> + Send + 'a;

    /// Runs `task` in the background: pyBYD's `asyncio.ensure_future`.
    fn spawn(&self, task: BoxFuture<'static, ()>);

    /// A random float between `low` and `high`: pyBYD's `random.uniform`.
    fn uniform(&self, low: f64, high: f64) -> f64;

    /// Connects the MQTT channel with `bootstrap`, decrypting with
    /// `decrypt_key` and delivering to `sink`, after stopping any previous
    /// channel: pyBYD's `BydMqttRuntime(...)` and `runtime.start(bootstrap)`.
    fn start_mqtt(
        &self,
        bootstrap: &MqttBootstrap,
        decrypt_key: &str,
        keepalive: i64,
        sink: Weak<dyn MqttSink>,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Stops the MQTT channel, if any: pyBYD's `runtime.stop()`. Nothing is
    /// delivered to the sink afterwards.
    fn stop_mqtt(&self);

    /// Whether the MQTT channel is started and not stopped: pyBYD's
    /// `runtime.is_running`.
    fn mqtt_running(&self) -> bool;

    /// Decrypts later messages with `decrypt_key`: pyBYD's
    /// `runtime.update_decrypt_key`.
    fn update_mqtt_key(&self, decrypt_key: &str);
}

impl<T: Env + ?Sized> Env for Arc<T> {
    fn now_ms(&self) -> i64 {
        (**self).now_ms()
    }

    fn monotonic(&self) -> f64 {
        (**self).monotonic()
    }

    fn random_bytes(&self) -> [u8; 16] {
        (**self).random_bytes()
    }

    fn sleep(&self, seconds: f64) -> impl Future<Output = ()> + Send {
        (**self).sleep(seconds)
    }

    fn post_secure(
        &self,
        endpoint: &str,
        outer: &Map<String, Value>,
    ) -> impl Future<Output = Result<Value, Error>> + Send {
        (**self).post_secure(endpoint, outer)
    }

    fn server_time_offset_ms(&self) -> i64 {
        (**self).server_time_offset_ms()
    }
}

impl<T: ClientEnv> ClientEnv for Arc<T> {
    fn wait_for<'a, U: Send + 'a>(
        &'a self,
        future: BoxFuture<'a, U>,
        timeout: f64,
    ) -> impl Future<Output = Option<U>> + Send + 'a {
        (**self).wait_for(future, timeout)
    }

    fn spawn(&self, task: BoxFuture<'static, ()>) {
        (**self).spawn(task);
    }

    fn uniform(&self, low: f64, high: f64) -> f64 {
        (**self).uniform(low, high)
    }

    fn start_mqtt(
        &self,
        bootstrap: &MqttBootstrap,
        decrypt_key: &str,
        keepalive: i64,
        sink: Weak<dyn MqttSink>,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        (**self).start_mqtt(bootstrap, decrypt_key, keepalive, sink)
    }

    fn stop_mqtt(&self) {
        (**self).stop_mqtt();
    }

    fn mqtt_running(&self) -> bool {
        (**self).mqtt_running()
    }

    fn update_mqtt_key(&self, decrypt_key: &str) {
        (**self).update_mqtt_key(decrypt_key);
    }
}
