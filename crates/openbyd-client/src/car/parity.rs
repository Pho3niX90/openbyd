//! The parity call of the car layer: a sequence of operations on a car
//! whose client is scripted.
//!
//! Every `car.<name>` call (`car.BydCar` and one per public method and
//! property) runs the same sequence; the recorded cases of each name
//! exercise that method. The arguments:
//!
//! ```json
//! {"vin": "TESTVIN0000000001", "vehicle": {...raw Vehicle...},
//!  "capabilities": {...raw VehicleCapabilities...} | null,
//!  "latest_config": {...raw VehicleLatestConfig...},
//!  "projection_ttl": 30.0, "start": 1000.0, "ops": [...]}
//! ```
//!
//! `latest_config`, when given, makes the capabilities with
//! `VehicleCapabilities.from_latest_config`; otherwise `capabilities` is
//! validated, and `null` leaves the car to assume its defaults. `start` is
//! the monotonic clock at the start. The operations:
//!
//! - `{"op": "call", "target": "car", "method": "update_realtime", "args": {...}, "replies": [...]}`
//!   calls a method of the car (`target` `"car"`) or of one of its features
//!   (`"lock"`, `"hvac"`, `"seat"`, `"steering"`, `"battery"`, `"finder"`,
//!   `"windows"`, `"trunk"`). Each client call the method makes takes the
//!   next reply: `{"result": raw, "context": ...}` (the raw result is
//!   validated as the client would) or `{"error": <scripted error>}`, either
//!   with `"pushes"`: MQTT pushes the client delivers to the car while the
//!   call is in flight. A call with no reply left fails with `ScriptExhausted`.
//!   `handle_mqtt_realtime`, `handle_mqtt_charging` and `handle_mqtt_energy`
//!   take `{"raw", "context"}` and validate it as the client would first.
//!   `handle_mqtt_status_push` takes `{"raw", "hvac", "charging", "context"}`:
//!   the realtime partial and, unless `null`, the HVAC and charging ones,
//!   validated in that order. A push of kind `"status_push"` has the same keys.
//! - `{"op": "advance", "seconds": 5.0}` moves the clock.
//! - `{"op": "read"}` changes nothing.
//!
//! After each operation every task the car started runs to completion
//! (pushes merge after the command in flight). The answer is
//! `{"capabilities", "views", "initial", "steps"}` (or `{"error"}` when
//! the car cannot be built), each step:
//!
//! ```json
//! {"calls": [{"method": "lock", "vin": "..."}], "result": null,
//!  "error": {...}, "changes": {...}, "active": [...], "held": 1, "notified": 2,
//!  "views": {...changed feature views...}, "unused": 1}
//! ```
//!
//! `result` (on success) or `error`; `changes` as in `state.VehicleStateEngine`;
//! `views` only when a feature's view changed; `unused` only when replies
//! were left over. See `parity/groups/car.py`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use openbyd_protocol::capabilities::calls::{
    climate_start_args, describe_error, schedule_params, scripted_error, seat_level_from_json,
    seat_position_from_json,
};
use openbyd_protocol::capabilities::validation_error;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::charging::ChargingStatus;
use openbyd_protocol::models::control::CommandAck;
use openbyd_protocol::models::control::{
    BatteryHeatParams, ChargeChangeResult, ClimateScheduleParams, ClimateStartParams,
    RemoteControlResult, SeatClimateParams,
};
use openbyd_protocol::models::energy::EnergyConsumption;
use openbyd_protocol::models::gps::GpsInfo;
use openbyd_protocol::models::hvac::HvacStatus;
use openbyd_protocol::models::latest_config::{VehicleCapabilities, VehicleLatestConfig};
use openbyd_protocol::models::ota::OtaUpgradeInfo;
use openbyd_protocol::models::realtime::VehicleRealtimeData;
use openbyd_protocol::models::smart_charging::{AcCurrentLimit, SmartChargingSchedule};
use openbyd_protocol::models::vehicle::Vehicle;
use openbyd_protocol::parity::{Args, Outcome, ParityError};
use openbyd_protocol::state::{DEFAULT_PROJECTION_TTL, FieldProjection};
use serde_json::{Map, Value, json};

