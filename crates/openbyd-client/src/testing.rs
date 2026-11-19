//! A fake BYD server for tests, bindings and benchmarks (feature `testing`).
//!
//! [`FakeBydServer`] listens on `127.0.0.1` on an ephemeral port and speaks
//! the real wire protocol: it opens the Bangcle envelope, checks the outer
//! payload's `checkcode` and signature as the real service would, handles
//! logins for the [`Account`]s it knows, and answers token requests with
//! `respondData` encrypted under the session's content key. A request
//! signed or checksummed wrongly is refused with its own code
//! ([`codes`]), so a signing bug fails a test instead of passing silently.
//!
//! What a token endpoint answers is up to the test: a handler per endpoint
//! ([`FakeBydServer::on`], [`FakeBydServer::reply`]) or a script of
//! replies used one per request ([`FakeBydServer::script`]). Every request
//! is recorded with its decoded payloads ([`FakeBydServer::requests`]).
//!
//! ```no_run
//! # async fn demo() -> std::io::Result<()> {
//! use openbyd_client::testing::{Account, FakeBydServer, Reply};
//! use serde_json::json;
//!
//! let server = FakeBydServer::start([Account::new("user@example.com", "secret")]).await?;
//! server.reply("/app/emqAuth/getEmqBrokerIp", Reply::Data(json!({"emqBorker": "x"})));
//! server.script("/control/getGpsInfo", [Reply::code("1008"), Reply::Data(json!({}))]);
//! // ... point a client at server.url() ...
//! assert_eq!(server.requests_to("/control/getGpsInfo").len(), 0);
//! server.shutdown().await;
//! # Ok(()) }
//! ```

#![cfg(feature = "testing")]

pub mod broker;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{COOKIE, SET_COOKIE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use openbyd_protocol::aes::{aes_decrypt_utf8, aes_encrypt_hex};
use openbyd_protocol::envelope::LOGIN_ENDPOINT;
use openbyd_protocol::hashing::{compute_checkcode, md5_hex, pwd_login_key, sha1_mixed};
use openbyd_protocol::pyjson::{DumpOptions, dumps, dumps_compact, loads};
use openbyd_protocol::signing::build_sign_string;
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinSet;

/// The codes the fake server refuses requests with.
pub mod codes {
    /// The outer `checkcode` does not match the payload. The fake's own code.
    pub const BAD_CHECKCODE: &str = "9001";
    /// The `sign` does not match (the real service's 1007).
    pub const BAD_SIGNATURE: &str = "1007";
    /// Login for an unknown account, or with the wrong password. The fake's own code.
    pub const LOGIN_FAILED: &str = "9002";
    /// The body, envelope or `encryData` could not be decoded. The fake's own code.
    pub const BAD_REQUEST: &str = "9003";
    /// No handler or script for the endpoint. The fake's own code.
    pub const NO_HANDLER: &str = "9004";
    /// The session is unknown or expired (the real service's 1005).
    pub const SESSION_EXPIRED: &str = "1005";
    /// A guest account did not send `userType: "1"` (the real service's 1001).
    pub const GUEST_NOT_DECLARED: &str = "1001";
    /// An owner sent `userType: "1"` (the real service's 8003).
    pub const NOT_A_GUEST: &str = "8003";
}

/// An account the fake server accepts logins for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// The login identifier.
    pub username: String,
    /// The password the login must be encrypted and signed with.
    pub password: String,
    /// The user id the login issues; assigned by the server when `None`.
    pub user_id: Option<String>,
    /// VINs this account only has guest (shared) access to.
    pub guest_vins: HashSet<String>,
    /// Whether requests naming a VIN must declare guest access exactly
    /// when the account is a guest for it, as the real service demands.
    pub user_type_rule: bool,
}

impl Account {
    /// An owner account with the `userType` rule on.
    pub fn new(username: &str, password: &str) -> Self {
        Self {
            username: username.to_owned(),
            password: password.to_owned(),
            user_id: None,
            guest_vins: HashSet::new(),
            user_type_rule: true,
        }
    }

