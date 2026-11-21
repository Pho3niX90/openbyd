//! `HttpEnv` against the fake BYD server, over real sockets.

#![cfg(feature = "testing")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use openbyd_client::Env;
use openbyd_client::api::Context;
use openbyd_client::api::common::{
    ErrorCodes, build_inner_base, post_token_json, post_vehicle_json,
};
use openbyd_client::api::gps::fetch_gps_endpoint;
use openbyd_client::http::{HttpEnv, HttpOptions, HttpPool};
use openbyd_client::testing::{Account, FakeBydServer, Reply, Request, codes};
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::envelope::{
    LOGIN_ENDPOINT, build_login_request, build_token_outer_envelope, parse_login_response,
};
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::session::{DEFAULT_SESSION_TTL, Session};
use serde_json::{Map, Value, json};

const VIN: &str = "TESTVIN0000000001";
const USER: &str = "user@example.com";
const PASSWORD: &str = "pa$$ wörd";

fn config(server: &FakeBydServer, username: &str, password: &str) -> BydConfig {
    let mut config = BydConfig::new(username, password);
    config.base_url = server.url();
    config
}

async fn login(env: &HttpEnv, config: &BydConfig) -> Result<Session, Error> {
    let outer = build_login_request(config, env.now_ms(), &env.random_bytes(), env.now_ms());
    let reply = env.post_secure(LOGIN_ENDPOINT, &outer).await?;
    let token = parse_login_response(&reply, &config.password)?;
    Ok(token.to_session(env.monotonic(), DEFAULT_SESSION_TTL))
}

fn inner(env: &HttpEnv, config: &BydConfig) -> Map<String, Value> {
    build_inner_base(config, env.now_ms(), &env.random_bytes(), Some(VIN), None)
}

async fn token_json(
    env: &HttpEnv,
    config: &BydConfig,
    session: &Session,
    endpoint: &str,
) -> Result<Value, Error> {
    let cx = Context::new(env, config, session);
    let inner = inner(env, config);
    post_token_json(
        cx,
        endpoint,
        &inner,
        None,
        Some(VIN),
        ErrorCodes::default(),
        None,
    )
    .await
}

struct Setup {
    server: FakeBydServer,
    pool: HttpPool,
    env: HttpEnv,
    config: BydConfig,
    session: Session,
}

async fn setup_with(account: Account, options: HttpOptions) -> Setup {
    let server = FakeBydServer::start([account]).await.unwrap();
    let pool = HttpPool::with_options(&options);
    let config = config(&server, USER, PASSWORD);
    let env = HttpEnv::for_config(&pool, &config);
    let session = login(&env, &config).await.unwrap();
    Setup {
        server,
        pool,
        env,
        config,
        session,
    }
}

async fn setup() -> Setup {
    setup_with(Account::new(USER, PASSWORD), HttpOptions::default()).await
}

#[tokio::test]
async fn login_then_token_and_vehicle_requests() {
    let s = setup().await;
    let user_id = s.server.user_id(USER).unwrap();
    assert_eq!(s.session.user_id(), user_id);
    let login = &s.server.requests_to(LOGIN_ENDPOINT)[0];
    assert_eq!(login.username.as_deref(), Some(USER));
    assert_eq!(login.code.as_deref(), Some("0"));
    assert_eq!(
        login.inner_field("timeZone").unwrap(),
        &json!(s.config.time_zone)
    );

    s.server.on("/app/echo", |request: &Request| {
        Reply::Data(json!({"echo": request.inner_field("vin").cloned()}))
    });
    let echoed = token_json(&s.env, &s.config, &s.session, "/app/echo")
        .await
        .unwrap();
    assert_eq!(echoed, json!({"echo": VIN}));

    s.server
        .reply("/vehicle/x", Reply::Data(json!({"ok": true})));
    let cx = Context::new(&s.env, &s.config, &s.session);
    let inner = inner(&s.env, &s.config);
    let reply = post_vehicle_json(cx, "/vehicle/x", &inner, VIN, None, ErrorCodes::default());
    assert_eq!(reply.await.unwrap(), json!({"ok": true}));
    let recorded = s.server.requests_to("/vehicle/x").pop().unwrap();
    assert_eq!(recorded.username.as_deref(), Some(USER));
    assert_eq!(recorded.outer.unwrap()["identifier"], json!(user_id));
    assert_eq!(recorded.inner.unwrap(), Value::Object(inner));
    assert!(s.server.connections() >= 1);
    s.server.shutdown().await;
}

