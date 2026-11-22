//! A scripted [`Env`] and the trace format of the flow parity tests.
//!
//! A flow's parity call runs it against [`ScriptedEnv`]: replies come from a
//! script, the clock only moves when the flow sleeps, and random values are
//! derived from a seed. The call returns a *trace* of what the flow did,
//! which `parity/api_support.py` produces from pyBYD under the same rules:
//!
//! ```json
//! {
//!   "steps": [{"post": "/endpoint", "outer": {...}}, {"sleep": 1.0}],
//!   "result": ...,               // or "error": {"class", "code", "endpoint", "status_code"}
//!   "guest_vins": {"VIN": true},
//!   "user_type_settled": [["VIN", "/endpoint"]],
//!   "unused_replies": 0
//! }
//! ```
//!
//! - The wall clock is `now_ms` plus the total time slept so far
//!   (`now_ms + int(slept * 1000)`); the monotonic clock is
//!   `now_ms / 1000 + slept`.
//! - The n-th random value (from 0) is `MD5("<seed>:<n>")`.
//! - A reply is `{"reply": <outer reply>}` or
//!   `{"transport_error": {"status_code": <int or null>}}`. Running out of
//!   replies fails the request with [`ErrorKind::ScriptExhausted`]. Either
//!   may carry `"date"`, the HTTP `Date` header it came with: the server
//!   clock offset follows it as pyBYD's `SecureTransport` follows the real
//!   header, read against the scripted wall clock.
//!
//! For the client parity tests ([`crate::client`]) it is also a
//! [`ClientEnv`], with a timeline of MQTT deliveries ([`ScriptedMqtt`]):
//!
//! - each delivery has a time `at` (seconds of `slept`); it is delivered at
//!   the first *delivery point* at or after that time: after a sleep, while
//!   a [`ClientEnv::wait_for`] waits (up to its deadline), and when the test
//!   calls [`ScriptedEnv::advance`] or [`ScriptedEnv::drain`]. Delivering
//!   moves the clock forward to `at` when that is later. Each delivery is the
//!   step `{"mqtt": index}`, or `{"mqtt_lost": index}` when no channel is
//!   running to receive it;
//! - a wait is the step `{"mqtt_wait": timeout}`; one that times out moves
//!   the clock to its deadline and adds `{"mqtt_timeout": timeout}`;
//! - a background task ([`ClientEnv::spawn`]) runs to its end right after
//!   the delivery that started it;
//! - starting the channel is `{"mqtt_start": {"bootstrap", "key",
//!   "keepalive"}}` and succeeds unless the next entry of the start script
//!   is `false`; stopping is `{"mqtt_stop": null}`, a key change
//!   `{"mqtt_key": key}`;
//! - the n-th `uniform(low, high)` (from 0) is `low + (high - low) * u`
//!   with `u` the first 13 hex digits of `MD5("<seed>:uniform:<n>")` over
//!   `16**13`.
//!
//! Not part of the public API.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::pin;
use std::sync::{Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Context as TaskContext, Poll, Waker};

use openbyd_protocol::config::BydConfig;
use openbyd_protocol::envelope::clock::sync_clock_from_header;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::hashing::md5_hex;
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::mqtt::{MqttBootstrap, MqttEvent};
use openbyd_protocol::parity::{Args, Outcome, ParityError};
use openbyd_protocol::session::Session;
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::env::{BoxFuture, ClientEnv, Env, MqttSink};

/// One scripted answer to a `post_secure`.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// The decoded outer reply.
    Outer(Value),
    /// The transport fails with a `BydTransportError` carrying this HTTP status.
    TransportError(Option<u16>),
}