    /// Sets the user id the login issues.
    pub fn with_user_id(mut self, user_id: &str) -> Self {
        self.user_id = Some(user_id.to_owned());
        self
    }

    /// Makes this account a guest for `vin`.
    pub fn guest(mut self, vin: &str) -> Self {
        self.guest_vins.insert(vin.to_owned());
        self
    }

    /// Turns the `userType` rule off.
    pub fn without_user_type_rule(mut self) -> Self {
        self.user_type_rule = false;
        self
    }
}

/// What the server answers to one request.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// Success: code `"0"` with this data as `respondData`, encrypted with
    /// the endpoint's key (the login key for a login, else the session's
    /// content key).
    Data(Value),
    /// An API error: this `code` and `message`, with no `respondData`.
    Code {
        /// The API code.
        code: String,
        /// The message.
        message: String,
    },
    /// This outer reply as it is, encrypted only by the envelope.
    Outer(Value),
    /// An HTTP status other than 200 with this body.
    Status(u16, String),
    /// Status 200 with this body, not enveloped.
    Raw(Vec<u8>),
    /// Waits, then answers the inner reply.
    Delay(Duration, Box<Reply>),
    /// Closes the connection without answering.
    Drop,
    /// Adds this `Set-Cookie` header to the inner reply.
    SetCookie(String, Box<Reply>),
    /// What the server would answer without a handler: a login for a
    /// login request, [`codes::NO_HANDLER`] otherwise.
    Default,
}

impl Reply {
    /// An API error with an empty message.
    pub fn code(code: &str) -> Self {
        Reply::Code {
            code: code.to_owned(),
            message: String::new(),
        }
    }

    /// This reply after `delay`.
    pub fn delayed(self, delay: Duration) -> Self {
        Reply::Delay(delay, Box::new(self))
    }

    /// This reply with a `Set-Cookie` header.
    pub fn with_cookie(self, header: &str) -> Self {
        Reply::SetCookie(header.to_owned(), Box::new(self))
    }
}

/// One request the server received, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// The path, for example `/app/account/login`.
    pub endpoint: String,
    /// The headers, names in lowercase, in the order received.
    pub headers: Vec<(String, String)>,
    /// The cookies the `Cookie` header carried.
    pub cookies: Vec<(String, String)>,
    /// The raw body.
    pub body: String,
    /// The outer payload, when the envelope opened.
    pub outer: Option<Map<String, Value>>,
    /// The decrypted inner payload, when it decrypted.
    pub inner: Option<Value>,
    /// The account the request was verified as.
    pub username: Option<String>,
    /// The API code answered; `None` for an HTTP-level reply. Not set yet
    /// when a handler sees the request.
    pub code: Option<String>,
    /// The HTTP status answered; `None` when the connection was dropped.
    pub status: Option<u16>,
}

impl Request {
    /// The value of a header (the name is matched in lowercase).
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The value of a cookie.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The decrypted inner payload's field, when there is one.
    pub fn inner_field(&self, name: &str) -> Option<&Value> {
        self.inner.as_ref().and_then(|inner| inner.get(name))
    }
}

type Handler = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

struct Tokens {
    sign_key: String,
    content_key: String,
    expired: bool,
}

struct AccountState {
    account: Account,
    user_id: String,
    tokens: Vec<Tokens>,
}

#[derive(Default)]
struct State {
    accounts: Vec<AccountState>,
    handlers: HashMap<String, Handler>,
    scripts: HashMap<String, VecDeque<Reply>>,
    requests: Vec<Request>,
    cookies: Vec<String>,
    issued: u64,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    connections: AtomicUsize,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

enum Runner {
    Task(tokio::task::JoinHandle<()>),
    Thread(std::thread::JoinHandle<()>),
}

/// A running fake BYD server. Dropping it stops it.
pub struct FakeBydServer {
    shared: Arc<Shared>,
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    runner: Option<Runner>,
}

impl fmt::Debug for FakeBydServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeBydServer")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl FakeBydServer {
    fn prepare(accounts: impl IntoIterator<Item = Account>) -> Arc<Shared> {
        let shared = Arc::new(Shared::default());
        for account in accounts {
            shared.add_account(account);
        }
        shared
    }

