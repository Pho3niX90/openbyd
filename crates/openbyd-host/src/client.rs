//! The client by method name: pyBYD's `BydClient` and `BydCar` with JSON
//! arguments and JSON results.
//!
//! [`Client::call`] runs the methods that wait on the network
//! ([`ASYNC_METHODS`]) and [`Client::call_sync`] the ones that answer at
//! once ([`SYNC_METHODS`]). Method names are pyBYD's; a car's methods are
//! prefixed `car.` and take the car's number (from `get_car`) as `car`.
//! Arguments are a JSON object keyed by pyBYD's parameter names. A model
//! comes back as pyBYD's `model_dump(mode="json")`.
//!
//! A client is created closed, as pyBYD's is before `async_start`: call
//! `start` first.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use openbyd_client::api::smart_charging::EnergyTypeArg;
use openbyd_client::api::standby::schedule_in_zone;
use openbyd_client::car::{ClimateStart, SeatLevel, SeatMode, SeatPosition};
use openbyd_client::client::{
    BydClient, ChargePollSettings, ClientCar, ClientOptions, GetCarOptions, PollSettings,
    ProductionEnv, ReloginPolicy, RequestOutcome, RequestRecord, RunPollSettings, SessionTokens,
    resolve_command_pwd,
};
use openbyd_client::http::HttpEnv;
use openbyd_protocol::coerce::{StrEnum, int_enum};
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::charging::ChargingStatus;
use openbyd_protocol::models::command_gating::CommandGateVerdict;
use openbyd_protocol::models::control::{
    BatteryHeatParams, ClimateScheduleParams, ClimateStartParams, SeatClimateParams,
};
use openbyd_protocol::models::energy::EnergyConsumption;
use openbyd_protocol::models::hvac::HvacStatus;
use openbyd_protocol::models::message_center::NotificationCategory;
use openbyd_protocol::models::realtime::VehicleRealtimeData;
use openbyd_protocol::models::standby::{StandbyPlan, StandbyPlanCommand, StandbySchedule};
use openbyd_protocol::models::vehicle::{EnergyType, Vehicle};
use openbyd_protocol::state::{StateChangedCallback, VehicleSnapshot};
use serde_json::{Map, Value, json};

use crate::args::Args;
use crate::error::HostError;
use crate::host::{DECISION_TIMEOUT, Host, Requests};
use crate::runtime;

/// The reasons the gate policy `"allow_missing_data"` lets through: the
/// car's capability data is incomplete, not refusing.
const MISSING_DATA_REASONS: [&str; 2] = ["function_no_missing", "learn_info_missing"];

/// The events a client sends only when its options ask for them (`events`).
pub const EVENTS: [&str; 6] = [
    "vehicle_info",
    "mqtt_event",
    "command_ack",
    "command_lifecycle",
    "mqtt_connect",
    "request",
];

/// The methods [`Client::call`] runs.
pub const ASYNC_METHODS: [&str; 76] = [
    "login",
    "ensure_session",
    "import_session",
    "start_mqtt",
    "get_vehicles",
    "get_car",
    "get_latest_configs",
    "get_latest_config",
    "get_vehicle_capabilities",
    "get_vehicle_realtime",
    "get_gps_info",
    "get_hvac_status",
    "get_charging_status",
    "get_charging_schedule",
    "get_charging_homepage",
    "get_ac_current_limit",
    "get_energy_consumption",
    "get_push_state",
    "set_push_state",
    "get_notifications",
    "get_all_notifications",
    "rename_vehicle",
    "get_booking_list",
    "get_ota_version",
    "set_ota_auto_upgrade",
    "get_empower_detail",
    "get_standby_plans",
    "save_standby_plan",
    "create_standby_plan",
    "set_default_standby_plan",
    "delete_standby_plan",
    "run_standby_plan",
    "verify_command_access",
    "lock",
    "unlock",
    "stop_climate",
    "flash_lights",
    "close_windows",
    "open_windows",
    "find_car",
    "open_trunk",
    "close_trunk",
    "start_climate",
    "schedule_climate",
    "set_seat_climate",
    "set_battery_heat",
    "save_charging_schedule",
    "toggle_smart_charging",
    "start_charging",
    "stop_charging",
    "car.update_realtime",
    "car.update_hvac",
    "car.update_gps",
    "car.update_charging",
    "car.update_energy",
    "car.start_charging",
    "car.stop_charging",
    "car.get_ac_current_limit",
    "car.save_charging_schedule",
    "car.get_ota_version",
    "car.set_ota_auto_upgrade",
    "car.lock.lock",
    "car.lock.unlock",
    "car.hvac.start",
    "car.hvac.stop",
    "car.hvac.schedule",
    "car.seat.heat",
    "car.seat.ventilation",
    "car.steering.heat",
    "car.battery.heat",
    "car.finder.find",
    "car.finder.flash_lights",
    "car.windows.close",
    "car.windows.open",
    "car.trunk.open",
    "car.trunk.close",
];

/// The methods [`Client::call_sync`] runs.
pub const SYNC_METHODS: [&str; 18] = [
    "start",
    "close",
    "resolve",
    "invalidate_session",
    "export_session",
    "commands_enabled",
    "mqtt_is_running",
    "get_command_ack_diagnostics",
    "seed_vehicle_metadata",
    "vehicle_metadata",
    "car.state",
    "car.capabilities",
    "car.features",
    "car.handle_mqtt_realtime",
    "car.handle_mqtt_status_push",
    "car.handle_mqtt_charging",
    "car.handle_mqtt_energy",
    "car.close",
];

