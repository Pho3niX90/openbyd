//! The client itself: sessions, reads, commands and the MQTT channel
//! (`pybyd/client.py`).
//!
//! [`BydClient`] is pyBYD's `BydClient`, with the same methods, argument
//! names and behaviour, built on the flows of [`crate::api`]. It is generic
//! over a [`ClientEnv`], so the whole client runs against the scripted
//! environment in the parity tests; in production it runs on
//! [`ProductionEnv`] (an [`HttpEnv`] and a [`crate::mqtt::MqttRuntime`]):
//!
//! ```no_run
//! # async fn demo() -> Result<(), openbyd_protocol::error::Error> {
//! use openbyd_client::client::BydClient;
//! use openbyd_client::http::HttpPool;
//! use openbyd_protocol::config::BydConfig;
//!
//! let pool = HttpPool::new(); // one per process, shared by every client
//! let client = BydClient::connect(BydConfig::new("user@example.com", "secret"), &pool);
//! client.login().await?;
//! for vehicle in client.get_vehicles().await? {
//!     let _realtime = client.get_vehicle_realtime(&vehicle.vin, Default::default()).await?;
//! }
//! client.async_close();
//! # Ok(()) }
//! ```
//!
//! # Many clients in one process
//!
//! A client is a handle (`Clone` shares it) over its own state: the
//! session, the caches, the pending commands and the MQTT waits, each
//! behind a short-lived lock of its own; no lock is held across an `await`
//! and none is shared between clients. Every method takes `&self` and may
//! run concurrently with the others. The HTTP connections come from the
//! shared [`crate::http::HttpPool`]; the MQTT channel is one tokio task per
//! client. Nothing spawns a thread or blocks.
//!
//! # What pyBYD's users patched, as options
//!
//! [`ClientOptions`] carries what applications used to monkeypatch: a
//! command [gate policy](ClientOptions::gate_policy), the default
//! [command retries](ClientOptions::command_retries), the
//! [PIN resolver](ClientOptions::pin_resolver), the
//! [re-login policy](ReloginPolicy), a [request observer](RequestRecord) and
//! the user callbacks. Sessions can be [exported](BydClient::export_session)
//! and [imported](BydClient::import_session), and vehicle metadata
//! [seeded](BydClient::seed_vehicle_metadata) without a vehicle list.
//!
//! # Cars
//!
//! [`BydClient::get_car`] builds and keeps one [`crate::car::BydCar`] per
//! VIN, as pyBYD's `get_car` does, and merges the MQTT pushes for that VIN
//! into it. An application with cars of its own (built with
//! `BydCar::new(client.clone(), ...)`) routes the pushes to them through a
//! [`CarRouter`].

/// pyBYD's `_call_with_reauth`: evaluates `$call` (an expression with
/// `.await`), and once more when it fails with
///
/// - `BydTokenRefreshError` (1030), after refreshing the tokens (or, when
///   that fails, a fresh login);
/// - `BydServerTimeError` (1003), at once: the transport has taken BYD's
///   clock from the reply's `Date` header;
/// - `BydSessionExpiredError`, after a fresh session;
/// - `BydServiceBusyError` (1008), when `$probe` is true (read-only vehicle
///   endpoints) and [`BydClient::recover_busy_session`] finds the session
///   was the problem and recovers it.
///
/// `$retry_if` is evaluated before each of the first three retries and can
/// veto them: a command passes "not dispatched yet", so it is never sent
/// twice. A failing re-login is the error.
///
/// A macro, not a method taking an async closure: the compiler cannot yet
/// prove such closures' futures `Send` for every lifetime, and every client
/// method must be `Send`.
macro_rules! reauth {
    ($client:expr, $probe:expr, $retry_if:expr, $call:expr) => {{
        use openbyd_protocol::error::ErrorKind as Kind;
        let mut retried = false;
        loop {
            match $call {
                Err(error) if !retried && error.is_a(Kind::BydServiceBusyError) => {
                    if $probe && $client.recover_busy_session(&error).await {
                        retried = true;
                    } else {
                        break Err(error);
                    }
                }
                Err(error)
                    if !retried
                        && (error.is_a(Kind::BydSessionExpiredError)
                            || error.is_a(Kind::BydServerTimeError)) =>
                {
                    if !$retry_if {
                        break Err(error);
                    }
                    retried = true;
                    let renewed = if error.is_a(Kind::BydTokenRefreshError) {
                        $client.refresh_or_relogin().await
                    } else if error.is_a(Kind::BydServerTimeError) {
                        Ok(())
                    } else {
                        $client.invalidate_session();
                        $client.ensure_session().await.map(drop)
                    };
                    if let Err(relogin) = renewed {
                        // With re-login disabled, the expiry BYD reported
                        // (its code says why) is more useful than ours.
                        break Err(if relogin.code() == $crate::client::RELOGIN_DISABLED {
                            error
                        } else {
                            relogin
                        });
                    }
                }
                other => break other,
            }
        }
    }};
}

/// pyBYD's `_authed_call`: runs the flow `$flow` with `$cx`, the context of
/// the current session, under [`reauth`]; `probe` turns on the 1008
/// session probe (pyBYD's `probe_busy_session=True`).
macro_rules! authed {
    ($client:expr, |$cx:ident| $flow:expr) => {
        authed!(@run $client, false, |$cx| $flow)
    };
    ($client:expr, probe, |$cx:ident| $flow:expr) => {
        authed!(@run $client, true, |$cx| $flow)
    };
    (@run $client:expr, $probe:expr, |$cx:ident| $flow:expr) => {
        reauth!(
            $client,
            $probe,
            true,
            async {
                let session = $client.ensure_session().await?;
                $client.require_transport()?;
                let $cx = $client.context(&session);
                $flow.await
            }
            .await
        )
    };
}

mod cars;
mod commands;
pub(crate) mod parity;
mod production;
pub(crate) mod push;
mod standby;

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use openbyd_protocol::coerce::IntEnum;
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::constants::{COMMAND_POLL_DELAYS_S, poll_delays};
use openbyd_protocol::envelope::{build_login_request, parse_login_response};
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::booking::BookingList;
use openbyd_protocol::models::charging::ChargingStatus;
use openbyd_protocol::models::command_gating::CommandGateVerdict;
use openbyd_protocol::models::command_gating::GUEST_PERMISSION_GPS;
use openbyd_protocol::models::control::{CommandAck, CommandAckEvent, CommandLifecycleEvent};
use openbyd_protocol::models::energy::EnergyConsumption;
use openbyd_protocol::models::gps::GpsInfo;
use openbyd_protocol::models::hvac::HvacStatus;
use openbyd_protocol::models::latest_config::{VehicleCapabilities, VehicleLatestConfig};
use openbyd_protocol::models::message_center::{NotificationCategory, VehicleMsgItemInfo};
use openbyd_protocol::models::ota::OtaUpgradeInfo;
use openbyd_protocol::models::push_notification::PushNotificationState;
use openbyd_protocol::models::realtime::VehicleRealtimeData;
use openbyd_protocol::models::smart_charging::{AcCurrentLimit, SmartChargingSchedule};
use openbyd_protocol::models::vehicle::{EmpowerDetail, EnergyType, Vehicle};
use openbyd_protocol::mqtt::{MqttBootstrap, MqttEvent};
use openbyd_protocol::pyjson::py_str;
use openbyd_protocol::session::Session;
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::control::truthy;
use crate::api::smart_charging::{
    ChangeChargeFetchFn, EnergyTypeArg, FetchFn, SaveChargingScheduleFetchFn,
    is_charge_change_ready,
};
use crate::api::standby::{SendFetchFn, is_send_ready};
use crate::api::vehicle::parse_model;
use crate::api::{
    booking, charging, energy, gps, hvac, latest_config, login, message_center, ota,
    push_notifications, realtime, smart_charging, vehicle, vehicle_settings,
};
use crate::env::{BoxFuture, ClientEnv, Env, MqttSink};
use crate::http::{HttpEnv, HttpPool};
use crate::mqtt::fetch_mqtt_bootstrap;

pub use cars::{ClientCar, ClientHandle, GetCarOptions};
pub use commands::{ControlParamsArg, resolve_command_pwd};
pub use production::ProductionEnv;
pub use standby::RunPollSettings;

/// Where the login request goes.
pub const LOGIN_ENDPOINT: &str = "/app/account/login";
/// Where a missing latest config is reported from.
pub const LATEST_CONFIG_ENDPOINT: &str = "/vehicle/vehicleswitch/getLatestConfig";

/// MQTT error codes meaning the requested data is unavailable (no GPS fix):
/// a matching push ends the wait without polling (pyBYD's
/// `_MQTT_DATA_UNAVAILABLE_CODES`).
pub const MQTT_DATA_UNAVAILABLE_CODES: [&str; 1] = ["6051"];

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Called with the VIN and the parsed realtime data of every MQTT
/// `vehicleInfo` push (pyBYD's `on_vehicle_info`).
pub type VehicleInfoCallback = Arc<dyn Fn(&str, &VehicleRealtimeData) + Send + Sync>;
/// Called with the event name, the VIN and the `respondData` of every MQTT
/// event that names a VIN (pyBYD's `on_mqtt_event`).
pub type MqttEventCallback = Arc<dyn Fn(&str, &str, &Map<String, Value>) + Send + Sync>;
/// Called with every remote-control acknowledgement over MQTT (pyBYD's `on_command_ack`).
pub type CommandAckCallback = Arc<dyn Fn(&CommandAckEvent) + Send + Sync>;
/// Called with every command lifecycle event (pyBYD's `on_command_lifecycle`).
pub type CommandLifecycleCallback = Arc<dyn Fn(&CommandLifecycleEvent) + Send + Sync>;
/// Called after every accepted MQTT connection (pyBYD's `on_mqtt_connect`).
pub type MqttConnectCallback = Arc<dyn Fn() + Send + Sync>;
/// The code of the `BydSessionExpiredError` a client with
/// [`ReloginPolicy::Never`] raises when it has no session at all.
pub const RELOGIN_DISABLED: &str = "relogin_disabled";
/// Called when the MQTT connection drops, with the reason. openbyd's
/// addition: pyBYD only logs it.
pub type MqttDisconnectCallback = Arc<dyn Fn(&str) + Send + Sync>;
/// Called on every refused MQTT connection, with how many in a row.
/// openbyd's addition: pyBYD only logs it.
pub type MqttConnackRefusedCallback = Arc<dyn Fn(u32) + Send + Sync>;
/// Decides whether a command may be sent despite (or because of) the
/// command gate's verdict: given the VIN and the verdict, returns `true` to
/// send. See [`ClientOptions::gate_policy`].
pub type GatePolicy = Arc<dyn Fn(&str, &CommandGateVerdict) -> bool + Send + Sync>;
/// Turns the `command_pwd` a caller passed (or `None`) and the config into
/// what is sent as `commandPwd`; empty means "no PIN". See
/// [`ClientOptions::pin_resolver`].
pub type PinResolver = Arc<dyn Fn(Option<&str>, &BydConfig) -> String + Send + Sync>;
/// Hears about every request the client sends. See [`RequestRecord`].
pub type RequestObserver = Arc<dyn Fn(&RequestRecord) + Send + Sync>;
/// Produces a new session when the current one has expired, for
/// [`ReloginPolicy::Handler`].
pub type ReloginHandler =
    Arc<dyn Fn() -> BoxFuture<'static, Result<SessionTokens, Error>> + Send + Sync>;