    /// Starts a server for `accounts` on the current tokio runtime.
    pub async fn start(accounts: impl IntoIterator<Item = Account>) -> std::io::Result<Self> {
        let shared = Self::prepare(accounts);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(serve(listener, shared.clone(), stopped));
        Ok(Self {
            shared,
            addr,
            stop: Some(stop),
            runner: Some(Runner::Task(task)),
        })
    }

    /// Starts a server for `accounts` on a thread of its own, for callers
    /// without a tokio runtime (Python, benchmarks). Stop it with
    /// [`FakeBydServer::shutdown_blocking`] or by dropping it.
    pub fn start_standalone(accounts: impl IntoIterator<Item = Account>) -> std::io::Result<Self> {
        let shared = Self::prepare(accounts);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let listener = runtime.block_on(TcpListener::bind(("127.0.0.1", 0)))?;
        let addr = listener.local_addr()?;
        let (stop, stopped) = oneshot::channel();
        let serving = shared.clone();
        let thread = std::thread::Builder::new()
            .name("fake-byd-server".into())
            .spawn(move || runtime.block_on(serve(listener, serving, stopped)))?;
        Ok(Self {
            shared,
            addr,
            stop: Some(stop),
            runner: Some(Runner::Thread(thread)),
        })
    }

    /// The base URL to give a client: `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The address the server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Accepts logins for one more account.
    pub fn add_account(&self, account: Account) {
        self.shared.add_account(account);
    }

    /// The user id logins for `username` receive.
    pub fn user_id(&self, username: &str) -> Option<String> {
        self.shared
            .state()
            .accounts
            .iter()
            .find(|state| state.account.username == username)
            .map(|state| state.user_id.clone())
    }

    /// Answers every verified request to `endpoint` with what `handler`
    /// returns (unless a script entry comes first). Replaces an earlier handler.
    pub fn on(&self, endpoint: &str, handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) {
        self.shared
            .state()
            .handlers
            .insert(endpoint.to_owned(), Arc::new(handler));
    }

    /// Answers every verified request to `endpoint` with `reply`.
    pub fn reply(&self, endpoint: &str, reply: Reply) {
        self.on(endpoint, move |_| reply.clone());
    }

    /// Queues replies for `endpoint`, one per verified request, ahead of
    /// its handler.
    pub fn script(&self, endpoint: &str, replies: impl IntoIterator<Item = Reply>) {
        self.shared
            .state()
            .scripts
            .entry(endpoint.to_owned())
            .or_default()
            .extend(replies);
    }

    /// Sends this `Set-Cookie` header with every reply.
    pub fn set_cookie(&self, header: &str) {
        self.shared.state().cookies.push(header.to_owned());
    }

    /// Expires every session of `username`: its token requests are
    /// answered [`codes::SESSION_EXPIRED`] until it logs in again.
    pub fn expire_sessions(&self, username: &str) {
        let mut state = self.shared.state();
        for account in &mut state.accounts {
            if account.account.username == username {
                for tokens in &mut account.tokens {
                    tokens.expired = true;
                }
            }
        }
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.shared.state().requests.clone()
    }

    /// The requests received for `endpoint`, in order.
    pub fn requests_to(&self, endpoint: &str) -> Vec<Request> {
        self.shared
            .state()
            .requests
            .iter()
            .filter(|request| request.endpoint == endpoint)
            .cloned()
            .collect()
    }

    /// Forgets the recorded requests.
    pub fn clear_requests(&self) {
        self.shared.state().requests.clear();
    }

    /// How many TCP connections the server has accepted.
    pub fn connections(&self) -> usize {
        self.shared.connections.load(Ordering::SeqCst)
    }

    fn signal_stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    /// Stops the server and waits until it has closed its socket and
    /// every connection.
    pub async fn shutdown(mut self) {
        self.signal_stop();
        match self.runner.take() {
            Some(Runner::Task(task)) => {
                let _ = task.await;
            }
            Some(Runner::Thread(thread)) => {
                let _ = tokio::task::spawn_blocking(move || thread.join()).await;
            }
            None => {}
        }
    }

