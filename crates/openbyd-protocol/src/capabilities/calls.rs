//! Parity calls for the capability classes and the seat enums.
//!
//! `capabilities.<file>.<Class>` builds the capability from its constructor
//! flags, reports its properties and, when a `method` is named, runs it
//! the way a bare `execute_command` would (register nothing, run the command):
//!
//! ```json
//! {"vin": "TESTVIN0000000001", "kwargs": {"available": true},
//!  "state": {"hvac": {...raw...}, "realtime": {...raw...}, "context": ...},
//!  "method": "lock", "args": {}, "reply": null, "refresh": null, "open_fn": true}
//! ```
//!
//! `state` is the snapshot `get_state` returns (`null` for none, HVAC
//! only); `reply` is what the client method does (`null` to succeed,
//! `{"error": {...}}` to raise); `refresh` is the HVAC capability's
//! `refresh_fn` (absent or `null` for none, `{"result": raw}` or
//! `{"error": {...}}`); `open_fn` says whether the windows capability has an
//! open command. The answer is
//! `{"properties": {...}, "specs": [...], "calls": [...], "error": {...}}`,
//! with `specs` once the command reached `execute_command` and `error` when
//! it raised. See `parity/groups/capabilities.py`.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::battery_heat::BatteryHeatCapability;
use super::command::{CommandPlan, validation_error};
use super::finder::FinderCapability;
use super::hvac::{ClimateStart, HvacCapability, HvacReading};
use super::lock::LockCapability;
use super::seat::{SeatAvailability, SeatCapability, SeatLevel, SeatMode, SeatPosition};
use super::steering::SteeringCapability;
use super::trunk::TrunkCapability;
use super::windows::WindowsCapability;
use crate::coerce::StrEnum;
use crate::error::{Error, ErrorKind};
use crate::models::base::BydModel;
use crate::models::control::ClimateScheduleParams;
use crate::models::hvac::HvacStatus;
use crate::models::realtime::VehicleRealtimeData;
use crate::models::vehicle::Vehicle;
use crate::parity::{Args, Outcome, ParityError};
use crate::state::{Projected, VehicleSnapshot};

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "capabilities.seat.SeatLevel" => seat_level(&args),
        "capabilities.seat.SeatPosition" => seat_position(&args),
        "capabilities.lock.LockCapability"
        | "capabilities.hvac.HvacCapability"
        | "capabilities.seat.SeatCapability"
        | "capabilities.steering.SteeringCapability"
        | "capabilities.battery_heat.BatteryHeatCapability"
        | "capabilities.finder.FinderCapability"
        | "capabilities.windows.WindowsCapability"
        | "capabilities.trunk.TrunkCapability" => capability_call(name, &args),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

fn value_error(message: &str) -> ParityError {
    ParityError::new("ValueError", message)
}

/// `SeatLevel(value)`: a value equal to a member's (`True`, `1.0`) finds it.
pub fn seat_level_from_json(value: &Value) -> Option<SeatLevel> {
    let number = match value {
        Value::Bool(flag) => i64::from(*flag),
        Value::Number(number) => match number.as_i64() {
            Some(int) => int,
            None => {
                let float = number.as_f64()?;
                if float.fract() != 0.0 || float.abs() > 4.0 {
                    return None;
                }
                float as i64
            }
        },
        _ => return None,
    };
    SeatLevel::from_value(number)
}

/// `SeatPosition(value)`: only the exact value string finds a member.
pub fn seat_position_from_json(value: &Value) -> Option<SeatPosition> {
    value.as_str().and_then(SeatPosition::from_value)
}

fn seat_level(args: &Args<'_>) -> Outcome {
    let level = seat_level_from_json(args.value("value"))
        .ok_or_else(|| value_error("not a valid SeatLevel"))?;
    Ok(json!({
        "name": level.name(),
        "value": level.value(),
        "command_value": level.to_command_value(),
        "status_value": level.to_status_value() as i64,
    }))
}