use super::{BydCar, CarBackend, CarInner, CarOptions, Clock, SeatMode, SeatPosition};

/// Every call name the sequence answers.
pub const CALLS: &[&str] = &[
    "car.BydCar",
    "car.vin",
    "car.state",
    "car.capabilities",
    "car.update_realtime",
    "car.update_hvac",
    "car.update_gps",
    "car.update_charging",
    "car.start_charging",
    "car.stop_charging",
    "car.update_energy",
    "car.get_ac_current_limit",
    "car.save_charging_schedule",
    "car.get_ota_version",
    "car.set_ota_auto_upgrade",
    "car.handle_mqtt_realtime",
    "car.handle_mqtt_status_push",
    "car.handle_mqtt_charging",
    "car.handle_mqtt_energy",
    "car.close",
];

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    CALLS.contains(&name).then(|| run(&Args(args)))
}

/// How often the runner yields after an operation so the car's tasks finish.
const DRAIN_YIELDS: usize = 20;

// ---------------------------------------------------------------------------
// The scripted client
// ---------------------------------------------------------------------------

/// A clock the sequence moves by hand.
struct ManualClock(Mutex<f64>);

impl ManualClock {
    fn advance(&self, seconds: f64) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) += seconds;
    }
}

impl Clock for ManualClock {
    fn monotonic(&self) -> f64 {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Default)]
struct Script {
    replies: VecDeque<Value>,
    calls: Vec<Value>,
}

/// A [`CarBackend`] that answers from the current operation's replies and
/// records every call.
#[derive(Default)]
pub struct ScriptedBackend {
    script: Mutex<Script>,
    car: OnceLock<Weak<CarInner<ScriptedBackend>>>,
}

/// An MQTT push, validated as the client validates it.
enum Push {
    Realtime(Box<VehicleRealtimeData>),
    Charging(Box<ChargingStatus>),
    Energy(Box<EnergyConsumption>),
    Status(
        Box<VehicleRealtimeData>,
        Option<Box<HvacStatus>>,
        Option<Box<ChargingStatus>>,
    ),
}

/// `raw` validated as `M`, or `None` when it is `null` (the section was not in the push).
fn optional<M: BydModel>(raw: &Value) -> Result<Option<Box<M>>, Error> {
    if raw.is_null() {
        return Ok(None);
    }
    Ok(Some(Box::new(
        M::parse(raw, &Value::Null).map_err(validation_error)?,
    )))
}

/// `push` is the whole push (or the call's arguments): `raw` and `context`,
/// and for a status push `hvac` and `charging` too.
fn parse_push(kind: &str, push: &Value) -> Result<Push, Error> {
    let part = |key| push.get(key).unwrap_or(&Value::Null);
    let (raw, context) = (part("raw"), part("context"));
    Ok(match kind {
        "status_push" => Push::Status(
            Box::new(VehicleRealtimeData::parse(raw, context).map_err(validation_error)?),
            optional(part("hvac"))?,
            optional(part("charging"))?,
        ),
        "realtime" => Push::Realtime(Box::new(
            VehicleRealtimeData::parse(raw, context).map_err(validation_error)?,
        )),
        "charging" => Push::Charging(Box::new(
            ChargingStatus::parse(raw, &Value::Null).map_err(validation_error)?,
        )),
        _ => Push::Energy(Box::new(
            EnergyConsumption::parse(raw, &Value::Null).map_err(validation_error)?,
        )),
    })
}

fn deliver(car: &Arc<CarInner<ScriptedBackend>>, push: Push) {
    match push {
        Push::Realtime(data) => {
            car.spawn_merge(move |engine, now| engine.update_realtime(*data, now))
        }
        Push::Charging(data) => {
            car.spawn_merge(move |engine, now| engine.update_charging(*data, now))
        }
        Push::Energy(data) => car.spawn_merge(move |engine, now| engine.update_energy(*data, now)),
        Push::Status(data, hvac, charging) => car.spawn_merge(move |engine, now| {
            engine.patch_status_push(Some(*data), hvac.map(|m| *m), charging.map(|m| *m), now);
        }),
    }
}

