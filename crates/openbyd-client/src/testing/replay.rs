//! Replay: the fake BYD server and the test broker answering a whole fleet
//! from a scenario file (`bench/replay/SCENARIO.md`), on a shared replay
//! clock with a speed factor (feature `testing`).
//!
//! [`ReplayServer`] accepts a login for every scenario account
//! (`<account>@example.com` / `secret`), answers each account's requests from
//! its cars' state at the current replay time ([`templates`] says which raw
//! field carries which scenario key), answers the recorded errors, expires
//! sessions at a `1005`, and has the broker publish each recorded push at its
//! time, sealed with the key of the account's newest session.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use openbyd_client::testing::replay::{ReplayOptions, ReplayServer, Scenario};
//!
//! let scenario = Scenario::load("bench/replay/samples/demo.jsonl".as_ref())?;
//! let replay = ReplayServer::start(scenario, ReplayOptions { manual: true, ..Default::default() }).await?;
//! replay.set_time(3600.0)?;
//! // ... point clients at replay.url(); pushes go to replay.broker_address() ...
//! replay.shutdown().await;
//! # Ok(()) }
//! ```
//!
//! What each endpoint answers:
//!
//! | Endpoint | Reply |
//! |---|---|
//! | `/app/account/login` | a login, as [`FakeBydServer`] does |
//! | `/app/account/getAllListByUserId` | the account's cars ([`Templates::vehicle`]) |
//! | `/app/emqAuth/getEmqBrokerIp` | the test broker's address |
//! | `/vehicleInfo/vehicle/vehicleRealTimeRequest` | a request serial |
//! | `/vehicleInfo/vehicle/vehicleRealTimeResult` | the realtime reply, ready at the first poll ([`Templates::realtime`]) |
//! | `/control/getGpsInfo` | a request serial |
//! | `/control/getGpsInfoResult` | the position ([`Templates::gps`]) |
//! | `/control/smartCharge/homePage` | charging status and schedule ([`Templates::charging`]) |
//! | `/control/getStatusNow` | climate ([`Templates::hvac`]) |
//! | `/vehicleInfo/vehicle/getEnergyConsumption` | energy use ([`Templates::energy`]) |
//! | `/vehicle/vehicleswitch/getLatestConfig` | capabilities per requested VIN of the account |
//! | `/control/getBookingList`, `/vehicle/vehicleswitch/getPushSwitchState`, `/control/smartCharge/getAcCurrentLimit`, `/app/messageCenter/getUnReadMessageList` | a fixed corpus payload |
//!
//! Anything else is answered as the fake server answers an endpoint without
//! a handler (code 9004). A request naming a VIN of another account gets
//! 1001. While an `error` line is in force, `1008` answers every request that
//! names the car; any other code answers the two realtime endpoints.

pub mod clock;
pub mod scenario;
pub mod templates;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use openbyd_protocol::hashing::md5_hex;
use serde_json::{Map, Value, json};
use tokio::task::JoinHandle;

pub use clock::{ReplayClock, Started};
pub use scenario::{CarState, Energy, Scenario, ScenarioError};
pub use templates::{Moment, Templates};

use super::broker::{BrokerStats, TestBroker};
use super::{Account, FakeBydServer, Reply, Request, ServerHandle, ServerStats};
use scenario::{EventKind, PASSWORD};
use templates::CarTemplates;

const LOGIN: &str = "/app/account/login";
const VEHICLES: &str = "/app/account/getAllListByUserId";
const BROKER_LOOKUP: &str = "/app/emqAuth/getEmqBrokerIp";
const REALTIME_TRIGGER: &str = "/vehicleInfo/vehicle/vehicleRealTimeRequest";
const REALTIME_RESULT: &str = "/vehicleInfo/vehicle/vehicleRealTimeResult";
const GPS_TRIGGER: &str = "/control/getGpsInfo";
const GPS_RESULT: &str = "/control/getGpsInfoResult";
const CHARGING: &str = "/control/smartCharge/homePage";
const HVAC: &str = "/control/getStatusNow";
const ENERGY: &str = "/vehicleInfo/vehicle/getEnergyConsumption";
const LATEST_CONFIG: &str = "/vehicle/vehicleswitch/getLatestConfig";