type Car = ClientCar<ProductionEnv>;

/// A client another language drives. See the module documentation.
pub struct Client {
    client: BydClient<ProductionEnv>,
    host: Arc<dyn Host>,
    requests: Arc<Requests>,
    cars: Mutex<Vec<Car>>,
}

fn dump<T: serde::Serialize>(value: &T) -> Result<Value, HostError> {
    serde_json::to_value(value)
        .map_err(|error| HostError::internal(format!("a result is not JSON: {error}")))
}

fn emit(host: &dyn Host, name: &str, payload: Value) {
    host.event(name, &payload.to_string());
}

fn fields(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn unknown_method(method: &str) -> HostError {
    HostError::new(ErrorKind::AttributeError, format!("no method {method:?}"))
}

/// The session as `export_session` gives it and `import_session` takes it.
fn tokens_to_json(tokens: &SessionTokens) -> Value {
    let mut vins: Vec<_> = tokens.guest_vins.iter().collect();
    vins.sort();
    let guests: Map<String, Value> = vins
        .into_iter()
        .map(|(vin, guest)| (vin.clone(), json!(guest)))
        .collect();
    json!({
        "user_id": tokens.user_id,
        "sign_token": tokens.sign_token,
        "encry_token": tokens.encry_token,
        "refresh_token": tokens.refresh_token,
        "ttl_remaining": tokens.ttl_remaining,
        "guest_vins": guests,
    })
}

fn tokens_from_json(value: &Map<String, Value>) -> Result<SessionTokens, String> {
    let text = |key: &str| -> Result<String, String> {
        match value.get(key) {
            Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
            _ => Err(format!("session[{key:?}] must be a non-empty string")),
        }
    };
    let ttl_remaining = match value.get("ttl_remaining") {
        None | Some(Value::Null) => None,
        Some(seconds) => Some(
            seconds
                .as_f64()
                .ok_or("session[\"ttl_remaining\"] must be a number or null")?,
        ),
    };
    // Optional: records saved before openbyd kept it have none.
    let refresh_token = match value.get("refresh_token") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(token)) => token.clone(),
        Some(_) => return Err("session[\"refresh_token\"] must be a string or null".into()),
    };
    let mut guest_vins = HashMap::new();
    match value.get("guest_vins") {
        None | Some(Value::Null) => {}
        Some(Value::Object(guests)) => {
            for (vin, guest) in guests {
                let guest = guest
                    .as_bool()
                    .ok_or("session[\"guest_vins\"] maps VINs to booleans")?;
                guest_vins.insert(vin.clone(), guest);
            }
        }
        Some(_) => return Err("session[\"guest_vins\"] must be an object".into()),
    }
    Ok(SessionTokens {
        user_id: text("user_id")?,
        sign_token: text("sign_token")?,
        encry_token: text("encry_token")?,
        refresh_token,
        ttl_remaining,
        guest_vins,
    })
}

fn record_to_json(record: &RequestRecord) -> Value {
    let (outcome, code, status, error): (&str, Option<&str>, Option<u16>, Option<&str>) =
        match &record.outcome {
            RequestOutcome::Reply { code } => ("reply", code.as_deref(), None, None),
            RequestOutcome::Transport { status } => ("transport", None, *status, None),
            RequestOutcome::Failed(kind) => ("failed", None, None, Some(kind.class_name())),
        };
    json!({
        "endpoint": record.endpoint,
        "outcome": outcome,
        "code": code,
        "status": status,
        "error": error,
        "duration": record.duration.as_secs_f64(),
    })
}

fn poll_settings(args: &Args) -> Result<PollSettings, HostError> {
    let defaults = PollSettings::default();
    Ok(PollSettings {
        poll_attempts: args.int_or("poll_attempts", defaults.poll_attempts)?,
        poll_interval: args.float_or("poll_interval", defaults.poll_interval)?,
        mqtt_timeout: args.opt_float("mqtt_timeout")?,
        signal_retries: args.int_or("signal_retries", defaults.signal_retries)?,
    })
}

fn charge_settings(args: &Args) -> Result<ChargePollSettings, HostError> {
    let defaults = ChargePollSettings::default();
    Ok(ChargePollSettings {
        mqtt_timeout: args.opt_float("mqtt_timeout")?,
        poll_attempts: args.int_or("poll_attempts", defaults.poll_attempts)?,
        // null or missing: the official 33 s ladder.
        poll_interval: args.opt_float("poll_interval")?,
    })
}

fn seat_position(args: &Args) -> Result<SeatPosition, HostError> {
    let text = args.str("position")?;
    SeatPosition::from_value(&text)
        .ok_or_else(|| HostError::value(format!("{text:?} is not a valid SeatPosition")))
}

fn seat_level(args: &Args) -> Result<SeatLevel, HostError> {
    let level = args.int("level")?;
    SeatLevel::from_value(level)
        .ok_or_else(|| HostError::value(format!("{level} is not a valid SeatLevel")))
}

fn climate_start(args: &Args) -> Result<ClimateStart, HostError> {
    let defaults = ClimateStart::default();
    Ok(ClimateStart {
        temperature: args.float_or("temperature", defaults.temperature)?,
        duration: args.int_or("duration", defaults.duration)?,
        cycle_mode: args.opt_int("cycle_mode")?,
        fan_level: args.opt_int("fan_level")?,
    })
}