fn params_json(dump: Map<String, Value>, control: Map<String, Value>) -> Value {
    json!({"dump": dump, "control_params": control})
}

impl ScriptedBackend {
    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues one reply (see the module documentation for its form).
    pub fn queue(&self, reply: Value) {
        self.script().replies.push_back(reply);
    }

    /// The calls made so far, cleared.
    pub fn take_calls(&self) -> Vec<Value> {
        std::mem::take(&mut self.script().calls)
    }

    /// Lets the backend deliver its scripted pushes to `car`.
    pub fn attach(car: &BydCar<ScriptedBackend>) {
        let _ = car.inner.backend.car.set(Arc::downgrade(&car.inner));
    }

    /// Records `call`, takes the next reply, delivers its pushes and gives
    /// the raw result and its context, or the scripted error.
    async fn answer(&self, call: Value) -> Result<(Value, Value), Error> {
        let reply = {
            let mut script = self.script();
            script.calls.push(call);
            script.replies.pop_front()
        };
        let reply = reply
            .ok_or_else(|| Error::new(ErrorKind::ScriptExhausted, "no scripted reply left"))?;
        let pushes = reply
            .get("pushes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !pushes.is_empty() {
            if let Some(car) = self.car.get().and_then(Weak::upgrade) {
                for push in &pushes {
                    let kind = push.get("kind").and_then(Value::as_str).unwrap_or_default();
                    // The client drops a push it cannot parse.
                    if let Ok(push) = parse_push(kind, push) {
                        deliver(&car, push);
                    }
                }
            }
            tokio::task::yield_now().await;
        }
        if let Some(error) = reply.get("error") {
            return Err(scripted_error(error)
                .unwrap_or_else(|bad| Error::new(ErrorKind::AssertionError, bad.message)));
        }
        Ok((
            reply.get("result").cloned().unwrap_or(Value::Null),
            reply.get("context").cloned().unwrap_or(Value::Null),
        ))
    }

    async fn model<M: BydModel>(&self, call: Value) -> Result<M, Error> {
        let (raw, context) = self.answer(call).await?;
        M::parse(&raw, &context).map_err(validation_error)
    }

    async fn command(&self, method: &str, vin: &str) -> Result<RemoteControlResult, Error> {
        self.model(json!({"method": method, "vin": vin})).await
    }

    async fn command_with(
        &self,
        method: &str,
        vin: &str,
        params: Value,
    ) -> Result<RemoteControlResult, Error> {
        self.model(json!({"method": method, "vin": vin, "params": params}))
            .await
    }
}

impl CarBackend for ScriptedBackend {
    async fn lock(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        self.command("lock", vin).await
    }

    async fn unlock(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        self.command("unlock", vin).await
    }

    async fn start_climate(
        &self,
        vin: &str,
        params: &ClimateStartParams,
    ) -> Result<RemoteControlResult, Error> {
        let params = params_json(params.model_dump(), params.to_control_params_map());
        self.command_with("start_climate", vin, params).await
    }

    async fn stop_climate(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        self.command("stop_climate", vin).await
    }

    async fn schedule_climate(
        &self,
        vin: &str,
        params: &ClimateScheduleParams,
    ) -> Result<RemoteControlResult, Error> {
        let params = params_json(params.model_dump(), params.to_control_params_map());
        self.command_with("schedule_climate", vin, params).await
    }

    async fn set_seat_climate(
        &self,
        vin: &str,
        params: &SeatClimateParams,
    ) -> Result<RemoteControlResult, Error> {
        let params = params_json(params.model_dump(), params.to_control_params_map());
        self.command_with("set_seat_climate", vin, params).await
    }