impl Reply {
    /// Reads one script entry.
    pub fn from_json(entry: &Value) -> Result<Self, ParityError> {
        if let Some(reply) = entry.get("reply") {
            return Ok(Reply::Outer(reply.clone()));
        }
        if let Some(failure) = entry.get("transport_error") {
            let status = match failure.get("status_code").unwrap_or(&Value::Null) {
                Value::Null => None,
                status => Some(
                    status
                        .as_u64()
                        .and_then(|status| u16::try_from(status).ok())
                        .ok_or_else(|| {
                            ParityError::bad_arguments("status_code must be a small integer")
                        })?,
                ),
            };
            return Ok(Reply::TransportError(status));
        }
        Err(ParityError::bad_arguments(
            "a script entry is {\"reply\": ...} or {\"transport_error\": ...}",
        ))
    }
}

/// What one scripted MQTT delivery carries.
#[derive(Debug, Clone, PartialEq)]
pub enum MqttDelivery {
    /// A decrypted event.
    Event(MqttEvent),
    /// A message that did not decrypt with an unconfirmed key.
    DecryptError,
    /// A refused CONNACK, the n-th in a row.
    ConnackRefused(u32),
    /// An accepted CONNACK.
    Connected,
}

/// One entry of the MQTT timeline: what arrives, and when.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptedMqtt {
    /// Its position in the script, as the trace names it.
    pub index: usize,
    /// Seconds of `slept` at which it arrives.
    pub at: f64,
    /// What arrives.
    pub delivery: MqttDelivery,
}

impl ScriptedMqtt {
    /// Reads the timeline: null, or a list of `{"at": seconds, ...}`
    /// objects with one of `"event": {event, vin, topic, payload}`,
    /// `"decrypt_error": true`, `"connack_refused": n` or `"connected": true`.
    pub fn from_json(entries: &Value) -> Result<Vec<Self>, ParityError> {
        let entries = match entries {
            Value::Null => return Ok(Vec::new()),
            Value::Array(entries) => entries,
            _ => return Err(ParityError::bad_arguments("mqtt must be a list")),
        };
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let at = entry.get("at").and_then(Value::as_f64).ok_or_else(|| {
                    ParityError::bad_arguments("an MQTT entry needs a numeric at")
                })?;
                let delivery = if let Some(event) = entry.get("event") {
                    MqttDelivery::Event(
                        MqttEvent::parse(event, &Value::Null)
                            .map_err(|_| ParityError::bad_arguments("not an MQTT event"))?,
                    )
                } else if entry.get("decrypt_error").is_some() {
                    MqttDelivery::DecryptError
                } else if let Some(count) = entry.get("connack_refused") {
                    MqttDelivery::ConnackRefused(
                        count
                            .as_u64()
                            .and_then(|count| u32::try_from(count).ok())
                            .ok_or_else(|| {
                                ParityError::bad_arguments("connack_refused is a count")
                            })?,
                    )
                } else if entry.get("connected").is_some() {
                    MqttDelivery::Connected
                } else {
                    return Err(ParityError::bad_arguments("unknown MQTT entry"));
                };
                Ok(Self {
                    index,
                    at,
                    delivery,
                })
            })
            .collect()
    }
}

/// The `Date` header of each script entry (`"date"`), if any.
pub fn dates_from_json(entries: &[Value]) -> Vec<Option<String>> {
    entries
        .iter()
        .map(|entry| entry.get("date").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

#[derive(Default)]
struct State {
    slept: f64,
    draws: u64,
    uniform_draws: u64,
    replies: VecDeque<Reply>,
    /// The `Date` header that comes with each reply.
    dates: VecDeque<Option<String>>,
    server_time_offset_ms: i64,
    steps: Vec<Value>,
    mqtt: VecDeque<ScriptedMqtt>,
    mqtt_starts: VecDeque<bool>,
    running: bool,
    sink: Option<Weak<dyn MqttSink>>,
    spawned: VecDeque<BoxFuture<'static, ()>>,
    stalled: bool,
}

/// An [`Env`] that answers from a script and records every request and sleep.
pub struct ScriptedEnv {
    now_ms: i64,
    seed: String,
    state: Mutex<State>,
}

impl fmt::Debug for ScriptedEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state();
        f.debug_struct("ScriptedEnv")
            .field("now_ms", &self.now_ms)
            .field("seed", &self.seed)
            .field("slept", &state.slept)
            .field("replies", &state.replies.len())
            .field("mqtt", &state.mqtt.len())
            .finish_non_exhaustive()
    }
}