fn seat_position(args: &Args<'_>) -> Outcome {
    let position = seat_position_from_json(args.value("value"))
        .ok_or_else(|| value_error("not a valid SeatPosition"))?;
    Ok(json!({"name": position.name(), "value": position.value()}))
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// An error a script asks for: `{"class", "code", "endpoint", "status_code"}`
/// (all but `class` optional), built as pyBYD's constructor builds it.
pub fn scripted_error(description: &Value) -> Result<Error, ParityError> {
    let kind = description
        .get("class")
        .and_then(Value::as_str)
        .and_then(ErrorKind::from_class_name)
        .ok_or_else(|| ParityError::bad_arguments("unknown scripted error class"))?;
    let text = |key: &str| {
        description
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let message = format!("scripted {}", kind.class_name());
    Ok(if kind.is_a(ErrorKind::BydApiError) {
        Error::api(kind, message, text("code"), text("endpoint"))
    } else if kind.is_a(ErrorKind::BydTransportError) {
        let status = description
            .get("status_code")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok());
        Error::transport(message, status, text("endpoint"))
    } else {
        Error::new(kind, message)
    })
}

/// An error as the traces show it: its class and pyBYD's attributes
/// (`null` where the class has no such attribute).
///
/// A validation failure whose validator let an exception escape that
/// [`ErrorKind`] has no member for (`OverflowError`, `OSError`) is a
/// `ValidationError` here; its message, from [`validation_error`], starts
/// with the escaping class, which is reported.
pub fn describe_error(error: &Error) -> Value {
    let api = error.is_a(ErrorKind::BydApiError);
    let transport = error.is_a(ErrorKind::BydTransportError);
    let class = match error.kind() {
        ErrorKind::ValidationError => error
            .message()
            .split_once(" (")
            .map(|(class, _)| class)
            .filter(|class| class.ends_with("Error") && !class.contains(' '))
            .unwrap_or("ValidationError"),
        _ => error.class_name(),
    };
    json!({
        "class": class,
        "code": if api { json!(error.code()) } else { Value::Null },
        "endpoint": if api || transport { json!(error.endpoint()) } else { Value::Null },
        "status_code": if transport { json!(error.status_code()) } else { Value::Null },
    })
}

/// What a scripted client method or refresh does: `Ok(raw result)` or the error it raises.
fn scripted_reply(reply: &Value) -> Result<Result<Value, Error>, ParityError> {
    match reply.get("error") {
        Some(error) => Ok(Err(scripted_error(error)?)),
        None => Ok(Ok(reply.get("result").cloned().unwrap_or(Value::Null))),
    }
}

// ---------------------------------------------------------------------------
// Capability classes
// ---------------------------------------------------------------------------

/// A constructor flag: absent gives the class default, `null` is unknown.
fn flag(
    kwargs: &Map<String, Value>,
    name: &str,
    default: Option<bool>,
) -> Result<Option<bool>, ParityError> {
    match kwargs.get(name) {
        None => Ok(default),
        Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(ParityError::bad_arguments(format!(
            "{name} must be a boolean or null"
        ))),
    }
}

/// The snapshot `get_state` returns: `None` when there is no `get_state`.
fn parse_state(vin: &str, state: &Value) -> Result<Option<VehicleSnapshot>, ParityError> {
    if state.is_null() {
        return Ok(None);
    }
    let state = Args(state);
    let mut snapshot = VehicleSnapshot::new(Arc::new(Vehicle::parse(
        &json!({"vin": vin}),
        &Value::Null,
    )?));
    if !state.value("hvac").is_null() {
        let hvac = HvacStatus::parse(state.value("hvac"), &Value::Null)?;
        snapshot.hvac = Some(Projected::new(Arc::new(hvac)));
    }
    if !state.value("realtime").is_null() {
        let realtime = VehicleRealtimeData::parse(state.value("realtime"), state.value("context"))?;
        snapshot.realtime = Some(Projected::new(Arc::new(realtime)));
    }
    Ok(Some(snapshot))
}

fn bool_arg(args: &Args<'_>, key: &str) -> Result<bool, ParityError> {
    args.bool(key)
}

fn optional_int(args: &Args<'_>, key: &str) -> Result<Option<i64>, ParityError> {
    match args.value(key) {
        Value::Null => Ok(None),
        value => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be an integer"))),
    }
}

fn seat_args(args: &Args<'_>) -> Result<(SeatPosition, SeatLevel), Error> {
    let position = seat_position_from_json(args.value("position"))
        .ok_or_else(|| Error::new(ErrorKind::ValueError, "not a valid SeatPosition"))?;
    let level = seat_level_from_json(args.value("level"))
        .ok_or_else(|| Error::new(ErrorKind::ValueError, "not a valid SeatLevel"))?;
    Ok((position, level))
}

/// The arguments of `HvacCapability.start`, with pyBYD's defaults.
pub fn climate_start_args(args: &Args<'_>) -> Result<ClimateStart, ParityError> {
    let defaults = ClimateStart::default();
    Ok(ClimateStart {
        temperature: match args.value("temperature") {
            Value::Null => defaults.temperature,
            value => value
                .as_f64()
                .ok_or_else(|| ParityError::bad_arguments("temperature must be a number"))?,
        },
        duration: optional_int(args, "duration")?.unwrap_or(defaults.duration),
        cycle_mode: optional_int(args, "cycle_mode")?,
        fan_level: optional_int(args, "fan_level")?,
    })
}

/// `ClimateScheduleParams(**args["params"])`.
pub fn schedule_params(args: &Args<'_>) -> Result<ClimateScheduleParams, Error> {
    let empty = Map::new();
    let kwargs = args.value("params").as_object().unwrap_or(&empty);
    ClimateScheduleParams::from_kwargs(kwargs).map_err(validation_error)
}

struct Run<'a> {
    snapshot: &'a VehicleSnapshot,
    vin: &'a str,
    reply: Result<Value, Error>,
    out: Map<String, Value>,
    calls: Vec<Value>,
}