    async fn set_battery_heat(
        &self,
        vin: &str,
        params: &BatteryHeatParams,
    ) -> Result<RemoteControlResult, Error> {
        let params = params_json(params.model_dump(), params.to_control_params_map());
        self.command_with("set_battery_heat", vin, params).await
    }

    async fn find_car(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        self.command("find_car", vin).await
    }

    async fn flash_lights(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        self.command("flash_lights", vin).await
    }

    async fn close_windows(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        self.command("close_windows", vin).await
    }

    async fn open_windows(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        self.command("open_windows", vin).await
    }

    async fn open_trunk(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        self.command("open_trunk", vin).await
    }

    async fn close_trunk(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        self.command("close_trunk", vin).await
    }

    async fn get_vehicle_realtime(&self, vin: &str) -> Result<VehicleRealtimeData, Error> {
        self.model(json!({"method": "get_vehicle_realtime", "vin": vin}))
            .await
    }

    async fn get_hvac_status(&self, vin: &str) -> Result<HvacStatus, Error> {
        self.model(json!({"method": "get_hvac_status", "vin": vin}))
            .await
    }

    async fn get_gps_info(&self, vin: &str) -> Result<GpsInfo, Error> {
        self.model(json!({"method": "get_gps_info", "vin": vin}))
            .await
    }

    /// As the client builds them: the status from the reply, the schedule
    /// from `{"vin": vin, **reply}`.
    async fn get_charging_homepage(
        &self,
        vin: &str,
    ) -> Result<(ChargingStatus, SmartChargingSchedule), Error> {
        let (raw, _) = self
            .answer(json!({"method": "get_charging_homepage", "vin": vin}))
            .await?;
        let status = ChargingStatus::parse(&raw, &Value::Null).map_err(validation_error)?;
        let Value::Object(fields) = raw else {
            return Err(Error::new(
                ErrorKind::TypeError,
                "the reply is not a mapping",
            ));
        };
        let mut merged = Map::new();
        merged.insert("vin".to_owned(), json!(vin));
        merged.extend(fields);
        let schedule = SmartChargingSchedule::parse(&Value::Object(merged), &Value::Null)
            .map_err(validation_error)?;
        Ok((status, schedule))
    }

    async fn start_charging(&self, vin: &str) -> Result<ChargeChangeResult, Error> {
        self.model(json!({"method": "start_charging", "vin": vin}))
            .await
    }

    async fn stop_charging(&self, vin: &str) -> Result<ChargeChangeResult, Error> {
        self.model(json!({"method": "stop_charging", "vin": vin}))
            .await
    }

    async fn get_energy_consumption(&self, vin: &str) -> Result<EnergyConsumption, Error> {
        self.model(json!({"method": "get_energy_consumption", "vin": vin}))
            .await
    }

    async fn get_ac_current_limit(&self, vin: &str) -> Result<AcCurrentLimit, Error> {
        self.model(json!({"method": "get_ac_current_limit", "vin": vin}))
            .await
    }

    async fn save_charging_schedule(
        &self,
        vin: &str,
        start_charge_time: &str,
        end_charge_time: &str,
        charge_way: &str,
        enabled: bool,
    ) -> Result<ChargeChangeResult, Error> {
        self.model(json!({
            "method": "save_charging_schedule",
            "vin": vin,
            "start_charge_time": start_charge_time,
            "end_charge_time": end_charge_time,
            "charge_way": charge_way,
            "enabled": enabled,
        }))
        .await
    }

    async fn get_ota_version(&self, vin: &str) -> Result<OtaUpgradeInfo, Error> {
        self.model(json!({"method": "get_ota_version", "vin": vin}))
            .await
    }

    async fn set_ota_auto_upgrade(
        &self,
        vin: &str,
        enabled: bool,
        auto_upgrade_time: &str,
        command_pwd: Option<&str>,
    ) -> Result<CommandAck, Error> {
        self.model(json!({
            "method": "set_ota_auto_upgrade",
            "vin": vin,
            "enabled": enabled,
            "auto_upgrade_time": auto_upgrade_time,
            "command_pwd": command_pwd,
        }))
        .await
    }
}