impl ScriptedEnv {
    /// An environment whose clock starts at `now_ms` and whose random values
    /// derive from `seed`.
    pub fn new(now_ms: i64, seed: &str, replies: Vec<Reply>) -> Self {
        Self {
            now_ms,
            seed: seed.to_owned(),
            state: Mutex::new(State {
                replies: replies.into(),
                ..State::default()
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the flow did so far: posts and sleeps, in order.
    pub fn steps(&self) -> Vec<Value> {
        self.state().steps.clone()
    }

    /// Adds a step of the test's own to the trace, such as a scripted MQTT
    /// wait or callback, so its place among the posts and sleeps is compared.
    pub fn record(&self, step: Value) {
        self.state().steps.push(step);
    }

    /// Gives the replies `Date` headers, in order (see [`dates_from_json`]).
    pub fn with_dates(self, dates: Vec<Option<String>>) -> Self {
        self.state().dates = dates.into();
        self
    }

    /// Sets the server clock offset, as earlier replies would have.
    pub fn set_server_time_offset_ms(&self, offset_ms: i64) {
        self.state().server_time_offset_ms = offset_ms;
    }

    /// How many replies the flow did not use.
    pub fn unused_replies(&self) -> usize {
        self.state().replies.len()
    }

    /// Adds the MQTT timeline and the start script (one entry per start:
    /// `false` makes that start fail; starts beyond the script succeed).
    pub fn with_mqtt(self, mqtt: Vec<ScriptedMqtt>, starts: Vec<bool>) -> Self {
        {
            let mut state = self.state();
            state.mqtt = mqtt.into();
            state.mqtt_starts = starts.into();
        }
        self
    }

    /// Makes the MQTT channel run with `sink`, without a step: a parity
    /// call's starting state.
    pub fn install_mqtt(&self, sink: Weak<dyn MqttSink>) {
        let mut state = self.state();
        state.running = true;
        state.sink = Some(sink);
    }

    /// How many MQTT deliveries are still to come.
    pub fn unused_mqtt(&self) -> usize {
        self.state().mqtt.len()
    }

    /// Whether a background task waited on something the script cannot give.
    pub fn stalled(&self) -> bool {
        self.state().stalled
    }

    /// Lets `seconds` pass without a sleep, as time passes between two calls
    /// (the step `{"advance": seconds}`), delivering what arrives meanwhile.
    pub fn advance(&self, seconds: f64) {
        let limit = {
            let mut state = self.state();
            state.slept += seconds;
            state.steps.push(json!({ "advance": seconds }));
            state.slept
        };
        while self.deliver_one(limit) {}
    }

    /// Delivers everything still to come, in order.
    pub fn drain(&self) {
        while self.deliver_one(f64::INFINITY) {}
    }

    /// Delivers the next MQTT entry if it arrives by `limit`, then runs the
    /// background tasks it started. `false` when nothing was due.
    fn deliver_one(&self, limit: f64) -> bool {
        let (entry, sink) = {
            let mut state = self.state();
            if state.mqtt.front().is_none_or(|entry| entry.at > limit) {
                return false;
            }
            let entry = state.mqtt.pop_front().expect("checked above");
            if entry.at > state.slept {
                state.slept = entry.at;
            }
            let sink = if state.running {
                state.sink.as_ref().and_then(Weak::upgrade)
            } else {
                None
            };
            let kind = if sink.is_some() { "mqtt" } else { "mqtt_lost" };
            state.steps.push(json!({ kind: entry.index }));
            (entry, sink)
        };
        if let Some(sink) = sink {
            match &entry.delivery {
                MqttDelivery::Event(event) => sink.on_event(event),
                MqttDelivery::DecryptError => sink.on_decrypt_error(),
                MqttDelivery::ConnackRefused(count) => sink.on_connack_refused(*count),
                MqttDelivery::Connected => sink.on_connected(),
            }
            self.run_spawned();
        }
        true
    }

    /// Runs the queued background tasks to their end, in order.
    fn run_spawned(&self) {
        loop {
            let Some(task) = self.state().spawned.pop_front() else {
                return;
            };
            if block_on(task).is_none() {
                self.state().stalled = true;
            }
        }
    }
}

impl Env for ScriptedEnv {
    fn now_ms(&self) -> i64 {
        self.now_ms + (self.state().slept * 1000.0) as i64
    }

    fn monotonic(&self) -> f64 {
        self.now_ms as f64 / 1000.0 + self.state().slept
    }

    fn random_bytes(&self) -> [u8; 16] {
        let draw = {
            let mut state = self.state();
            state.draws += 1;
            state.draws - 1
        };
        let hex = md5_hex(&format!("{}:{draw}", self.seed));
        let mut bytes = [0u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte =
                u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).expect("md5_hex gives hex");
        }
        bytes
    }

    async fn sleep(&self, seconds: f64) {
        let limit = {
            let mut state = self.state();
            state.slept += seconds;
            state.steps.push(json!({ "sleep": seconds }));
            state.slept
        };
        while self.deliver_one(limit) {}
    }

    async fn post_secure(
        &self,
        endpoint: &str,
        outer: &Map<String, Value>,
    ) -> Result<Value, Error> {
        let now_ms = self.now_ms();
        let mut state = self.state();
        state
            .steps
            .push(json!({ "post": endpoint, "outer": outer }));
        if !state.replies.is_empty()
            && let Some(Some(date)) = state.dates.pop_front()
        {
            let offset = state.server_time_offset_ms;
            state.server_time_offset_ms = sync_clock_from_header(Some(&date), now_ms, offset)
                .map_err(|_| crate::http::clock_overflow(endpoint))?;
        }
        match state.replies.pop_front() {
            None => Err(Error::new(
                ErrorKind::ScriptExhausted,
                format!("no scripted reply left for {endpoint}"),
            )),
            Some(Reply::Outer(reply)) => Ok(reply),
            Some(Reply::TransportError(status)) => Err(Error::transport(
                format!("scripted transport error from {endpoint}"),
                status,
                endpoint,
            )),
        }
    }

    fn server_time_offset_ms(&self) -> i64 {
        self.state().server_time_offset_ms
    }
}

impl ClientEnv for ScriptedEnv {
    async fn wait_for<'a, T: Send + 'a>(
        &'a self,
        mut future: BoxFuture<'a, T>,
        timeout: f64,
    ) -> Option<T> {
        let deadline = {
            let mut state = self.state();
            state.steps.push(json!({ "mqtt_wait": timeout }));
            state.slept + timeout
        };
        let mut cx = TaskContext::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return Some(value);
            }
            if !self.deliver_one(deadline) {
                break;
            }
        }
        let mut state = self.state();
        if deadline > state.slept {
            state.slept = deadline;
        }
        state.steps.push(json!({ "mqtt_timeout": timeout }));
        None
    }

    fn spawn(&self, task: BoxFuture<'static, ()>) {
        self.state().spawned.push_back(task);
    }

    fn uniform(&self, low: f64, high: f64) -> f64 {
        let draw = {
            let mut state = self.state();
            state.uniform_draws += 1;
            state.uniform_draws - 1
        };
        let hex = md5_hex(&format!("{}:uniform:{draw}", self.seed));
        let unit =
            u64::from_str_radix(&hex[..13], 16).expect("md5_hex gives hex") as f64 / 16f64.powi(13);
        low + (high - low) * unit
    }

    async fn start_mqtt(
        &self,
        bootstrap: &MqttBootstrap,
        decrypt_key: &str,
        keepalive: i64,
        sink: Weak<dyn MqttSink>,
    ) -> Result<(), Error> {
        let mut state = self.state();
        state.steps.push(json!({"mqtt_start": {
            "bootstrap": bootstrap.dump(),
            "key": decrypt_key,
            "keepalive": keepalive,
        }}));
        if !state.mqtt_starts.pop_front().unwrap_or(true) {
            return Err(Error::new(
                ErrorKind::BydError,
                "scripted MQTT start failure",
            ));
        }
        state.running = true;
        state.sink = Some(sink);
        Ok(())
    }

    fn stop_mqtt(&self) {
        let mut state = self.state();
        state.steps.push(json!({ "mqtt_stop": null }));
        state.running = false;
        state.sink = None;
    }

    fn mqtt_running(&self) -> bool {
        self.state().running
    }

    fn update_mqtt_key(&self, decrypt_key: &str) {
        self.state().steps.push(json!({ "mqtt_key": decrypt_key }));
    }
}

/// Runs a future that never waits on anything outside the script, as every
/// flow does against [`ScriptedEnv`]. `None` if it is still pending after
/// many polls, which means it waited on something the script cannot give.
pub fn block_on<F: Future>(future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    let mut cx = TaskContext::from_waker(Waker::noop());
    for _ in 0..1000 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return Some(value);
        }
    }
    None
}