/// What the client does when it needs a new session: the session's TTL ran
/// out, the cloud said it expired, or MQTT asks for a re-login.
#[derive(Clone, Default)]
pub enum ReloginPolicy {
    /// Log in again with the configured password, as pyBYD does.
    #[default]
    Password,
    /// Never log in by itself: the call fails with
    /// `BydSessionExpiredError` and MQTT recovery stays idle. The error
    /// keeps the code of BYD's own expiry reply (1002, 1005, ...) when a call
    /// was refused; it is [`RELOGIN_DISABLED`] when there was no session to
    /// begin with. The rest of this note: MQTT
    /// recovery stays idle. For clients restored from saved tokens that
    /// must never send the password; the application logs in or imports a
    /// new session itself.
    Never,
    /// Ask the application for a new session (for example one refreshed
    /// elsewhere); an error from the handler is the call's error.
    Handler(ReloginHandler),
}

impl fmt::Debug for ReloginPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Password => "Password",
            Self::Never => "Never",
            Self::Handler(_) => "Handler",
        })
    }
}

/// Car-layer hooks: where MQTT pushes for a VIN go besides the callbacks
/// (pyBYD routes them to its `BydCar` instances).
pub trait CarRouter: Send + Sync {
    /// A parsed `vehicleInfo` push.
    fn handle_mqtt_realtime(&self, vin: &str, data: &VehicleRealtimeData);
    /// A parsed `smartCharge` push.
    fn handle_mqtt_charging(&self, vin: &str, data: &ChargingStatus);
    /// A parsed `energyConsumption` push.
    fn handle_mqtt_energy(&self, vin: &str, data: &EnergyConsumption);
    /// A parsed `vehicleStatusPush`: partial models to lay over what is
    /// known ([`BydCar::handle_mqtt_status_push`](crate::car::BydCar::handle_mqtt_status_push)),
    /// `hvac` and `charging` only when the push had their section.
    ///
    /// Ignored unless implemented, so routers written before pyBYD handled
    /// these pushes keep compiling.
    fn handle_mqtt_status_push(
        &self,
        vin: &str,
        data: &VehicleRealtimeData,
        hvac: Option<&HvacStatus>,
        charging: Option<&ChargingStatus>,
    ) {
        let _ = (vin, data, hvac, charging);
    }
}

/// How a request ended, for a [`RequestObserver`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestOutcome {
    /// The API answered; `code` is the reply's `code` as text (`"0"` is
    /// success), `None` when the reply had none.
    Reply {
        /// The reply's code.
        code: Option<String>,
    },
    /// The HTTP request failed (`BydTransportError`), with its status when
    /// the server answered.
    Transport {
        /// The HTTP status.
        status: Option<u16>,
    },
    /// Any other failure (the reply could not be decoded, ...).
    Failed(ErrorKind),
}

/// One request the client sent, for metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRecord {
    /// The endpoint, such as `"/control/getGpsInfo"`.
    pub endpoint: String,
    /// How it ended.
    pub outcome: RequestOutcome,
    /// How long it took.
    pub duration: Duration,
}

/// A session as an application stores it, to build a client from later
/// without logging in ([`BydClient::import_session`], [`BydClient::export_session`]).
/// Its `Debug` output leaves the three tokens out.
#[derive(Clone, PartialEq)]
pub struct SessionTokens {
    /// The account's user ID.
    pub user_id: String,
    /// The token requests are signed with. A secret.
    pub sign_token: String,
    /// The token the content key derives from. A secret.
    pub encry_token: String,
    /// The token that buys new tokens on 1030; empty when the login gave
    /// none (and in records saved before openbyd kept it). A secret.
    pub refresh_token: String,
    /// Seconds the session has left; `None` for no expiry.
    pub ttl_remaining: Option<f64>,
    /// Whether the account reaches each VIN as a guest.
    pub guest_vins: HashMap<String, bool>,
}

impl std::fmt::Debug for SessionTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTokens")
            .field("user_id", &self.user_id)
            .field("sign_token", &"<redacted>")
            .field("encry_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("ttl_remaining", &self.ttl_remaining)
            .field("guest_vins", &self.guest_vins)
            .finish()
    }
}

/// Everything a client is built with besides its config: pyBYD's
/// constructor arguments, and the options openbyd adds in place of
/// monkeypatching. Start from `ClientOptions::default()`.
#[derive(Clone)]
#[non_exhaustive]
pub struct ClientOptions {
    /// pyBYD's `on_vehicle_info`.
    pub on_vehicle_info: Option<VehicleInfoCallback>,
    /// pyBYD's `on_mqtt_event`.
    pub on_mqtt_event: Option<MqttEventCallback>,
    /// pyBYD's `on_command_ack`.
    pub on_command_ack: Option<CommandAckCallback>,
    /// pyBYD's `on_command_lifecycle`.
    pub on_command_lifecycle: Option<CommandLifecycleCallback>,
    /// pyBYD's `on_mqtt_connect`.
    pub on_mqtt_connect: Option<MqttConnectCallback>,
    /// Every dropped MQTT connection, with the reason (openbyd's addition).
    pub on_mqtt_disconnect: Option<MqttDisconnectCallback>,
    /// Every refused MQTT connection, with the count in a row (openbyd's addition).
    pub on_mqtt_connack_refused: Option<MqttConnackRefusedCallback>,
    /// How long a sent command waits for its MQTT acknowledgement before it
    /// counts as expired (default 300 seconds; not above 0 means 300).
    pub command_ack_ttl_seconds: f64,
    /// Overrides the command gate. `None` (the default) sends a command
    /// only when the verdict is `supported`, as pyBYD does. A policy that
    /// returns `true` for the reasons `"function_no_missing"` and
    /// `"learn_info_missing"` lets commands through for cars whose
    /// capability data is incomplete.
    pub gate_policy: Option<GatePolicy>,
    /// `command_retries` of every remote command (default 1, pyBYD's: a
    /// command is sent once; the car's refusal is never sent again).
    /// 1 sends a refused or unconfirmed command once and never again.
    pub command_retries: i64,
    /// Turns a given PIN into the `commandPwd` sent. `None` (the default)
    /// is pyBYD's [`resolve_command_pwd`]: a 32-digit hex string is taken
    /// as an MD5 digest and upper-cased, anything else is hashed. A
    /// resolver that returns digests unchanged keeps their case.
    pub pin_resolver: Option<PinResolver>,
    /// What to do when a new session is needed.
    pub relogin: ReloginPolicy,
    /// Hears about every request, with its outcome and duration.
    pub request_observer: Option<RequestObserver>,
    /// Where MQTT pushes go for the car layer.
    pub car_router: Option<Arc<dyn CarRouter>>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            on_vehicle_info: None,
            on_mqtt_event: None,
            on_command_ack: None,
            on_command_lifecycle: None,
            on_mqtt_connect: None,
            on_mqtt_disconnect: None,
            on_mqtt_connack_refused: None,
            command_ack_ttl_seconds: 300.0,
            gate_policy: None,
            command_retries: 1,
            pin_resolver: None,
            relogin: ReloginPolicy::Password,
            request_observer: None,
            car_router: None,
        }
    }
}

impl fmt::Debug for ClientOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |present: bool| if present { "set" } else { "none" };
        f.debug_struct("ClientOptions")
            .field("on_vehicle_info", &set(self.on_vehicle_info.is_some()))
            .field("on_mqtt_event", &set(self.on_mqtt_event.is_some()))
            .field("on_command_ack", &set(self.on_command_ack.is_some()))
            .field(
                "on_command_lifecycle",
                &set(self.on_command_lifecycle.is_some()),
            )
            .field("on_mqtt_connect", &set(self.on_mqtt_connect.is_some()))
            .field(
                "on_mqtt_disconnect",
                &set(self.on_mqtt_disconnect.is_some()),
            )
            .field(
                "on_mqtt_connack_refused",
                &set(self.on_mqtt_connack_refused.is_some()),
            )
            .field("command_ack_ttl_seconds", &self.command_ack_ttl_seconds)
            .field("gate_policy", &set(self.gate_policy.is_some()))
            .field("command_retries", &self.command_retries)
            .field("pin_resolver", &set(self.pin_resolver.is_some()))
            .field("relogin", &self.relogin)
            .field("request_observer", &set(self.request_observer.is_some()))
            .field("car_router", &set(self.car_router.is_some()))
            .finish()
    }
}

/// How a trigger-and-poll read waits (pyBYD's keyword arguments of
/// `get_vehicle_realtime` and `get_gps_info`, with their defaults).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PollSettings {
    /// HTTP polls after the MQTT wait (default 10).
    pub poll_attempts: i64,
    /// Seconds slept before each poll; none when not above 0 (default 1.5).
    pub poll_interval: f64,
    /// Seconds to wait for the MQTT push; `None` is `config.mqtt_timeout`.
    pub mqtt_timeout: Option<f64>,
    /// "Data unavailable" answers in a row that end the polling (default 2).
    pub signal_retries: i64,
}

impl Default for PollSettings {
    fn default() -> Self {
        Self {
            poll_attempts: 10,
            poll_interval: 1.5,
            mqtt_timeout: None,
            signal_retries: 2,
        }
    }
}

/// How a charging change waits (pyBYD's keyword arguments of
/// `start_charging` and `save_charging_schedule`, with their defaults).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChargePollSettings {
    /// Seconds to wait for the MQTT push; `None` is `config.mqtt_timeout`.
    pub mqtt_timeout: Option<f64>,
    /// HTTP polls after the MQTT wait (default 10).
    pub poll_attempts: i64,
    /// Seconds slept before each poll, or `None` (the default) for the
    /// official app's 33 s ladder ([`COMMAND_POLL_DELAYS_S`]).
    pub poll_interval: Option<f64>,
}

impl Default for ChargePollSettings {
    fn default() -> Self {
        Self {
            mqtt_timeout: None,
            poll_attempts: 10,
            poll_interval: None,
        }
    }
}