// ---------------------------------------------------------------------------
// The sequence
// ---------------------------------------------------------------------------

const SECTIONS: [&str; 6] = [
    "realtime",
    "hvac",
    "gps",
    "charging",
    "energy",
    "charging_schedule",
];

/// What changed between two snapshot dumps (as `state.VehicleStateEngine` reports it).
fn changes(before: &Value, after: &Value) -> Value {
    let text = Value::to_string;
    let mut changed = Map::new();
    for name in SECTIONS {
        let (old, new) = (&before[name], &after[name]);
        if text(old) == text(new) {
            continue;
        }
        let entry = match (old, new) {
            (Value::Object(old), Value::Object(new)) => Value::Object(
                new.iter()
                    .filter(|(field, value)| old.get(*field).map(text) != Some(text(value)))
                    .map(|(field, value)| (field.clone(), value.clone()))
                    .collect(),
            ),
            _ => new.clone(),
        };
        changed.insert(name.to_owned(), entry);
    }
    Value::Object(changed)
}

/// What each feature object shows: its availability and, for climate, the scale.
fn views(car: &BydCar<ScriptedBackend>) -> Map<String, Value> {
    let (low, high) = car.hvac.temp_range();
    let mut modes = Map::new();
    for position in [
        SeatPosition::Driver,
        SeatPosition::Copilot,
        SeatPosition::RearLeft,
        SeatPosition::RearRight,
    ] {
        modes.insert(
            openbyd_protocol::coerce::StrEnum::value(position).to_owned(),
            json!({
                "heat": car.seat.is_mode_available(position, SeatMode::Heat),
                "vent": car.seat.is_mode_available(position, SeatMode::Vent),
            }),
        );
    }
    let views = json!({
        "lock": {"available": car.lock.available()},
        "hvac": {
            "available": car.hvac.available(),
            "temp_offset": car.hvac.temp_offset(),
            "temp_range": [low, high],
        },
        "seat": {"modes": modes},
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
    });
    match views {
        Value::Object(views) => views,
        _ => unreachable!("a JSON object"),
    }
}

fn dumped<M: BydModel>(outcome: Result<M, Error>) -> Result<Value, Error> {
    outcome.map(|model| model.dump())
}

fn done(outcome: Result<(), Error>) -> Result<Value, Error> {
    outcome.map(|()| Value::Null)
}

fn unknown(target: &str, method: &str) -> ParityError {
    ParityError::bad_arguments(format!("unknown method {target}.{method}"))
}

fn text_arg(args: &Args<'_>, key: &str) -> Result<String, ParityError> {
    Ok(args.str(key)?.to_owned())
}