/// The trace form of an error: its class and the attributes pyBYD's
/// exception carries (`null` where the class has no such attribute).
pub fn describe(error: &Error) -> Value {
    let api = error.is_a(ErrorKind::BydApiError);
    let transport = error.is_a(ErrorKind::BydTransportError);
    json!({
        "class": error.class_name(),
        "code": if api { json!(error.code()) } else { Value::Null },
        "endpoint": if api || transport { json!(error.endpoint()) } else { Value::Null },
        "status_code": if transport { json!(error.status_code()) } else { Value::Null },
    })
}

/// Everything a flow parity call sets up: the scripted environment, the
/// config, the session with its guest state, and the flow's own arguments.
///
/// The call's arguments are `config` (`BydConfig` keyword arguments),
/// `session` (`Session` keyword arguments), `guest_vins` (VIN to bool),
/// `user_type_settled` (a list of `[vin, endpoint]`), `now_ms`, `seed`,
/// `replies` (the script) and `args` (the function's own arguments).
pub struct Scenario {
    env: ScriptedEnv,
    config: BydConfig,
    session: Session,
    args: Value,
}

impl Scenario {
    /// Reads a flow parity call's arguments.
    pub fn from_args(args: &Value) -> Result<Self, ParityError> {
        let call = Args(args);
        let entries = call
            .value("replies")
            .as_array()
            .ok_or_else(|| ParityError::bad_arguments("replies must be a list"))?;
        let replies = entries
            .iter()
            .map(Reply::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let seed = call.str("seed")?;
        let env = ScriptedEnv::new(call.i64("now_ms")?, seed, replies)
            .with_dates(dates_from_json(entries));
        // The transport's clock offset before the flow (default 0).
        match call.value("server_time_offset_ms") {
            Value::Null => {}
            _ => env.set_server_time_offset_ms(call.i64("server_time_offset_ms")?),
        }
        let object = |key: &str| {
            call.value(key)
                .as_object()
                .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be an object")))
        };
        let config = BydConfig::from_kwargs(object("config")?)?;
        let session = Session::from_kwargs(object("session")?, env.monotonic())?;
        if let Some(guests) = call.value("guest_vins").as_object() {
            for (vin, guest) in guests {
                let guest = guest
                    .as_bool()
                    .ok_or_else(|| ParityError::bad_arguments("guest_vins values are booleans"))?;
                session.set_guest(vin, guest);
            }
        }
        for pair in call
            .value("user_type_settled")
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            match (pair[0].as_str(), pair[1].as_str()) {
                (Some(vin), Some(endpoint)) => session.settle_user_type(vin, endpoint),
                _ => {
                    return Err(ParityError::bad_arguments(
                        "user_type_settled holds [vin, endpoint] pairs",
                    ));
                }
            }
        }
        Ok(Self {
            env,
            config,
            session,
            args: call.value("args").clone(),
        })
    }