/// What each feature of a car offers: availability, and the climate scale.
fn features(car: &Car) -> Value {
    let (low, high) = car.hvac.temp_range();
    let mut seats = Map::new();
    for position in [
        SeatPosition::Driver,
        SeatPosition::Copilot,
        SeatPosition::RearLeft,
        SeatPosition::RearRight,
    ] {
        seats.insert(
            StrEnum::value(position).to_owned(),
            json!({
                "heat": car.seat.is_mode_available(position, SeatMode::Heat),
                "ventilation": car.seat.is_mode_available(position, SeatMode::Vent),
            }),
        );
    }
    json!({
        "lock": {"available": car.lock.available()},
        "hvac": {
            "available": car.hvac.available(),
            "temp_offset": car.hvac.temp_offset(),
            "temp_range": [low, high],
        },
        "seat": seats,
        "steering": {"available": car.steering.available()},
        "battery": {"available": car.battery.available()},
        "finder": {
            "find_available": car.finder.find_available(),
            "flash_available": car.finder.flash_available(),
        },
        "windows": {
            "close_available": car.windows.close_available(),
            "open_available": car.windows.open_available(),
        },
        "trunk": {
            "open_available": car.trunk.open_available(),
            "close_available": car.trunk.close_available(),
        },
    })
}

/// The options of a client besides its config, as a binding gives them.
struct HostOptions<'a> {
    args: &'a Args,
    host: &'a Arc<dyn Host>,
    requests: &'a Arc<Requests>,
}

impl HostOptions<'_> {
    fn wants(&self, event: &str) -> Result<bool, HostError> {
        match self.args.get("events") {
            None => Ok(false),
            Some(_) => Ok(self
                .args
                .strings("events")?
                .iter()
                .any(|name| name == event)),
        }
    }

    fn choice(&self, key: &str, choices: &[&str]) -> Result<String, HostError> {
        let chosen = self
            .args
            .opt_str(key)?
            .unwrap_or_else(|| choices[0].to_owned());
        if choices.contains(&chosen.as_str()) {
            Ok(chosen)
        } else {
            Err(HostError::value(format!(
                "{key} must be one of {choices:?}, not {chosen:?}"
            )))
        }
    }

    fn build(&self) -> Result<ClientOptions, HostError> {
        let mut options = ClientOptions::default();
        let ttl = self
            .args
            .float_or("command_ack_ttl_seconds", options.command_ack_ttl_seconds)?;
        if !ttl.is_finite() {
            return Err(HostError::value("command_ack_ttl_seconds must be finite"));
        }
        options.command_ack_ttl_seconds = ttl;
        options.command_retries = self
            .args
            .int_or("command_retries", options.command_retries)?;
        if options.command_retries < 1 {
            return Err(HostError::value("command_retries must be at least 1"));
        }
        for name in self
            .args
            .get("events")
            .map_or(Ok(Vec::new()), |_| self.args.strings("events"))?
        {
            if !EVENTS.contains(&name.as_str()) {
                return Err(HostError::value(format!("no event {name:?}")));
            }
        }

        if self.wants("vehicle_info")? {
            let host = Arc::clone(self.host);
            options.on_vehicle_info = Some(Arc::new(move |vin, data| {
                emit(
                    &*host,
                    "vehicle_info",
                    json!({"vin": vin, "data": data.dump()}),
                );
            }));
        }
        if self.wants("mqtt_event")? {
            let host = Arc::clone(self.host);
            options.on_mqtt_event = Some(Arc::new(move |event, vin, data| {
                emit(
                    &*host,
                    "mqtt_event",
                    json!({"event": event, "vin": vin, "data": data}),
                );
            }));
        }
        if self.wants("command_ack")? {
            let host = Arc::clone(self.host);
            options.on_command_ack = Some(Arc::new(move |ack| {
                emit(&*host, "command_ack", json!({"ack": ack.dump()}));
            }));
        }
        if self.wants("command_lifecycle")? {
            let host = Arc::clone(self.host);
            options.on_command_lifecycle = Some(Arc::new(move |event| {
                emit(&*host, "command_lifecycle", json!({"event": event.dump()}));
            }));
        }
        if self.wants("mqtt_connect")? {
            let host = Arc::clone(self.host);
            options.on_mqtt_connect = Some(Arc::new(move || {
                emit(&*host, "mqtt_connect", json!({}));
            }));
        }
        if self.wants("request")? {
            let host = Arc::clone(self.host);
            options.request_observer = Some(Arc::new(move |record| {
                emit(&*host, "request", json!({"record": record_to_json(record)}));
            }));
        }

        match self
            .choice("gate_policy", &["strict", "allow_missing_data", "host"])?
            .as_str()
        {
            "allow_missing_data" => {
                options.gate_policy = Some(Arc::new(|_vin, verdict: &CommandGateVerdict| {
                    verdict.supported || MISSING_DATA_REASONS.contains(&verdict.reason.as_str())
                }));
            }
            "host" => {
                let host = Arc::clone(self.host);
                let requests = Arc::clone(self.requests);
                options.gate_policy = Some(Arc::new(move |vin, verdict: &CommandGateVerdict| {
                    let question = fields(json!({"vin": vin, "verdict": verdict.dump()}));
                    requests
                        .decide(&*host, "gate_policy", question, DECISION_TIMEOUT)
                        .and_then(|answer| answer.as_bool())
                        .unwrap_or(verdict.supported)
                }));
            }
            _ => {}
        }
        if self.choice("pin_resolver", &["default", "host"])? == "host" {
            let host = Arc::clone(self.host);
            let requests = Arc::clone(self.requests);
            options.pin_resolver = Some(Arc::new(move |given, config: &BydConfig| {
                let question = fields(json!({
                    "command_pwd": given,
                    "control_pin": config.control_pin,
                }));
                requests
                    .decide(&*host, "pin_resolver", question, DECISION_TIMEOUT)
                    .and_then(|answer| answer.as_str().map(str::to_owned))
                    .unwrap_or_else(|| resolve_command_pwd(given, config))
            }));
        }
        options.relogin = match self
            .choice("relogin", &["password", "never", "host"])?
            .as_str()
        {
            "never" => ReloginPolicy::Never,
            "host" => {
                let host = Arc::clone(self.host);
                let requests = Arc::clone(self.requests);
                ReloginPolicy::Handler(Arc::new(move || {
                    let host = Arc::clone(&host);
                    let requests = Arc::clone(&requests);
                    Box::pin(async move {
                        let answer = requests.ask(&*host, "relogin", Map::new()).await?;
                        let no_session = |message: String| {
                            Error::new(
                                ErrorKind::BydError,
                                format!("relogin returned no session: {message}"),
                            )
                        };
                        let session = answer
                            .as_object()
                            .ok_or_else(|| no_session("a session is an object".into()))?;
                        tokens_from_json(session).map_err(no_session)
                    })
                }))
            }
            _ => ReloginPolicy::Password,
        };
        Ok(options)
    }
}

