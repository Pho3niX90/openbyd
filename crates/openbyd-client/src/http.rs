//! The real environment: HTTP(S) transport, system clock, randomness and sleeping (`pybyd/_transport.py`).
//!
//! [`HttpEnv`] is the production [`Env`]. Its `post_secure` is pyBYD's
//! `SecureTransport.post_secure`: the outer payload goes out in a Bangcle
//! envelope with pyBYD's headers, the reply is decoded back to JSON, and a
//! non-200 status or a network failure is a `BydTransportError`.
//!
//! Many accounts can live in one process. An [`HttpPool`] is one connection
//! pool (hyper's HTTP/1.1 client over rustls, on tokio); every [`HttpEnv`]
//! made from it shares its connections but keeps its own cookie jar, as
//! each pyBYD client keeps its own `aiohttp.CookieJar`. Neither spawns a
//! thread, and nothing blocks the executor (DNS lookups run on tokio's
//! blocking pool).
//!
//! Use a pool from one tokio runtime: its connections are tasks on the
//! runtime that opened them.

mod cookies;

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{COOKIE, SET_COOKIE};
use hyper::{Request, Uri};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::envelope::{REQUEST_HEADERS, decode_response_body, encode_request_body};
use openbyd_protocol::error::{Error, ErrorKind};
use serde_json::{Map, Value};

use crate::env::Env;
use cookies::CookieJar;

/// How long a connection attempt may take. pyBYD's effective value is
/// aiohttp's default `sock_connect=30`.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a whole request may take, from connecting to the last byte of
/// the reply. pyBYD's effective value is aiohttp's default `total=300`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long an idle connection stays in the pool. aiohttp's default
/// `keepalive_timeout` is 15 seconds.
pub const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// The header aiohttp adds to every request on top of pyBYD's own
/// ([`REQUEST_HEADERS`]); its default `accept-encoding` is overridden by
/// pyBYD's `identity`.
pub const DEFAULT_ACCEPT: (&str, &str) = ("accept", "*/*");

/// Settings of an [`HttpPool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpOptions {
    /// The limit on opening a TCP connection ([`DEFAULT_CONNECT_TIMEOUT`]).
    pub connect_timeout: Duration,
    /// The limit on a whole request ([`DEFAULT_TIMEOUT`]).
    pub timeout: Duration,
    /// How long idle connections are kept ([`DEFAULT_POOL_IDLE_TIMEOUT`]).
    pub pool_idle_timeout: Duration,
    /// The most idle connections kept per host (default: no limit).
    pub pool_max_idle_per_host: usize,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            timeout: DEFAULT_TIMEOUT,
            pool_idle_timeout: DEFAULT_POOL_IDLE_TIMEOUT,
            pool_max_idle_per_host: usize::MAX,
        }
    }
}

type HyperClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// A connection pool shared by any number of [`HttpEnv`]s.
///
/// Cloning is cheap and shares the pool. It speaks HTTP/1.1 (as aiohttp
/// does) over plain TCP for `http://` URLs and over rustls with Mozilla's
/// roots for `https://` ones.
#[derive(Clone)]
pub struct HttpPool {
    client: HyperClient,
    timeout: Duration,
}

impl fmt::Debug for HttpPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpPool")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Default for HttpPool {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpPool {
    /// A pool with the default [`HttpOptions`].
    pub fn new() -> Self {
        Self::with_options(&HttpOptions::default())
    }

    /// A pool with the given settings.
    pub fn with_options(options: &HttpOptions) -> Self {
        let mut tcp = HttpConnector::new();
        tcp.enforce_http(false);
        tcp.set_nodelay(true);
        tcp.set_connect_timeout(Some(options.connect_timeout));
        let connector = HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(rustls::crypto::ring::default_provider())
            .expect("ring supports rustls' default protocol versions")
            .https_or_http()
            .enable_http1()
            .wrap_connector(tcp);
        let client = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(options.pool_idle_timeout)
            .pool_max_idle_per_host(options.pool_max_idle_per_host)
            .build(connector);
        Self {
            client,
            timeout: options.timeout,
        }
    }

    /// The limit on a whole request.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

struct Inner {
    pool: HttpPool,
    base_url: String,
    cookies: Mutex<CookieJar>,
}

/// The production [`Env`]: one account's view of the network.
///
/// Requests go to `base_url` followed by the endpoint, through the shared
/// [`HttpPool`], with this environment's cookies. Cloning is cheap and
/// shares the cookie jar, so clones belong to the same account; make a new
/// one with [`HttpEnv::new`] for another account.
#[derive(Clone)]
pub struct HttpEnv {
    inner: Arc<Inner>,
}

impl fmt::Debug for HttpEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpEnv")
            .field("base_url", &self.inner.base_url)
            .field("pool", &self.inner.pool)
            .finish_non_exhaustive()
    }
}

