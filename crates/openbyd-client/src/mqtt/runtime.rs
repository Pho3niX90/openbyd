//! One MQTT connection per account, as a lightweight tokio task (pyBYD's
//! `BydMqttRuntime`).
//!
//! pyBYD runs a paho-mqtt client with its own thread per account. Here each
//! [`MqttRuntime`] owns at most one tokio task, which holds the connection,
//! reconnects when it drops and decrypts every message. An idle connection
//! costs that task, its socket and TLS buffers and a few small allocations,
//! so one process can hold thousands of accounts.
//!
//! # Using a runtime
//!
//! ```no_run
//! # async fn demo(bootstrap: openbyd_client::mqtt::MqttBootstrap, key: &str) {
//! use std::time::Duration;
//! use openbyd_client::mqtt::{EventFilter, MqttNotice, MqttRuntime, MqttRuntimeOptions};
//!
//! let runtime = MqttRuntime::new(key, MqttRuntimeOptions::default());
//! let mut notices = runtime.subscribe(); // before start, to see the first "connected"
//! runtime.start(&bootstrap).await.expect("broker reachable");
//!
//! // A request flow registers its wait *before* triggering the request.
//! let reply = runtime.expect(EventFilter::new("TESTVIN0000000001").event("remoteControl"));
//! // ... post the command ...
//! let event = reply.wait(Duration::from_secs(10)).await;
//!
//! while let Ok(notice) = notices.recv().await {
//!     match notice {
//!         MqttNotice::Event(event) => println!("{} for {:?}", event.event, event.vin),
//!         MqttNotice::DecryptFailed => { /* log in again, then update_decrypt_key */ }
//!         _ => {}
//!     }
//! }
//! runtime.stop().await;
//! # }
//! ```
//!
//! # What reaches the owner
//!
//! - **Notices** ([`MqttNotice`]): every event, and "connected", "CONNACK
//!   refused (n)", "decrypt failed, please re-authenticate" and
//!   "disconnected (reason)". They go out on a bounded broadcast channel
//!   ([`MqttRuntime::subscribe`]); any number of receivers may listen. The
//!   connection task never waits for a receiver: one that falls more than
//!   [`MqttRuntimeOptions::notice_capacity`] notices behind loses the oldest
//!   and is told how many (`RecvError::Lagged`). A receiver that panics or
//!   is dropped does not affect the connection.
//! - **Waits** ([`MqttRuntime::expect`], [`MqttRuntime::wait_for`]): a
//!   request flow registers what it waits for (VIN, event name, and any
//!   further test such as a request serial) and gets the first matching
//!   event, even if it is slow to poll. Matching happens on the connection
//!   task, which delivers into a one-shot slot and never blocks; a test
//!   that panics counts as "no match" and closes that wait.
//! - **Status** ([`MqttRuntime::status`]): whether it is running and
//!   connected, the refusal count and whether the key is confirmed, readable
//!   at any time without listening.
//!
//! # How paho's behaviour maps here
//!
//! - **Connect**: TLS with the platform's root certificates (as paho's
//!   `tls_set()`), MQTT v5, the bootstrap's client id, username and
//!   password and the configured keepalive. Clean start is set on the first
//!   CONNECT until the broker answers one, then cleared, as paho's
//!   `MQTT_CLEAN_START_FIRST_ONLY`. Neither side asks for a session expiry,
//!   so the broker keeps no session either way and the subscription is made
//!   again after every successful CONNACK, as pyBYD's `on_connect` does.
//! - **Reconnect**: after a lost connection or a refused CONNACK, the task
//!   waits and reconnects with the same credentials: 1 s, then doubling up
//!   to 120 s, reset only by a successful CONNACK. These are paho's
//!   defaults (`reconnect_delay_set(1, 120)`, `_reconnect_wait`), without
//!   jitter, as in paho.
//! - **Keepalive**: rumqttc pings every keepalive period and drops the
//!   connection when a ping goes unanswered until the next one; paho pings
//!   only after a quiet period and drops after a further quiet period. Both
//!   notice a dead broker within about two periods. A keepalive below 5
//!   seconds (or 0, which paho reads as "no pings") is refused: rumqttc
//!   requires at least 5.
//! - **Packet size**: rumqttc limits incoming packets to 10 KiB unless told
//!   otherwise, which is too small for vehicle data, so the CONNECT carries
//!   a Maximum Packet Size of [`MqttRuntimeOptions::max_packet_size`]
//!   (1 MiB, EMQX's own default limit). paho sends none.
//! - **Start**: like paho's `connect()`, [`MqttRuntime::start`] fails when
//!   the broker cannot be reached (DNS, TCP, TLS, no CONNACK within
//!   [`MqttRuntimeOptions::connect_timeout`]); a refused CONNACK is not a
//!   failure but a notice, followed by the reconnect loop.
//! - **Stop**: [`MqttRuntime::stop`] sends DISCONNECT when connected, then
//!   drops the connection and waits for the task to end, so the socket is
//!   closed when it returns (pyBYD had to close paho's socket by hand to
//!   stop file descriptors leaking under frequent restarts).

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use openbyd_protocol::mqtt::{MessageDecoder, MessageOutcome, MqttBootstrap, MqttEvent};
use openbyd_protocol::redact::redact_for_log;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{Packet, Publish};
use rumqttc::v5::{AsyncClient, ConnectionError, Event, EventLoop, MqttOptions};
use rumqttc::{Outgoing, TlsConfiguration, Transport};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