#[cfg(feature = "testing")]
fn test_environment(http: HttpEnv) -> Result<ProductionEnv, HostError> {
    Ok(ProductionEnv::with_mqtt_options(
        http,
        openbyd_client::testing::broker::runtime_options(),
    ))
}

#[cfg(not(feature = "testing"))]
fn test_environment(_http: HttpEnv) -> Result<ProductionEnv, HostError> {
    Err(HostError::value(
        "test_tls needs a build with the cargo feature 'testing'",
    ))
}

impl Client {
    /// A closed client for `config` (an object of `BydConfig`'s fields)
    /// with `options`, reporting to `host`.
    ///
    /// `options` is an object or `null`:
    ///
    /// - `events`: the names of the events to send (see [`EVENTS`]); none by default.
    /// - `command_ack_ttl_seconds` (300), `command_retries` (1): as pyBYD's.
    /// - `gate_policy`: `"strict"`, `"allow_missing_data"` or `"host"` (ask
    ///   with a `gate_policy` event).
    /// - `pin_resolver`: `"default"` or `"host"` (ask with a `pin_resolver` event).
    /// - `relogin`: `"password"`, `"never"` or `"host"` (ask with a `relogin` event).
    /// - `test_tls`: trust the test broker's certificate (feature `testing`).
    pub fn new(config: Value, options: Value, host: Arc<dyn Host>) -> Result<Arc<Self>, HostError> {
        let config = Args::new(json!({ "config": config }))?;
        let config = BydConfig::from_kwargs(config.object("config")?)?;
        let args = Args::new(options)?;
        let requests = Arc::new(Requests::default());
        let options = HostOptions {
            args: &args,
            host: &host,
            requests: &requests,
        }
        .build()?;
        let _entered = runtime::handle().enter();
        let http = HttpEnv::for_config(runtime::pool(), &config);
        let env = if args.bool_or("test_tls", false)? {
            test_environment(http)?
        } else {
            ProductionEnv::new(http)
        };
        let client = BydClient::new(config, env, options);
        // pyBYD's client is unusable until `async_start`.
        client.async_close();
        Ok(Arc::new(Self {
            client,
            host,
            requests,
            cars: Mutex::new(Vec::new()),
        }))
    }

    fn cars(&self) -> MutexGuard<'_, Vec<Car>> {
        self.cars.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn car(&self, args: &Args) -> Result<Car, HostError> {
        let number = args.int("car")?;
        usize::try_from(number)
            .ok()
            .and_then(|number| number.checked_sub(1))
            .and_then(|index| self.cars().get(index).cloned())
            .ok_or_else(|| HostError::value(format!("no car {number}")))
    }

    /// The number of `car` among the cars handed out, adding it when new.
    fn car_number(&self, car: &Car) -> usize {
        let mut cars = self.cars();
        match cars.iter().position(|known| Arc::ptr_eq(known, car)) {
            Some(index) => index + 1,
            None => {
                cars.push(Arc::clone(car));
                cars.len()
            }
        }
    }