/// How long after a 1008 session probe another may run (seconds).
pub const BUSY_PROBE_COOLDOWN_S: f64 = 60.0;
/// How long after a recovery that a 1008 probe triggered no probe runs (seconds).
pub const BUSY_REAUTH_COOLDOWN_S: f64 = 600.0;

// ---------------------------------------------------------------------------
// The observed environment
// ---------------------------------------------------------------------------

/// The client's environment with the request observer around `post_secure`.
pub struct Observed<E> {
    env: E,
    observer: Option<RequestObserver>,
}

fn outcome_of(result: &Result<Value, Error>) -> RequestOutcome {
    match result {
        Ok(reply) => RequestOutcome::Reply {
            code: reply.get("code").map(py_str),
        },
        Err(error) if error.is_a(ErrorKind::BydTransportError) => RequestOutcome::Transport {
            status: error.status_code(),
        },
        Err(error) => RequestOutcome::Failed(error.kind()),
    }
}

impl<E: Env> Env for Observed<E> {
    fn now_ms(&self) -> i64 {
        self.env.now_ms()
    }

    fn monotonic(&self) -> f64 {
        self.env.monotonic()
    }

    fn random_bytes(&self) -> [u8; 16] {
        self.env.random_bytes()
    }

    fn sleep(&self, seconds: f64) -> impl Future<Output = ()> + Send {
        self.env.sleep(seconds)
    }

    fn server_time_offset_ms(&self) -> i64 {
        self.env.server_time_offset_ms()
    }

    async fn post_secure(
        &self,
        endpoint: &str,
        outer: &Map<String, Value>,
    ) -> Result<Value, Error> {
        let Some(observer) = &self.observer else {
            return self.env.post_secure(endpoint, outer).await;
        };
        let started = Instant::now();
        let result = self.env.post_secure(endpoint, outer).await;
        let record = RequestRecord {
            endpoint: endpoint.to_owned(),
            outcome: outcome_of(&result),
            duration: started.elapsed(),
        };
        guard("request observer", || observer(&record));
        result
    }
}

impl<E: ClientEnv> ClientEnv for Observed<E> {
    fn wait_for<'a, T: Send + 'a>(
        &'a self,
        future: BoxFuture<'a, T>,
        timeout: f64,
    ) -> impl Future<Output = Option<T>> + Send + 'a {
        self.env.wait_for(future, timeout)
    }

    fn spawn(&self, task: BoxFuture<'static, ()>) {
        self.env.spawn(task);
    }

    fn uniform(&self, low: f64, high: f64) -> f64 {
        self.env.uniform(low, high)
    }

    fn start_mqtt(
        &self,
        bootstrap: &MqttBootstrap,
        decrypt_key: &str,
        keepalive: i64,
        sink: Weak<dyn MqttSink>,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.env.start_mqtt(bootstrap, decrypt_key, keepalive, sink)
    }

    fn stop_mqtt(&self) {
        self.env.stop_mqtt();
    }

    fn mqtt_running(&self) -> bool {
        self.env.mqtt_running()
    }

    fn update_mqtt_key(&self, decrypt_key: &str) {
        self.env.update_mqtt_key(decrypt_key);
    }
}

/// Runs a user hook, as pyBYD runs callbacks inside `try/except Exception`:
/// a panic is logged and does not reach the client.
fn guard(what: &str, hook: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(hook)).is_err() {
        log::debug!("{what} callback failed");
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// A pending MQTT wait (pyBYD's `_MqttWaiter`).
struct Waiter {
    id: u64,
    vin: String,
    event_type: Option<String>,
    serial: Option<Value>,
    /// Gets the event's `respondData`, or `None` when the wait is cancelled.
    slot: tokio::sync::oneshot::Sender<Option<Map<String, Value>>>,
}

/// A command waiting for its acknowledgement (pyBYD's `_PendingCommand`).
#[derive(Debug, Clone)]
struct PendingCommand {
    vin: String,
    serial: Value,
    command: String,
    created_at: f64,
}

#[derive(Default)]
struct State {
    /// pyBYD's `_transport is None`: before `async_start` or after `async_close`.
    closed: bool,
    session: Option<Arc<Session>>,
    /// The session whose guest map is the client's (pyBYD's `_guest_vins`
    /// is the last session's dict).
    guest_owner: Option<Arc<Session>>,
    /// The guest map before any session.
    guest_vins: HashMap<String, bool>,
    /// pyBYD's `_vehicle_metadata_by_vin`, in insertion order.
    vehicles: Vec<(String, Vehicle)>,
    capabilities: HashMap<String, VehicleCapabilities>,
    commands_enabled: bool,
    /// pyBYD's `_mqtt_runtime is not None`.
    mqtt_started: bool,
    waiters: Vec<Waiter>,
    next_waiter: u64,
    reauth_at: f64,
    decrypt_reauths: i64,
    recovery_attempts: i64,
    recovery_last_at: f64,
    /// pyBYD's `_busy_probe_last_at`: the last 1008 session probe.
    busy_probe_last_at: Option<f64>,
    /// pyBYD's `_busy_reauth_last_at`: the last recovery a probe started.
    busy_reauth_last_at: Option<f64>,
    pending: Vec<PendingCommand>,
    matched: i64,
    expired: i64,
    uncorrelated: i64,
    data_poll_serials: Vec<Value>,
}

impl State {
    fn guest_map(&self) -> HashMap<String, bool> {
        match &self.guest_owner {
            Some(session) => session.guest_vins(),
            None => self.guest_vins.clone(),
        }
    }

    /// `self._guest_vins.setdefault(vin, guest)`.
    fn guest_setdefault(&mut self, vin: &str, guest: bool) {
        match &self.guest_owner {
            Some(session) => {
                if !session.guest_vins().contains_key(vin) {
                    session.set_guest(vin, guest);
                }
            }
            None => {
                self.guest_vins.entry(vin.to_owned()).or_insert(guest);
            }
        }
    }

    fn vehicle(&self, vin: &str) -> Option<&Vehicle> {
        self.vehicles
            .iter()
            .find(|(key, _)| key == vin)
            .map(|(_, vehicle)| vehicle)
    }

    /// `self._vehicle_metadata_by_vin[vin] = vehicle`, keeping the key's place.
    fn put_vehicle(&mut self, vehicle: Vehicle) {
        match self
            .vehicles
            .iter_mut()
            .find(|(key, _)| *key == vehicle.vin)
        {
            Some(entry) => entry.1 = vehicle,
            None => self.vehicles.push((vehicle.vin.clone(), vehicle)),
        }
    }
}

/// What a client handle shares.
pub(crate) struct Inner<E: ClientEnv> {
    env: Observed<E>,
    config: BydConfig,
    options: ClientOptions,
    me: Weak<Inner<E>>,
    state: Mutex<State>,
    /// pyBYD's `_cars`: the cars [`BydClient::get_car`] built.
    cars: Mutex<HashMap<String, ClientCar<E>>>,
}

/// The async client for the BYD API: pyBYD's `BydClient`. See the
/// [module documentation](self).
pub struct BydClient<E: ClientEnv> {
    inner: Arc<Inner<E>>,
}

impl<E: ClientEnv> Clone for BydClient<E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<E: ClientEnv> fmt::Debug for BydClient<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BydClient")
            .field("username", &self.inner.config.username)
            .field("logged_in", &self.state().session.is_some())
            .finish_non_exhaustive()
    }
}

impl BydClient<ProductionEnv> {
    /// A client for `config` on the shared HTTP `pool`, with MQTT, ready to
    /// [`login`](Self::login). Must be used inside a tokio runtime.
    pub fn connect(config: BydConfig, pool: &HttpPool) -> Self {
        Self::connect_with(config, pool, ClientOptions::default())
    }

    /// [`connect`](Self::connect) with options.
    pub fn connect_with(config: BydConfig, pool: &HttpPool, options: ClientOptions) -> Self {
        let env = ProductionEnv::new(HttpEnv::for_config(pool, &config));
        Self::new(config, env, options)
    }
}

/// What a call on a closed client fails with (pyBYD's `_require_transport`).
fn not_initialized() -> Error {
    Error::new(
        ErrorKind::BydError,
        "Client not initialized. Use 'async with BydClient(...) as client:'",
    )
}

/// Seconds as pyBYD stores a TTL: `session_ttl` when above 0, else no expiry.
fn session_ttl(config: &BydConfig) -> f64 {
    if config.session_ttl > 0.0 {
        config.session_ttl
    } else {
        f64::INFINITY
    }
}