/// The code BYD answers for a VIN the account cannot reach.
const NOT_YOURS: &str = "1001";
/// The busy code, which answers every read of the car.
const BUSY: &str = "1008";

/// What reply timestamps count from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Epoch {
    /// The scenario header's `start`: the same request at the same `t` gets
    /// the same reply.
    #[default]
    Scenario,
    /// The wall time the replay started at minus `start_at`, for a client
    /// that compares BYD's times with its own clock.
    Now,
}

impl Epoch {
    /// `"scenario"` or `"now"`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "scenario" => Some(Epoch::Scenario),
            "now" => Some(Epoch::Now),
            _ => None,
        }
    }
}

/// The repository's corpus directory, `parity/corpus`, as this crate was
/// built from it.
pub fn default_corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parity/corpus")
}

/// How to run a replay.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// The corpus directory the reply templates come from; `None` for the
    /// built-in payloads only. Default: [`default_corpus`].
    pub corpus: Option<PathBuf>,
    /// Scenario seconds per wall second (wall clock only).
    pub speed: f64,
    /// The scenario time the clock starts from.
    pub start_at: f64,
    /// What reply timestamps count from.
    pub epoch: Epoch,
    /// Added to every reply, logins included.
    pub delay: Duration,
    /// The HTTP port; 0 for an ephemeral one.
    pub port: u16,
    /// The MQTT port; 0 for an ephemeral one.
    pub mqtt_port: u16,
    /// A manual clock ([`ReplayClock::manual`]) instead of wall time.
    pub manual: bool,
    /// Whether the server and broker record every request and packet (off
    /// for a long run; the counters are kept either way).
    pub record: bool,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            corpus: Some(default_corpus()),
            speed: 1.0,
            start_at: 0.0,
            epoch: Epoch::Scenario,
            delay: Duration::ZERO,
            port: 0,
            mqtt_port: 0,
            manual: false,
            record: false,
        }
    }
}

/// When a replay started ([`ReplayServer::start_clock`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StartInfo {
    /// The clock's start.
    pub started: Started,
    /// The epoch reply timestamps count from, in milliseconds.
    pub epoch_unix_ms: i64,
}

impl StartInfo {
    /// `{"wall_unix_ms", "start_at", "speed", "epoch_unix_ms"}`.
    pub fn to_json(&self) -> Value {
        json!({
            "wall_unix_ms": self.started.wall_unix_ms,
            "start_at": self.started.start_at,
            "speed": self.started.speed,
            "epoch_unix_ms": self.epoch_unix_ms,
        })
    }
}

/// The replay's own counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayCounters {
    /// Requests answered with a scenario error, by code.
    pub errors_by_code: BTreeMap<String, u64>,
    /// `1005` events fired.
    pub sessions_expired: u64,
    /// Pushes that reached at least one connection.
    pub pushes_sent: u64,
    /// Pushes with no session or no subscriber to reach.
    pub pushes_missed: u64,
}

/// Every counter of a replay ([`ReplayServer::stats`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayStats {
    /// The fake server's.
    pub server: ServerStats,
    /// The broker's.
    pub broker: BrokerStats,
    /// The replay's.
    pub replay: ReplayCounters,
    /// Accounts in the scenario.
    pub accounts: usize,
    /// Cars in the scenario.
    pub cars: usize,
}