    /// Runs a method of [`SYNC_METHODS`] and returns its result.
    pub fn call_sync(&self, method: &str, args: Value) -> Result<Value, HostError> {
        let args = Args::new(args)?;
        let _entered = runtime::handle().enter();
        let client = &self.client;
        Ok(match method {
            "start" => {
                client.async_start();
                Value::Null
            }
            "close" => {
                for car in self.cars().iter() {
                    car.close();
                }
                client.async_close();
                self.requests.abandon();
                Value::Null
            }
            "resolve" => {
                let request = u64::try_from(args.int("request")?)
                    .map_err(|_| HostError::value("request must not be negative"))?;
                let answer = match args.get("error") {
                    Some(error) => Err(HostError::to_error(error)),
                    None => Ok(args.get("value").cloned().unwrap_or(Value::Null)),
                };
                json!(self.requests.resolve(request, answer))
            }
            "invalidate_session" => {
                client.invalidate_session();
                Value::Null
            }
            "export_session" => client
                .export_session()
                .map_or(Value::Null, |tokens| tokens_to_json(&tokens)),
            "commands_enabled" => json!(client.commands_enabled()),
            "mqtt_is_running" => json!(client.mqtt_is_running()),
            "get_command_ack_diagnostics" => dump(&client.get_command_ack_diagnostics())?,
            "seed_vehicle_metadata" => {
                let energy_type: EnergyType = int_enum(&json!(args.int("energy_type")?))?;
                client.seed_vehicle_metadata(
                    &args.str("vin")?,
                    energy_type,
                    args.opt_str("model_name")?.as_deref(),
                )?;
                Value::Null
            }
            "vehicle_metadata" => match client.vehicle_metadata(&args.str("vin")?) {
                Some(vehicle) => dump(&vehicle)?,
                None => Value::Null,
            },
            "car.state" => self.car(&args)?.state().dump(),
            "car.capabilities" => dump(self.car(&args)?.capabilities())?,
            "car.features" => features(&self.car(&args)?),
            "car.handle_mqtt_realtime" => {
                let car = self.car(&args)?;
                let raw = Value::Object(args.object("data")?.clone());
                let energy_type = match car.state().vehicle.energy_type {
                    EnergyType::Unknown => EnergyType::Ev,
                    known => known,
                };
                let context = json!({"energy_type": {"EnergyType": energy_type as i64}});
                car.handle_mqtt_realtime(VehicleRealtimeData::parse_owned(raw, &context)?);
                Value::Null
            }
            "car.handle_mqtt_status_push" => {
                let car = self.car(&args)?;
                let raw = Value::Object(args.object("data")?.clone());
                let energy_type = match car.state().vehicle.energy_type {
                    EnergyType::Unknown => EnergyType::Ev,
                    known => known,
                };
                let context = json!({"energy_type": {"EnergyType": energy_type as i64}});
                let data = VehicleRealtimeData::parse_owned(raw, &context)?;
                let hvac = match args.opt_object("hvac")? {
                    Some(raw) => Some(HvacStatus::parse_owned(
                        Value::Object(raw.clone()),
                        &Value::Null,
                    )?),
                    None => None,
                };
                let charging = match args.opt_object("charging")? {
                    Some(raw) => Some(ChargingStatus::parse_owned(
                        Value::Object(raw.clone()),
                        &Value::Null,
                    )?),
                    None => None,
                };
                car.handle_mqtt_status_push(data, hvac, charging);
                Value::Null
            }
            "car.handle_mqtt_charging" => {
                let raw = Value::Object(args.object("data")?.clone());
                self.car(&args)?
                    .handle_mqtt_charging(ChargingStatus::parse_owned(raw, &Value::Null)?);
                Value::Null
            }
            "car.handle_mqtt_energy" => {
                let raw = Value::Object(args.object("data")?.clone());
                self.car(&args)?
                    .handle_mqtt_energy(EnergyConsumption::parse_owned(raw, &Value::Null)?);
                Value::Null
            }
            "car.close" => {
                self.car(&args)?.close();
                Value::Null
            }
            other => return Err(unknown_method(other)),
        })
    }

