//! A small MQTT v5 broker over TLS on 127.0.0.1, for tests of the MQTT
//! runtime and of whole clients (feature `testing`).
//!
//! It speaks just enough MQTT v5 for the runtime (CONNECT, SUBSCRIBE,
//! PUBLISH, PINGREQ, DISCONNECT), records everything it is sent (unless told
//! not to, [`TestBroker::set_recording`]), counts connections and
//! subscriptions ([`TestBroker::stats`]), and can refuse connections, publish
//! to subscribers, drop every connection and restart. Its certificate is made
//! for the test run, with `127.0.0.1` as an IP subject alternative name;
//! clients of this process trust it through [`client_tls`] (or
//! [`runtime_options`]), others through its PEM ([`certificate_pem`]).
//!
//! ```no_run
//! # async fn demo() {
//! use openbyd_client::testing::broker::{TestBroker, runtime_options};
//!
//! let broker = TestBroker::start().await;
//! // ... start an MqttRuntime with runtime_options() on 127.0.0.1:broker.port() ...
//! broker.publish("oversea/res/1", b"payload".to_vec());
//! # }
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use openbyd_protocol::aes::aes_encrypt_hex;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{
    ConnAck, ConnectReturnCode, Packet, PingResp, Publish, SubAck, SubscribeReasonCode,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::mqtt::{MqttRuntimeOptions, client_config, rustls};

/// How long [`TestBroker::until`] waits.
pub const WAIT: Duration = Duration::from_secs(10);

struct TestTls {
    server: TlsAcceptor,
    client: Arc<rustls::ClientConfig>,
    pem: String,
}

fn tls() -> &'static TestTls {
    static TLS: OnceLock<TestTls> = OnceLock::new();
    TLS.get_or_init(|| {
        let certified =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("certificate");
        let cert = certified.cert.der().clone();
        let pem = certified.cert.pem();
        let key =
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .expect("server config");
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).expect("root");
        TestTls {
            server: TlsAcceptor::from(Arc::new(server)),
            client: client_config(roots).expect("client config"),
            pem,
        }
    })
}

/// The brokers' certificate (the same for every broker of this process) as
/// PEM, for a client in another process or language to trust.
pub fn certificate_pem() -> String {
    tls().pem.clone()
}

/// A TLS client configuration that trusts the brokers of this process.
pub fn client_tls() -> Arc<rustls::ClientConfig> {
    Arc::clone(&tls().client)
}

/// Runtime options for tests: the test certificate, and quick reconnects.
pub fn runtime_options() -> MqttRuntimeOptions {
    MqttRuntimeOptions {
        tls: Some(client_tls()),
        reconnect_min_delay: Duration::from_millis(20),
        reconnect_max_delay: Duration::from_millis(200),
        connect_timeout: Duration::from_secs(10),
        ..MqttRuntimeOptions::default()
    }
}

/// Something the broker saw.
#[derive(Debug, Clone, PartialEq)]
pub enum Seen {
    /// A CONNECT.
    Connect {
        /// The connection's number, from 0.
        conn: u64,
        /// The client identifier.
        client_id: String,
        /// The user name, if any.
        username: Option<String>,
        /// The password, if any.
        password: Option<String>,
        /// The keepalive in seconds.
        keep_alive: u16,
        /// The clean start flag.
        clean_start: bool,
        /// The Maximum Packet Size property, if sent.
        max_packet_size: Option<u32>,
    },
    /// A SUBSCRIBE filter.
    Subscribe {
        /// The connection.
        conn: u64,
        /// The topic filter.
        topic: String,
        /// The requested QoS.
        qos: QoS,
    },
    /// A PINGREQ.
    PingReq {
        /// The connection.
        conn: u64,
    },
    /// A DISCONNECT.
    Disconnect {
        /// The connection.
        conn: u64,
    },
    /// The connection closed.
    Closed {
        /// The connection.
        conn: u64,
    },
}

enum Command {
    Publish(Box<Publish>),
    Drop,
}

struct Peer {
    topics: Vec<String>,
    commands: mpsc::UnboundedSender<Command>,
}

/// The broker's counters ([`TestBroker::stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BrokerStats {
    /// Connections open now.
    pub open: u64,
    /// CONNECTs received since the last reset.
    pub connects: u64,
    /// Topic filters subscribed since the last reset.
    pub subscriptions: u64,
}

#[derive(Default)]
struct BrokerState {
    seen: Mutex<Vec<Seen>>,
    /// Whether `seen` is not kept (recording switched off).
    not_recording: AtomicBool,
    connects: AtomicU64,
    subscriptions: AtomicU64,
    peers: Mutex<HashMap<u64, Peer>>,
    next_conn: AtomicU64,
    /// How many of the next CONNECTs to refuse.
    refuse: Mutex<usize>,
    /// Connections whose socket is still open on the broker's side.
    open: AtomicUsize,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The broker. See the [module documentation](self).
pub struct TestBroker {
    port: u16,
    state: Arc<BrokerState>,
    acceptor: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for TestBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestBroker")
            .field("port", &self.port)
            .field("open", &self.open())
            .finish_non_exhaustive()
    }
}