/// Settings for an [`MqttRuntime`]. Start from `MqttRuntimeOptions::default()`.
#[derive(Clone)]
#[non_exhaustive]
pub struct MqttRuntimeOptions {
    /// Seconds between keepalive pings (pyBYD's `config.mqtt_keepalive`,
    /// 120 by default). At least 5.
    pub keepalive: u16,
    /// The first wait before reconnecting (paho: 1 s).
    pub reconnect_min_delay: Duration,
    /// The longest wait before reconnecting (paho: 120 s).
    pub reconnect_max_delay: Duration,
    /// How long one attempt may take to connect, finish the TLS handshake
    /// and receive the CONNACK.
    pub connect_timeout: Duration,
    /// The largest packet the broker may send, announced in the CONNECT.
    pub max_packet_size: u32,
    /// How many notices a slow receiver may fall behind before it loses
    /// the oldest.
    pub notice_capacity: usize,
    /// The TLS settings. `None` uses the platform's root certificates,
    /// loaded once per process. Plain TCP is not offered.
    pub tls: Option<Arc<rustls::ClientConfig>>,
}

impl Default for MqttRuntimeOptions {
    fn default() -> Self {
        Self {
            keepalive: 120,
            reconnect_min_delay: Duration::from_secs(1),
            reconnect_max_delay: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(30),
            max_packet_size: 1024 * 1024,
            notice_capacity: 32,
            tls: None,
        }
    }
}

impl fmt::Debug for MqttRuntimeOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttRuntimeOptions")
            .field("keepalive", &self.keepalive)
            .field("reconnect_min_delay", &self.reconnect_min_delay)
            .field("reconnect_max_delay", &self.reconnect_max_delay)
            .field("connect_timeout", &self.connect_timeout)
            .field("max_packet_size", &self.max_packet_size)
            .field("notice_capacity", &self.notice_capacity)
            .field("tls", &self.tls.as_ref().map(|_| "custom"))
            .finish()
    }
}

/// What the runtime tells its owner. See the [module documentation](self).
#[derive(Debug, Clone)]
pub enum MqttNotice {
    /// A message decrypted into an event.
    Event(Arc<MqttEvent>),
    /// The broker accepted the connection and the topic was subscribed:
    /// after `start` and after every automatic reconnection.
    Connected,
    /// The broker refused the connection; `consecutive` counts refusals
    /// since the last accepted connection. The runtime keeps retrying with
    /// the same credentials, which cannot succeed if they are stale: after
    /// a few refusals the owner should log in again and restart it.
    ConnackRefused {
        /// Refusals in a row, from 1.
        consecutive: u32,
        /// The broker's reason code.
        reason: String,
    },
    /// A message did not decrypt before any message had decrypted with the
    /// current key: the key is probably stale. The owner should log in
    /// again and call [`MqttRuntime::update_decrypt_key`] or restart.
    DecryptFailed,
    /// The connection was lost or an attempt failed; the runtime will
    /// reconnect. Not sent for [`MqttRuntime::stop`].
    Disconnected {
        /// What happened, for logs.
        reason: String,
    },
}

/// A snapshot of a runtime's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MqttStatus {
    /// Started and not stopped.
    pub running: bool,
    /// The broker accepted the current connection.
    pub connected: bool,
    /// Refused CONNACKs since the last accepted one.
    pub consecutive_refusals: u32,
    /// A message has decrypted with the current key.
    pub key_confirmed: bool,
}

