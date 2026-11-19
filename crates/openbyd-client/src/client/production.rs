//! The production [`ClientEnv`]: HTTP through an [`HttpEnv`], MQTT through
//! an [`MqttRuntime`], waits and background tasks on tokio.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::mqtt::MqttBootstrap;
use serde_json::{Map, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::env::{BoxFuture, ClientEnv, Env, MqttSink};
use crate::http::HttpEnv;
use crate::mqtt::{MqttNotice, MqttRuntime, MqttRuntimeOptions};

/// The running channel: the runtime and the task that hands its notices to
/// the client.
struct Channel {
    runtime: Arc<MqttRuntime>,
    forwarder: JoinHandle<()>,
}

/// One account's real environment: an [`HttpEnv`] (sharing the process's
/// [`crate::http::HttpPool`]) and, once started, one [`MqttRuntime`] with a
/// task forwarding its notices to the client. Must be used inside a tokio
/// runtime.
pub struct ProductionEnv {
    http: HttpEnv,
    mqtt_options: MqttRuntimeOptions,
    channel: Mutex<Option<Channel>>,
}

impl fmt::Debug for ProductionEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProductionEnv")
            .field("http", &self.http)
            .field("mqtt_running", &self.mqtt_running())
            .finish_non_exhaustive()
    }
}

impl ProductionEnv {
    /// The environment of one account, with default MQTT settings.
    pub fn new(http: HttpEnv) -> Self {
        Self::with_mqtt_options(http, MqttRuntimeOptions::default())
    }

    /// The environment of one account with MQTT `options` (TLS roots,
    /// reconnect delays); the keepalive comes from the client's config.
    pub fn with_mqtt_options(http: HttpEnv, options: MqttRuntimeOptions) -> Self {
        Self {
            http,
            mqtt_options: options,
            channel: Mutex::new(None),
        }
    }

    /// The HTTP environment.
    pub fn http(&self) -> &HttpEnv {
        &self.http
    }

    /// The running MQTT runtime, if any.
    pub fn mqtt_runtime(&self) -> Option<Arc<MqttRuntime>> {
        self.channel()
            .as_ref()
            .map(|channel| Arc::clone(&channel.runtime))
    }

    fn channel(&self) -> MutexGuard<'_, Option<Channel>> {
        self.channel.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take_channel(&self) -> Option<Channel> {
        self.channel().take()
    }
}

impl Drop for ProductionEnv {
    fn drop(&mut self) {
        if let Some(channel) = self.take_channel() {
            channel.forwarder.abort();
        }
    }
}

impl Env for ProductionEnv {
    fn now_ms(&self) -> i64 {
        self.http.now_ms()
    }

    fn monotonic(&self) -> f64 {
        self.http.monotonic()
    }

    fn random_bytes(&self) -> [u8; 16] {
        self.http.random_bytes()
    }

    async fn sleep(&self, seconds: f64) {
        self.http.sleep(seconds).await;
    }

    async fn post_secure(
        &self,
        endpoint: &str,
        outer: &Map<String, Value>,
    ) -> Result<Value, Error> {
        self.http.post_secure(endpoint, outer).await
    }
}

/// A duration tokio accepts, or `None` for "no limit".
fn duration(seconds: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|duration| *duration < Duration::from_secs(10 * 365 * 24 * 3600))
}

/// Hands the runtime's notices to the sink until either goes away.
async fn forward(
    mut notices: tokio::sync::broadcast::Receiver<MqttNotice>,
    sink: Weak<dyn MqttSink>,
) {
    loop {
        let notice = match notices.recv().await {
            Ok(notice) => notice,
            Err(RecvError::Lagged(skipped)) => {
                log::warn!("MQTT notices fell behind; {skipped} were dropped");
                continue;
            }
            Err(RecvError::Closed) => return,
        };
        let Some(sink) = sink.upgrade() else {
            return;
        };
        match notice {
            MqttNotice::Event(event) => sink.on_event(&event),
            MqttNotice::Connected => sink.on_connected(),
            MqttNotice::ConnackRefused { consecutive, .. } => sink.on_connack_refused(consecutive),
            MqttNotice::DecryptFailed => sink.on_decrypt_error(),
            MqttNotice::Disconnected { .. } => {}
        }
    }
}

impl ClientEnv for ProductionEnv {
    async fn wait_for<'a, T: Send + 'a>(
        &'a self,
        future: BoxFuture<'a, T>,
        timeout: f64,
    ) -> Option<T> {
        match duration(timeout) {
            Some(limit) => tokio::time::timeout(limit, future).await.ok(),
            None => Some(future.await),
        }
    }

    fn spawn(&self, task: BoxFuture<'static, ()>) {
        tokio::spawn(task);
    }

    fn uniform(&self, low: f64, high: f64) -> f64 {
        let mut bytes = [0u8; 8];
        if getrandom::getrandom(&mut bytes).is_err() {
            return low + (high - low) / 2.0;
        }
        // 53 random bits, as Python's `random.random()`.
        let unit = (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64;
        low + (high - low) * unit
    }

    async fn start_mqtt(
        &self,
        bootstrap: &MqttBootstrap,
        decrypt_key: &str,
        keepalive: i64,
        sink: Weak<dyn MqttSink>,
    ) -> Result<(), Error> {
        self.stop_mqtt();
        let mut options = self.mqtt_options.clone();
        options.keepalive = u16::try_from(keepalive).unwrap_or(u16::MAX);
        let runtime = Arc::new(MqttRuntime::new(decrypt_key, options));
        // Subscribed before start, so the first "connected" is heard.
        let notices = runtime.subscribe();
        runtime
            .start(bootstrap)
            .await
            .map_err(|error| Error::new(ErrorKind::BydError, error.to_string()))?;
        let forwarder = tokio::spawn(forward(notices, sink));
        if let Some(old) = self.channel().replace(Channel { runtime, forwarder }) {
            old.forwarder.abort();
            tokio::spawn(async move { old.runtime.stop().await });
        }
        Ok(())
    }

    fn stop_mqtt(&self) {
        if let Some(channel) = self.take_channel() {
            channel.forwarder.abort();
            // Stops waits and the reconnect loop now; the clean DISCONNECT
            // completes in the background.
            tokio::spawn(async move { channel.runtime.stop().await });
        }
    }

    fn mqtt_running(&self) -> bool {
        self.channel()
            .as_ref()
            .is_some_and(|channel| channel.runtime.is_running())
    }

    fn update_mqtt_key(&self, decrypt_key: &str) {
        if let Some(channel) = self.channel().as_ref() {
            channel.runtime.update_decrypt_key(decrypt_key);
        }
    }
}
