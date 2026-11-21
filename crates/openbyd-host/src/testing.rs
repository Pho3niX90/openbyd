//! The fake BYD server and the MQTT test broker by call name (feature `testing`).
//!
//! Every binding tests itself against these, so none needs its own wrapper
//! around them. [`call`] takes a name and a JSON object and returns JSON; a
//! server or a broker is named by the number its `start` returned.
//!
//! | Call | Arguments | Result |
//! |---|---|---|
//! | `server.start` | `accounts`: a list of accounts | `{"server", "url"}` |
//! | `server.add_account` | `server`, an account's fields | |
//! | `server.reply` | `server`, `endpoint`, `reply` | |
//! | `server.script` | `server`, `endpoint`, `replies` | |
//! | `server.requests` | `server`, `endpoint` (optional) | the requests received |
//! | `server.clear_requests` | `server` | |
//! | `server.expire_sessions` | `server`, `username` | |
//! | `server.user_id` | `server`, `username` | the account's user ID or `null` |
//! | `server.stop` | `server` | |
//! | `broker.start` | | `{"broker", "address", "port"}` |
//! | `broker.push` | `broker`, `session`, `body` | connections reached |
//! | `broker.subscribers` | `broker`, `topic` | connections subscribed |
//! | `broker.open`, `broker.connects` | `broker` | a count |
//! | `broker.refuse_next` | `broker`, `count` | |
//! | `broker.drop_all`, `broker.stop` | `broker` | |
//!
//! An account is `{"username", "password"}` with optional `user_id`,
//! `guest_vins` and `user_type_rule`. A reply is one of `{"data": ...}`,
//! `{"code": "1008", "message": ""}`, `{"outer": ...}`, `{"status": 500,
//! "body": ""}`, `{"drop": true}` or `{"default": true}`, with optional
//! `delay` (seconds) and `cookie` (a `Set-Cookie` header).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use openbyd_client::testing::broker::TestBroker;
use openbyd_client::testing::{Account, FakeBydServer, Reply, Request};
use serde_json::{Value, json};

use crate::args::Args;
use crate::error::HostError;
use crate::runtime;

#[derive(Default)]
struct Registry {
    next: AtomicU64,
    servers: Mutex<HashMap<u64, FakeBydServer>>,
    brokers: Mutex<HashMap<u64, Arc<TestBroker>>>,
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Registry::default)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn number(args: &Args, key: &str) -> Result<u64, HostError> {
    u64::try_from(args.int(key)?).map_err(|_| HostError::value(format!("no {key} of that number")))
}

fn with_server<T>(args: &Args, action: impl FnOnce(&FakeBydServer) -> T) -> Result<T, HostError> {
    let id = number(args, "server")?;
    lock(&registry().servers)
        .get(&id)
        .map(action)
        .ok_or_else(|| HostError::value(format!("no server {id}")))
}

fn broker(args: &Args) -> Result<Arc<TestBroker>, HostError> {
    let id = number(args, "broker")?;
    lock(&registry().brokers)
        .get(&id)
        .cloned()
        .ok_or_else(|| HostError::value(format!("no broker {id}")))
}

fn account(args: &Args) -> Result<Account, HostError> {
    let mut account = Account::new(&args.str("username")?, &args.str("password")?);
    if let Some(user_id) = args.opt_str("user_id")? {
        account = account.with_user_id(&user_id);
    }
    if args.get("guest_vins").is_some() {
        account.guest_vins = args.strings("guest_vins")?.into_iter().collect();
    }
    account.user_type_rule = args.bool_or("user_type_rule", true)?;
    Ok(account)
}

fn reply(value: &Value) -> Result<Reply, HostError> {
    let args = Args::new(value.clone())?;
    let mut reply = if let Some(data) = args.get("data") {
        Reply::Data(data.clone())
    } else if args.get("code").is_some() {
        Reply::Code {
            code: args.str("code")?,
            message: args.opt_str("message")?.unwrap_or_default(),
        }
    } else if let Some(outer) = args.get("outer") {
        Reply::Outer(outer.clone())
    } else if args.get("status").is_some() {
        let status = u16::try_from(args.int("status")?)
            .map_err(|_| HostError::value("status must be an HTTP status"))?;
        Reply::Status(status, args.opt_str("body")?.unwrap_or_default())
    } else if args.bool_or("drop", false)? {
        Reply::Drop
    } else if args.bool_or("default", false)? {
        Reply::Default
    } else {
        return Err(HostError::value(
            "a reply needs data, code, outer, status, drop or default",
        ));
    };
    if let Some(seconds) = args.opt_float("delay")? {
        let delay = Duration::try_from_secs_f64(seconds)
            .map_err(|_| HostError::value("delay must be a finite, non-negative number"))?;
        reply = reply.delayed(delay);
    }
    if let Some(header) = args.opt_str("cookie")? {
        reply = reply.with_cookie(&header);
    }
    Ok(reply)
}