/// Why [`MqttRuntime::start`] failed. The runtime is then not running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MqttStartError {
    /// The bootstrap's port is not a TCP port.
    InvalidPort(i64),
    /// The keepalive is below the 5 seconds rumqttc needs.
    InvalidKeepalive(u16),
    /// The platform's root certificates could not be loaded.
    Certificates(String),
    /// The broker could not be reached, or did not answer in time.
    Connect(String),
}

impl fmt::Display for MqttStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPort(port) => write!(f, "invalid MQTT broker port {port}"),
            Self::InvalidKeepalive(seconds) => {
                write!(
                    f,
                    "MQTT keepalive must be at least 5 seconds, not {seconds}"
                )
            }
            Self::Certificates(message) => write!(f, "no root certificates: {message}"),
            Self::Connect(message) => write!(f, "MQTT connect failed: {message}"),
        }
    }
}

impl std::error::Error for MqttStartError {}

/// Why a wait for an event ended without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitError {
    /// No matching event arrived in time.
    Timeout,
    /// The runtime was not running when the wait was registered.
    NotRunning,
    /// The runtime was stopped, restarted or dropped, or the wait's own
    /// test panicked.
    Closed,
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Timeout => "no matching MQTT event in time",
            Self::NotRunning => "the MQTT runtime is not running",
            Self::Closed => "the MQTT runtime stopped",
        })
    }
}

impl std::error::Error for WaitError {}

type Predicate = Arc<dyn Fn(&MqttEvent) -> bool + Send + Sync>;

/// Which event a wait is for: pyBYD's `_MqttWaiter`. The event must name
/// the VIN; the event name and a further test are optional.
#[derive(Clone)]
pub struct EventFilter {
    vin: String,
    event: Option<String>,
    test: Option<Predicate>,
}

impl EventFilter {
    /// Any event for `vin`.
    pub fn new(vin: impl Into<String>) -> Self {
        Self {
            vin: vin.into(),
            event: None,
            test: None,
        }
    }

    /// Only events named `event` (`"vehicleInfo"`, `"remoteControl"`, ...).
    pub fn event(mut self, event: impl Into<String>) -> Self {
        self.event = Some(event.into());
        self
    }

    /// Only events `test` accepts, for example one carrying a given request serial.
    pub fn matching(mut self, test: impl Fn(&MqttEvent) -> bool + Send + Sync + 'static) -> Self {
        self.test = Some(Arc::new(test));
        self
    }

    /// Whether `event` is what this filter waits for. `Err` when the test panicked.
    fn accepts(&self, event: &MqttEvent) -> Result<bool, ()> {
        if event.vin.as_deref() != Some(self.vin.as_str())
            || self.event.as_ref().is_some_and(|name| *name != event.event)
        {
            return Ok(false);
        }
        match &self.test {
            None => Ok(true),
            Some(test) => catch_unwind(AssertUnwindSafe(|| test(event))).map_err(|_| ()),
        }
    }
}

impl fmt::Debug for EventFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventFilter")
            .field("vin", &self.vin)
            .field("event", &self.event)
            .field("test", &self.test.as_ref().map(|_| "fn"))
            .finish()
    }
}

struct Waiter {
    id: u64,
    filter: EventFilter,
    slot: oneshot::Sender<Arc<MqttEvent>>,
}

/// What the connection task and the handle share.
struct Shared {
    decoder: Mutex<MessageDecoder>,
    notices: broadcast::Sender<MqttNotice>,
    waiters: Mutex<Vec<Waiter>>,
    next_waiter: AtomicU64,
    /// Which `start` the current task belongs to.
    generation: AtomicU64,
    running: AtomicBool,
    connected: AtomicBool,
    refusals: AtomicU32,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn notify(&self, notice: MqttNotice) {
        // No receiver is not an error: nobody listens yet.
        let _ = self.notices.send(notice);
    }

    /// Hands `event` to every wait it matches.
    fn deliver(&self, event: &Arc<MqttEvent>) {
        let mut waiters = lock(&self.waiters);
        for waiter in std::mem::take(&mut *waiters) {
            match waiter.filter.accepts(event) {
                Ok(false) => waiters.push(waiter),
                // A wait whose receiver is gone simply drops the event.
                Ok(true) => drop(waiter.slot.send(Arc::clone(event))),
                Err(()) => log::warn!("MQTT event filter panicked; closing that wait"),
            }
        }
    }