    /// Runs a method of [`ASYNC_METHODS`] and returns its result. Spawn it
    /// on the shared runtime ([`runtime::spawn`]).
    #[allow(clippy::too_many_lines)]
    pub async fn call(self: Arc<Self>, method: String, args: Value) -> Result<Value, HostError> {
        let args = Args::new(args)?;
        let client = &self.client;
        if let Some(method) = method.strip_prefix("car.") {
            return self.call_car(method, &args).await;
        }
        // Every method but these four takes the VIN.
        let vin = match method.as_str() {
            "login"
            | "ensure_session"
            | "import_session"
            | "start_mqtt"
            | "get_vehicles"
            | "get_latest_configs"
            | "get_notifications"
            | "get_all_notifications" => String::new(),
            _ => args.str("vin")?,
        };
        let vin = vin.as_str();
        let pwd = args.opt_str("command_pwd")?;
        let pwd = pwd.as_deref();
        Ok(match method.as_str() {
            "login" => {
                client.login().await?;
                Value::Null
            }
            "ensure_session" => {
                let session = client.ensure_session().await?;
                json!({
                    "user_id": session.user_id(),
                    "sign_token": session.sign_token(),
                    "encry_token": session.encry_token(),
                    "created_at": session.created_at(),
                    "ttl": if session.ttl().is_finite() { json!(session.ttl()) } else { json!("Infinity") },
                })
            }
            "import_session" => {
                let tokens = tokens_from_json(args.object("session")?).map_err(HostError::value)?;
                client.import_session(tokens).await;
                Value::Null
            }
            "start_mqtt" => {
                client.start_mqtt().await;
                Value::Null
            }
            "get_vehicles" => dump(&client.get_vehicles().await?)?,
            "get_car" => {
                let vehicle = match args.opt_object("vehicle")? {
                    None => None,
                    Some(raw) => Some(Vehicle::parse(&Value::Object(raw.clone()), &Value::Null)?),
                };
                let on_state_changed = args.bool_or("state_events", false)?.then(|| {
                    let host = Arc::clone(&self.host);
                    Box::new(move |vin: &str, snapshot: &VehicleSnapshot| {
                        emit(
                            &*host,
                            "state_changed",
                            json!({"vin": vin, "state": snapshot.dump()}),
                        );
                    }) as StateChangedCallback
                });
                let defaults = GetCarOptions::default();
                let options = GetCarOptions {
                    vehicle,
                    on_state_changed,
                    projection_ttl: args.float_or("projection_ttl", defaults.projection_ttl)?,
                };
                let car = client.get_car(vin, options).await?;
                json!({"car": self.car_number(&car), "vin": car.vin()})
            }
            "get_latest_configs" => {
                let configs = client.get_latest_configs(&args.strings("vins")?).await?;
                let mut by_vin = Map::new();
                for (vin, config) in configs {
                    by_vin.insert(vin, dump(&config)?);
                }
                Value::Object(by_vin)
            }
            "get_latest_config" => dump(&client.get_latest_config(vin).await?)?,
            "get_vehicle_capabilities" => {
                let force_refresh = args.bool_or("force_refresh", false)?;
                dump(&client.get_vehicle_capabilities(vin, force_refresh).await)?
            }
            "get_vehicle_realtime" => {
                let settings = poll_settings(&args)?;
                dump(&client.get_vehicle_realtime(vin, settings).await?)?
            }
            "get_gps_info" => {
                let settings = poll_settings(&args)?;
                dump(&client.get_gps_info(vin, settings).await?)?
            }
            "get_hvac_status" => dump(&client.get_hvac_status(vin).await?)?,
            "get_charging_status" => dump(&client.get_charging_status(vin).await?)?,
            "get_charging_schedule" => dump(&client.get_charging_schedule(vin).await?)?,
            "get_charging_homepage" => {
                let (status, schedule) = client.get_charging_homepage(vin).await?;
                json!({"status": dump(&status)?, "schedule": dump(&schedule)?})
            }
            "get_ac_current_limit" => {
                let energy_type = match args.get("energy_type") {
                    None => None,
                    Some(Value::String(text)) => Some(EnergyTypeArg::Text(text.clone())),
                    Some(_) => Some(EnergyTypeArg::Int(args.int("energy_type")?)),
                };
                dump(&client.get_ac_current_limit(vin, energy_type).await?)?
            }
            "get_energy_consumption" => dump(&client.get_energy_consumption(vin).await?)?,
            "get_push_state" => dump(&client.get_push_state(vin).await?)?,
            "set_push_state" => {
                let enable = args.bool("enable")?;
                let push_type = args.int_or("push_type", 701)?;
                dump(&client.set_push_state(vin, enable, push_type).await?)?
            }
            "get_notifications" => {
                let category = notification_category(args.int_or("category", 1)?)?;
                dump(&client.get_notifications(category).await?)?
            }
            "get_all_notifications" => {
                let mut by_category = Map::new();
                for (category, messages) in client.get_all_notifications().await? {
                    by_category.insert((category as i64).to_string(), dump(&messages)?);
                }
                Value::Object(by_category)
            }
            "rename_vehicle" => dump(&client.rename_vehicle(vin, &args.str("name")?).await?)?,
            "get_booking_list" => dump(&client.get_booking_list(vin).await?)?,
            "get_ota_version" => dump(&client.get_ota_version(vin).await?)?,
            "set_ota_auto_upgrade" => {
                let time = args
                    .opt_str("auto_upgrade_time")?
                    .unwrap_or_else(|| "2:00".to_owned());
                dump(
                    &client
                        .set_ota_auto_upgrade(vin, args.bool("enabled")?, &time, pwd)
                        .await?,
                )?
            }
            "get_empower_detail" => {
                let empower_id = args.opt_int("empower_id")?;
                dump(&client.get_empower_detail(vin, empower_id).await?)?
            }
            "get_standby_plans" => dump(&client.get_standby_plans(vin).await?)?,
            "save_standby_plan" => {
                let plan =
                    StandbyPlan::parse(&Value::Object(args.object("plan")?.clone()), &Value::Null)?;
                let schedule = args
                    .opt_object("schedule")?
                    .map(parse_schedule)
                    .transpose()?;
                dump(
                    &client
                        .save_standby_plan(vin, &plan, schedule.as_ref())
                        .await?,
                )?
            }
            "create_standby_plan" => {
                let name = args.str("name")?;
                let commands = parse_commands(&args)?;
                let schedule = args
                    .opt_object("schedule")?
                    .map(parse_schedule)
                    .transpose()?;
                dump(
                    &client
                        .create_standby_plan(vin, &name, &commands, schedule.as_ref())
                        .await?,
                )?
            }
            "set_default_standby_plan" => dump(
                &client
                    .set_default_standby_plan(vin, &args.str("plan_id")?)
                    .await?,
            )?,
            "delete_standby_plan" => dump(
                &client
                    .delete_standby_plan(vin, &args.str("plan_id")?)
                    .await?,
            )?,
            "run_standby_plan" => {
                let defaults = RunPollSettings::default();
                let settings = RunPollSettings {
                    poll_attempts: args.int_or("poll_attempts", defaults.poll_attempts)?,
                    poll_interval: args.float_or("poll_interval", defaults.poll_interval)?,
                };
                dump(
                    &client
                        .run_standby_plan(vin, &args.str("plan_id")?, settings)
                        .await?,
                )?
            }
            "verify_command_access" => dump(&client.verify_command_access(vin, pwd).await?)?,
            "lock" => dump(&client.lock(vin, pwd).await?)?,
            "unlock" => dump(&client.unlock(vin, pwd).await?)?,
            "stop_climate" => dump(&client.stop_climate(vin, pwd).await?)?,
            "flash_lights" => dump(&client.flash_lights(vin, pwd).await?)?,
            "close_windows" => dump(&client.close_windows(vin, pwd).await?)?,
            "open_windows" => dump(&client.open_windows(vin, pwd).await?)?,
            "find_car" => dump(&client.find_car(vin, pwd).await?)?,
            "open_trunk" => dump(&client.open_trunk(vin, pwd).await?)?,
            "close_trunk" => dump(&client.close_trunk(vin, pwd).await?)?,
            "start_climate" => {
                let params = ClimateStartParams::from_kwargs(args.object("params")?)?;
                dump(&client.start_climate(vin, &params, pwd).await?)?
            }
            "schedule_climate" => {
                let params = ClimateScheduleParams::from_kwargs(args.object("params")?)?;
                dump(&client.schedule_climate(vin, &params, pwd).await?)?
            }
            "set_seat_climate" => {
                let params = SeatClimateParams::from_kwargs(args.object("params")?)?;
                dump(&client.set_seat_climate(vin, &params, pwd).await?)?
            }
            "set_battery_heat" => {
                let params = BatteryHeatParams::from_kwargs(args.object("params")?)?;
                dump(&client.set_battery_heat(vin, &params, pwd).await?)?
            }
            "save_charging_schedule" => {
                let settings = charge_settings(&args)?;
                let result = client
                    .save_charging_schedule(
                        vin,
                        &args.str("start_charge_time")?,
                        &args.str("end_charge_time")?,
                        &args.str("charge_way")?,
                        args.bool_or("enabled", true)?,
                        settings,
                    )
                    .await?;
                dump(&result)?
            }
            "toggle_smart_charging" => dump(
                &client
                    .toggle_smart_charging(vin, args.bool("enable")?)
                    .await?,
            )?,
            "start_charging" => {
                let settings = charge_settings(&args)?;
                dump(&client.start_charging(vin, settings).await?)?
            }
            "stop_charging" => {
                let settings = charge_settings(&args)?;
                dump(&client.stop_charging(vin, settings).await?)?
            }
            other => return Err(unknown_method(other)),
        })
    }