    /// The context flows run in.
    pub fn context(&self) -> Context<'_, ScriptedEnv> {
        Context::new(&self.env, &self.config, &self.session)
    }

    /// The flow's own arguments.
    pub fn args(&self) -> Args<'_> {
        Args(&self.args)
    }

    /// Runs `flow` to the end and returns its trace, with the result turned
    /// into JSON by `to_json`.
    pub fn run<T>(
        &self,
        flow: impl Future<Output = Result<T, Error>>,
        to_json: impl FnOnce(T) -> Value,
    ) -> Outcome {
        let outcome = block_on(flow).ok_or_else(|| {
            ParityError::new(
                "Stalled",
                "the flow waited on something the script cannot give",
            )
        })?;
        let guests: BTreeMap<String, bool> = self.session.guest_vins().into_iter().collect();
        let mut settled: Vec<(String, String)> =
            self.session.user_type_settled().into_iter().collect();
        settled.sort();
        let mut trace = Map::new();
        trace.insert("steps".into(), Value::Array(self.env.steps()));
        match outcome {
            Ok(value) => trace.insert("result".into(), to_json(value)),
            Err(error) => trace.insert("error".into(), describe(&error)),
        };
        trace.insert("guest_vins".into(), json!(guests));
        trace.insert("user_type_settled".into(), json!(settled));
        trace.insert("unused_replies".into(), json!(self.env.unused_replies()));
        Ok(Value::Object(trace))
    }
}