#[tokio::test]
async fn gps_flow_end_to_end() {
    let s = setup().await;
    s.server.reply(
        "/control/getGpsInfo",
        Reply::Data(json!({"requestSerial": "SERIAL1"})),
    );
    s.server
        .on("/control/getGpsInfoResult", |request: &Request| {
            assert_eq!(
                request.inner_field("requestSerial"),
                Some(&json!("SERIAL1"))
            );
            Reply::Data(json!({"latitude": 52.0, "longitude": 4.0}))
        });
    s.server
        .script("/control/getGpsInfoResult", [Reply::code("6051")]);
    let cx = Context::new(&s.env, &s.config, &s.session);
    let (info, serial) = fetch_gps_endpoint(cx, "/control/getGpsInfo", VIN, None)
        .await
        .unwrap();
    assert_eq!(info, json!({"requestSerial": "SERIAL1"}));
    assert_eq!(serial.as_deref(), Some("SERIAL1"));
    let unavailable = fetch_gps_endpoint(cx, "/control/getGpsInfoResult", VIN, Some("SERIAL1"))
        .await
        .unwrap_err();
    assert_eq!(unavailable.kind(), ErrorKind::BydDataUnavailableError);
    let (info, serial) = fetch_gps_endpoint(cx, "/control/getGpsInfoResult", VIN, Some("SERIAL1"))
        .await
        .unwrap();
    assert_eq!(info, json!({"latitude": 52.0, "longitude": 4.0}));
    assert_eq!(serial.as_deref(), Some("SERIAL1"));
}

#[tokio::test]
async fn headers_are_pybyds() {
    let s = setup().await;
    let request = &s.server.requests()[0];
    let mut names: Vec<&str> = request
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "accept",
            "accept-encoding",
            "content-length",
            "content-type",
            "host",
            "user-agent"
        ]
    );
    assert_eq!(request.header("Accept-Encoding"), Some("identity"));
    assert_eq!(
        request.header("content-type"),
        Some("application/json; charset=UTF-8")
    );
    assert_eq!(request.header("user-agent"), Some("okhttp/4.12.0"));
    assert_eq!(request.header("accept"), Some("*/*"));
    assert_eq!(request.header("host").unwrap(), s.server.addr().to_string());
    assert!(request.body.starts_with("{\"request\": \"F"));
    assert_eq!(request.header("x-missing"), None);
}

#[tokio::test]
async fn cookies_persist_per_environment() {
    let s = setup().await;
    s.server.set_cookie("route=r1; Path=/");
    s.server.reply("/app/a", Reply::Data(json!({})));
    token_json(&s.env, &s.config, &s.session, "/app/a")
        .await
        .unwrap();
    assert_eq!(s.env.cookies(), [("route".to_owned(), "r1".to_owned())]);
    token_json(&s.env, &s.config, &s.session, "/app/a")
        .await
        .unwrap();
    let requests = s.server.requests_to("/app/a");
    assert_eq!(requests[0].cookie("route"), None);
    assert_eq!(requests[1].cookie("route"), Some("r1"));
    assert_eq!(requests[1].cookie("other"), None);

    // A second environment on the same pool has its own jar; clones share one.
    let other = HttpEnv::new(&s.pool, s.server.url());
    assert!(other.cookies().is_empty());
    assert_eq!(s.env.clone().cookies().len(), 1);
    s.env.clear_cookies();
    assert!(s.env.cookies().is_empty());
}

#[tokio::test]
async fn an_expired_cookie_is_dropped() {
    let s = setup().await;
    s.server.reply("/app/a", Reply::Data(json!({})));
    s.server.set_cookie("gone=1; Max-Age=0");
    token_json(&s.env, &s.config, &s.session, "/app/a")
        .await
        .unwrap();
    assert!(s.env.cookies().is_empty());
}

fn assert_transport(error: &Error, endpoint: &str, status: Option<u16>) {
    assert_eq!(error.kind(), ErrorKind::BydTransportError, "{error}");
    assert_eq!(error.endpoint(), endpoint);
    assert_eq!(error.status_code(), status);
}