impl TestBroker {
    /// Starts a broker on an ephemeral port of 127.0.0.1, on the current
    /// tokio runtime.
    pub async fn start() -> Arc<Self> {
        Self::start_on(0).await.expect("bind")
    }

    /// Starts a broker on `127.0.0.1:<port>` (0 for an ephemeral port), on
    /// the current tokio runtime.
    pub async fn start_on(port: u16) -> std::io::Result<Arc<Self>> {
        let listener = super::listen(port)?;
        let port = listener.local_addr()?.port();
        let broker = Arc::new(Self {
            port,
            state: Arc::new(BrokerState::default()),
            acceptor: Mutex::new(None),
        });
        broker.listen(listener);
        Ok(broker)
    }

    /// Switches the recording of what the broker sees ([`TestBroker::seen`])
    /// on (the default) or off. The counters are kept either way.
    pub fn set_recording(&self, on: bool) {
        self.state.not_recording.store(!on, Ordering::SeqCst);
    }

    /// The counters since the start or the last [`TestBroker::reset_stats`].
    pub fn stats(&self) -> BrokerStats {
        BrokerStats {
            open: self.open() as u64,
            connects: self.state.connects.load(Ordering::SeqCst),
            subscriptions: self.state.subscriptions.load(Ordering::SeqCst),
        }
    }