    fn close_waiters(&self) {
        lock(&self.waiters).clear();
    }
}

/// A registered wait for one event; see [`MqttRuntime::expect`].
///
/// Dropping it cancels the wait.
#[must_use = "a wait does nothing unless awaited with `wait`"]
pub struct EventWaiter {
    id: u64,
    shared: Arc<Shared>,
    slot: Option<oneshot::Receiver<Arc<MqttEvent>>>,
}

impl EventWaiter {
    /// Waits up to `timeout` for the event.
    pub async fn wait(mut self, timeout: Duration) -> Result<Arc<MqttEvent>, WaitError> {
        let Some(slot) = self.slot.take() else {
            return Err(WaitError::NotRunning);
        };
        match tokio::time::timeout(timeout, slot).await {
            Ok(Ok(event)) => Ok(event),
            Ok(Err(_)) => Err(WaitError::Closed),
            Err(_) => Err(WaitError::Timeout),
        }
    }
}

impl Drop for EventWaiter {
    fn drop(&mut self) {
        let id = self.id;
        lock(&self.shared.waiters).retain(|waiter| waiter.id != id);
    }
}

impl fmt::Debug for EventWaiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventWaiter").field("id", &self.id).finish()
    }
}

/// The running connection: how to stop it and the task to wait for.
struct Connection {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

/// One account's MQTT connection. See the [module documentation](self).
///
/// All methods take `&self`; share it with an `Arc`. `start` and `stop`
/// must run inside a tokio runtime and are serialised against each other.
/// Dropping a running runtime aborts its task, which closes the socket
/// without a DISCONNECT; call [`stop`](Self::stop) for a clean shutdown.
pub struct MqttRuntime {
    shared: Arc<Shared>,
    options: MqttRuntimeOptions,
    connection: tokio::sync::Mutex<Option<Connection>>,
}

impl fmt::Debug for MqttRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttRuntime")
            .field("status", &self.status())
            .field("options", &self.options)
            .finish()
    }
}

impl MqttRuntime {
    /// A stopped runtime that will decrypt messages with `decrypt_key_hex`
    /// (the session's content key).
    pub fn new(decrypt_key_hex: &str, options: MqttRuntimeOptions) -> Self {
        let (notices, _) = broadcast::channel(options.notice_capacity.max(1));
        Self {
            shared: Arc::new(Shared {
                decoder: Mutex::new(MessageDecoder::new(decrypt_key_hex)),
                notices,
                waiters: Mutex::new(Vec::new()),
                next_waiter: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                running: AtomicBool::new(false),
                connected: AtomicBool::new(false),
                refusals: AtomicU32::new(0),
            }),
            options,
            connection: tokio::sync::Mutex::new(None),
        }
    }

    /// A receiver of every notice from now on, across restarts.
    pub fn subscribe(&self) -> broadcast::Receiver<MqttNotice> {
        self.shared.notices.subscribe()
    }