/// A list of strings, or `None` for null: a code set argument such as
/// `not_supported_codes`.
pub fn code_set(value: &Value) -> Result<Option<Vec<String>>, ParityError> {
    match value {
        Value::Null => Ok(None),
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(|| ParityError::bad_arguments("a code set is a list of strings")),
        _ => Err(ParityError::bad_arguments("a code set is a list or null")),
    }
}

/// An `extra_code_map` argument: a list of `[codes, "ClassName"]`, standing
/// for pyBYD's `{frozenset(codes): Class}`. A set equal to an earlier one
/// keeps the earlier position and takes the later class, as a dict does.
pub fn code_map(value: &Value) -> Result<Vec<(Vec<String>, ErrorKind)>, ParityError> {
    let mut map: Vec<(Vec<String>, ErrorKind)> = Vec::new();
    for entry in value.as_array().map(Vec::as_slice).unwrap_or_default() {
        let codes = code_set(&entry[0])?.unwrap_or_default();
        let kind = entry[1]
            .as_str()
            .and_then(ErrorKind::from_class_name)
            .ok_or_else(|| ParityError::bad_arguments("unknown error class"))?;
        let same = |other: &Vec<String>| {
            codes.iter().all(|code| other.contains(code))
                && other.iter().all(|code| codes.contains(code))
        };
        match map.iter_mut().find(|(other, _)| same(other)) {
            Some(existing) => existing.1 = kind,
            None => map.push((codes, kind)),
        }
    }
    Ok(map)
}