impl Run<'_> {
    /// A bare `execute_command`: record the specs, then run the command.
    fn execute(&mut self, plan: Result<CommandPlan, Error>) -> Result<(), Error> {
        let plan = plan?;
        self.out.insert(
            "specs".to_owned(),
            Value::Array(plan.specs.iter().map(|spec| spec.to_json()).collect()),
        );
        let call = plan.resolve(self.snapshot);
        self.calls.push(call.to_json(self.vin));
        self.reply.clone().map(drop)
    }
}

fn capability_call(name: &str, args: &Args<'_>) -> Outcome {
    let vin = args.str("vin")?;
    let empty = Map::new();
    let kwargs = args.value("kwargs").as_object().unwrap_or(&empty);
    let state = parse_state(vin, args.value("state"))?;
    let reply = match args.value("reply") {
        Value::Null => Ok(Value::Null),
        reply => scripted_reply(reply)?,
    };
    let method = args.opt_str("method")?;
    let method_args = Args(args.value("args"));
    let blank = VehicleSnapshot::new(Arc::new(Vehicle::parse(
        &json!({"vin": vin}),
        &Value::Null,
    )?));
    let mut run = Run {
        snapshot: state.as_ref().unwrap_or(&blank),
        vin,
        reply,
        out: Map::new(),
        calls: Vec::new(),
    };
    let short = name.rsplit('.').next().unwrap_or_default();
    let (properties, outcome): (Value, Option<Result<(), Error>>) = match short {
        "LockCapability" => {
            let cap = LockCapability::new(vin, flag(kwargs, "available", Some(true))?);
            let outcome = method.map(|method| match method {
                "lock" => run.execute(cap.lock()),
                _ => run.execute(cap.unlock()),
            });
            (json!({"available": cap.available()}), outcome)
        }
        "SteeringCapability" => {
            let cap = SteeringCapability::new(vin, flag(kwargs, "available", Some(true))?);
            let outcome = match method {
                None => None,
                Some(_) => Some(run.execute(cap.heat(bool_arg(&method_args, "on")?))),
            };
            (json!({"available": cap.available()}), outcome)
        }
        "BatteryHeatCapability" => {
            let cap = BatteryHeatCapability::new(vin, flag(kwargs, "available", Some(true))?);
            let outcome = match method {
                None => None,
                Some(_) => Some(run.execute(cap.heat(bool_arg(&method_args, "on")?))),
            };
            (json!({"available": cap.available()}), outcome)
        }
        "FinderCapability" => {
            let cap = FinderCapability::new(
                vin,
                flag(kwargs, "find_available", Some(true))?,
                flag(kwargs, "flash_available", Some(true))?,
            );
            let outcome = method.map(|method| match method {
                "find" => run.execute(cap.find()),
                _ => run.execute(cap.flash_lights()),
            });
            let properties = json!({
                "find_available": cap.find_available(),
                "flash_available": cap.flash_available(),
            });
            (properties, outcome)
        }
        "TrunkCapability" => {
            let cap = TrunkCapability::new(
                vin,
                flag(kwargs, "open_available", Some(true))?,
                flag(kwargs, "close_available", Some(true))?,
            );
            let outcome = method.map(|method| match method {
                "open" => run.execute(cap.open()),
                _ => run.execute(cap.close()),
            });
            let properties = json!({
                "open_available": cap.open_available(),
                "close_available": cap.close_available(),
            });
            (properties, outcome)
        }
        "WindowsCapability" => {
            let can_open = !matches!(args.value("open_fn"), Value::Bool(false));
            let cap = WindowsCapability::new(
                vin,
                flag(kwargs, "close_available", Some(true))?,
                flag(kwargs, "open_available", Some(true))?,
                can_open,
            );
            let outcome = method.map(|method| match method {
                "open" => run.execute(cap.open()),
                _ => run.execute(cap.close()),
            });
            let properties = json!({
                "close_available": cap.close_available(),
                "open_available": cap.open_available(),
            });
            (properties, outcome)
        }
        "SeatCapability" => {
            let defaults = SeatAvailability::default();
            let flags = SeatAvailability {
                driver_heat: flag(kwargs, "driver_heat_available", defaults.driver_heat)?,
                driver_ventilation: flag(
                    kwargs,
                    "driver_ventilation_available",
                    defaults.driver_ventilation,
                )?,
                passenger_heat: flag(kwargs, "passenger_heat_available", defaults.passenger_heat)?,
                passenger_ventilation: flag(
                    kwargs,
                    "passenger_ventilation_available",
                    defaults.passenger_ventilation,
                )?,
                rear_left_heat: flag(kwargs, "rear_left_heat_available", None)?,
                rear_left_ventilation: flag(kwargs, "rear_left_ventilation_available", None)?,
                rear_right_heat: flag(kwargs, "rear_right_heat_available", None)?,
                rear_right_ventilation: flag(kwargs, "rear_right_ventilation_available", None)?,
            };
            let cap = SeatCapability::new(vin, flags);
            let outcome = method.map(|method| {
                let (position, level) = seat_args(&method_args)?;
                match method {
                    "heat" => run.execute(cap.heat(position, level)),
                    _ => run.execute(cap.ventilation(position, level)),
                }
            });
            (seat_modes(&cap), outcome)
        }
        _ => {
            let cap = HvacCapability::new(vin, flag(kwargs, "available", Some(true))?);
            let reading = state
                .as_ref()
                .and_then(|state| state.hvac.as_ref())
                .map(HvacReading::from_projected);
            let (low, high) = HvacCapability::temp_range(reading.as_ref());
            let properties = json!({
                "available": cap.available(),
                "temp_offset": HvacCapability::temp_offset(reading.as_ref()),
                "temp_range": [low, high],
            });
            let outcome = match method {
                None => None,
                Some("start") => {
                    let start = climate_start_args(&method_args)?;
                    let refresh = match args.value("refresh") {
                        Value::Null => None,
                        refresh => Some(scripted_reply(refresh)?),
                    };
                    Some(hvac_start(&cap, &start, reading, refresh, &mut run))
                }
                Some("stop") => Some(run.execute(cap.stop())),
                Some(_) => Some(
                    schedule_params(&method_args)
                        .and_then(|params| run.execute(cap.schedule(params))),
                ),
            };
            (properties, outcome)
        }
    };
    let mut out = std::mem::take(&mut run.out);
    let mut answer = Map::new();
    answer.insert("properties".to_owned(), properties);
    if let Some(specs) = out.remove("specs") {
        answer.insert("specs".to_owned(), specs);
    }
    answer.insert("calls".to_owned(), Value::Array(run.calls));
    if let Some(Err(error)) = outcome {
        answer.insert("error".to_owned(), describe_error(&error));
    }
    Ok(Value::Object(answer))
}

