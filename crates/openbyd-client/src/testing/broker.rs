//! A small MQTT v5 broker over TLS on 127.0.0.1, for tests of the MQTT
//! runtime and of whole clients (feature `testing`).
//!
//! It speaks just enough MQTT v5 for the runtime (CONNECT, SUBSCRIBE,
//! PUBLISH, PINGREQ, DISCONNECT), records everything it is sent, and can
//! refuse connections, publish to subscribers, drop every connection and
//! restart. Its certificate is made for the test run; clients trust it
//! through [`client_tls`] (or [`runtime_options`]).
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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use openbyd_protocol::aes::aes_encrypt_hex;
use rumqttc::mqttbytes::QoS;
use rumqttc::mqttbytes::v5::{
    ConnAck, ConnectAuth, ConnectReturnCode, Packet, Publish, SubAck, SubscribeReasonCode,
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
}

fn tls() -> &'static TestTls {
    static TLS: OnceLock<TestTls> = OnceLock::new();
    TLS.get_or_init(|| {
        let certified =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("certificate");
        let cert = certified.cert.der().clone();
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
        }
    })
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

#[derive(Default)]
struct BrokerState {
    seen: Mutex<Vec<Seen>>,
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
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("address").port();
        let broker = Arc::new(Self {
            port,
            state: Arc::new(BrokerState::default()),
            acceptor: Mutex::new(None),
        });
        broker.listen(listener);
        broker
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
        let listener = TcpListener::bind(("127.0.0.1", self.port))
            .await
            .expect("rebind");
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
    let record = |seen: Seen| lock(&state.seen).push(seen);
    let (commands, mut inbox) = mpsc::unbounded_channel();
    let mut buffer = BytesMut::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    'connection: loop {
        // Every complete packet in the buffer.
        loop {
            match Packet::read(&mut buffer, None) {
                Ok(packet) => {
                    let reply = match packet {
                        Packet::Connect(connect, _, login) => {
                            let (username, password) = credentials(login);
                            record(Seen::Connect {
                                conn,
                                client_id: connect.client_id.clone(),
                                username,
                                password,
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
                        Packet::PingReq => {
                            record(Seen::PingReq { conn });
                            Some(Packet::PingResp)
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
                Err(rumqttc::mqttbytes::Error::InsufficientBytes(_)) => break,
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

/// The user name and password of a CONNECT, the password read as UTF-8.
fn credentials(auth: ConnectAuth) -> (Option<String>, Option<String>) {
    let text = |password: bytes::Bytes| String::from_utf8_lossy(&password).into_owned();
    match auth {
        ConnectAuth::None => (None, None),
        ConnectAuth::Username { username } => (Some(username), None),
        ConnectAuth::Password { password } => (None, Some(text(password))),
        ConnectAuth::UsernamePassword { username, password } => {
            (Some(username), Some(text(password)))
        }
    }
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