#[tokio::test]
async fn transport_errors() {
    let s = setup().await;
    let post = |endpoint: &'static str| token_json(&s.env, &s.config, &s.session, endpoint);

    let long = "x".repeat(500);
    s.server
        .reply("/e/status", Reply::Status(502, long.clone()));
    let error = post("/e/status").await.unwrap_err();
    assert_transport(&error, "/e/status", Some(502));
    assert_eq!(
        error.message(),
        format!("HTTP 502 from /e/status: {}", &long[..200])
    );

    s.server.reply("/e/json", Reply::Raw(b"not json".to_vec()));
    let error = post("/e/json").await.unwrap_err();
    assert_transport(&error, "/e/json", None);
    assert_eq!(error.message(), "Invalid JSON from /e/json: not json");

    s.server.reply("/e/missing", Reply::Raw(b"{}".to_vec()));
    assert_transport(&post("/e/missing").await.unwrap_err(), "/e/missing", None);

    s.server.reply("/e/utf8", Reply::Raw(vec![0xff, 0xfe]));
    let error = post("/e/utf8").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UnicodeDecodeError);

    s.server.reply(
        "/e/outer",
        Reply::Outer(json!({"code": "1008", "message": "busy"})),
    );
    let error = post("/e/outer").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydServiceBusyError);

    let error = post("/e/unhandled").await.unwrap_err();
    assert_eq!(error.code(), codes::NO_HANDLER);

    // Nothing listening.
    s.server.shutdown().await;
    let error = post("/e/refused").await.unwrap_err();
    assert_transport(&error, "/e/refused", None);
    assert!(
        error
            .message()
            .starts_with("Request to /e/refused failed: "),
        "{error}"
    );
}

#[tokio::test]
async fn a_closed_connection_is_a_transport_error_and_the_pool_recovers() {
    let s = setup().await;
    s.server
        .script("/e/drop", [Reply::Drop, Reply::Data(json!({"again": 1}))]);
    let error = token_json(&s.env, &s.config, &s.session, "/e/drop")
        .await
        .unwrap_err();
    assert_transport(&error, "/e/drop", None);
    let requests = s.server.requests_to("/e/drop");
    assert_eq!(
        (requests[0].code.as_ref(), requests[0].status),
        (None, None)
    );
    let again = token_json(&s.env, &s.config, &s.session, "/e/drop").await;
    assert_eq!(again.unwrap(), json!({"again": 1}));
}

#[tokio::test]
async fn a_slow_reply_hits_the_timeout() {
    let options = HttpOptions {
        timeout: Duration::from_millis(300),
        ..HttpOptions::default()
    };
    let s = setup_with(Account::new(USER, PASSWORD), options).await;
    s.server.script(
        "/e/slow",
        [Reply::Data(json!({})).delayed(Duration::from_secs(5))],
    );
    s.server
        .reply("/e/slow", Reply::Default.delayed(Duration::from_millis(10)));
    let started = Instant::now();
    let error = token_json(&s.env, &s.config, &s.session, "/e/slow")
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_transport(&error, "/e/slow", None);
    assert_eq!(
        error.message(),
        "Request to /e/slow failed: timed out after 0.3 s"
    );
    // A delay inside the limit is fine.
    let error = token_json(&s.env, &s.config, &s.session, "/e/slow")
        .await
        .unwrap_err();
    assert_eq!(error.code(), codes::NO_HANDLER);
}