    /// Whether the runtime was started and not stopped since.
    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::SeqCst)
    }

    /// The runtime's state right now.
    pub fn status(&self) -> MqttStatus {
        MqttStatus {
            running: self.is_running(),
            connected: self.shared.connected.load(Ordering::SeqCst),
            consecutive_refusals: self.shared.refusals.load(Ordering::SeqCst),
            key_confirmed: lock(&self.shared.decoder).key_confirmed(),
        }
    }

    /// Decrypts later messages with `decrypt_key_hex`, which then has to be
    /// confirmed again by a message that decrypts.
    pub fn update_decrypt_key(&self, decrypt_key_hex: &str) {
        lock(&self.shared.decoder).update_decrypt_key(decrypt_key_hex);
    }

    /// Registers a wait for the first event `filter` accepts, from now on.
    /// Register before sending the request whose answer it waits for, then
    /// [`wait`](EventWaiter::wait) on it.
    pub fn expect(&self, filter: EventFilter) -> EventWaiter {
        let id = self.shared.next_waiter.fetch_add(1, Ordering::SeqCst);
        let mut waiter = EventWaiter {
            id,
            shared: Arc::clone(&self.shared),
            slot: None,
        };
        let mut waiters = lock(&self.shared.waiters);
        if self.is_running() {
            let (slot, receiver) = oneshot::channel();
            waiters.push(Waiter { id, filter, slot });
            waiter.slot = Some(receiver);
        }
        waiter
    }

    /// Waits up to `timeout` for the first event `filter` accepts.
    pub async fn wait_for(
        &self,
        filter: EventFilter,
        timeout: Duration,
    ) -> Result<Arc<MqttEvent>, WaitError> {
        self.expect(filter).wait(timeout).await
    }

    /// Waits up to `timeout` for the next event named `event` for `vin`.
    pub async fn wait_for_event(
        &self,
        vin: &str,
        event: &str,
        timeout: Duration,
    ) -> Result<Arc<MqttEvent>, WaitError> {
        self.wait_for(EventFilter::new(vin).event(event), timeout)
            .await
    }

    /// Connects to the bootstrap's broker and subscribes to its topic,
    /// stopping any previous connection first.
    ///
    /// Returns once the first attempt has an answer: `Ok` when the broker
    /// accepted it or refused it (a refusal is a notice and the runtime
    /// keeps retrying), `Err` when the broker could not be reached.
    pub async fn start(&self, bootstrap: &MqttBootstrap) -> Result<(), MqttStartError> {
        let mut connection = self.connection.lock().await;
        self.stop_locked(&mut connection).await;
        log::debug!(
            "MQTT runtime start requested host={} port={} topic={} client_id={}",
            bootstrap.broker_host,
            bootstrap.broker_port,
            bootstrap.topic,
            bootstrap.client_id
        );
        let options = self.mqtt_options(bootstrap)?;
        lock(&self.shared.decoder).reset();
        let (client, mut eventloop) = AsyncClient::new(options, 4);
        let first = eventloop.poll().await;
        let generation = self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut task = Task {
            shared: Arc::clone(&self.shared),
            generation,
            client,
            eventloop,
            topic: bootstrap.topic.clone(),
            backoff: Backoff::new(
                self.options.reconnect_min_delay,
                self.options.reconnect_max_delay,
            ),
        };
        if let Err(error) = &first
            && !matches!(error, ConnectionError::ConnectionRefused(_))
        {
            return Err(MqttStartError::Connect(error.to_string()));
        }
        // Running before the first notice, so a receiver reacting to it can wait.
        self.shared.running.store(true, Ordering::SeqCst);
        let first = task.handle(first);
        let (stop, stopped) = oneshot::channel();
        let handle = tokio::spawn(task.run(first, stopped));
        *connection = Some(Connection { stop, task: handle });
        log::debug!("MQTT network loop started");
        Ok(())
    }

    /// Disconnects and ends the connection task. When it returns the
    /// socket is closed. Pending waits end with [`WaitError::Closed`].
    pub async fn stop(&self) {
        let mut connection = self.connection.lock().await;
        self.stop_locked(&mut connection).await;
    }

    async fn stop_locked(&self, connection: &mut Option<Connection>) {
        self.shared.running.store(false, Ordering::SeqCst);
        self.shared.close_waiters();
        let Some(Connection { stop, task }) = connection.take() else {
            return;
        };
        log::debug!("MQTT disconnect requested");
        let _ = stop.send(());
        // A panic in the task has already ended it; nothing more to release.
        let _ = task.await;
        self.shared.connected.store(false, Ordering::SeqCst);
        log::debug!("MQTT network loop stopped (socket released)");
    }

    fn mqtt_options(&self, bootstrap: &MqttBootstrap) -> Result<MqttOptions, MqttStartError> {
        let port = u16::try_from(bootstrap.broker_port)
            .map_err(|_| MqttStartError::InvalidPort(bootstrap.broker_port))?;
        if self.options.keepalive < 5 {
            return Err(MqttStartError::InvalidKeepalive(self.options.keepalive));
        }
        let tls = match &self.options.tls {
            Some(tls) => Arc::clone(tls),
            None => platform_tls()?,
        };
        let mut options = MqttOptions::new(
            bootstrap.client_id.clone(),
            bootstrap.broker_host.clone(),
            port,
        );
        options
            .set_credentials(bootstrap.username.clone(), bootstrap.password.clone())
            .set_keep_alive(Duration::from_secs(self.options.keepalive.into()))
            .set_clean_start(true)
            .set_connection_timeout(self.options.connect_timeout.as_secs().max(1))
            .set_max_packet_size(Some(self.options.max_packet_size))
            // Only QoS 0 is used: keep the in-flight table small.
            .set_outgoing_inflight_upper_limit(1)
            .set_transport(Transport::Tls(TlsConfiguration::Rustls(tls)));
        Ok(options)
    }
}