impl ReplayStats {
    /// The JSON the `stats` command prints: `{"requests", "by_endpoint",
    /// "by_code", "connections", "logins", "mqtt": {"open", "connects",
    /// "subscriptions", "pushes_sent", "pushes_missed"}, "replay":
    /// {"errors_by_code", "sessions_expired", "accounts", "cars"}}`.
    pub fn to_json(&self) -> Value {
        json!({
            "requests": self.server.requests,
            "by_endpoint": self.server.by_endpoint,
            "by_code": self.server.by_code,
            "connections": self.server.connections,
            "logins": self.server.logins,
            "mqtt": {
                "open": self.broker.open,
                "connects": self.broker.connects,
                "subscriptions": self.broker.subscriptions,
                "pushes_sent": self.replay.pushes_sent,
                "pushes_missed": self.replay.pushes_missed,
            },
            "replay": {
                "errors_by_code": self.replay.errors_by_code,
                "sessions_expired": self.replay.sessions_expired,
                "accounts": self.accounts,
                "cars": self.cars,
            },
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the handlers and the scheduler share.
struct Core {
    scenario: Scenario,
    templates: Templates,
    choices: Vec<CarTemplates>,
    usernames: Vec<String>,
    fixed: std::collections::HashMap<&'static str, Value>,
    clock: Arc<ReplayClock>,
    epoch: Epoch,
    broker: Arc<TestBroker>,
    server: ServerHandle,
    /// The next schedule entry to fire.
    cursor: Mutex<usize>,
    counters: Mutex<ReplayCounters>,
}

fn serial(vin: &str, t: u32, what: &str) -> String {
    md5_hex(&format!("{vin}:{t}:{what}")).to_uppercase()
}

impl Core {
    fn epoch_ms(&self) -> i64 {
        match self.epoch {
            Epoch::Scenario => self.scenario.header.start_unix * 1000,
            Epoch::Now => {
                let wall = self
                    .clock
                    .started()
                    .map_or_else(clock::wall_unix_ms, |started| started.wall_unix_ms);
                wall - (self.clock.start_at() * 1000.0).round() as i64
            }
        }
    }

    fn moment(&self, t: u32) -> Moment {
        Moment {
            t,
            epoch_ms: self.epoch_ms(),
        }
    }

    fn count_error(&self, code: &str) {
        *lock(&self.counters)
            .errors_by_code
            .entry(code.to_owned())
            .or_default() += 1;
    }

    /// Fires every schedule entry that is due, once each.
    fn catch_up(&self) {
        if self.clock.started().is_none() {
            return;
        }
        let now = self.clock.now();
        let mut cursor = lock(&self.cursor);
        while let Some(event) = self.scenario.schedule.get(*cursor) {
            if f64::from(event.t) > now {
                break;
            }
            self.fire(event);
            *cursor += 1;
        }
    }

    fn next_event(&self) -> Option<u32> {
        let cursor = *lock(&self.cursor);
        self.scenario.schedule.get(cursor).map(|event| event.t)
    }

    fn fire(&self, event: &scenario::Event) {
        match &event.kind {
            EventKind::Expire(account) => {
                self.server.expire_sessions(&self.usernames[*account]);
                lock(&self.counters).sessions_expired += 1;
            }
            EventKind::Push(push) => {
                let car = &self.scenario.cars[push.car];
                let username = &self.usernames[car.account];
                let state = push
                    .fields
                    .over(self.scenario.lookup(push.car, event.t).state);
                let sent = match (
                    self.server.user_id(username),
                    self.server.newest_content_key(username),
                ) {
                    (Some(user_id), Some(key)) => {
                        let body = self.templates.push(
                            push.event,
                            self.choices[push.car],
                            car,
                            &state,
                            self.moment(event.t),
                        );
                        self.broker
                            .publish_sealed(&format!("oversea/res/{user_id}"), &body, &key)
                    }
                    _ => 0,
                };
                let mut counters = lock(&self.counters);
                if sent > 0 {
                    counters.pushes_sent += 1;
                } else {
                    counters.pushes_missed += 1;
                }
            }
        }
    }

    /// The reply to a verified request to one of the replay's endpoints.
    fn answer(&self, endpoint: &str, request: &Request) -> Reply {
        let Some(account) = request
            .username
            .as_deref()
            .and_then(|username| self.scenario.account_for_username(username))
        else {
            return Reply::code(NOT_YOURS);
        };
        let t = self.clock.second();
        match endpoint {
            VEHICLES => {
                let cars = &self.scenario.accounts[account].cars;
                let list = cars
                    .iter()
                    .map(|&car| self.templates.vehicle(&self.scenario.cars[car]))
                    .collect();
                return Reply::Data(Value::Array(list));
            }
            BROKER_LOOKUP => return Reply::Data(json!({"emqBorker": self.broker.address()})),
            LATEST_CONFIG => {
                let wanted: Vec<String> = request
                    .inner_field("vinList")
                    .and_then(Value::as_str)
                    .and_then(|list| serde_json::from_str(list).ok())
                    .unwrap_or_default();
                let mut configs = Map::new();
                for vin in wanted {
                    let Some(car) = self
                        .scenario
                        .car_index(&vin)
                        .filter(|&car| self.scenario.cars[car].account == account)
                    else {
                        continue;
                    };
                    // A busy car makes the whole read busy: 1008 answers every
                    // read that names it.
                    if self.scenario.lookup(car, t).error == Some(BUSY) {
                        self.count_error(BUSY);
                        return Reply::code(BUSY);
                    }
                    configs.insert(vin, self.templates.latest_config());
                }
                return Reply::Data(Value::Object(configs));
            }
            _ => {}
        }
        let Some(vin) = request.inner_field("vin").and_then(Value::as_str) else {
            return Reply::Data(self.fixed.get(endpoint).cloned().unwrap_or(Value::Null));
        };
        let Some(index) = self
            .scenario
            .car_index(vin)
            .filter(|&car| self.scenario.cars[car].account == account)
        else {
            return Reply::code(NOT_YOURS);
        };
        let car = &self.scenario.cars[index];
        let lookup = self.scenario.lookup(index, t);
        let realtime = matches!(endpoint, REALTIME_TRIGGER | REALTIME_RESULT);
        if let Some(code) = lookup.error
            && (code == BUSY || realtime)
        {
            self.count_error(code);
            return Reply::code(code);
        }
        let state = lookup.state;
        let at = self.moment(t);
        let serial_in = request.inner_field("requestSerial").and_then(Value::as_str);
        Reply::Data(match endpoint {
            REALTIME_TRIGGER => json!({"requestSerial": serial(vin, t, "realtime")}),
            REALTIME_RESULT => {
                self.templates
                    .realtime(self.choices[index], car, state, at, serial_in)
            }
            GPS_TRIGGER => json!({"requestSerial": serial(vin, t, "gps")}),
            GPS_RESULT => self.templates.gps(index, state, at, serial_in),
            CHARGING => self.templates.charging(car, state, at),
            HVAC => self.templates.hvac(state),
            ENERGY => self.templates.energy(car, state, at),
            other => self
                .fixed
                .get(other)
                .map_or(Value::Null, |fixed| templates::with_vin(fixed, vin)),
        })
    }
}

/// Fires the schedule as the clock reaches each entry.
async fn run_schedule(core: Arc<Core>) {
    loop {
        let changed = core.clock.changed();
        tokio::pin!(changed);
        changed.as_mut().enable();
        core.catch_up();
        let wait = core
            .next_event()
            .and_then(|t| core.clock.wall_until(f64::from(t)));
        match wait {
            Some(wait) => {
                tokio::select! {
                    _ = &mut changed => {}
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            None => changed.await,
        }
    }
}

/// A running replay: a [`FakeBydServer`] and a [`TestBroker`] driven by a
/// [`Scenario`] on a [`ReplayClock`]. See the [module documentation](self).
pub struct ReplayServer {
    server: FakeBydServer,
    broker: Arc<TestBroker>,
    core: Arc<Core>,
    _scheduler: AbortOnDrop,
}

/// Stops a task when dropped.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl std::fmt::Debug for ReplayServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayServer")
            .field("url", &self.server.url())
            .field("broker", &self.broker.address())
            .finish_non_exhaustive()
    }
}

impl ReplayServer {
    /// Starts the server, the broker and the push scheduler on the current
    /// tokio runtime. The clock stands at `start_at` until
    /// [`ReplayServer::start_clock`] (or, on a manual clock, until it is set).
    pub async fn start(scenario: Scenario, options: ReplayOptions) -> std::io::Result<Self> {
        let accounts = scenario
            .accounts
            .iter()
            .map(|account| Account::new(&account.username(), PASSWORD));
        let server = FakeBydServer::start_on(options.port, accounts).await?;
        let broker = TestBroker::start_on(options.mqtt_port).await?;
        server.set_recording(options.record);
        broker.set_recording(options.record);
        let duration = f64::from(scenario.header.duration_s);
        let clock = Arc::new(if options.manual {
            ReplayClock::manual(options.start_at, options.speed, duration)
        } else {
            ReplayClock::wall(options.start_at, options.speed, duration)
        });
        let templates = Templates::load(options.corpus.as_deref());
        let mut by_model = std::collections::HashMap::new();
        let choices = scenario
            .cars
            .iter()
            .map(|car| {
                *by_model
                    .entry((car.model.clone(), car.energy))
                    .or_insert_with(|| templates.choose(&car.model, car.energy))
            })
            .collect();
        let start_at = clock.start_at();
        let cursor = scenario
            .schedule
            .partition_point(|event| f64::from(event.t) < start_at);
        let core = Arc::new(Core {
            usernames: scenario.accounts.iter().map(|a| a.username()).collect(),
            fixed: templates.fixed(),
            choices,
            templates,
            scenario,
            clock,
            epoch: options.epoch,
            broker: broker.clone(),
            server: server.handle(),
            cursor: Mutex::new(cursor),
            counters: Mutex::new(ReplayCounters::default()),
        });
        let delay = options.delay;
        let delayed = move |reply: Reply| {
            if delay.is_zero() {
                reply
            } else {
                reply.delayed(delay)
            }
        };
        if !delay.is_zero() {
            server.on(LOGIN, move |_| Reply::Default.delayed(delay));
        }
        let endpoints = [
            VEHICLES,
            BROKER_LOOKUP,
            REALTIME_TRIGGER,
            REALTIME_RESULT,
            GPS_TRIGGER,
            GPS_RESULT,
            CHARGING,
            HVAC,
            ENERGY,
            LATEST_CONFIG,
        ];
        let fixed: Vec<&'static str> = core.fixed.keys().copied().collect();
        for endpoint in endpoints.into_iter().chain(fixed) {
            let core = core.clone();
            server.on(endpoint, move |request| {
                delayed(core.answer(endpoint, request))
            });
        }
        // A request is checked against the sessions as they are at its
        // moment: a 1005 due now expires them before the check.
        let hook = Arc::downgrade(&core);
        server.before_each(move || {
            if let Some(core) = hook.upgrade() {
                core.catch_up();
            }
        });
        let scheduler = AbortOnDrop(tokio::spawn(run_schedule(core.clone())));
        Ok(Self {
            server,
            broker,
            core,
            _scheduler: scheduler,
        })
    }

    /// The base URL to give a client.
    pub fn url(&self) -> String {
        self.server.url()
    }

    /// The broker's address as BYD's broker lookup gives it.
    pub fn broker_address(&self) -> String {
        self.broker.address()
    }

    /// The fake server underneath.
    pub fn server(&self) -> &FakeBydServer {
        &self.server
    }

    /// The broker underneath.
    pub fn broker(&self) -> &Arc<TestBroker> {
        &self.broker
    }

    /// The scenario.
    pub fn scenario(&self) -> &Scenario {
        &self.core.scenario
    }

    /// The replay clock.
    pub fn clock(&self) -> &ReplayClock {
        &self.core.clock
    }

    /// The epoch reply timestamps count from, in milliseconds.
    pub fn epoch_ms(&self) -> i64 {
        self.core.epoch_ms()
    }

    /// The note of the realtime template a car's replies are built from,
    /// `None` for the built-in one.
    pub fn template_note(&self, vin: &str) -> Option<&str> {
        let car = self.core.scenario.car_index(vin)?;
        self.core.templates.awake_note(self.core.choices[car])
    }

    /// Starts the clock (a second call answers the same) and fires anything due.
    pub fn start_clock(&self) -> StartInfo {
        let started = self.core.clock.start();
        self.core.catch_up();
        StartInfo {
            started,
            epoch_unix_ms: self.core.epoch_ms(),
        }
    }

    /// Sets a manual clock to `t` and fires everything due by then before
    /// returning, so the next request sees the moment complete.
    pub fn set_time(&self, t: f64) -> Result<(), String> {
        self.core.clock.set(t)?;
        self.core.catch_up();
        Ok(())
    }

    /// Moves a manual clock on by `seconds`, as [`ReplayServer::set_time`].
    pub fn advance(&self, seconds: f64) -> Result<(), String> {
        self.core.clock.advance(seconds)?;
        self.core.catch_up();
        Ok(())
    }

    /// `{"t", "started", "speed", "wall_unix_ms", "finished"}`.
    pub fn clock_json(&self) -> Value {
        let clock = &self.core.clock;
        json!({
            "t": clock.now(),
            "started": clock.started().is_some(),
            "speed": clock.speed(),
            "wall_unix_ms": clock::wall_unix_ms(),
            "finished": clock.finished(),
        })
    }

    /// Every counter.
    pub fn stats(&self) -> ReplayStats {
        ReplayStats {
            server: self.server.stats(),
            broker: self.broker.stats(),
            replay: lock(&self.core.counters).clone(),
            accounts: self.core.scenario.accounts.len(),
            cars: self.core.scenario.cars.len(),
        }
    }

    /// Zeroes every counter (not the clock).
    pub fn reset_stats(&self) {
        self.server.reset_stats();
        self.broker.reset_stats();
        *lock(&self.core.counters) = ReplayCounters::default();
    }

    /// Stops the scheduler, the server and the broker.
    pub async fn shutdown(self) {
        let Self {
            server,
            broker,
            _scheduler,
            ..
        } = self;
        drop(_scheduler);
        server.shutdown().await;
        drop(broker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 100, "accounts": 1, "cars": 1}
{"type": "account", "account": "acct-0001", "cars": ["TESTVIN0000000001"]}
{"type": "car", "vin": "TESTVIN0000000001", "account": "acct-0001", "model": "seal", "energy": "ev"}
{"type": "error", "vin": "TESTVIN0000000001", "t": 10, "code": "6002"}"#;

    fn request(username: Option<&str>, inner: Value) -> Request {
        Request {
            endpoint: String::new(),
            headers: Vec::new(),
            cookies: Vec::new(),
            body: String::new(),
            outer: None,
            inner: Some(inner),
            username: username.map(str::to_owned),
            code: None,
            status: None,
        }
    }

    fn data(reply: Reply) -> Value {
        match reply {
            Reply::Data(data) => data,
            other => panic!("not data: {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_error_table_and_odd_requests() {
        let scenario = Scenario::parse(TEXT).unwrap();
        let options = ReplayOptions {
            manual: true,
            corpus: None,
            ..ReplayOptions::default()
        };
        let replay = ReplayServer::start(scenario, options).await.unwrap();
        let core = &replay.core;
        let user = Some("acct-0001@example.com");
        let car = json!({"vin": "TESTVIN0000000001"});
        // A request from nobody the scenario knows, or for a car not its own.
        assert_eq!(
            core.answer(VEHICLES, &request(Some("x@example.com"), json!({}))),
            Reply::code("1001")
        );
        assert_eq!(
            core.answer(VEHICLES, &request(None, json!({}))),
            Reply::code("1001")
        );
        let foreign = json!({"vin": "TESTVIN0000000009"});
        assert_eq!(
            core.answer(HVAC, &request(user, foreign)),
            Reply::code("1001")
        );
        // No VIN where one is expected: the fixed payload, or nothing.
        assert_eq!(
            data(core.answer(HVAC, &request(user, json!({})))),
            Value::Null
        );
        assert_eq!(
            data(core.answer(
                "/app/messageCenter/getUnReadMessageList",
                &request(user, json!({}))
            )),
            json!([])
        );
        // A latest-config request without a usable list answers no cars.
        assert_eq!(
            data(core.answer(LATEST_CONFIG, &request(user, json!({"vinList": 5})))),
            json!({})
        );
        // An error before any state: the realtime reads fail, the rest
        // answer the unseen (asleep) car.
        replay.set_time(10.0).unwrap();
        for endpoint in [REALTIME_TRIGGER, REALTIME_RESULT] {
            assert_eq!(
                core.answer(endpoint, &request(user, car.clone())),
                Reply::code("6002")
            );
        }
        let gps = data(core.answer(GPS_TRIGGER, &request(user, car.clone())));
        assert_eq!(gps["requestSerial"].as_str().map(str::len), Some(32));
        assert_eq!(
            data(core.answer(HVAC, &request(user, car.clone())))["status"],
            json!(0)
        );
        assert_eq!(lock(&core.counters).errors_by_code["6002"], 2);
        assert!(format!("{replay:?}").contains("ReplayServer"));
        assert_eq!(Epoch::parse("now"), Some(Epoch::Now));
        assert_eq!(Epoch::parse("then"), None);
        replay.shutdown().await;
    }

    /// acct-0001 has cars 1 and 2, acct-0002 car 3; cars 1 and 3 are busy from t=10.
    const BUSY_TEXT: &str = r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 100, "accounts": 2, "cars": 3}
{"type": "account", "account": "acct-0001", "cars": ["TESTVIN0000000001", "TESTVIN0000000002"]}
{"type": "account", "account": "acct-0002", "cars": ["TESTVIN0000000003"]}
{"type": "car", "vin": "TESTVIN0000000001", "account": "acct-0001", "model": "seal", "energy": "ev"}
{"type": "car", "vin": "TESTVIN0000000002", "account": "acct-0001", "model": "seal", "energy": "ev"}
{"type": "car", "vin": "TESTVIN0000000003", "account": "acct-0002", "model": "seal", "energy": "ev"}
{"type": "error", "vin": "TESTVIN0000000001", "t": 10, "code": "1008"}
{"type": "error", "vin": "TESTVIN0000000003", "t": 10, "code": "1008"}"#;

    #[tokio::test]
    async fn a_busy_car_makes_the_capabilities_read_busy() {
        let scenario = Scenario::parse(BUSY_TEXT).unwrap();
        let options = ReplayOptions {
            manual: true,
            corpus: None,
            ..ReplayOptions::default()
        };
        let replay = ReplayServer::start(scenario, options).await.unwrap();
        let core = &replay.core;
        let user = Some("acct-0001@example.com");
        let configs = |vins: &[&str]| {
            let list = serde_json::to_string(vins).unwrap();
            core.answer(LATEST_CONFIG, &request(user, json!({"vinList": list})))
        };
        let keys = |reply: Reply| -> Vec<String> {
            data(reply).as_object().unwrap().keys().cloned().collect()
        };
        replay.set_time(9.0).unwrap();
        assert_eq!(keys(configs(&["TESTVIN0000000001"])), ["TESTVIN0000000001"]);
        replay.set_time(10.0).unwrap();
        // One busy VIN.
        assert_eq!(configs(&["TESTVIN0000000001"]), Reply::code(BUSY));
        // Two of the account's VINs, one busy: the read is busy.
        assert_eq!(
            configs(&["TESTVIN0000000002", "TESTVIN0000000001"]),
            Reply::code(BUSY)
        );
        assert_eq!(keys(configs(&["TESTVIN0000000002"])), ["TESTVIN0000000002"]);
        // A busy VIN of another account in the list changes nothing.
        assert_eq!(
            keys(configs(&["TESTVIN0000000003", "TESTVIN0000000002"])),
            ["TESTVIN0000000002"]
        );
        assert_eq!(lock(&core.counters).errors_by_code[BUSY], 2);
        replay.shutdown().await;
    }
}