    /// Zeroes the counters of [`TestBroker::stats`] (not `open`).
    pub fn reset_stats(&self) {
        self.state.connects.store(0, Ordering::SeqCst);
        self.state.subscriptions.store(0, Ordering::SeqCst);
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The broker address as BYD's broker lookup gives it: `127.0.0.1:<port>`.
    pub fn address(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn listen(&self, listener: TcpListener) {
        let state = Arc::clone(&self.state);
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    continue;
                };
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Ok(stream) = tls().server.accept(socket).await {
                        serve(state, stream).await;
                    }
                });
            }
        });
        *lock(&self.acceptor) = Some(task);
    }

    /// Stops accepting, drops every connection, and listens again on the same port.
    pub async fn restart(&self) {
        let acceptor = lock(&self.acceptor).take();
        if let Some(task) = acceptor {
            task.abort();
            let _ = task.await;
        }
        self.drop_all();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let listener = super::listen(self.port).expect("rebind");
        self.listen(listener);
    }

    /// Drops every connection.
    pub fn drop_all(&self) {
        for peer in lock(&self.state.peers).values() {
            let _ = peer.commands.send(Command::Drop);
        }
    }

    /// Refuses the next `count` CONNECTs (Not Authorized).
    pub fn refuse_next(&self, count: usize) {
        *lock(&self.state.refuse) = count;
    }

    /// Sends `payload` to every connection subscribed to `topic`; returns how many.
    pub fn publish(&self, topic: &str, payload: impl Into<bytes::Bytes>) -> usize {
        let payload = payload.into();
        let peers = lock(&self.state.peers);
        let mut sent = 0;
        for peer in peers.values() {
            if peer.topics.iter().any(|subscribed| subscribed == topic) {
                let publish = Publish::new(topic, QoS::AtMostOnce, payload.clone(), None);
                if peer
                    .commands
                    .send(Command::Publish(Box::new(publish)))
                    .is_ok()
                {
                    sent += 1;
                }
            }
        }
        sent
    }

    /// Publishes `body` encrypted with `key` (hex), as BYD's pushes are.
    pub fn publish_sealed(&self, topic: &str, body: &Value, key: &str) -> usize {
        self.publish(topic, sealed(body, key))
    }

    /// Everything the broker saw so far.
    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.state.seen).clone()
    }

    /// Connections still open on the broker's side.
    pub fn open(&self) -> usize {
        self.state.open.load(Ordering::SeqCst)
    }

    /// Connections subscribed to `topic`.
    pub fn subscribers(&self, topic: &str) -> usize {
        let peers = lock(&self.state.peers);
        peers
            .values()
            .filter(|peer| peer.topics.iter().any(|subscribed| subscribed == topic))
            .count()
    }

    /// Waits until `condition` holds for what the broker saw; panics after [`WAIT`].
    pub async fn until(&self, what: &str, condition: impl Fn(&[Seen]) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !condition(&self.seen()) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; saw {:#?}",
                self.seen()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits until a connection is subscribed to `topic`; panics after [`WAIT`].
    pub async fn until_subscribed(&self, topic: &str) {
        let deadline = Instant::now() + WAIT;
        while self.subscribers(topic) == 0 {
            assert!(Instant::now() < deadline, "no subscription for {topic}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

impl Drop for TestBroker {
    fn drop(&mut self) {
        if let Some(task) = lock(&self.acceptor).take() {
            task.abort();
        }
        self.drop_all();
    }
}

/// `body` as BYD sends a push: AES-encrypted JSON, as hex text.
pub fn sealed(body: &Value, key: &str) -> Vec<u8> {
    aes_encrypt_hex(&body.to_string(), key)
        .expect("encrypts")
        .into_bytes()
}

async fn serve(
    state: Arc<BrokerState>,
    mut stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
) {
    let conn = state.next_conn.fetch_add(1, Ordering::SeqCst);
    state.open.fetch_add(1, Ordering::SeqCst);
    let record = |seen: Seen| {
        match seen {
            Seen::Connect { .. } => state.connects.fetch_add(1, Ordering::SeqCst),
            Seen::Subscribe { .. } => state.subscriptions.fetch_add(1, Ordering::SeqCst),
            _ => 0,
        };
        if !state.not_recording.load(Ordering::SeqCst) {
            lock(&state.seen).push(seen);
        }
    };
    let (commands, mut inbox) = mpsc::unbounded_channel();
    let mut buffer = BytesMut::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    'connection: loop {
        // Every complete packet in the buffer.
        loop {
            // rumqttc writes a plain DISCONNECT as `E0 00`, which its own
            // decoder rejects (it wants a reason code), so read it here.
            if buffer.starts_with(&[0xE0, 0x00]) {
                record(Seen::Disconnect { conn });
                break 'connection;
            }
            match Packet::read(&mut buffer, None) {
                Ok(packet) => {
                    let reply = match packet {
                        Packet::Connect(connect, _, login) => {
                            record(Seen::Connect {
                                conn,
                                client_id: connect.client_id.clone(),
                                username: login.as_ref().map(|login| login.username.clone()),
                                password: login.as_ref().map(|login| login.password.clone()),
                                keep_alive: connect.keep_alive,
                                clean_start: connect.clean_start,
                                max_packet_size: connect
                                    .properties
                                    .as_ref()
                                    .and_then(|properties| properties.max_packet_size),
                            });
                            let refuse = {
                                let mut left = lock(&state.refuse);
                                let refuse = *left > 0;
                                *left = left.saturating_sub(1);
                                refuse
                            };
                            if refuse {
                                let refused = Packet::ConnAck(ConnAck {
                                    session_present: false,
                                    code: ConnectReturnCode::NotAuthorized,
                                    properties: None,
                                });
                                let _ = write(&mut stream, &refused).await;
                                break 'connection;
                            }
                            lock(&state.peers).insert(
                                conn,
                                Peer {
                                    topics: Vec::new(),
                                    commands: commands.clone(),
                                },
                            );
                            Some(Packet::ConnAck(ConnAck {
                                session_present: false,
                                code: ConnectReturnCode::Success,
                                properties: None,
                            }))
                        }
                        Packet::Subscribe(subscribe) => {
                            let mut codes = Vec::new();
                            for filter in subscribe.filters {
                                record(Seen::Subscribe {
                                    conn,
                                    topic: filter.path.clone(),
                                    qos: filter.qos,
                                });
                                if let Some(peer) = lock(&state.peers).get_mut(&conn) {
                                    peer.topics.push(filter.path);
                                }
                                codes.push(SubscribeReasonCode::Success(QoS::AtMostOnce));
                            }
                            Some(Packet::SubAck(SubAck {
                                pkid: subscribe.pkid,
                                return_codes: codes,
                                properties: None,
                            }))
                        }
                        Packet::PingReq(_) => {
                            record(Seen::PingReq { conn });
                            Some(Packet::PingResp(PingResp))
                        }
                        Packet::Disconnect(_) => {
                            record(Seen::Disconnect { conn });
                            break 'connection;
                        }
                        _ => None,
                    };
                    if let Some(reply) = reply
                        && write(&mut stream, &reply).await.is_err()
                    {
                        break 'connection;
                    }
                }
                Err(rumqttc::v5::mqttbytes::Error::InsufficientBytes(_)) => break,
                Err(_) => break 'connection,
            }
        }
        tokio::select! {
            read = stream.read(&mut chunk) => match read {
                Ok(0) | Err(_) => break 'connection,
                Ok(count) => buffer.extend_from_slice(&chunk[..count]),
            },
            command = inbox.recv() => match command {
                Some(Command::Publish(publish)) => {
                    if write(&mut stream, &Packet::Publish(*publish)).await.is_err() {
                        break 'connection;
                    }
                }
                Some(Command::Drop) | None => break 'connection,
            },
        }
    }
    lock(&state.peers).remove(&conn);
    drop(stream);
    record(Seen::Closed { conn });
    state.open.fetch_sub(1, Ordering::SeqCst);
}

async fn write(
    stream: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    packet: &Packet,
) -> std::io::Result<()> {
    let mut out = BytesMut::new();
    packet.write(&mut out, None).expect("encodable packet");
    stream.write_all(&out).await?;
    stream.flush().await
}