#[tokio::test]
async fn a_bad_signature_or_checkcode_is_refused() {
    let s = setup().await;
    s.server.reply("/e/ok", Reply::Data(json!({})));
    let inner = inner(&s.env, &s.config);
    let (outer, _) = build_token_outer_envelope(
        &s.config,
        &s.session,
        &inner,
        s.env.now_ms(),
        None,
        s.env.now_ms(),
    );

    let mut forged = outer.clone();
    forged.insert("sign".into(), json!("0000"));
    // Keep the checkcode right, so only the signature is wrong.
    forged.shift_remove("checkcode");
    let checkcode = openbyd_protocol::hashing::compute_checkcode(&Value::Object(forged.clone()));
    forged.insert("checkcode".into(), json!(checkcode));
    let reply = s.env.post_secure("/e/ok", &forged).await.unwrap();
    assert_eq!(reply["code"], codes::BAD_SIGNATURE);

    let mut tampered = outer.clone();
    tampered.insert("checkcode".into(), json!("nope"));
    let reply = s.env.post_secure("/e/ok", &tampered).await.unwrap();
    assert_eq!(reply["code"], codes::BAD_CHECKCODE);

    // Signed with another session's key.
    let stranger = Session::new(s.session.user_id(), "other", "other", 0.0, 1.0);
    let (outer, _) = build_token_outer_envelope(&s.config, &stranger, &inner, 1, None, 1);
    let reply = s.env.post_secure("/e/ok", &outer).await.unwrap();
    assert_eq!(reply["code"], codes::BAD_REQUEST);
    let mut unsigned_user_type =
        build_token_outer_envelope(&s.config, &s.session, &inner, 1, None, 1).0;
    unsigned_user_type.shift_remove("checkcode");
    unsigned_user_type.insert("userType".into(), json!("1"));
    let checkcode =
        openbyd_protocol::hashing::compute_checkcode(&Value::Object(unsigned_user_type.clone()));
    unsigned_user_type.insert("checkcode".into(), json!(checkcode));
    let reply = s
        .env
        .post_secure("/e/ok", &unsigned_user_type)
        .await
        .unwrap();
    assert_eq!(reply["code"], codes::BAD_SIGNATURE);

    let reply = s.env.post_secure("/e/ok", &Map::new()).await.unwrap();
    assert_eq!(reply["code"], codes::BAD_CHECKCODE);
    let reply = s.env.post_secure("/e/ok", &outer).await.unwrap();
    assert_eq!(reply["code"], codes::BAD_REQUEST);
    let refused: Vec<_> = s
        .server
        .requests_to("/e/ok")
        .into_iter()
        .map(|request| request.code.unwrap())
        .collect();
    assert_eq!(refused, ["1007", "9001", "9003", "1007", "9001", "9003"]);
}

#[tokio::test]
async fn bad_logins_are_refused() {
    let server = FakeBydServer::start([Account::new(USER, PASSWORD).with_user_id("42")])
        .await
        .unwrap();
    let pool = HttpPool::new();
    let env = HttpEnv::new(&pool, server.url());
    let auth = |error: Error, code: &str| {
        assert_eq!(error.kind(), ErrorKind::BydAuthenticationError);
        assert_eq!(error.code(), code);
    };
    auth(
        login(&env, &config(&server, "nobody@example.com", PASSWORD))
            .await
            .unwrap_err(),
        codes::LOGIN_FAILED,
    );
    auth(
        login(&env, &config(&server, USER, "wrong"))
            .await
            .unwrap_err(),
        codes::LOGIN_FAILED,
    );

    let good = config(&server, USER, PASSWORD);
    let mut outer = build_login_request(&good, 1, &[0; 16], 1);
    outer.insert("sign".into(), json!("forged"));
    outer.shift_remove("checkcode");
    let checkcode = openbyd_protocol::hashing::compute_checkcode(&Value::Object(outer.clone()));
    outer.insert("checkcode".into(), json!(checkcode));
    let reply = env.post_secure(LOGIN_ENDPOINT, &outer).await.unwrap();
    assert_eq!(reply["code"], codes::BAD_SIGNATURE);

    // A token request before any login, and a body that is no envelope.
    let session = Session::new("42", "s", "e", 0.0, 1.0);
    let error = token_json(&env, &good, &session, "/e").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydSessionExpiredError);
    let unknown = Session::new("7", "s", "e", 0.0, 1.0);
    let error = token_json(&env, &good, &unknown, "/e").await.unwrap_err();
    assert_eq!(error.code(), codes::SESSION_EXPIRED);

    // A scripted login reply.
    server.script(
        LOGIN_ENDPOINT,
        [Reply::code("1101"), Reply::Data(json!({"token": {}}))],
    );
    auth(login(&env, &good).await.unwrap_err(), "1101");
    auth(login(&env, &good).await.unwrap_err(), "");
    assert_eq!(login(&env, &good).await.unwrap().user_id(), "42");
    server.shutdown_blocking();
}

#[tokio::test]
async fn garbage_bodies_are_refused() {
    let s = setup().await;
    // post_secure always sends an envelope, so send a broken one by hand.
    let response = raw_post(s.server.addr(), "/e", "{\"request\": \"Gnot-an-envelope\"}").await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    let recorded = s.server.requests_to("/e").pop().unwrap();
    assert_eq!(recorded.code.as_deref(), Some(codes::BAD_REQUEST));
    assert_eq!(recorded.cookies, [("a".to_owned(), "1".to_owned())]);
    assert!(recorded.outer.is_none());
}