/// `HvacCapability.start` with a scripted `refresh_fn`.
fn hvac_start(
    cap: &HvacCapability,
    start: &ClimateStart,
    mut reading: Option<HvacReading>,
    refresh: Option<Result<Value, Error>>,
    run: &mut Run<'_>,
) -> Result<(), Error> {
    let time_span = cap.check_start(start)?;
    if reading.is_none()
        && let Some(refresh) = refresh
    {
        run.calls.push(json!({"method": "refresh"}));
        reading = match refresh {
            Ok(raw) => Some(HvacReading::from_status(
                &HvacStatus::parse(&raw, &Value::Null).map_err(validation_error)?,
            )),
            Err(error) if error.is_a(ErrorKind::BydError) => None,
            Err(error) => return Err(error),
        };
    }
    run.execute(cap.start(start, time_span, reading.as_ref()))
}

/// Every seat and mode's availability: `{"driver": {"heat": true, "vent": true}, ...}`.
fn seat_modes(cap: &SeatCapability) -> Value {
    let mut modes = Map::new();
    for position in SeatPosition::MEMBERS {
        modes.insert(
            position.value().to_owned(),
            json!({
                "heat": cap.is_mode_available(*position, SeatMode::Heat),
                "vent": cap.is_mode_available(*position, SeatMode::Vent),
            }),
        );
    }
    json!({"modes": modes})
}