fn list<'a>(args: &'a Args, key: &str) -> Result<&'a Vec<Value>, HostError> {
    args.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| HostError::type_error(format!("{key} must be a list")))
}

fn request_to_json(request: &Request) -> Value {
    let pairs = |pairs: &[(String, String)]| -> Value {
        pairs
            .iter()
            .map(|(name, value)| json!([name, value]))
            .collect()
    };
    json!({
        "endpoint": request.endpoint,
        "headers": pairs(&request.headers),
        "cookies": pairs(&request.cookies),
        "body": request.body,
        "outer": request.outer,
        "inner": request.inner,
        "username": request.username,
        "code": request.code,
        "status": request.status,
    })
}

/// Runs the call `name`. See the module documentation.
pub fn call(name: &str, args: Value) -> Result<Value, HostError> {
    let args = Args::new(args)?;
    let registry = registry();
    let next = || registry.next.fetch_add(1, Ordering::Relaxed) + 1;
    Ok(match name {
        "server.start" => {
            let accounts = match args.get("accounts") {
                None => Vec::new(),
                Some(_) => list(&args, "accounts")?
                    .iter()
                    .map(|fields| account(&Args::new(fields.clone())?))
                    .collect::<Result<Vec<_>, _>>()?,
            };
            let server = FakeBydServer::start_standalone(accounts).map_err(|error| {
                HostError::internal(format!("the fake server did not start: {error}"))
            })?;
            let (id, url) = (next(), server.url());
            lock(&registry.servers).insert(id, server);
            json!({"server": id, "url": url})
        }
        "server.add_account" => {
            let account = account(&args)?;
            with_server(&args, |server| server.add_account(account))?;
            Value::Null
        }
        "server.reply" => {
            let endpoint = args.str("endpoint")?;
            let reply = reply(args.get("reply").unwrap_or(&Value::Null))?;
            with_server(&args, |server| server.reply(&endpoint, reply))?;
            Value::Null
        }
        "server.script" => {
            let endpoint = args.str("endpoint")?;
            let replies = list(&args, "replies")?
                .iter()
                .map(reply)
                .collect::<Result<Vec<_>, _>>()?;
            with_server(&args, |server| server.script(&endpoint, replies))?;
            Value::Null
        }
        "server.requests" => {
            let endpoint = args.opt_str("endpoint")?;
            let requests = with_server(&args, |server| match &endpoint {
                Some(endpoint) => server.requests_to(endpoint),
                None => server.requests(),
            })?;
            requests.iter().map(request_to_json).collect()
        }
        "server.clear_requests" => {
            with_server(&args, FakeBydServer::clear_requests)?;
            Value::Null
        }
        "server.expire_sessions" => {
            let username = args.str("username")?;
            with_server(&args, |server| server.expire_sessions(&username))?;
            Value::Null
        }
        "server.user_id" => {
            let username = args.str("username")?;
            json!(with_server(&args, |server| server.user_id(&username))?)
        }
        "server.stop" => {
            let id = number(&args, "server")?;
            let server = lock(&registry.servers).remove(&id);
            if let Some(server) = server {
                server.shutdown_blocking();
            }
            Value::Null
        }
        "broker.start" => {
            let broker = runtime::block_on(TestBroker::start());
            let (id, port) = (next(), broker.port());
            lock(&registry.brokers).insert(id, broker);
            json!({"broker": id, "address": format!("127.0.0.1:{port}"), "port": port})
        }
        "broker.push" => {
            let session = args.object("session")?;
            let field = |name: &str| -> Result<&str, HostError> {
                session
                    .get(name)
                    .and_then(Value::as_str)
                    .ok_or_else(|| HostError::type_error(format!("session needs {name}")))
            };
            let topic = format!("oversea/res/{}", field("user_id")?);
            let key = openbyd_protocol::hashing::md5_hex(field("encry_token")?);
            let body = args.get("body").cloned().unwrap_or(Value::Null);
            json!(broker(&args)?.publish_sealed(&topic, &body, &key))
        }
        "broker.subscribers" => json!(broker(&args)?.subscribers(&args.str("topic")?)),
        "broker.open" => json!(broker(&args)?.open()),
        "broker.connects" => json!(broker(&args)?.stats().connects),
        "broker.refuse_next" => {
            let count = usize::try_from(args.int("count")?)
                .map_err(|_| HostError::value("count must not be negative"))?;
            broker(&args)?.refuse_next(count);
            Value::Null
        }
        "broker.drop_all" => {
            broker(&args)?.drop_all();
            Value::Null
        }
        "broker.stop" => {
            let id = number(&args, "broker")?;
            let broker = lock(&registry.brokers).remove(&id);
            if let Some(broker) = broker {
                // Its tasks stop when the last handle goes, inside the runtime.
                let _entered = runtime::handle().enter();
                drop(broker);
            }
            Value::Null
        }
        other => return Err(HostError::value(format!("no testing call {other:?}"))),
    })
}
