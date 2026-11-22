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

    fn server_time_offset_ms(&self) -> i64 {
        self.http.server_time_offset_ms()
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
            MqttNotice::Disconnected { reason } => sink.on_disconnected(&reason),
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use openbyd_protocol::mqtt::MqttEvent;

    use super::*;
    use crate::http::HttpPool;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl MqttSink for Recorder {
        fn on_event(&self, event: &MqttEvent) {
            self.0
                .lock()
                .unwrap()
                .push(format!("event {}", event.event));
        }
        fn on_decrypt_error(&self) {
            self.0.lock().unwrap().push("decrypt".into());
        }
        fn on_connack_refused(&self, consecutive: u32) {
            self.0
                .lock()
                .unwrap()
                .push(format!("refused {consecutive}"));
        }
        fn on_connected(&self) {
            self.0.lock().unwrap().push("connected".into());
        }
    }

    fn env() -> ProductionEnv {
        ProductionEnv::new(HttpEnv::new(&HttpPool::new(), "http://127.0.0.1:9"))
    }

    #[test]
    fn durations_tokio_accepts() {
        assert_eq!(duration(1.5), Some(Duration::from_millis(1500)));
        assert_eq!(duration(f64::INFINITY), None);
        assert_eq!(duration(f64::NAN), None);
        assert_eq!(duration(-1.0), None);
        assert_eq!(duration(1e12), None);
    }

    #[tokio::test]
    async fn waits_sleeps_and_randomness() {
        let env = env();
        assert_eq!(env.wait_for(Box::pin(async { 7 }), 1.0).await, Some(7));
        assert_eq!(
            env.wait_for(Box::pin(std::future::pending::<()>()), 0.01)
                .await,
            None
        );
        assert_eq!(
            env.wait_for(Box::pin(async { 1 }), f64::INFINITY).await,
            Some(1)
        );
        for _ in 0..100 {
            let draw = env.uniform(0.8, 1.2);
            assert!((0.8..=1.2).contains(&draw), "{draw}");
        }
        env.sleep(0.0).await;
        assert!(env.now_ms() > 0 && env.monotonic() >= 0.0);
        assert_ne!(env.random_bytes(), env.random_bytes());
        let (sender, receiver) = tokio::sync::oneshot::channel();
        env.spawn(Box::pin(async move {
            let _ = sender.send(3);
        }));
        assert_eq!(receiver.await.unwrap(), 3);
        let error = env.post_secure("/x", &Map::new()).await.unwrap_err();
        assert!(error.is_a(ErrorKind::BydTransportError));
    }

    #[tokio::test]
    async fn without_a_channel_nothing_runs() {
        let env = env();
        assert!(!env.mqtt_running());
        assert!(env.mqtt_runtime().is_none());
        env.update_mqtt_key("00112233445566778899AABBCCDDEEFF");
        env.stop_mqtt();
        assert!(format!("{env:?}").contains("mqtt_running: false"));
        assert_eq!(env.http().base_url(), "http://127.0.0.1:9");
        let bootstrap = MqttBootstrap {
            user_id: "1".into(),
            broker_host: "127.0.0.1".into(),
            broker_port: 70_000,
            topic: "t".into(),
            client_id: "c".into(),
            username: "1".into(),
            password: "p".into(),
        };
        let sink: Arc<dyn MqttSink> = Arc::new(Recorder::default());
        let error = env
            .start_mqtt(&bootstrap, "00", 120, Arc::downgrade(&sink))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BydError);
        assert!(!env.mqtt_running());
    }

    #[tokio::test]
    async fn notices_reach_the_sink_until_it_goes() {
        let (sender, receiver) = tokio::sync::broadcast::channel(4);
        let recorder = Arc::new(Recorder::default());
        let sink: Arc<dyn MqttSink> = recorder.clone();
        let event = MqttEvent {
            event: "vehicleInfo".into(),
            vin: None,
            topic: "t".into(),
            payload: Map::new(),
        };
        // Five notices into a channel of four: the first is lost (lagged).
        sender.send(MqttNotice::Connected).unwrap();
        sender.send(MqttNotice::Event(Arc::new(event))).unwrap();
        sender
            .send(MqttNotice::ConnackRefused {
                consecutive: 3,
                reason: "NotAuthorized".into(),
            })
            .unwrap();
        sender.send(MqttNotice::DecryptFailed).unwrap();
        sender
            .send(MqttNotice::Disconnected {
                reason: "gone".into(),
            })
            .unwrap();
        drop(sender);
        forward(receiver, Arc::downgrade(&sink)).await;
        assert_eq!(
            recorder.0.lock().unwrap().clone(),
            ["event vehicleInfo", "refused 3", "decrypt"]
        );

        // A sink that is gone ends the forwarding at the next notice.
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        let gone: Arc<dyn MqttSink> = Arc::new(Recorder::default());
        let weak = Arc::downgrade(&gone);
        drop(gone);
        sender.send(MqttNotice::Connected).unwrap();
        forward(receiver, weak).await;
    }
}