    /// Stops the server and waits for it without a runtime. On a server
    /// started with [`FakeBydServer::start`] it only signals the stop.
    pub fn shutdown_blocking(mut self) {
        self.signal_stop();
        if let Some(Runner::Thread(thread)) = self.runner.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FakeBydServer {
    fn drop(&mut self) {
        self.signal_stop();
    }
}

async fn serve(listener: TcpListener, shared: Arc<Shared>, mut stopped: oneshot::Receiver<()>) {
    // Dropping the set when the loop ends aborts every open connection.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stopped => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                shared.connections.fetch_add(1, Ordering::SeqCst);
                let shared = shared.clone();
                connections.spawn(async move {
                    let service = service_fn(move |request| handle(shared.clone(), request));
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

/// The error that makes hyper close a connection without answering.
#[derive(Debug)]
struct Dropped;

impl fmt::Display for Dropped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("connection dropped on purpose")
    }
}

impl std::error::Error for Dropped {}

/// Who a verified request came from and the key its reply is encrypted with.
struct Verified {
    username: String,
    user_id: Option<String>,
    key: String,
}

/// The outcome of checking a request: verified, or refused with a code.
type Checked = Result<Verified, (&'static str, String)>;

fn refuse(code: &'static str, message: impl Into<String>) -> (&'static str, String) {
    (code, message.into())
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) => openbyd_protocol::pyjson::py_str(other),
        None => String::new(),
    }
}

fn signed_with(
    inner: &Map<String, Value>,
    outer: &Map<String, Value>,
    keys: &[&str],
    password: &str,
) -> bool {
    let mut fields = inner.clone();
    for key in keys {
        if let Some(value) = outer.get(*key) {
            fields.insert((*key).to_owned(), value.clone());
        }
    }
    outer.get("sign").and_then(Value::as_str)
        == Some(&sha1_mixed(&build_sign_string(&fields, password)))
}

fn decrypt_inner(outer: &Map<String, Value>, key: &str) -> Option<Map<String, Value>> {
    let data = outer.get("encryData")?.as_str()?;
    let text = aes_decrypt_utf8(data, key).ok()?;
    match loads(&text).ok()? {
        Value::Object(inner) => Some(inner),
        _ => None,
    }
}

impl Shared {
    fn add_account(&self, account: Account) {
        let mut state = self.state();
        let user_id = account
            .user_id
            .clone()
            .unwrap_or_else(|| (10_001 + state.accounts.len()).to_string());
        state.accounts.push(AccountState {
            account,
            user_id,
            tokens: Vec::new(),
        });
    }

    fn verify_login(&self, outer: &Map<String, Value>, request: &mut Request) -> Checked {
        let state = self.state();
        let identifier = text_of(outer.get("identifier"));
        let account = state
            .accounts
            .iter()
            .find(|state| state.account.username == identifier)
            .ok_or_else(|| refuse(codes::LOGIN_FAILED, "unknown account"))?;
        let password = &account.account.password;
        let key = pwd_login_key(password);
        let inner = decrypt_inner(outer, &key)
            .ok_or_else(|| refuse(codes::LOGIN_FAILED, "wrong password"))?;
        request.inner = Some(Value::Object(inner.clone()));
        let signed = [
            "appName",
            "countryCode",
            "functionType",
            "identifier",
            "identifierType",
            "language",
            "reqTimestamp",
        ];
        if !signed_with(&inner, outer, &signed, &md5_hex(password)) {
            return Err(refuse(codes::BAD_SIGNATURE, "sign check failed"));
        }
        Ok(Verified {
            username: identifier,
            user_id: None,
            key,
        })
    }

    fn verify_token(&self, outer: &Map<String, Value>, request: &mut Request) -> Checked {
        let state = self.state();
        let identifier = text_of(outer.get("identifier"));
        let account = state
            .accounts
            .iter()
            .find(|state| state.user_id == identifier)
            .ok_or_else(|| refuse(codes::SESSION_EXPIRED, "unknown user"))?;
        let mut decrypted = false;
        let signed = [
            "countryCode",
            "identifier",
            "imeiMD5",
            "language",
            "reqTimestamp",
            "userType",
        ];
        for tokens in account.tokens.iter().rev() {
            let Some(inner) = decrypt_inner(outer, &tokens.content_key) else {
                continue;
            };
            decrypted = true;
            request.inner = Some(Value::Object(inner.clone()));
            if !signed_with(&inner, outer, &signed, &tokens.sign_key) {
                continue;
            }
            if tokens.expired {
                return Err(refuse(codes::SESSION_EXPIRED, "session expired"));
            }
            let rules = &account.account;
            if let (true, Some(Value::String(vin))) = (rules.user_type_rule, inner.get("vin")) {
                let declared = outer.get("userType").and_then(Value::as_str) == Some("1");
                match (rules.guest_vins.contains(vin), declared) {
                    (true, false) => {
                        return Err(refuse(
                            codes::GUEST_NOT_DECLARED,
                            "guest must declare userType",
                        ));
                    }
                    (false, true) => return Err(refuse(codes::NOT_A_GUEST, "not a guest")),
                    _ => {}
                }
            }
            return Ok(Verified {
                username: rules.username.clone(),
                user_id: Some(identifier),
                key: tokens.content_key.clone(),
            });
        }
        Err(if decrypted {
            refuse(codes::BAD_SIGNATURE, "sign check failed")
        } else if account.tokens.is_empty() {
            refuse(codes::SESSION_EXPIRED, "not logged in")
        } else {
            refuse(
                codes::BAD_REQUEST,
                "encryData does not decrypt with any session key",
            )
        })
    }

    /// Issues fresh tokens for a verified login and returns its `respondData` plaintext.
    fn login(&self, username: &str) -> Value {
        let mut state = self.state();
        state.issued += 1;
        let serial = state.issued;
        let account = state
            .accounts
            .iter_mut()
            .find(|state| state.account.username == username)
            .expect("a verified login names a known account");
        let sign_token = format!("sign-{}-{serial}", account.user_id);
        let encry_token = format!("encry-{}-{serial}", account.user_id);
        account.tokens.push(Tokens {
            sign_key: md5_hex(&sign_token),
            content_key: md5_hex(&encry_token),
            expired: false,
        });
        json!({"token": {
            "userId": account.user_id,
            "signToken": sign_token,
            "encryToken": encry_token,
        }})
    }

    /// The reply chosen for a verified request: a script entry, then the
    /// handler, then the default.
    fn choose(&self, request: &Request) -> Reply {
        let handler = {
            let mut state = self.state();
            if let Some(reply) = state
                .scripts
                .get_mut(&request.endpoint)
                .and_then(VecDeque::pop_front)
            {
                return reply;
            }
            state.handlers.get(&request.endpoint).cloned()
        };
        handler.map_or(Reply::Default, |handler| handler(request))
    }
}

fn enveloped(outer: &Value) -> String {
    let envelope = openbyd_crypto::encode_envelope(dumps_compact(outer).as_bytes());
    dumps(&json!({ "response": envelope }), DumpOptions::DEFAULT)
}

fn api_error(code: &str, message: &str) -> Value {
    json!({"code": code, "message": message})
}

/// What goes back on the wire.
enum Answer {
    Outer(Value),
    Http(u16, Vec<u8>),
    Drop,
}

fn parse_cookies(headers: &hyper::HeaderMap) -> Vec<(String, String)> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

fn open_body(body: &str) -> Option<Map<String, Value>> {
    let body: Value = serde_json::from_str(body).ok()?;
    let bytes = openbyd_crypto::decode_envelope(body.get("request")?.as_str()?).ok()?;
    match loads(std::str::from_utf8(&bytes).ok()?).ok()? {
        Value::Object(outer) => Some(outer),
        _ => None,
    }
}

async fn handle(
    shared: Arc<Shared>,
    incoming: hyper::Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Dropped> {
    let (parts, body) = incoming.into_parts();
    let body = body.collect().await.map_err(|_| Dropped)?.to_bytes();
    let mut request = Request {
        endpoint: parts.uri.path().to_owned(),
        headers: parts
            .headers
            .iter()
            .map(|(name, value)| {
                let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
                (name.as_str().to_owned(), value)
            })
            .collect(),
        cookies: parse_cookies(&parts.headers),
        body: String::from_utf8_lossy(&body).into_owned(),
        outer: None,
        inner: None,
        username: None,
        code: None,
        status: None,
    };
    let (delay, cookies, answer) = decide(&shared, &mut request);
    request.code = match &answer {
        Answer::Outer(outer) => Some(text_of(outer.get("code"))),
        _ => None,
    };
    request.status = match &answer {
        Answer::Outer(_) => Some(200),
        Answer::Http(status, _) => Some(*status),
        Answer::Drop => None,
    };
    shared.state().requests.push(request);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let (status, body) = match answer {
        Answer::Outer(outer) => (200, enveloped(&outer).into_bytes()),
        Answer::Http(status, body) => (status, body),
        Answer::Drop => return Err(Dropped),
    };
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() =
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = response.headers_mut();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json;charset=UTF-8"),
    );
    let all = shared.state().cookies.clone();
    for cookie in all.iter().chain(&cookies) {
        if let Ok(value) = cookie.parse() {
            headers.append(SET_COOKIE, value);
        }
    }
    Ok(response)
}

/// Decodes and verifies a request, then works out the delay, the extra
/// cookies and the answer.
fn decide(shared: &Shared, request: &mut Request) -> (Duration, Vec<String>, Answer) {
    let (delay, cookies) = (Duration::ZERO, Vec::new());
    let Some(outer) = open_body(&request.body) else {
        let answer = Answer::Outer(api_error(codes::BAD_REQUEST, "bad envelope"));
        return (delay, cookies, answer);
    };
    request.outer = Some(outer.clone());
    let mut unsigned = outer.clone();
    let checkcode = unsigned.shift_remove("checkcode");
    if checkcode.as_ref().and_then(Value::as_str)
        != Some(&compute_checkcode(&Value::Object(unsigned)))
    {
        let answer = Answer::Outer(api_error(codes::BAD_CHECKCODE, "checkcode mismatch"));
        return (delay, cookies, answer);
    }
    let login = request.endpoint == LOGIN_ENDPOINT;
    let checked = if login {
        shared.verify_login(&outer, request)
    } else {
        shared.verify_token(&outer, request)
    };
    let verified = match checked {
        Ok(verified) => verified,
        Err((code, message)) => {
            return (delay, cookies, Answer::Outer(api_error(code, &message)));
        }
    };
    request.username = Some(verified.username.clone());
    let mut reply = shared.choose(request);
    let (mut delay, mut cookies) = (delay, cookies);
    let success = |data: &Value| {
        let mut outer = Map::new();
        outer.insert("code".into(), json!("0"));
        outer.insert("message".into(), json!("SUCCESS"));
        if let Some(user_id) = &verified.user_id {
            outer.insert("identifier".into(), json!(user_id));
        }
        let encrypted = aes_encrypt_hex(&dumps_compact(data), &verified.key)
            .expect("session keys are 16 bytes of hex");
        outer.insert("respondData".into(), json!(encrypted));
        Answer::Outer(Value::Object(outer))
    };
    let answer = loop {
        break match reply {
            Reply::Delay(wait, inner) => {
                delay += wait;
                reply = *inner;
                continue;
            }
            Reply::SetCookie(cookie, inner) => {
                cookies.push(cookie);
                reply = *inner;
                continue;
            }
            Reply::Data(data) => success(&data),
            Reply::Default if login => success(&shared.login(&verified.username)),
            Reply::Default => {
                Answer::Outer(api_error(codes::NO_HANDLER, "no handler for this endpoint"))
            }
            Reply::Code { code, message } => Answer::Outer(api_error(&code, &message)),
            Reply::Outer(outer) => Answer::Outer(outer),
            Reply::Status(status, body) => Answer::Http(status, body.into_bytes()),
            Reply::Raw(body) => Answer::Http(200, body),
            Reply::Drop => Answer::Drop,
        };
    };
    (delay, cookies, answer)
}