impl Drop for MqttRuntime {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        self.shared.close_waiters();
        if let Some(connection) = self.connection.get_mut().take() {
            connection.task.abort();
        }
    }
}

/// The client configuration with the platform's root certificates, built once.
fn platform_tls() -> Result<Arc<rustls::ClientConfig>, MqttStartError> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let loaded = rustls_native_certs::load_native_certs();
            let mut roots = rustls::RootCertStore::empty();
            let (added, _) = roots.add_parsable_certificates(loaded.certs);
            if added == 0 {
                return Err(format!("{:?}", loaded.errors));
            }
            client_config(roots)
        })
        .clone()
        .map_err(MqttStartError::Certificates)
}

/// A TLS client configuration trusting `roots`, on the `ring` provider.
pub fn client_config(roots: rustls::RootCertStore) -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// paho's reconnect delay: `min`, then doubling up to `max`, reset by a
/// successful CONNACK (`Client._reconnect_wait`).
#[derive(Debug, Clone)]
struct Backoff {
    min: Duration,
    max: Duration,
    current: Option<Duration>,
}

impl Backoff {
    fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max,
            current: None,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let next = match self.current {
            None => self.min,
            Some(current) => current.saturating_mul(2).min(self.max),
        };
        self.current = Some(next);
        next
    }

    fn reset(&mut self) {
        self.current = None;
    }
}

/// What to do after handling one poll result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    Poll,
    Reconnect,
}

/// The connection task's state.
struct Task {
    shared: Arc<Shared>,
    generation: u64,
    client: AsyncClient,
    eventloop: EventLoop,
    topic: String,
    backoff: Backoff,
}

/// Clears the running flags if the task ends while it is still current
/// (a panic, as `stop` clears them itself).
struct RunningGuard {
    shared: Arc<Shared>,
    generation: u64,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if self.shared.generation.load(Ordering::SeqCst) == self.generation {
            self.shared.running.store(false, Ordering::SeqCst);
            self.shared.connected.store(false, Ordering::SeqCst);
        }
    }
}

impl Task {
    async fn run(mut self, first: Next, mut stop: oneshot::Receiver<()>) {
        let _guard = RunningGuard {
            shared: Arc::clone(&self.shared),
            generation: self.generation,
        };
        let mut next = first;
        loop {
            if next == Next::Reconnect {
                let delay = self.backoff.next_delay();
                log::debug!("MQTT reconnecting in {delay:?}");
                tokio::select! {
                    biased;
                    _ = &mut stop => return,
                    () = tokio::time::sleep(delay) => {}
                }
            }
            let polled = tokio::select! {
                biased;
                _ = &mut stop => {
                    self.disconnect().await;
                    return;
                }
                polled = self.eventloop.poll() => polled,
            };
            next = self.handle(polled);
        }
    }