impl<E: ClientEnv> BydClient<E> {
    /// A client for `config` in `env`, started (pyBYD's `BydClient(config,
    /// ...)` followed by `async_start()`).
    pub fn new(config: BydConfig, env: E, options: ClientOptions) -> Self {
        let observer = options.request_observer.clone();
        let inner = Arc::new_cyclic(|me| Inner {
            env: Observed { env, observer },
            config,
            options,
            me: me.clone(),
            state: Mutex::new(State::default()),
            cars: Mutex::new(HashMap::new()),
        });
        Self { inner }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn cars(&self) -> MutexGuard<'_, HashMap<String, ClientCar<E>>> {
        self.inner
            .cars
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The environment the client runs in.
    pub fn env(&self) -> &E {
        &self.inner.env.env
    }

    /// The configuration.
    pub fn config(&self) -> &BydConfig {
        &self.inner.config
    }

    /// The options the client was built with.
    pub fn options(&self) -> &ClientOptions {
        &self.inner.options
    }

    fn context<'a>(&'a self, session: &'a Session) -> Context<'a, Observed<E>> {
        Context::new(&self.inner.env, &self.inner.config, session)
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// Makes the client usable again after [`async_close`](Self::async_close)
    /// (pyBYD's `async_start`). A new client is already started.
    pub fn async_start(&self) {
        self.state().closed = false;
    }

    /// Closes and forgets the cars, stops MQTT, drops the capability cache
    /// and makes every call fail until [`async_start`](Self::async_start)
    /// (pyBYD's `async_close`).
    pub fn async_close(&self) {
        self.close_cars();
        self.state().capabilities.clear();
        self.stop_mqtt();
        self.state().closed = true;
    }

    /// pyBYD's `_require_transport`.
    fn require_transport(&self) -> Result<(), Error> {
        if self.state().closed {
            return Err(not_initialized());
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Sessions
    // -----------------------------------------------------------------------

    /// Logs in with the configured password and starts MQTT (pyBYD's `login`).
    pub async fn login(&self) -> Result<(), Error> {
        self.login_session().await.map(drop)
    }

    async fn login_session(&self) -> Result<Arc<Session>, Error> {
        self.require_transport()?;
        let env = &self.inner.env;
        let config = &self.inner.config;
        // A login refused for the clock (1003) is tried once more: the
        // transport has taken BYD's clock from the reply's `Date` header.
        let mut attempt = 0;
        let token = loop {
            let now_ms = env.now_ms() + env.server_time_offset_ms();
            let random = env.random_bytes();
            let outer = build_login_request(config, now_ms, &random, env.now_ms());
            let response = env.post_secure(LOGIN_ENDPOINT, &outer).await?;
            match parse_login_response(&response, &config.password) {
                Ok(token) => break token,
                Err(error) if error.is_a(ErrorKind::BydServerTimeError) && attempt == 0 => {
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        };
        let session = token.to_session(env.monotonic(), session_ttl(config));
        Ok(self.install_session(session).await)
    }

    /// pyBYD's `_refresh_or_relogin`: new tokens from `/app/token/refresh`
    /// when the session has a refresh token, else (or when that fails with
    /// a pyBYD error) a fresh session.
    ///
    /// The refreshed session keeps the guest state but, as in pyBYD, the
    /// client's guest map stays the old session's until the next login.
    pub(crate) async fn refresh_or_relogin(&self) -> Result<(), Error> {
        let session = self.state().session.clone();
        self.refresh_or_relogin_from(session).await
    }

    /// [`Self::refresh_or_relogin`] for the session `current` (read by the
    /// caller in the same step as its own checks). The refreshed session
    /// replaces it only while `current` is still the client's session and
    /// the client is open, and a failed refresh drops it only then.
    ///
    /// pyBYD, single-threaded, assigns the refreshed session after its await
    /// unconditionally. openbyd's tasks really run in parallel, so meanwhile
    /// another one may have installed a session (kept, and `ensure_session`
    /// uses it rather than logging in again), cleared it
    /// (`invalidate_session`, or a failed refresh now logging in) or closed
    /// the client; a late refresh then installs nothing.
    async fn refresh_or_relogin_from(&self, current: Option<Arc<Session>>) -> Result<(), Error> {
        let still_current = |state: &State, session: &Arc<Session>| {
            !state.closed
                && state
                    .session
                    .as_ref()
                    .is_some_and(|installed| Arc::ptr_eq(installed, session))
        };
        let Some(session) = current else {
            return self.ensure_session().await.map(drop);
        };
        if !session.refresh_token().is_empty() && !self.state().closed {
            match login::refresh_session_token(self.context(&session)).await {
                Ok(token) => {
                    let refreshed = Arc::new(session.with_refreshed_tokens(
                        &token.sign_token,
                        &token.encry_token,
                        Some(&token.refresh_token),
                        self.inner.env.monotonic(),
                    ));
                    let had_runtime = {
                        let mut state = self.state();
                        if still_current(&state, &session) {
                            state.session = Some(Arc::clone(&refreshed));
                            state.mqtt_started
                        } else {
                            false
                        }
                    };
                    if had_runtime {
                        self.inner.env.update_mqtt_key(refreshed.content_key());
                    }
                    return Ok(());
                }
                Err(error) if error.is_a(ErrorKind::BydError) => {
                    log::debug!("Token refresh failed; falling back to full re-login: {error}");
                }
                Err(error) => return Err(error),
            }
        }
        {
            let mut state = self.state();
            if still_current(&state, &session) {
                state.session = None;
            }
        }
        self.ensure_session().await.map(drop)
    }

    /// pyBYD's `_recover_busy_session`: whether a 1008 from a read-only
    /// vehicle endpoint was really an invalidated session (BYD's
    /// single-session rule answers 1008 on vehicle endpoints while account
    /// endpoints report the expiry), and the session was recovered so the
    /// call can be tried once more.
    ///
    /// It asks the vehicle list with the current session, at most every
    /// [`BUSY_PROBE_COOLDOWN_S`] and not within [`BUSY_REAUTH_COOLDOWN_S`] of
    /// a recovery it started: 1030 refreshes the tokens, an expired session
    /// logs in again. A session another call already replaced counts as
    /// recovered. Any other outcome keeps the 1008.
    pub(crate) async fn recover_busy_session(&self, error: &Error) -> bool {
        if error.code() != "1008" {
            return false;
        }
        let session = {
            let state = self.state();
            match (&state.session, state.closed) {
                (Some(session), false) => Arc::clone(session),
                _ => return false,
            }
        };
        let now = self.inner.env.monotonic();
        {
            let mut state = self.state();
            if state
                .busy_reauth_last_at
                .is_some_and(|last| now - last < BUSY_REAUTH_COOLDOWN_S)
            {
                log::debug!("Skipping 1008 session probe (re-auth cooldown active)");
                return false;
            }
            if state
                .busy_probe_last_at
                .is_some_and(|last| now - last < BUSY_PROBE_COOLDOWN_S)
            {
                log::debug!("Skipping 1008 session probe (probe cooldown active)");
                return false;
            }
            state.busy_probe_last_at = Some(now);
        }
        let probe = vehicle::fetch_vehicle_list(self.context(&session)).await;
        let probe_error = match probe {
            Ok(_) => {
                log::debug!("1008 session probe succeeded: the session is valid");
                return false;
            }
            Err(probe_error) if probe_error.is_a(ErrorKind::BydSessionExpiredError) => probe_error,
            Err(probe_error) => {
                log::debug!("1008 session probe failed ({probe_error}); keeping the 1008");
                return false;
            }
        };
        let refresh = probe_error.is_a(ErrorKind::BydTokenRefreshError);
        let Some(current) = self.claim_busy_reauth(&session, !refresh) else {
            return true;
        };
        let recovered = if refresh {
            log::info!("1008 reflects a refreshable session; refreshing it");
            self.refresh_or_relogin_from(current).await
        } else {
            log::info!("1008 reflects an invalidated session; logging in again");
            self.ensure_session().await.map(drop)
        };
        recovered.is_ok()
    }

    /// The step after a 1008 probe found the session expired, taken under
    /// one lock as pyBYD's has no await in it. `None` when another call
    /// replaced the session meanwhile with a live one, which is then used.
    /// Otherwise it stamps the re-auth cooldown, drops the session if
    /// `drop_probed` and it is still the `probed` one, and returns the
    /// session the recovery starts from.
    fn claim_busy_reauth(
        &self,
        probed: &Arc<Session>,
        drop_probed: bool,
    ) -> Option<Option<Arc<Session>>> {
        let mut state = self.state();
        let now = self.inner.env.monotonic();
        let is_probed = |installed: &Arc<Session>| Arc::ptr_eq(installed, probed);
        if state
            .session
            .as_ref()
            .is_some_and(|current| !is_probed(current) && !current.is_expired(now))
        {
            return None;
        }
        state.busy_reauth_last_at = Some(now);
        let current = state.session.clone();
        if drop_probed && state.session.as_ref().is_some_and(is_probed) {
            state.session = None;
        }
        Some(current)
    }

    /// The second half of pyBYD's `login`: the new session takes over the
    /// guest map, MQTT gets the new key and is restarted.
    async fn install_session(&self, session: Session) -> Arc<Session> {
        let session = Arc::new(session);
        let had_runtime = {
            let mut state = self.state();
            for (vin, guest) in state.guest_map() {
                session.set_guest(&vin, guest);
            }
            state.session = Some(Arc::clone(&session));
            state.guest_owner = Some(Arc::clone(&session));
            state.mqtt_started
        };
        if had_runtime {
            self.inner.env.update_mqtt_key(session.content_key());
        }
        self.stop_mqtt();
        self.ensure_mqtt_started().await;
        session
    }

    /// The current session, logging in (as the [`ReloginPolicy`] says) when
    /// there is none or it has expired (pyBYD's `ensure_session`).
    pub async fn ensure_session(&self) -> Result<Arc<Session>, Error> {
        let current = self.state().session.clone();
        if let Some(session) = current
            && !session.is_expired(self.inner.env.monotonic())
        {
            return Ok(session);
        }
        self.relogin().await
    }

    /// A new session as the [`ReloginPolicy`] says.
    async fn relogin(&self) -> Result<Arc<Session>, Error> {
        match &self.inner.options.relogin {
            ReloginPolicy::Password => self.login_session().await,
            ReloginPolicy::Never => Err(Error::api(
                ErrorKind::BydSessionExpiredError,
                "Session expired and re-login is disabled for this client",
                RELOGIN_DISABLED,
                "",
            )),
            ReloginPolicy::Handler(handler) => {
                let tokens = handler().await?;
                Ok(self.import_session(tokens).await)
            }
        }
    }

    /// Forgets the session: the next call logs in again (pyBYD's
    /// `invalidate_session`). Command access stays verified.
    pub fn invalidate_session(&self) {
        self.state().session = None;
    }

    /// The current session, if any, without checking its age.
    pub fn session(&self) -> Option<Arc<Session>> {
        self.state().session.clone()
    }

    /// Installs a saved session as if a login had returned it (the guest
    /// map is the saved one, merged into what the client knows) and starts
    /// MQTT with it. Returns the session.
    pub async fn import_session(&self, tokens: SessionTokens) -> Arc<Session> {
        {
            let mut state = self.state();
            for (vin, guest) in &tokens.guest_vins {
                match &state.guest_owner {
                    Some(session) => session.set_guest(vin, *guest),
                    None => {
                        state.guest_vins.insert(vin.clone(), *guest);
                    }
                }
            }
        }
        let ttl = tokens.ttl_remaining.unwrap_or(f64::INFINITY);
        let session = Session::new(
            &tokens.user_id,
            &tokens.sign_token,
            &tokens.encry_token,
            self.inner.env.monotonic(),
            ttl,
        )
        .with_refresh_token(&tokens.refresh_token);
        self.install_session(session).await
    }

    /// Whether the account reaches each VIN as a guest, as the client knows
    /// it now. Unlike [`export_session`](Self::export_session) it is there
    /// after [`invalidate_session`](Self::invalidate_session), so guest flips
    /// learnt during a refused call can still be saved (openbyd's addition).
    pub fn guest_vins(&self) -> HashMap<String, bool> {
        self.state().guest_map()
    }

    /// The current session as an application stores it, or `None` when
    /// logged out.
    pub fn export_session(&self) -> Option<SessionTokens> {
        let (session, guest_vins) = {
            let state = self.state();
            (state.session.clone()?, state.guest_map())
        };
        let remaining = session.ttl() - session.age(self.inner.env.monotonic());
        Some(SessionTokens {
            user_id: session.user_id().to_owned(),
            sign_token: session.sign_token().to_owned(),
            encry_token: session.encry_token().to_owned(),
            refresh_token: session.refresh_token().to_owned(),
            ttl_remaining: remaining.is_finite().then_some(remaining.max(0.0)),
            guest_vins,
        })
    }

    // -----------------------------------------------------------------------
    // Vehicles and caches
    // -----------------------------------------------------------------------

    /// The account's vehicles. Refreshes the metadata cache and seeds the
    /// guest map for VINs it does not know yet (pyBYD's `get_vehicles`).
    pub async fn get_vehicles(&self) -> Result<Vec<Vehicle>, Error> {
        let vehicles = authed!(self, |cx| vehicle::fetch_vehicle_list(cx))?;
        let mut state = self.state();
        state.vehicles.clear();
        for vehicle in vehicles.iter().filter(|vehicle| !vehicle.vin.is_empty()) {
            state.put_vehicle(vehicle.clone());
        }
        // A car shared with this account must be addressed as a guest; the
        // list gives the first guess and never overrides what the cloud's
        // answers already settled.
        for vehicle in vehicles.iter().filter(|vehicle| !vehicle.vin.is_empty()) {
            let guest = vehicle.empower_id.is_some_and(|id| id != 0) || vehicle.is_shared();
            state.guest_setdefault(&vehicle.vin, guest);
        }
        Ok(vehicles)
    }

    /// Caches metadata for `vin` as if the vehicle list had returned it, so
    /// realtime, energy and AC-limit reads need no vehicle list.
    /// `energy_type` is BYD's value (0 EV, 1 ICE, 2 hybrid).
    pub fn seed_vehicle_metadata(
        &self,
        vin: &str,
        energy_type: EnergyType,
        model_name: Option<&str>,
    ) -> Result<(), Error> {
        let mut raw = json!({"vin": vin, "energyType": energy_type.value().to_string()});
        if let Some(model_name) = model_name {
            raw["modelName"] = json!(model_name);
        }
        let vehicle: Vehicle = parse_model(&raw)?;
        self.state().put_vehicle(vehicle);
        Ok(())
    }

    /// The cached metadata of `vin`, if the vehicle list or a seed gave it.
    pub fn vehicle_metadata(&self, vin: &str) -> Option<Vehicle> {
        self.state().vehicle(vin).cloned()
    }

    /// pyBYD's `_guest_permissions_for`: whether the cached vehicle is
    /// shared with this account, and the permission codes its grant holds
    /// (`(false, None)` for an owner or an unknown car).
    pub(crate) fn guest_permissions_for(&self, vin: &str) -> (bool, Option<BTreeSet<String>>) {
        match self.state().vehicle(vin) {
            Some(cached) if cached.is_shared() => (true, Some(cached.granted_permission_codes())),
            _ => (false, None),
        }
    }

    /// A shared guest's grant in detail (pyBYD's `get_empower_detail`).
    ///
    /// Without `empower_id`, the cached vehicle's is used (the vehicle list
    /// is fetched once on a miss); a car with none fails with
    /// `BydEndpointNotSupportedError` (code `"not_shared_vehicle"`).
    pub async fn get_empower_detail(
        &self,
        vin: &str,
        empower_id: Option<i64>,
    ) -> Result<EmpowerDetail, Error> {
        let mut resolved = empower_id;
        if resolved.is_none() {
            let mut cached = self.vehicle_metadata(vin);
            if cached.is_none() {
                self.get_vehicles().await?;
                cached = self.vehicle_metadata(vin);
            }
            resolved = cached.and_then(|vehicle| vehicle.empower_id);
        }
        let Some(empower_id) = resolved else {
            return Err(Error::api(
                ErrorKind::BydEndpointNotSupportedError,
                format!(
                    "get_empower_detail requires empower_id (VIN {vin} is not a shared guest grant)"
                ),
                "not_shared_vehicle",
                vehicle::EMPOWER_DETAIL_ENDPOINT,
            ));
        };
        authed!(self, probe, |cx| vehicle::fetch_empower_detail_by_vin(
            cx, vin, empower_id
        ))
    }

    /// pyBYD's `_energy_type_for`: the cached energy type, fetching the
    /// vehicle list once on a miss; EV when unknown.
    async fn energy_type_for(&self, vin: &str) -> Result<EnergyType, Error> {
        let mut cached = self.vehicle_metadata(vin);
        if cached.is_none() {
            self.get_vehicles().await?;
            cached = self.vehicle_metadata(vin);
        }
        Ok(match cached {
            Some(vehicle) if vehicle.energy_type != EnergyType::Unknown => vehicle.energy_type,
            _ => EnergyType::Ev,
        })
    }

    /// pyBYD's `_vehicle_fun_learn_info_for`: the cached learn info, `None`
    /// when unknown or empty (a failing vehicle list counts as unknown).
    async fn vehicle_fun_learn_info_for(&self, vin: &str) -> Option<Map<String, Value>> {
        let mut cached = self.vehicle_metadata(vin);
        if cached.is_none() {
            if self.get_vehicles().await.is_err() {
                log::debug!("get_vehicles failed while resolving learn_info");
                return None;
            }
            cached = self.vehicle_metadata(vin);
        }
        let info: Map<String, Value> = cached?
            .vehicle_fun_learn_info
            .iter()
            .map(|(key, value)| (key.clone(), json!(value)))
            .collect();
        (!info.is_empty()).then_some(info)
    }

    /// The latest config of each VIN the cloud knows (pyBYD's `get_latest_configs`).
    pub async fn get_latest_configs(
        &self,
        vins: &[String],
    ) -> Result<Vec<(String, VehicleLatestConfig)>, Error> {
        authed!(self, probe, |cx| latest_config::fetch_latest_config(
            cx, vins
        ))
    }

    /// The latest config of one VIN; `BydDataUnavailableError` (code
    /// `"latest_config_missing"`) when the reply has none (pyBYD's `get_latest_config`).
    pub async fn get_latest_config(&self, vin: &str) -> Result<VehicleLatestConfig, Error> {
        let configs = self.get_latest_configs(&[vin.to_owned()]).await?;
        configs
            .into_iter()
            .find(|(key, _)| key == vin)
            .map(|(_, config)| config)
            .ok_or_else(|| {
                Error::api(
                    ErrorKind::BydDataUnavailableError,
                    format!("Latest config unavailable for VIN {vin}"),
                    "latest_config_missing",
                    LATEST_CONFIG_ENDPOINT,
                )
            })
    }

    /// What the vehicle supports, cached per VIN; a failing fetch gives
    /// "unknown" capabilities (reason `"fetch_error"`), cached too (pyBYD's
    /// `get_vehicle_capabilities`).
    pub async fn get_vehicle_capabilities(
        &self,
        vin: &str,
        force_refresh: bool,
    ) -> VehicleCapabilities {
        if !force_refresh && let Some(cached) = self.state().capabilities.get(vin).cloned() {
            return cached;
        }
        let capabilities = match self.get_latest_config(vin).await {
            Ok(latest) => VehicleCapabilities::from_latest_config(vin, &latest),
            Err(_) => {
                log::debug!("Failed to build capabilities");
                VehicleCapabilities::unknown(vin, "fetch_error")
            }
        };
        self.state()
            .capabilities
            .insert(vin.to_owned(), capabilities.clone());
        capabilities
    }

    // -----------------------------------------------------------------------
    // Reads
    // -----------------------------------------------------------------------

    /// pyBYD's `_trigger_and_poll`: trigger, wait for the MQTT push, then
    /// poll over HTTP. Returns the payload to validate.
    async fn trigger_and_poll(&self, read: TriggeredRead<'_>) -> Result<Value, Error> {
        let session = self.ensure_session().await?;
        self.require_transport()?;
        let cx = self.context(&session);
        let fetch = &read.fetch;
        let (trigger_info, serial) = fetch
            .fetch(cx, read.trigger_endpoint, read.vin, None)
            .await?;
        if let Some(dispatched) = read.dispatched {
            dispatched.store(true, Ordering::SeqCst);
        }
        let merged = match &trigger_info {
            Value::Object(_) => trigger_info.clone(),
            _ => json!({}),
        };
        if let Value::Object(info) = &trigger_info
            && fetch.is_ready(info)?
        {
            return Ok(merged);
        }
        if !truthy(&serial) {
            return Ok(merged);
        }
        self.add_data_poll_serial(&serial)?;
        let result = self.wait_then_poll(cx, &read, merged, serial.clone()).await;
        self.discard_data_poll_serial(&serial);
        result
    }

    async fn wait_then_poll(
        &self,
        cx: Context<'_, Observed<E>>,
        read: &TriggeredRead<'_>,
        mut merged: Value,
        mut serial: Value,
    ) -> Result<Value, Error> {
        let fetch = &read.fetch;
        let pushed = self
            .mqtt_wait(
                read.vin,
                read.mqtt_event_type,
                Some(&serial),
                read.mqtt_timeout,
            )
            .await?;
        if let Some(raw) = &pushed {
            if fetch.is_ready(raw)? {
                log::debug!("{} data received via MQTT", read.label);
                return Ok(Value::Object(raw.clone()));
            }
            if let Some(Value::String(code)) = raw.get("code")
                && MQTT_DATA_UNAVAILABLE_CODES.contains(&code.as_str())
            {
                return Ok(merged);
            }
        }
        let mut unavailable = 0;
        let delays = poll_delays(
            read.poll_attempts,
            read.poll_interval,
            &COMMAND_POLL_DELAYS_S,
        );
        for delay in delays {
            if delay > 0.0 {
                cx.env.sleep(delay).await;
            }
            match fetch
                .fetch(cx, read.poll_endpoint, read.vin, Some(&serial))
                .await
            {
                Ok((latest, next)) => {
                    serial = next;
                    unavailable = 0;
                    if latest.is_object() {
                        merged = latest.clone();
                    }
                    if let Value::Object(latest) = &latest
                        && fetch.is_ready(latest).unwrap_or(false)
                    {
                        break;
                    }
                }
                Err(error) if error.is_a(ErrorKind::BydSessionExpiredError) => return Err(error),
                Err(error) if error.is_a(ErrorKind::BydDataUnavailableError) => {
                    unavailable += 1;
                    if unavailable >= read.signal_retries {
                        break;
                    }
                }
                Err(_) => unavailable = 0,
            }
        }
        Ok(merged)
    }

    /// pyBYD's `self._data_poll_serials.add(serial)`: a list or object is
    /// unhashable (`TypeError`).
    fn add_data_poll_serial(&self, serial: &Value) -> Result<(), Error> {
        let key = push::hash_key(serial).ok_or_else(|| {
            Error::new(ErrorKind::TypeError, "unhashable type for a request serial")
        })?;
        let mut state = self.state();
        if !state
            .data_poll_serials
            .iter()
            .any(|known| push::hash_key(known).as_deref() == Some(key.as_str()))
        {
            state.data_poll_serials.push(serial.clone());
        }
        Ok(())
    }

    fn discard_data_poll_serial(&self, serial: &Value) {
        let key = push::hash_key(serial);
        self.state()
            .data_poll_serials
            .retain(|known| push::hash_key(known) != key);
    }

    /// Realtime data: triggers a refresh, waits for the MQTT `vehicleInfo`
    /// push, and polls over HTTP if it does not come (pyBYD's
    /// `get_vehicle_realtime`).
    pub async fn get_vehicle_realtime(
        &self,
        vin: &str,
        settings: PollSettings,
    ) -> Result<VehicleRealtimeData, Error> {
        let energy_type = self.energy_type_for(vin).await?;
        let context = json!({"energy_type": {"EnergyType": energy_type.value()}});
        reauth!(
            self,
            true,
            true,
            async {
                let raw = self
                    .trigger_and_poll(TriggeredRead {
                        vin,
                        trigger_endpoint: "/vehicleInfo/vehicle/vehicleRealTimeRequest",
                        poll_endpoint: "/vehicleInfo/vehicle/vehicleRealTimeResult",
                        fetch: ReadFetch::Realtime(energy_type),
                        label: "Realtime",
                        mqtt_event_type: Some("vehicleInfo"),
                        mqtt_timeout: settings.mqtt_timeout,
                        poll_attempts: settings.poll_attempts,
                        poll_interval: Some(settings.poll_interval),
                        signal_retries: settings.signal_retries,
                        dispatched: None,
                    })
                    .await?;
                VehicleRealtimeData::parse_owned(raw, &context)
                    .map_err(crate::api::vehicle::model_error)
            }
            .await
        )
    }

    /// The vehicle's position: triggers a fix, waits for the MQTT push with
    /// its serial, and polls over HTTP if it does not come (pyBYD's `get_gps_info`).
    ///
    /// A shared guest whose grant (in the cached vehicle list) lacks the
    /// location permission fails with `BydEndpointNotSupportedError` (code
    /// `"guest_permission_denied"`) before anything is sent.
    pub async fn get_gps_info(&self, vin: &str, settings: PollSettings) -> Result<GpsInfo, Error> {
        if let Some(cached) = self.vehicle_metadata(vin)
            && cached.is_shared()
            && !cached.has_permission(GUEST_PERMISSION_GPS)
        {
            return Err(Error::api(
                ErrorKind::BydEndpointNotSupportedError,
                format!(
                    "get_gps_info blocked for shared VIN {vin}: guest permission \
                     '{GUEST_PERMISSION_GPS}' not granted"
                ),
                "guest_permission_denied",
                "/control/getGpsInfo",
            ));
        }
        reauth!(
            self,
            true,
            true,
            async {
                let raw = self
                    .trigger_and_poll(TriggeredRead {
                        vin,
                        trigger_endpoint: "/control/getGpsInfo",
                        poll_endpoint: "/control/getGpsInfoResult",
                        fetch: ReadFetch::Gps,
                        label: "GPS",
                        mqtt_event_type: None,
                        mqtt_timeout: settings.mqtt_timeout,
                        poll_attempts: settings.poll_attempts,
                        poll_interval: Some(settings.poll_interval),
                        signal_retries: settings.signal_retries,
                        dispatched: None,
                    })
                    .await?;
                parse_model(&raw)
            }
            .await
        )
    }

    /// Climate status (pyBYD's `get_hvac_status`).
    pub async fn get_hvac_status(&self, vin: &str) -> Result<HvacStatus, Error> {
        authed!(self, probe, |cx| hvac::fetch_hvac_status(cx, vin))
    }

    /// Charging status (pyBYD's `get_charging_status`).
    pub async fn get_charging_status(&self, vin: &str) -> Result<ChargingStatus, Error> {
        authed!(self, probe, |cx| charging::fetch_charging_status(cx, vin))
    }

    /// The smart charging schedule (pyBYD's `get_charging_schedule`).
    pub async fn get_charging_schedule(&self, vin: &str) -> Result<SmartChargingSchedule, Error> {
        authed!(self, probe, |cx| smart_charging::fetch_charging_schedule(
            cx, vin
        ))
    }

    /// Live charging state and schedule from one `homePage` call (pyBYD's
    /// `get_charging_homepage`).
    pub async fn get_charging_homepage(
        &self,
        vin: &str,
    ) -> Result<(ChargingStatus, SmartChargingSchedule), Error> {
        let raw = authed!(self, probe, |cx| charging::fetch_charging_homepage(cx, vin))?;
        let status: ChargingStatus = parse_model(&Value::Object(raw.clone()))?;
        let schedule: SmartChargingSchedule =
            parse_model(&crate::api::control::with_vin(vin, &raw, false))?;
        Ok((status, schedule))
    }

    /// Supported and active AC charging current limits; `energy_type`
    /// defaults to the vehicle's own (pyBYD's `get_ac_current_limit`).
    pub async fn get_ac_current_limit(
        &self,
        vin: &str,
        energy_type: Option<EnergyTypeArg>,
    ) -> Result<AcCurrentLimit, Error> {
        let energy_type = match energy_type {
            Some(energy_type) => energy_type,
            None => self.energy_type_for(vin).await?.into(),
        };
        authed!(self, probe, |cx| smart_charging::fetch_ac_current_limit(
            cx,
            vin,
            energy_type.clone()
        ))
    }

    /// Energy consumption, asked with the vehicle's energy type and model
    /// (pyBYD's `get_energy_consumption`).
    pub async fn get_energy_consumption(&self, vin: &str) -> Result<EnergyConsumption, Error> {
        let power_type = self.energy_type_for(vin).await?;
        let auto_model_name = self.vehicle_metadata(vin).and_then(|vehicle| {
            [vehicle.out_model_type, vehicle.model_name]
                .into_iter()
                .find(|name| !name.is_empty())
        });
        authed!(self, probe, |cx| energy::fetch_energy_consumption(
            cx,
            vin,
            power_type,
            auto_model_name.as_deref()
        ))
    }

    /// Push notification state (pyBYD's `get_push_state`).
    pub async fn get_push_state(&self, vin: &str) -> Result<PushNotificationState, Error> {
        authed!(self, probe, |cx| push_notifications::fetch_push_state(
            cx, vin
        ))
    }

    /// Turns a push notification type on or off; pyBYD's default type is
    /// 701, the vehicle status push (pyBYD's `set_push_state`).
    pub async fn set_push_state(
        &self,
        vin: &str,
        enable: bool,
        push_type: i64,
    ) -> Result<CommandAck, Error> {
        authed!(self, |cx| push_notifications::set_push_state(
            cx, vin, enable, push_type
        ))
    }

    /// The account's unread inbox messages of one category (pyBYD's
    /// `get_notifications`; its default category is `VEHICLE`).
    pub async fn get_notifications(
        &self,
        category: NotificationCategory,
    ) -> Result<Vec<VehicleMsgItemInfo>, Error> {
        let response = authed!(self, |cx| message_center::fetch_notifications(cx, category))?;
        Ok(response.messages)
    }

    /// The unread inbox messages of every category, in pyBYD's order
    /// (pyBYD's `get_all_notifications`).
    pub async fn get_all_notifications(
        &self,
    ) -> Result<Vec<(NotificationCategory, Vec<VehicleMsgItemInfo>)>, Error> {
        let mut all = Vec::new();
        for category in [NotificationCategory::Vehicle, NotificationCategory::System] {
            all.push((category, self.get_notifications(category).await?));
        }
        Ok(all)
    }

    /// Renames the vehicle (pyBYD's `rename_vehicle`).
    pub async fn rename_vehicle(&self, vin: &str, name: &str) -> Result<CommandAck, Error> {
        authed!(self, |cx| vehicle_settings::rename_vehicle(cx, vin, name))
    }

    /// The scheduled-climate timers (pyBYD's `get_booking_list`).
    pub async fn get_booking_list(&self, vin: &str) -> Result<BookingList, Error> {
        authed!(self, probe, |cx| booking::fetch_booking_list(cx, vin))
    }

    /// The OTA firmware version, release notes and night auto-upgrade
    /// settings (pyBYD's `get_ota_version`).
    pub async fn get_ota_version(&self, vin: &str) -> Result<OtaUpgradeInfo, Error> {
        authed!(self, probe, |cx| ota::fetch_ota_version(cx, vin))
    }

    /// Turns the night auto-upgrade on or off and sets its time, `H:mm`
    /// (pyBYD's `set_ota_auto_upgrade`; its default time is `"2:00"`).
    /// Needs [`commands_enabled`](Self::commands_enabled) and a PIN, as a
    /// command does; `command_pwd` defaults to the config's `control_pin`.
    pub async fn set_ota_auto_upgrade(
        &self,
        vin: &str,
        enabled: bool,
        auto_upgrade_time: &str,
        command_pwd: Option<&str>,
    ) -> Result<CommandAck, Error> {
        let resolved = self.require_command_pwd(command_pwd)?;
        authed!(self, |cx| ota::sync_auto_upgrade(
            cx,
            vin,
            enabled,
            auto_upgrade_time,
            &resolved
        ))
    }

    // -----------------------------------------------------------------------
    // MQTT start and stop
    // -----------------------------------------------------------------------

    /// Starts MQTT if enabled and not running; failures are logged, never
    /// raised (pyBYD's `_ensure_mqtt_started`).
    async fn ensure_mqtt_started(&self) {
        if !self.inner.config.mqtt_enabled || self.mqtt_is_running() {
            return;
        }
        let session = {
            let state = self.state();
            if state.closed {
                return;
            }
            state.session.clone()
        };
        let Some(session) = session else {
            return;
        };
        let bootstrap = match fetch_mqtt_bootstrap(self.context(&session)).await {
            Ok(bootstrap) => bootstrap,
            Err(error) => {
                log::debug!("MQTT startup failed: {error}");
                return;
            }
        };
        let sink: Weak<dyn MqttSink> = self.inner.me.clone();
        match self
            .inner
            .env
            .start_mqtt(
                &bootstrap,
                session.content_key(),
                self.inner.config.mqtt_keepalive,
                sink,
            )
            .await
        {
            Ok(()) => self.state().mqtt_started = true,
            Err(error) => log::debug!("MQTT startup failed: {error}"),
        }
    }

    /// Starts MQTT for the current session if it is not running, as a login
    /// does. For a client whose MQTT stopped (see the recovery ceiling).
    pub async fn start_mqtt(&self) {
        self.ensure_mqtt_started().await;
    }

    /// Whether the MQTT channel runs.
    pub fn mqtt_is_running(&self) -> bool {
        self.state().mqtt_started && self.inner.env.mqtt_running()
    }

    /// Stops MQTT and ends every pending wait as if it timed out, so the
    /// caller polls over HTTP instead (pyBYD's `_stop_mqtt`, since #98).
    pub fn stop_mqtt(&self) {
        let (started, waiters) = {
            let mut state = self.state();
            let started = std::mem::take(&mut state.mqtt_started);
            (started, std::mem::take(&mut state.waiters))
        };
        if started {
            self.inner.env.stop_mqtt();
        }
        for waiter in waiters {
            let _ = waiter.slot.send(None);
        }
    }
}

/// One `_trigger_and_poll` call's arguments.
struct TriggeredRead<'a> {
    vin: &'a str,
    trigger_endpoint: &'a str,
    poll_endpoint: &'a str,
    fetch: ReadFetch<'a>,
    label: &'a str,
    mqtt_event_type: Option<&'a str>,
    mqtt_timeout: Option<f64>,
    poll_attempts: i64,
    /// `None` is the official ladder.
    poll_interval: Option<f64>,
    signal_retries: i64,
    /// pyBYD's `on_trigger_dispatched`: set once the trigger was accepted.
    dispatched: Option<&'a AtomicBool>,
}

/// The `fetch_fn` and `is_ready` of a `_trigger_and_poll`.
enum ReadFetch<'a> {
    Realtime(EnergyType),
    Gps,
    Charge(&'a ChangeChargeFetchFn),
    Save(&'a SaveChargingScheduleFetchFn),
    StandbySend(&'a SendFetchFn),
}

impl ReadFetch<'_> {
    /// Fetches `endpoint`; returns the reply and the serial to poll with next.
    async fn fetch<E: Env>(
        &self,
        cx: Context<'_, E>,
        endpoint: &str,
        vin: &str,
        serial: Option<&Value>,
    ) -> Result<(Value, Value), Error> {
        // `build_inner_base` sends `str(serial)` when it is truthy.
        let text = serial.filter(|serial| truthy(serial)).map(py_str);
        match self {
            Self::Realtime(energy_type) => {
                let (info, next) = realtime::fetch_realtime_endpoint(
                    cx,
                    endpoint,
                    vin,
                    text.as_deref(),
                    *energy_type,
                )
                .await?;
                Ok((Value::Object(info), next.unwrap_or(Value::Null)))
            }
            Self::Gps => {
                let (info, next) =
                    gps::fetch_gps_endpoint(cx, endpoint, vin, text.as_deref()).await?;
                Ok((info, next.map_or(Value::Null, Value::String)))
            }
            Self::Charge(fetch) => fetch.fetch(cx, endpoint, vin, serial).await,
            Self::Save(fetch) => fetch.fetch(cx, endpoint, vin, serial).await,
            Self::StandbySend(fetch) => fetch.fetch(cx, endpoint, vin, serial).await,
        }
    }

    fn is_ready(&self, raw: &Map<String, Value>) -> Result<bool, Error> {
        match self {
            Self::Realtime(_) => VehicleRealtimeData::is_ready_raw(&Value::Object(raw.clone()))
                .map_err(crate::api::control::model_error),
            Self::Gps => Ok(gps::is_gps_info_ready(raw)),
            Self::Charge(_) | Self::Save(_) => Ok(is_charge_change_ready(raw)),
            Self::StandbySend(_) => Ok(is_send_ready(raw)),
        }
    }
}

impl<E: ClientEnv> MqttSink for Inner<E> {
    fn on_event(&self, event: &MqttEvent) {
        if let Some(inner) = self.me.upgrade() {
            BydClient { inner }.on_mqtt_event(event);
        }
    }

    fn on_decrypt_error(&self) {
        if let Some(inner) = self.me.upgrade() {
            BydClient { inner }.schedule_mqtt_reauth();
        }
    }

    fn on_connack_refused(&self, consecutive: u32) {
        if let Some(inner) = self.me.upgrade() {
            BydClient { inner }.on_mqtt_connack_refused(consecutive);
        }
    }

    fn on_connected(&self) {
        if let Some(inner) = self.me.upgrade() {
            BydClient { inner }.on_mqtt_connected();
        }
    }

    fn on_disconnected(&self, reason: &str) {
        if let Some(inner) = self.me.upgrade() {
            BydClient { inner }.on_mqtt_disconnected(reason);
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use openbyd_protocol::aes::aes_encrypt_hex;

    use super::*;
    use crate::scripted::{MqttDelivery, Reply, ScriptedEnv, ScriptedMqtt, block_on};

    const VIN: &str = "TESTVIN0000000001";

    fn config() -> BydConfig {
        let mut config = BydConfig::new("user@example.com", "p");
        config.mqtt_enabled = false;
        config
    }

    fn scripted(replies: Vec<Reply>, options: ClientOptions) -> BydClient<Arc<ScriptedEnv>> {
        BydClient::new(
            config(),
            Arc::new(ScriptedEnv::new(1_000_000, "s", replies)),
            options,
        )
    }

    fn tokens(ttl_remaining: Option<f64>) -> SessionTokens {
        SessionTokens {
            user_id: "1".into(),
            sign_token: "sign".into(),
            encry_token: "encry".into(),
            refresh_token: String::new(),
            ttl_remaining,
            guest_vins: HashMap::from([(VIN.to_owned(), true)]),
        }
    }

    fn sealed(data: Value) -> Reply {
        let key = openbyd_protocol::hashing::md5_hex("encry");
        Reply::Outer(json!({
            "code": "0",
            "respondData": aes_encrypt_hex(&data.to_string(), &key).unwrap(),
        }))
    }

    fn posts(client: &BydClient<Arc<ScriptedEnv>>) -> usize {
        client
            .env()
            .steps()
            .iter()
            .filter(|step| step.get("post").is_some())
            .count()
    }

    /// A client whose request observer applies `change` to the client's
    /// state when a request to `endpoint` completes: another task acting
    /// while this one awaits it.
    fn changed_during(
        endpoint: &'static str,
        change: impl Fn(&mut State) + Send + Sync + 'static,
        replies: Vec<Reply>,
        mut options: ClientOptions,
    ) -> BydClient<Arc<ScriptedEnv>> {
        let holder: Arc<std::sync::OnceLock<Weak<Inner<Arc<ScriptedEnv>>>>> = Arc::default();
        let seen = Arc::clone(&holder);
        options.request_observer = Some(Arc::new(move |record: &RequestRecord| {
            if record.endpoint == endpoint
                && let Some(inner) = seen.get().and_then(Weak::upgrade)
            {
                change(&mut BydClient { inner }.state());
            }
        }));
        let client = scripted(replies, options);
        holder.set(Arc::downgrade(&client.inner)).ok();
        client
    }

    /// [`changed_during`] installing `fresh` as the session.
    fn replaced_during(
        endpoint: &'static str,
        fresh: &Arc<Session>,
        replies: Vec<Reply>,
    ) -> BydClient<Arc<ScriptedEnv>> {
        let session = Arc::clone(fresh);
        let install = move |state: &mut State| state.session = Some(Arc::clone(&session));
        changed_during(endpoint, install, replies, ClientOptions::default())
    }

    fn sealed_for(encry_token: &str, data: Value) -> Reply {
        let key = openbyd_protocol::hashing::md5_hex(encry_token);
        Reply::Outer(json!({
            "code": "0",
            "respondData": aes_encrypt_hex(&data.to_string(), &key).unwrap(),
        }))
    }

    fn logins(client: &BydClient<Arc<ScriptedEnv>>) -> usize {
        client
            .env()
            .steps()
            .iter()
            .filter(|step| step["post"] == LOGIN_ENDPOINT)
            .count()
    }

    #[test]
    fn a_session_replaced_during_the_1008_probe_is_used_without_a_login() {
        // pyBYD's test_busy_1008_concurrent_session_refresh_skips_duplicate_relogin:
        // the probe says the session is gone, but another task has installed
        // a new one meanwhile; the call is retried with it, nobody logs in.
        let fresh = Arc::new(Session::new("1", "sign2", "encry2", 1_000.0, f64::INFINITY));
        let busy = Reply::Outer(json!({"code": "1008", "message": "busy"}));
        let expired = Reply::Outer(json!({"code": "1011", "message": "kicked"}));
        let client = replaced_during(
            "/app/account/getAllListByUserId",
            &fresh,
            vec![busy, expired, sealed_for("encry2", json!({"acSwitch": 1}))],
        );
        block_on(client.import_session(tokens(None))).unwrap();
        let hvac = block_on(client.get_hvac_status(VIN)).unwrap().unwrap();
        assert_eq!(hvac.ac_switch.map(|state| state.value()), Some(1));
        assert_eq!(logins(&client), 0);
        assert!(Arc::ptr_eq(&client.session().unwrap(), &fresh));
        assert_eq!(client.state().busy_reauth_last_at, None);
        assert_eq!(client.env().unused_replies(), 0);
    }

    #[test]
    fn a_session_replaced_during_a_token_refresh_is_kept() {
        // The refresh answers, but another task installed a session while it
        // was on its way: that one stays, the refreshed one is dropped.
        let fresh = Arc::new(Session::new("1", "sign2", "encry2", 1_000.0, f64::INFINITY));
        let refresh = sealed(json!({"signToken": "sign3", "encryToken": "encry3"}));
        let client = replaced_during(
            "/app/token/refresh",
            &fresh,
            vec![
                Reply::Outer(json!({"code": "1030", "message": "refresh"})),
                refresh,
                sealed_for("encry2", json!({"acSwitch": 1})),
            ],
        );
        let mut saved = tokens(None);
        saved.refresh_token = "R".into();
        block_on(client.import_session(saved)).unwrap();
        block_on(client.get_hvac_status(VIN)).unwrap().unwrap();
        assert!(Arc::ptr_eq(&client.session().unwrap(), &fresh));
        assert_eq!(logins(&client), 0);
    }

    #[test]
    fn a_refresh_after_the_session_was_cleared_installs_nothing() {
        // Another task drops the session (invalidate_session, or a failed
        // refresh that is logging in) while the refresh is on its way: the
        // late refreshed session is not installed, and the retry takes a
        // session the usual way (here the relogin handler's).
        let mut options = ClientOptions::default();
        options.relogin = ReloginPolicy::Handler(Arc::new(|| Box::pin(async { Ok(tokens(None)) })));
        let refresh = sealed(json!({"signToken": "sign3", "encryToken": "encry3"}));
        let client = changed_during(
            "/app/token/refresh",
            |state| state.session = None,
            vec![
                Reply::Outer(json!({"code": "1030", "message": "refresh"})),
                refresh,
                sealed(json!({"acSwitch": 1})),
            ],
            options,
        );
        let mut saved = tokens(None);
        saved.refresh_token = "R".into();
        block_on(client.import_session(saved)).unwrap();
        block_on(client.get_hvac_status(VIN)).unwrap().unwrap();
        let session = client.session().unwrap();
        assert_eq!(session.sign_token(), "sign");
        assert!(
            session.refresh_token().is_empty(),
            "the handler's session, not the refreshed one"
        );
    }

    #[test]
    fn session_tokens_debug_leaves_the_tokens_out() {
        let mut saved = tokens(Some(5.0));
        saved.refresh_token = "R-secret".into();
        let shown = format!("{saved:?}");
        for secret in ["sign", "encry", "R-secret"] {
            assert!(!shown.contains(&format!("\"{secret}\"")), "{shown}");
        }
        assert!(
            shown.contains("user_id: \"1\"") && shown.contains("Some(5.0)"),
            "{shown}"
        );
    }

    #[test]
    fn a_refresh_after_close_installs_nothing() {
        let refresh = sealed(json!({"signToken": "sign3", "encryToken": "encry3"}));
        let client = changed_during(
            "/app/token/refresh",
            |state| state.closed = true,
            vec![
                Reply::Outer(json!({"code": "1030", "message": "refresh"})),
                refresh,
            ],
            ClientOptions::default(),
        );
        let mut saved = tokens(None);
        saved.refresh_token = "R".into();
        block_on(client.import_session(saved)).unwrap();
        let _ = block_on(client.get_hvac_status(VIN));
        let kept = client.state().session.clone();
        assert!(kept.is_none_or(|session| session.sign_token() != "sign3"));
    }

    /// A client with a session and the MQTT channel running, with `mqtt` on
    /// its timeline; the re-login a decrypt failure asks for imports a session.
    fn mqtt_client(mqtt: Vec<ScriptedMqtt>) -> BydClient<Arc<ScriptedEnv>> {
        let mut options = ClientOptions::default();
        options.relogin = ReloginPolicy::Handler(Arc::new(|| Box::pin(async { Ok(tokens(None)) })));
        let env = Arc::new(ScriptedEnv::new(1_000_000, "s", vec![]).with_mqtt(mqtt, vec![]));
        let client = BydClient::new(config(), env, options);
        block_on(client.import_session(tokens(None))).unwrap();
        client.state().mqtt_started = true;
        let sink: Weak<dyn MqttSink> = client.inner.me.clone();
        client.env().install_mqtt(sink);
        client
    }

    #[test]
    fn a_wait_after_mqtt_stopped_does_not_wait() {
        let client = mqtt_client(vec![]);
        client.stop_mqtt();
        let waited = block_on(client.mqtt_wait(VIN, None, None, Some(30.0))).unwrap();
        assert_eq!(waited.unwrap(), None);
        assert!(client.state().waiters.is_empty());
        assert!(
            client
                .env()
                .steps()
                .iter()
                .all(|step| step.get("mqtt_wait").is_none())
        );
    }

    #[test]
    fn stopping_mqtt_ends_a_wait_registered_before_it() {
        // A decrypt failure during the wait asks for a re-login, which
        // restarts MQTT: the wait ends then, not after its 30 seconds.
        let failure = ScriptedMqtt {
            index: 0,
            at: 1.0,
            delivery: MqttDelivery::DecryptError,
        };
        let client = mqtt_client(vec![failure]);
        let waited = block_on(client.mqtt_wait(VIN, None, None, Some(30.0))).unwrap();
        assert_eq!(waited.unwrap(), None);
        assert!(client.state().waiters.is_empty());
        let steps = client.env().steps();
        assert!(steps.iter().any(|step| step.get("mqtt_wait").is_some()));
        assert!(
            steps.iter().all(|step| step.get("mqtt_timeout").is_none()),
            "{steps:?}"
        );
    }

    #[test]
    fn a_never_policy_refuses_without_sending() {
        let mut options = ClientOptions::default();
        options.relogin = ReloginPolicy::Never;
        let client = scripted(vec![], options);
        block_on(client.import_session(tokens(Some(0.0)))).unwrap();
        let error = block_on(client.get_hvac_status(VIN)).unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BydSessionExpiredError);
        assert_eq!(error.code(), "relogin_disabled");
        assert_eq!(posts(&client), 0);
        // MQTT recovery stays idle as well.
        client.schedule_mqtt_reauth();
        assert_eq!(posts(&client), 0);
    }

    #[test]
    fn a_handler_policy_hands_over_a_session() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let mut options = ClientOptions::default();
        options.relogin = ReloginPolicy::Handler(Arc::new(move || {
            let call = counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    Ok(tokens(None))
                } else {
                    Err(Error::new(ErrorKind::BydError, "no more"))
                }
            })
        }));
        let client = scripted(vec![sealed(json!({}))], options);
        block_on(client.get_hvac_status(VIN)).unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let exported = client.export_session().unwrap();
        assert_eq!(exported.ttl_remaining, None);
        assert!(exported.guest_vins[VIN]);
        client.invalidate_session();
        let error = block_on(client.get_hvac_status(VIN)).unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BydError);
        assert!(client.export_session().is_none());
    }

    #[test]
    fn the_observer_sees_every_outcome() {
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&outcomes);
        let mut options = ClientOptions::default();
        options.request_observer = Some(Arc::new(move |record: &RequestRecord| {
            seen.lock().unwrap().push(record.outcome.clone());
        }));
        let client = scripted(
            vec![
                sealed(json!({})),
                Reply::TransportError(Some(502)),
                Reply::Outer(json!([])),
            ],
            options,
        );
        block_on(client.import_session(tokens(None))).unwrap();
        for _ in 0..4 {
            let _ = block_on(client.get_hvac_status(VIN)).unwrap();
        }
        assert_eq!(
            outcomes.lock().unwrap().clone(),
            [
                RequestOutcome::Reply {
                    code: Some("0".into())
                },
                RequestOutcome::Transport { status: Some(502) },
                RequestOutcome::Reply { code: None },
                RequestOutcome::Failed(ErrorKind::ScriptExhausted),
            ]
        );
    }

    #[test]
    fn a_panicking_callback_does_not_reach_the_client() {
        let mut options = ClientOptions::default();
        options.on_mqtt_connect = Some(Arc::new(|| panic!("callback bug")));
        options.request_observer = Some(Arc::new(|_: &RequestRecord| panic!("observer bug")));
        let env = Arc::new(ScriptedEnv::new(0, "s", vec![sealed(json!({}))]).with_mqtt(
            vec![ScriptedMqtt {
                index: 0,
                at: 0.0,
                delivery: MqttDelivery::Connected,
            }],
            vec![],
        ));
        let client = BydClient::new(config(), Arc::clone(&env), options);
        block_on(client.import_session(tokens(None))).unwrap();
        client.state().mqtt_started = true;
        let sink: Weak<dyn MqttSink> = client.inner.me.clone();
        env.install_mqtt(sink);
        env.drain();
        assert!(block_on(client.get_hvac_status(VIN)).unwrap().is_ok());
    }

    #[test]
    fn small_accessors_and_metadata() {
        let mut options = ClientOptions::default();
        options.command_retries = 1;
        let client = scripted(vec![], options);
        assert_eq!(client.options().command_retries, 1);
        assert!(!client.config().mqtt_enabled);
        assert!(format!("{client:?}").contains("BydClient"));
        assert!(format!("{:?}", client.options()).contains("command_retries: 1"));
        let failing: ReloginHandler =
            Arc::new(|| Box::pin(async { Err(Error::new(ErrorKind::BydError, "x")) }));
        for (policy, name) in [
            (ReloginPolicy::Password, "Password"),
            (ReloginPolicy::Never, "Never"),
            (ReloginPolicy::Handler(failing), "Handler"),
        ] {
            assert_eq!(format!("{policy:?}"), name);
        }
        client
            .seed_vehicle_metadata(VIN, EnergyType::Hybrid, Some("SONG"))
            .unwrap();
        let vehicle = client.vehicle_metadata(VIN).unwrap();
        assert_eq!(
            (vehicle.energy_type, vehicle.model_name.as_str()),
            (EnergyType::Hybrid, "SONG")
        );
        assert!(client.vehicle_metadata("OTHER").is_none());
        // Without a session nothing starts.
        block_on(client.start_mqtt()).unwrap();
        assert!(!client.mqtt_is_running());
        assert!(client.export_session().is_none());
        assert!(client.session().is_none());
    }

    #[test]
    fn a_closed_client_does_not_start_mqtt() {
        let mut config = config();
        config.mqtt_enabled = true;
        let client = BydClient::new(
            config,
            Arc::new(ScriptedEnv::new(0, "s", vec![])),
            ClientOptions::default(),
        );
        // The broker lookup finds no reply: MQTT stays off, quietly.
        block_on(client.import_session(tokens(None))).unwrap();
        assert!(!client.mqtt_is_running());
        client.async_close();
        block_on(client.start_mqtt()).unwrap();
        assert_eq!(posts(&client), 1);
    }

    #[test]
    fn a_guest_map_before_any_session_is_kept() {
        let client = scripted(vec![], ClientOptions::default());
        client.state().guest_vins.insert("OTHER".into(), false);
        block_on(client.import_session(tokens(None))).unwrap();
        let guests = client.export_session().unwrap().guest_vins;
        assert_eq!(guests.len(), 2);
        let mut state = client.state();
        state.guest_owner = None;
        state.guest_setdefault("NEW", true);
        state.guest_setdefault("NEW", false);
        assert!(state.guest_vins["NEW"]);
    }

    #[test]
    fn a_dropped_client_ignores_late_deliveries() {
        let client = scripted(vec![], ClientOptions::default());
        let inner = Arc::clone(&client.inner);
        drop(client);
        // A sink whose client is gone does nothing.
        let orphan = Inner {
            env: Observed {
                env: Arc::new(ScriptedEnv::new(0, "s", vec![])),
                observer: None,
            },
            config: config(),
            options: ClientOptions::default(),
            me: Weak::new(),
            state: Mutex::new(State::default()),
            cars: Mutex::new(HashMap::new()),
        };
        let event = MqttEvent {
            event: "vehicleInfo".into(),
            vin: Some(VIN.into()),
            topic: "t".into(),
            payload: Map::new(),
        };
        orphan.on_event(&event);
        orphan.on_decrypt_error();
        orphan.on_connack_refused(3);
        orphan.on_connected();
        drop(inner);
    }

    #[tokio::test]
    async fn connect_builds_a_production_client() {
        let pool = HttpPool::new();
        let client = BydClient::connect(BydConfig::new("user@example.com", "p"), &pool);
        assert!(!client.mqtt_is_running());
        let mut options = ClientOptions::default();
        options.command_retries = 1;
        let other =
            BydClient::connect_with(BydConfig::new("user@example.com", "p"), &pool, options);
        assert_eq!(other.options().command_retries, 1);
        assert_eq!(
            other.env().http().base_url(),
            client.env().http().base_url()
        );
    }
}