fn failed(endpoint: &str, error: &(dyn std::error::Error + 'static)) -> Error {
    // aiohttp's messages name the cause; hyper's top-level error is terse,
    // so follow the chain.
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    Error::transport(
        format!("Request to {endpoint} failed: {text}"),
        None,
        endpoint,
    )
}

impl HttpEnv {
    /// An environment with an empty cookie jar that sends requests to
    /// `base_url` through `pool`.
    pub fn new(pool: &HttpPool, base_url: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Inner {
                pool: pool.clone(),
                base_url: base_url.into(),
                cookies: Mutex::new(CookieJar::default()),
            }),
        }
    }

    /// An environment for the config's `base_url`.
    pub fn for_config(pool: &HttpPool, config: &BydConfig) -> Self {
        Self::new(pool, config.base_url.clone())
    }

    /// The URL endpoints are appended to.
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// The pool this environment sends through.
    pub fn pool(&self) -> &HttpPool {
        &self.inner.pool
    }

    fn jar(&self) -> MutexGuard<'_, CookieJar> {
        self.inner
            .cookies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The cookies held now, as `(name, value)` pairs.
    pub fn cookies(&self) -> Vec<(String, String)> {
        self.jar().pairs(SystemTime::now())
    }

    /// Forgets every cookie.
    pub fn clear_cookies(&self) {
        self.jar().clear();
    }

    /// Posts `body` to `endpoint` and returns the status and the body text.
    async fn exchange(&self, endpoint: &str, body: String) -> Result<(u16, String), Error> {
        let url = format!("{}{endpoint}", self.inner.base_url);
        let uri: Uri = url.parse().map_err(|error| failed(endpoint, &error))?;
        let mut request = Request::post(uri.clone());
        for (name, value) in REQUEST_HEADERS.iter().chain([&DEFAULT_ACCEPT]) {
            request = request.header(*name, *value);
        }
        if let Some(cookie) = self.jar().header(&uri, SystemTime::now()) {
            request = request.header(COOKIE, cookie);
        }
        let request = request
            .body(Full::new(Bytes::from(body)))
            .map_err(|error| failed(endpoint, &error))?;
        let response = self
            .inner
            .pool
            .client
            .request(request)
            .await
            .map_err(|error| failed(endpoint, &error))?;
        let status = response.status().as_u16();
        {
            let mut jar = self.jar();
            let now = SystemTime::now();
            for value in response.headers().get_all(SET_COOKIE) {
                if let Ok(value) = value.to_str() {
                    jar.store(&uri, value, now);
                }
            }
        }
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|error| failed(endpoint, &error))?
            .to_bytes();
        // aiohttp's `resp.text()` decodes strictly, before the status is checked.
        let text = String::from_utf8(bytes.into())
            .map_err(|error| Error::new(ErrorKind::UnicodeDecodeError, error.to_string()))?;
        Ok((status, text))
    }
}

impl Env for HttpEnv {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as i64)
    }

    fn monotonic(&self) -> f64 {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed().as_secs_f64()
    }

    fn random_bytes(&self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).expect("the operating system's random source works");
        bytes
    }

    async fn sleep(&self, seconds: f64) {
        // asyncio.sleep returns at once for zero or less and waits
        // (practically) forever for infinity.
        let duration = Duration::try_from_secs_f64(seconds).unwrap_or(if seconds > 0.0 {
            Duration::MAX
        } else {
            Duration::ZERO
        });
        tokio::time::sleep(duration).await;
    }

    async fn post_secure(
        &self,
        endpoint: &str,
        outer: &Map<String, Value>,
    ) -> Result<Value, Error> {
        let body = encode_request_body(outer);
        let timeout = self.inner.pool.timeout;
        let (status, text) = tokio::time::timeout(timeout, self.exchange(endpoint, body))
            .await
            .map_err(|_| {
                Error::transport(
                    format!(
                        "Request to {endpoint} failed: timed out after {} s",
                        timeout.as_secs_f64()
                    ),
                    None,
                    endpoint,
                )
            })??;
        decode_response_body(endpoint, status, &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync + Clone>() {}

    #[test]
    fn environments_are_shareable() {
        assert_send_sync::<HttpEnv>();
        assert_send_sync::<HttpPool>();
        let pool = HttpPool::default();
        let env = HttpEnv::for_config(&pool, &BydConfig::new("user@example.com", "p"));
        assert_eq!(env.base_url(), "https://dilinkappoversea-eu.byd.auto");
        assert_eq!(env.pool().timeout(), DEFAULT_TIMEOUT);
        assert!(format!("{env:?}").contains("dilinkappoversea"));
    }

    #[tokio::test]
    async fn clock_randomness_and_sleep() {
        let env = HttpEnv::new(&HttpPool::new(), "http://127.0.0.1:1");
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!((env.now_ms() - wall).abs() < 1_000);
        let before = env.monotonic();
        env.sleep(0.02).await;
        assert!(env.monotonic() - before >= 0.019);
        env.sleep(-1.0).await;
        env.sleep(f64::NAN).await;
        assert_ne!(env.random_bytes(), env.random_bytes());
        let forever = tokio::time::timeout(Duration::from_millis(10), env.sleep(f64::INFINITY));
        assert!(forever.await.is_err());
    }

    #[tokio::test]
    async fn a_bad_url_is_a_transport_error() {
        let env = HttpEnv::new(&HttpPool::new(), "not a url");
        let error = env.post_secure("/e", &Map::new()).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BydTransportError);
        assert_eq!(error.endpoint(), "/e");
        assert_eq!(error.status_code(), None);
        assert!(error.message().starts_with("Request to /e failed: "));
    }
}