    /// Sends DISCONNECT if connected; the socket closes when the task ends.
    async fn disconnect(&mut self) {
        if !self.shared.connected.swap(false, Ordering::SeqCst) {
            return;
        }
        if self.client.try_disconnect().is_err() {
            return;
        }
        let sent = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match self.eventloop.poll().await {
                    Ok(Event::Outgoing(Outgoing::Disconnect)) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        });
        let _ = sent.await;
    }

    fn handle(&mut self, polled: Result<Event, ConnectionError>) -> Next {
        match polled {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                self.connected();
                Next::Poll
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                self.message(&publish);
                Next::Poll
            }
            // Acknowledgements, pings, our own requests. (A DISCONNECT from
            // the broker arrives as an error.)
            Ok(_) => Next::Poll,
            Err(ConnectionError::ConnectionRefused(code)) => {
                self.shared.connected.store(false, Ordering::SeqCst);
                // Any CONNACK ends paho's "first connect".
                self.eventloop.options.set_clean_start(false);
                let consecutive = self.shared.refusals.fetch_add(1, Ordering::SeqCst) + 1;
                log::warn!(
                    "MQTT connect failed reason={code:?} (consecutive refusals={consecutive})"
                );
                self.shared.notify(MqttNotice::ConnackRefused {
                    consecutive,
                    reason: format!("{code:?}"),
                });
                Next::Reconnect
            }
            Err(error) => {
                self.shared.connected.store(false, Ordering::SeqCst);
                log::debug!("MQTT disconnected reason={error}");
                self.shared.notify(MqttNotice::Disconnected {
                    reason: error.to_string(),
                });
                Next::Reconnect
            }
        }
    }

    fn connected(&mut self) {
        self.eventloop.options.set_clean_start(false);
        self.backoff.reset();
        self.shared.refusals.store(0, Ordering::SeqCst);
        self.shared.connected.store(true, Ordering::SeqCst);
        log::debug!("MQTT connected; subscribe topic={}", self.topic);
        // The request channel is empty right after a CONNACK, so this fits.
        if let Err(error) = self
            .client
            .try_subscribe(self.topic.clone(), QoS::AtMostOnce)
        {
            log::warn!("MQTT subscribe could not be queued: {error}");
        }
        self.shared.notify(MqttNotice::Connected);
    }

    fn message(&self, publish: &Publish) {
        let outcome = lock(&self.shared.decoder).handle(&publish.topic, &publish.payload);
        match outcome {
            MessageOutcome::Event(event) => {
                if log::log_enabled!(log::Level::Debug) {
                    log::debug!(
                        "MQTT decoded topic={} event={} vin={:?} payload={}",
                        event.topic,
                        event.event,
                        event.vin,
                        redact_for_log(&Value::Object(event.payload.clone()))
                    );
                }
                let event = Arc::new(event);
                self.shared.deliver(&event);
                self.shared.notify(MqttNotice::Event(event));
            }
            MessageOutcome::DecryptFailed => {
                log::debug!(
                    "MQTT decrypt failed ({} bytes, head={}) - likely stale key, requesting re-auth",
                    publish.payload.len(),
                    head(&publish.payload)
                );
                self.shared.notify(MqttNotice::DecryptFailed);
            }
            MessageOutcome::NotForSession => log::debug!(
                "MQTT payload not for this session ({} bytes, head={}), ignoring",
                publish.payload.len(),
                head(&publish.payload)
            ),
            MessageOutcome::Invalid(error) => log::warn!(
                "MQTT payload parse failed ({} bytes, head={}): {}",
                publish.payload.len(),
                head(&publish.payload),
                error.message()
            ),
        }
    }
}

/// The first 64 bytes of a payload in hex, for logs.
fn head(payload: &[u8]) -> String {
    if payload.is_empty() {
        return "<empty>".to_owned();
    }
    payload
        .iter()
        .take(64)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_follows_paho() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(120));
        let delays: Vec<u64> = (0..9).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 64, 120, 120]);
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn heads_and_texts() {
        assert_eq!(head(&[]), "<empty>");
        assert_eq!(head(&[0xab; 70]).len(), 128);
        assert!(
            MqttStartError::InvalidPort(70000)
                .to_string()
                .contains("70000")
        );
        assert!(
            MqttStartError::InvalidKeepalive(1)
                .to_string()
                .contains('5')
        );
        assert!(
            MqttStartError::Certificates("x".into())
                .to_string()
                .contains('x')
        );
        assert!(
            MqttStartError::Connect("y".into())
                .to_string()
                .contains('y')
        );
        for error in [WaitError::Timeout, WaitError::NotRunning, WaitError::Closed] {
            assert!(!error.to_string().is_empty());
        }
        let options = MqttRuntimeOptions::default();
        assert!(format!("{options:?}").contains("keepalive: 120"));
        let filter = EventFilter::new("V").event("e").matching(|_| true);
        assert!(format!("{filter:?}").contains("\"V\""));
    }

    #[test]
    fn filters_match_like_pybyd_waiters() {
        let event = MqttEvent {
            event: "remoteControl".into(),
            vin: Some("V".into()),
            topic: "t".into(),
            payload: serde_json::Map::new(),
        };
        assert_eq!(EventFilter::new("V").accepts(&event), Ok(true));
        assert_eq!(EventFilter::new("W").accepts(&event), Ok(false));
        assert_eq!(
            EventFilter::new("V").event("vehicleInfo").accepts(&event),
            Ok(false)
        );
        let no_vin = MqttEvent {
            vin: None,
            ..event.clone()
        };
        assert_eq!(EventFilter::new("").accepts(&no_vin), Ok(false));
        assert_eq!(
            EventFilter::new("V").matching(|_| false).accepts(&event),
            Ok(false)
        );
        let panicking = EventFilter::new("V").matching(|_| panic!("test"));
        assert_eq!(panicking.accepts(&event), Err(()));
    }
}