    /// The methods of a car, named without the `car.` prefix.
    async fn call_car(&self, method: &str, args: &Args) -> Result<Value, HostError> {
        if !ASYNC_METHODS.contains(&format!("car.{method}").as_str()) {
            return Err(unknown_method(&format!("car.{method}")));
        }
        let car = self.car(args)?;
        Ok(match method {
            "update_realtime" => dump(&car.update_realtime().await?)?,
            "update_hvac" => dump(&car.update_hvac().await?)?,
            "update_gps" => dump(&car.update_gps().await?)?,
            "update_charging" => dump(&car.update_charging().await?)?,
            "update_energy" => dump(&car.update_energy().await?)?,
            "start_charging" => dump(&car.start_charging().await?)?,
            "stop_charging" => dump(&car.stop_charging().await?)?,
            "get_ac_current_limit" => dump(&car.get_ac_current_limit().await?)?,
            "get_ota_version" => dump(&car.get_ota_version().await?)?,
            "set_ota_auto_upgrade" => {
                let time = args
                    .opt_str("auto_upgrade_time")?
                    .unwrap_or_else(|| "2:00".to_owned());
                let pwd = args.opt_str("command_pwd")?;
                car.set_ota_auto_upgrade(args.bool("enabled")?, &time, pwd.as_deref())
                    .await?;
                Value::Null
            }
            "save_charging_schedule" => {
                let result = car
                    .save_charging_schedule(
                        &args.str("start_charge_time")?,
                        &args.str("end_charge_time")?,
                        &args.str("charge_way")?,
                        args.bool_or("enabled", true)?,
                    )
                    .await?;
                dump(&result)?
            }
            command => {
                match command {
                    "lock.lock" => car.lock.lock().await?,
                    "lock.unlock" => car.lock.unlock().await?,
                    "hvac.start" => car.hvac.start(climate_start(args)?).await?,
                    "hvac.stop" => car.hvac.stop().await?,
                    "hvac.schedule" => {
                        let params = ClimateScheduleParams::from_kwargs(args.object("params")?)?;
                        car.hvac.schedule(params).await?;
                    }
                    "seat.heat" => {
                        car.seat
                            .heat(seat_position(args)?, seat_level(args)?)
                            .await?;
                    }
                    "seat.ventilation" => {
                        car.seat
                            .ventilation(seat_position(args)?, seat_level(args)?)
                            .await?;
                    }
                    "steering.heat" => car.steering.heat(args.bool("on")?).await?,
                    "battery.heat" => car.battery.heat(args.bool("on")?).await?,
                    "finder.find" => car.finder.find().await?,
                    "finder.flash_lights" => car.finder.flash_lights().await?,
                    "windows.close" => car.windows.close().await?,
                    "windows.open" => car.windows.open().await?,
                    "trunk.open" => car.trunk.open().await?,
                    _ => car.trunk.close().await?,
                }
                Value::Null
            }
        })
    }
}

fn notification_category(value: i64) -> Result<NotificationCategory, HostError> {
    NotificationCategory::from_json(&json!(value))
        .ok_or_else(|| HostError::value(format!("{value} is not a valid NotificationCategory")))
}

/// Parses a `StandbySchedule` argument and, for a daily schedule with `zone`,
/// moves `at` into that IANA time zone so the next run follows its wall clock.
fn parse_schedule(map: &Map<String, Value>) -> Result<StandbySchedule, HostError> {
    let zone = match map.get("zone") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.as_str()),
        Some(_) => return Err(HostError::type_error("zone must be a string")),
    };
    let schedule = StandbySchedule::from_kwargs(map)?;
    Ok(schedule_in_zone(schedule, zone)?)
}

fn parse_commands(args: &Args) -> Result<Vec<StandbyPlanCommand>, HostError> {
    let items = args
        .get("commands")
        .and_then(Value::as_array)
        .ok_or_else(|| HostError::type_error("commands must be a list"))?;
    items
        .iter()
        .map(|item| StandbyPlanCommand::parse(item, &Value::Null).map_err(HostError::from))
        .collect()
}