/// Borrowed views of [`code_set`] and [`code_map`] results, in the shape
/// [`crate::api::common::ErrorCodes`] holds.
pub fn borrow_codes(codes: &[String]) -> Vec<&str> {
    codes.iter().map(String::as_str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_and_randomness_follow_the_rules() {
        let env = ScriptedEnv::new(1_000, "s", vec![]);
        assert_eq!(env.now_ms(), 1_000);
        assert_eq!(env.monotonic(), 1.0);
        assert!(block_on(env.sleep(0.25)).is_some());
        assert_eq!(env.now_ms(), 1_250);
        assert_eq!(env.monotonic(), 1.25);
        let first = env.random_bytes();
        assert_ne!(first, env.random_bytes());
        assert_eq!(
            openbyd_protocol::api::common::random_hex(&first),
            md5_hex("s:0")
        );
        assert_eq!(env.steps(), vec![json!({"sleep": 0.25})]);
    }

    #[test]
    fn replies_are_used_in_order() {
        let env = ScriptedEnv::new(
            0,
            "s",
            vec![
                Reply::Outer(json!({"code": "0"})),
                Reply::TransportError(Some(502)),
            ],
        );
        let outer = Map::new();
        assert_eq!(
            block_on(env.post_secure("/a", &outer)).unwrap().unwrap(),
            json!({"code": "0"})
        );
        let error = block_on(env.post_secure("/b", &outer))
            .unwrap()
            .unwrap_err();
        assert_eq!(
            describe(&error),
            json!({"class": "BydTransportError", "code": null, "endpoint": "/b", "status_code": 502})
        );
        let error = block_on(env.post_secure("/c", &outer))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ScriptExhausted);
        assert_eq!(
            describe(&error),
            json!({"class": "ScriptExhausted", "code": null, "endpoint": null, "status_code": null})
        );
        assert_eq!(env.steps().len(), 3);
        assert_eq!(env.unused_replies(), 0);
    }

    #[test]
    fn a_pending_future_is_reported() {
        assert!(block_on(std::future::pending::<()>()).is_none());
    }

    #[test]
    fn script_entries_are_checked() {
        assert_eq!(
            Reply::from_json(&json!({"transport_error": {}})).unwrap(),
            Reply::TransportError(None)
        );
        for bad in [
            json!({}),
            json!({"transport_error": {"status_code": "x"}}),
            json!({"transport_error": {"status_code": 70000}}),
        ] {
            assert_eq!(Reply::from_json(&bad).unwrap_err().kind, "BadArguments");
        }
    }

    #[test]
    fn scenario_arguments_are_checked() {
        let good = json!({
            "config": {"username": "user@example.com", "password": "p"},
            "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
            "now_ms": 5, "seed": "x", "replies": [], "args": {},
        });
        let scenario = Scenario::from_args(&good).unwrap();
        assert_eq!(scenario.session.created_at(), 0.005);
        let with = |key: &str, value: Value| {
            let mut args = good.clone();
            args[key] = value;
            Scenario::from_args(&args).err().map(|error| error.kind)
        };
        assert_eq!(with("replies", json!(1)).unwrap(), "BadArguments");
        assert_eq!(with("replies", json!([{}])).unwrap(), "BadArguments");
        assert_eq!(with("config", json!([])).unwrap(), "BadArguments");
        assert_eq!(with("config", json!({})).unwrap(), "ValidationError");
        assert_eq!(with("guest_vins", json!({"V": 1})).unwrap(), "BadArguments");
        assert_eq!(
            with("user_type_settled", json!([[1, "/e"]])).unwrap(),
            "BadArguments"
        );
        assert!(with("user_type_settled", json!([["V", "/e"]])).is_none());
        let stalled = scenario.run(std::future::pending::<Result<(), Error>>(), |_| Value::Null);
        assert_eq!(stalled.unwrap_err().kind, "Stalled");
    }

    #[test]
    fn code_arguments_are_checked() {
        assert_eq!(code_set(&json!(null)).unwrap(), None);
        assert!(code_set(&json!([1])).is_err());
        assert!(code_set(&json!("1001")).is_err());
        let map = code_map(&json!([
            [["1", "2"], "BydApiError"],
            [["3"], "BydRateLimitError"],
            [["2", "1"], "BydServiceBusyError"],
        ]))
        .unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map[0].1, ErrorKind::BydServiceBusyError);
        assert!(code_map(&json!([[["1"], "Nope"]])).is_err());
        assert_eq!(borrow_codes(&["a".to_owned()]), vec!["a"]);
    }
}