/// Runs one `call` operation.
async fn call(
    car: &BydCar<ScriptedBackend>,
    target: &str,
    method: &str,
    args: &Args<'_>,
) -> Result<Result<Value, Error>, ParityError> {
    Ok(match (target, method) {
        ("car", "vin") => Ok(json!(car.vin())),
        ("car", "state") => Ok(car.state().dump()),
        ("car", "capabilities") => Ok(car.capabilities().dump()),
        ("car", "close") => {
            car.close();
            Ok(Value::Null)
        }
        ("car", "update_realtime") => dumped(car.update_realtime().await),
        ("car", "update_hvac") => dumped(car.update_hvac().await),
        ("car", "update_gps") => dumped(car.update_gps().await),
        ("car", "update_charging") => dumped(car.update_charging().await),
        ("car", "update_energy") => dumped(car.update_energy().await),
        ("car", "start_charging") => dumped(car.start_charging().await),
        ("car", "stop_charging") => dumped(car.stop_charging().await),
        ("car", "get_ac_current_limit") => dumped(car.get_ac_current_limit().await),
        ("car", "get_ota_version") => dumped(car.get_ota_version().await),
        ("car", "set_ota_auto_upgrade") => {
            let time = match args.value("auto_upgrade_time") {
                Value::Null => "2:00".to_owned(),
                _ => text_arg(args, "auto_upgrade_time")?,
            };
            done(
                car.set_ota_auto_upgrade(
                    args.bool("enabled")?,
                    &time,
                    args.opt_str("command_pwd")?,
                )
                .await,
            )
        }
        ("car", "save_charging_schedule") => {
            let enabled = match args.value("enabled") {
                Value::Null => true,
                _ => args.bool("enabled")?,
            };
            dumped(
                car.save_charging_schedule(
                    &text_arg(args, "start_charge_time")?,
                    &text_arg(args, "end_charge_time")?,
                    &text_arg(args, "charge_way")?,
                    enabled,
                )
                .await,
            )
        }
        (
            "car",
            "handle_mqtt_realtime"
            | "handle_mqtt_charging"
            | "handle_mqtt_energy"
            | "handle_mqtt_status_push",
        ) => {
            let kind = method.trim_start_matches("handle_mqtt_");
            match parse_push(kind, args.0) {
                Err(error) => Err(error),
                Ok(Push::Status(data, hvac, charging)) => {
                    car.handle_mqtt_status_push(*data, hvac.map(|m| *m), charging.map(|m| *m));
                    Ok(Value::Null)
                }
                Ok(Push::Realtime(data)) => {
                    car.handle_mqtt_realtime(*data);
                    Ok(Value::Null)
                }
                Ok(Push::Charging(data)) => {
                    car.handle_mqtt_charging(*data);
                    Ok(Value::Null)
                }
                Ok(Push::Energy(data)) => {
                    car.handle_mqtt_energy(*data);
                    Ok(Value::Null)
                }
            }
        }
        ("lock", "lock") => done(car.lock.lock().await),
        ("lock", "unlock") => done(car.lock.unlock().await),
        ("hvac", "start") => done(car.hvac.start(climate_start_args(args)?).await),
        ("hvac", "stop") => done(car.hvac.stop().await),
        ("hvac", "schedule") => match schedule_params(args) {
            Ok(params) => done(car.hvac.schedule(params).await),
            Err(error) => Err(error),
        },
        ("seat", "heat" | "ventilation") => {
            let position = seat_position_from_json(args.value("position"));
            let level = seat_level_from_json(args.value("level"));
            match (position, level) {
                (Some(position), Some(level)) if method == "heat" => {
                    done(car.seat.heat(position, level).await)
                }
                (Some(position), Some(level)) => done(car.seat.ventilation(position, level).await),
                _ => Err(Error::new(
                    ErrorKind::ValueError,
                    "not a seat position and level",
                )),
            }
        }
        ("steering", "heat") => done(car.steering.heat(args.bool("on")?).await),
        ("battery", "heat") => done(car.battery.heat(args.bool("on")?).await),
        ("finder", "find") => done(car.finder.find().await),
        ("finder", "flash_lights") => done(car.finder.flash_lights().await),
        ("windows", "close") => done(car.windows.close().await),
        ("windows", "open") => done(car.windows.open().await),
        ("trunk", "open") => done(car.trunk.open().await),
        ("trunk", "close") => done(car.trunk.close().await),
        _ => return Err(unknown(target, method)),
    })
}

fn capabilities_arg(
    args: &Args<'_>,
    vin: &str,
) -> Result<Option<VehicleCapabilities>, ParityError> {
    let latest = args.value("latest_config");
    if !latest.is_null() {
        let latest = VehicleLatestConfig::parse(latest, &Value::Null)?;
        return Ok(Some(VehicleCapabilities::from_latest_config(vin, &latest)));
    }
    match args.value("capabilities") {
        Value::Null => Ok(None),
        raw => Ok(Some(VehicleCapabilities::parse(raw, &Value::Null)?)),
    }
}

fn number(args: &Args<'_>, key: &str) -> Result<f64, ParityError> {
    args.value(key)
        .as_f64()
        .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be a number")))
}