async fn raw_post(addr: std::net::SocketAddr, path: &str, body: &str) -> String {
    use std::io::{Read, Write};
    let request = format!(
        "POST {path} HTTP/1.1
host: x
cookie: a=1; b
content-length: {}
connection: close

{body}",
        body.len()
    );
    tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn expired_sessions_are_refused_until_a_new_login() {
    let s = setup().await;
    s.server.reply("/e", Reply::Data(json!({"n": 1})));
    s.server.expire_sessions(USER);
    let error = token_json(&s.env, &s.config, &s.session, "/e")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydSessionExpiredError);
    assert_eq!(error.code(), codes::SESSION_EXPIRED);
    let fresh = login(&s.env, &s.config).await.unwrap();
    assert_eq!(
        token_json(&s.env, &s.config, &fresh, "/e").await.unwrap(),
        json!({"n": 1})
    );
    s.server.add_account(Account::new("late@example.com", "p"));
    let late = config(&s.server, "late@example.com", "p");
    assert!(login(&s.env, &late).await.is_ok());
}

#[tokio::test]
async fn the_user_type_rule_is_enforced() {
    let s = setup_with(
        Account::new(USER, PASSWORD).guest(VIN),
        HttpOptions::default(),
    )
    .await;
    s.server
        .reply("/control/x", Reply::Data(json!({"done": true})));
    // The session starts out believing it owns the car: refused with 1001,
    // retried as a guest, and the session learns.
    let cx = Context::new(&s.env, &s.config, &s.session);
    let inner = inner(&s.env, &s.config);
    let reply = post_vehicle_json(cx, "/control/x", &inner, VIN, None, ErrorCodes::default());
    assert_eq!(reply.await.unwrap(), json!({"done": true}));
    assert!(s.session.is_guest(VIN));
    let codes_seen: Vec<_> = s
        .server
        .requests_to("/control/x")
        .into_iter()
        .map(|request| request.code.unwrap())
        .collect();
    assert_eq!(codes_seen, [codes::GUEST_NOT_DECLARED, "0"]);

    // An owner claiming to be a guest is refused with 8003.
    let server = FakeBydServer::start([Account::new(USER, PASSWORD)])
        .await
        .unwrap();
    let config = config(&server, USER, PASSWORD);
    let env = HttpEnv::for_config(&s.pool, &config);
    let session = login(&env, &config).await.unwrap();
    let cx = Context::new(&env, &config, &session);
    let error = post_token_json(
        cx,
        "/e",
        &inner,
        None,
        Some(VIN),
        ErrorCodes::default(),
        Some("1"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), codes::NOT_A_GUEST);

    // And with the rule off, anything goes.
    let lax = FakeBydServer::start([Account::new(USER, PASSWORD)
        .guest(VIN)
        .without_user_type_rule()])
    .await
    .unwrap();
    lax.reply("/e", Reply::Data(json!({})));
    let config = self::config(&lax, USER, PASSWORD);
    let env = HttpEnv::for_config(&s.pool, &config);
    let session = login(&env, &config).await.unwrap();
    assert!(token_json(&env, &config, &session, "/e").await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_concurrent_requests_on_one_environment() {
    let s = Arc::new(setup().await);
    let served = Arc::new(AtomicUsize::new(0));
    let counter = served.clone();
    s.server.on("/e", move |request: &Request| {
        counter.fetch_add(1, Ordering::SeqCst);
        Reply::Data(json!({"random": request.inner_field("random").cloned()}))
            .delayed(Duration::from_millis(5))
    });
    let mut tasks = Vec::new();
    for _ in 0..200 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let cx = Context::new(&s.env, &s.config, &s.session);
            let inner = inner(&s.env, &s.config);
            let reply = post_token_json(cx, "/e", &inner, None, None, ErrorCodes::default(), None)
                .await
                .unwrap();
            assert_eq!(reply["random"], inner["random"]);
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(served.load(Ordering::SeqCst), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_environments_share_one_pool() {
    let accounts: Vec<Account> = (0..200)
        .map(|n| Account::new(&format!("user{n}@example.com"), "p"))
        .collect();
    let server = Arc::new(FakeBydServer::start(accounts).await.unwrap());
    server.on("/e", |request: &Request| {
        Reply::Data(json!({"cookie": request.cookie("account").map(str::to_owned)}))
    });
    // Each login hands the account its own cookie.
    server.on(LOGIN_ENDPOINT, |request: &Request| {
        let cookie = format!("account={}; Path=/", request.username.as_deref().unwrap());
        Reply::Default.with_cookie(&cookie)
    });
    let pool = HttpPool::new();
    let mut tasks = Vec::new();
    for n in 0..200 {
        let (server, pool) = (server.clone(), pool.clone());
        tasks.push(tokio::spawn(async move {
            let config = config(&server, &format!("user{n}@example.com"), "p");
            let env = HttpEnv::for_config(&pool, &config);
            let session = login(&env, &config).await.unwrap();
            let reply = token_json(&env, &config, &session, "/e").await.unwrap();
            assert_eq!(reply, json!({"cookie": format!("user{n}@example.com")}));
            session.user_id().to_owned()
        }));
    }
    let mut users = Vec::new();
    for task in tasks {
        users.push(task.await.unwrap());
    }
    users.sort();
    users.dedup();
    assert_eq!(users.len(), 200);
    assert_eq!(server.requests().len(), 400);

    // One after another, different environments reuse one pool's connections.
    // This is measured on a second server and pool: after a burst of 200
    // connects, a slow machine can still be delivering retried connection
    // attempts to the first server, which would be counted as new ones.
    let quiet = FakeBydServer::start([Account::new("quiet@example.com", "p")])
        .await
        .unwrap();
    let quiet_pool = HttpPool::new();
    for _ in 0..50 {
        let config = config(&quiet, "quiet@example.com", "p");
        let env = HttpEnv::for_config(&quiet_pool, &config);
        login(&env, &config).await.unwrap();
    }
    assert_eq!(quiet.requests().len(), 50);
    assert_eq!(
        quiet.connections(),
        1,
        "fifty logins should share one connection"
    );
}

#[tokio::test]
async fn cookies_do_not_leak_between_environments() {
    let server = FakeBydServer::start([
        Account::new("a@example.com", "p"),
        Account::new("b@example.com", "p"),
    ])
    .await
    .unwrap();
    server.reply("/e", Reply::Data(json!({})));
    server.script(
        LOGIN_ENDPOINT,
        [Reply::Default.with_cookie("account=a; Path=/")],
    );
    let pool = HttpPool::new();
    let a = HttpEnv::new(&pool, server.url());
    let b = HttpEnv::new(&pool, server.url());
    let config_a = config(&server, "a@example.com", "p");
    let config_b = config(&server, "b@example.com", "p");
    let session_a = login(&a, &config_a).await.unwrap();
    let session_b = login(&b, &config_b).await.unwrap();
    token_json(&a, &config_a, &session_a, "/e").await.unwrap();
    token_json(&b, &config_b, &session_b, "/e").await.unwrap();
    let cookies: Vec<_> = server
        .requests()
        .iter()
        .map(|request| {
            (
                request.username.clone().unwrap(),
                request.cookie("account").map(str::to_owned),
            )
        })
        .collect();
    let a_ = |cookie: Option<&str>| ("a@example.com".to_owned(), cookie.map(str::to_owned));
    let b_ = |cookie: Option<&str>| ("b@example.com".to_owned(), cookie.map(str::to_owned));
    assert_eq!(cookies, [a_(None), b_(None), a_(Some("a")), b_(None)]);
    server.clear_requests();
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn https_to_a_plain_server_fails_cleanly() {
    let server = FakeBydServer::start([]).await.unwrap();
    let env = HttpEnv::new(&HttpPool::new(), format!("https://{}", server.addr()));
    let error = env.post_secure("/e", &Map::new()).await.unwrap_err();
    assert_transport(&error, "/e", None);
}

#[test]
fn a_standalone_server_runs_without_a_runtime() {
    let server = FakeBydServer::start_standalone([Account::new(USER, PASSWORD)]).unwrap();
    assert!(format!("{server:?}").contains("127.0.0.1"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let url = server.url();
    runtime.block_on(async {
        let mut config = BydConfig::new(USER, PASSWORD);
        config.base_url = url;
        let env = HttpEnv::for_config(&HttpPool::new(), &config);
        login(&env, &config).await.unwrap();
    });
    server.shutdown_blocking();
    // Another one, shut down from async code, and one simply dropped.
    let server = FakeBydServer::start_standalone([]).unwrap();
    runtime.block_on(server.shutdown());
    drop(FakeBydServer::start_standalone([]).unwrap());
}