impl Drop for Client {
    fn drop(&mut self) {
        // MQTT's tasks hold the client's core; stop them so it is freed.
        let _entered = runtime::handle().enter();
        self.client.async_close();
        self.requests.abandon();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_checked() {
        let good = fields(
            json!({"user_id": "1", "sign_token": "s", "encry_token": "e", "refresh_token": "r",
            "ttl_remaining": 5.0, "guest_vins": {"V": true}}),
        );
        let tokens = tokens_from_json(&good).unwrap();
        assert_eq!(tokens.ttl_remaining, Some(5.0));
        assert_eq!(tokens.refresh_token, "r");
        assert!(tokens.guest_vins["V"]);
        assert_eq!(tokens_to_json(&tokens), Value::Object(good));
        // A record saved before the refresh token was kept still imports.
        let old = fields(json!({"user_id": "1", "sign_token": "s", "encry_token": "e"}));
        assert_eq!(tokens_from_json(&old).unwrap().refresh_token, "");
        for bad in [
            json!({"user_id": "", "sign_token": "s", "encry_token": "e"}),
            json!({"user_id": "1", "sign_token": "s", "encry_token": "e", "ttl_remaining": "x"}),
            json!({"user_id": "1", "sign_token": "s", "encry_token": "e", "guest_vins": []}),
            json!({"user_id": "1", "sign_token": "s", "encry_token": "e", "guest_vins": {"V": 1}}),
            json!({"user_id": "1", "sign_token": "s", "encry_token": "e", "refresh_token": 5}),
        ] {
            assert!(tokens_from_json(&fields(bad.clone())).is_err(), "{bad}");
        }
    }

    #[test]
    fn request_records_are_flat() {
        let record = |outcome| RequestRecord {
            endpoint: "/e".into(),
            outcome,
            duration: std::time::Duration::from_millis(250),
        };
        let reply = record_to_json(&record(RequestOutcome::Reply {
            code: Some("0".into()),
        }));
        assert_eq!(
            reply,
            json!({"endpoint": "/e", "outcome": "reply", "code": "0", "status": null,
                "error": null, "duration": 0.25})
        );
        let transport = record_to_json(&record(RequestOutcome::Transport { status: Some(502) }));
        assert_eq!(transport["outcome"], "transport");
        assert_eq!(transport["status"], 502);
        let failed = record_to_json(&record(RequestOutcome::Failed(ErrorKind::JsonDecodeError)));
        assert_eq!(failed["error"], "JSONDecodeError");
    }

    #[test]
    fn method_names_are_unique() {
        let mut names: Vec<&str> = ASYNC_METHODS.iter().chain(&SYNC_METHODS).copied().collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn daily_schedule_zone_follows_wall_clock_across_dst() {
        use openbyd_client::api::standby::schedule_fields;
        use openbyd_client::scripted::ScriptedEnv;

        // 2026-10-24 08:00+02:00 in Europe/Madrid; clocks fall back to +01:00 overnight.
        let schedule = parse_schedule(&fields(json!({
            "repeat": "daily",
            "at": "2026-10-24T08:00:00+02:00",
            "zone": "Europe/Madrid",
        })))
        .unwrap();
        // Now is 2026-10-24T09:00:00Z (past 08:00+02:00 = 06:00Z).
        let env = ScriptedEnv::new(1_792_832_400_000, "", Vec::new());
        let saved = schedule_fields(&env, &schedule, None).unwrap();
        assert_eq!(saved["executeType"], "1");
        // Next run is 2026-10-25T08:00:00+01:00 = 2026-10-25T07:00:00Z (25 hours after 06:00Z).
        assert_eq!(saved["executeTime"], 1_792_911_600_000_i64);

        // Without zone, fixed +02:00 steps by 24 hours to 2026-10-25T06:00:00Z (07:00+01:00).
        let plain = parse_schedule(&fields(json!({
            "repeat": "daily",
            "at": "2026-10-24T08:00:00+02:00",
        })))
        .unwrap();
        let saved_plain = schedule_fields(&env, &plain, None).unwrap();
        assert_eq!(saved_plain["executeTime"], 1_792_908_000_000_i64);

        // Non-daily schedules ignore zone, even an unknown name.
        let once = parse_schedule(&fields(json!({
            "repeat": "once",
            "at": "2026-10-24T08:00:00+02:00",
            "zone": "Mars/Olympus_Mons",
        })))
        .unwrap();
        assert_eq!(
            schedule_fields(&env, &once, None).unwrap()["executeType"],
            "0"
        );

        let missing = parse_schedule(&fields(json!({
            "repeat": "daily",
            "at": "2026-10-24T08:00:00+02:00",
            "zone": "Mars/Olympus_Mons",
        })))
        .unwrap_err();
        assert_eq!(missing.class, "ZoneInfoNotFoundError");
        assert_eq!(missing.bases, ["KeyError", "LookupError"]);

        let bad_key = parse_schedule(&fields(json!({
            "repeat": "daily",
            "at": "2026-10-24T08:00:00+02:00",
            "zone": "/Europe/Madrid",
        })))
        .unwrap_err();
        assert_eq!(bad_key.class, "ValueError");

        let wrong_type = parse_schedule(&fields(json!({
            "repeat": "daily",
            "at": "2026-10-24T08:00:00+02:00",
            "zone": 42,
        })))
        .unwrap_err();
        assert_eq!(wrong_type.class, "TypeError");
    }
}