fn run(args: &Args<'_>) -> Outcome {
    let vin = args.str("vin")?.to_owned();
    let vehicle = Vehicle::parse(args.value("vehicle"), &Value::Null)?;
    let capabilities = capabilities_arg(args, &vin)?;
    let projection_ttl = match args.value("projection_ttl") {
        Value::Null => DEFAULT_PROJECTION_TTL,
        _ => number(args, "projection_ttl")?,
    };
    let clock = Arc::new(ManualClock(Mutex::new(number(args, "start")?)));
    let ops = args
        .value("ops")
        .as_array()
        .ok_or_else(|| ParityError::bad_arguments("ops must be a list"))?;
    let notified = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&notified);
    let options = CarOptions {
        capabilities,
        on_state_changed: Some(Box::new(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
        })),
        projection_ttl,
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
    };
    let car = match BydCar::new(ScriptedBackend::default(), vin, vehicle, options) {
        Ok(car) => car,
        Err(error) => return Ok(json!({"error": describe_error(&error)})),
    };
    ScriptedBackend::attach(&car);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|error| ParityError::new("RuntimeError", error.to_string()))?;
    runtime.block_on(tokio::task::unconstrained(async {
        let initial = car.state().dump();
        let initial_views = views(&car);
        let mut answer = Map::new();
        answer.insert("capabilities".to_owned(), car.capabilities().dump());
        answer.insert("views".to_owned(), Value::Object(initial_views.clone()));
        answer.insert("initial".to_owned(), initial.clone());
        let (mut before, mut before_views) = (initial, initial_views);
        let mut steps = Vec::with_capacity(ops.len());
        for op in ops {
            let op = Args(op);
            notified.store(0, Ordering::SeqCst);
            {
                let mut script = car.inner.backend.script();
                script.calls.clear();
                script.replies = op
                    .value("replies")
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into();
            }
            let mut step = Map::new();
            let outcome = match op.str("op")? {
                "call" => Some(
                    call(
                        &car,
                        op.str("target")?,
                        op.str("method")?,
                        &Args(op.value("args")),
                    )
                    .await?,
                ),
                "advance" => {
                    clock.advance(number(&op, "seconds")?);
                    None
                }
                "read" => None,
                other => return Err(ParityError::bad_arguments(format!("unknown op {other:?}"))),
            };
            for _ in 0..DRAIN_YIELDS {
                tokio::task::yield_now().await;
            }
            let (calls, unused) = {
                let mut script = car.inner.backend.script();
                (std::mem::take(&mut script.calls), script.replies.len())
            };
            step.insert("calls".to_owned(), Value::Array(calls));
            match outcome {
                Some(Ok(result)) => {
                    step.insert("result".to_owned(), result);
                }
                Some(Err(error)) => {
                    step.insert("error".to_owned(), describe_error(&error));
                }
                None => {}
            }
            let after = car.state().dump();
            step.insert("changes".to_owned(), changes(&before, &after));
            let (active, held) = {
                let engine = car.inner.engine.lock().await;
                let active: Vec<Value> = engine
                    .active_projections(clock.monotonic())
                    .iter()
                    .map(FieldProjection::to_json)
                    .collect();
                (active, engine.held_projections().len())
            };
            step.insert("active".to_owned(), Value::Array(active));
            step.insert("held".to_owned(), json!(held));
            step.insert(
                "notified".to_owned(),
                json!(notified.load(Ordering::SeqCst)),
            );
            let after_views = views(&car);
            let changed_views: Map<String, Value> = after_views
                .iter()
                .filter(|(name, view)| before_views.get(*name) != Some(view))
                .map(|(name, view)| (name.clone(), view.clone()))
                .collect();
            if !changed_views.is_empty() {
                step.insert("views".to_owned(), Value::Object(changed_views));
            }
            if unused > 0 {
                step.insert("unused".to_owned(), json!(unused));
            }
            steps.push(Value::Object(step));
            before = after;
            before_views = after_views;
        }
        answer.insert("steps".to_owned(), Value::Array(steps));
        Ok(Value::Object(answer))
    }))
}
