//! The client's parity calls: `client.<method>` runs one [`BydClient`]
//! method against a script and returns its trace, which
//! `parity/api_support.py` (`run_client`) produces from pyBYD's real
//! `BydClient` under the same rules.
//!
//! The call's arguments:
//!
//! ```json
//! {
//!   "config": {...}, "now_ms": 0, "seed": "s",
//!   "replies": [...],               // HTTP replies, as for the flows
//!   "mqtt": [{"at": 1.0, "event": {...}}, ...],   // see crate::scripted
//!   "mqtt_starts": [true, false],   // whether each MQTT start succeeds
//!   "callbacks": true,              // record the user callbacks as steps
//!   "options": {"command_ack_ttl_seconds": 300, "command_retries": 1,
//!               "gate_bypass": ["function_no_missing"], "pin_passthrough": true},
//!   "state": {"session": {...}, "guest_vins": {}, "user_type_settled": [],
//!             "commands_enabled": true, "vehicles": [...],
//!             "capabilities": {"VIN": {...latest config...}}, "mqtt": true},
//!   "setup": [{"method": "login", "args": {}}, {"advance": 10.0}],
//!   "args": {...}                   // the method's own arguments
//! }
//! ```
//!
//! The trace is `{"steps", "result" | "error", "state", "unused_replies",
//! "unused_mqtt"}`. Steps are the flows' posts and sleeps, the MQTT steps of
//! [`crate::scripted`], `{"setup_error": class}` for a setup call that
//! failed, and one step per callback: `{"on_vehicle_info": [vin, data]}`,
//! `{"on_mqtt_event": [event, vin, respond_data]}`, `{"on_command_ack":
//! ack}`, `{"on_command_lifecycle": event}`, `{"on_mqtt_connect": null}`.
//! Everything still due on the MQTT timeline is delivered after the call.

use std::collections::BTreeMap;
use std::sync::Arc;

use openbyd_protocol::config::BydConfig;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::control::{
    BatteryHeatParams, ClimateScheduleParams, ClimateStartParams, SeatClimateParams,
};
use openbyd_protocol::models::latest_config::{VehicleCapabilities, VehicleLatestConfig};
use openbyd_protocol::models::message_center::NotificationCategory;
use openbyd_protocol::models::vehicle::Vehicle;
use openbyd_protocol::parity::{Args, Outcome, ParityError};
use openbyd_protocol::session::Session;
use serde_json::{Map, Value, json};

use super::{BydClient, ChargePollSettings, ClientOptions, PollSettings};
use crate::api::control::model_error;
use crate::api::smart_charging::EnergyTypeArg;
use crate::api::vehicle::{dump_all, parse_model};
use crate::env::Env;
use crate::scripted::{Reply, ScriptedEnv, ScriptedMqtt, block_on, describe};

type Client = BydClient<Arc<ScriptedEnv>>;

/// A float as the trace writes it: non-finite values as text.
fn number(value: f64) -> Value {
    if value.is_finite() {
        json!(value)
    } else if value.is_nan() {
        json!("nan")
    } else if value > 0.0 {
        json!("inf")
    } else {
        json!("-inf")
    }
}

/// The class a failure is reported as: an exception pydantic let escape
/// keeps its own name.
fn error_trace(error: &Error) -> Value {
    let mut described = describe(error);
    for name in ["OverflowError", "OSError"] {
        if error.kind() == ErrorKind::ValueError && error.message().starts_with(name) {
            described["class"] = json!(name);
        }
    }
    described
}

fn object<'a>(args: &Args<'a>, key: &str) -> Result<Option<&'a Map<String, Value>>, ParityError> {
    match args.value(key) {
        Value::Null => Ok(None),
        Value::Object(map) => Ok(Some(map)),
        _ => Err(ParityError::bad_arguments(format!(
            "{key} must be an object"
        ))),
    }
}

fn float_or(args: &Args<'_>, key: &str, default: f64) -> Result<f64, ParityError> {
    match args.value(key) {
        Value::Null => Ok(default),
        value => value
            .as_f64()
            .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be a number"))),
    }
}

fn opt_float(args: &Args<'_>, key: &str) -> Result<Option<f64>, ParityError> {
    match args.value(key) {
        Value::Null => Ok(None),
        value => value
            .as_f64()
            .map(Some)
            .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be a number"))),
    }
}

fn int_or(args: &Args<'_>, key: &str, default: i64) -> Result<i64, ParityError> {
    match args.value(key) {
        Value::Null => Ok(default),
        _ => args.i64(key),
    }
}

fn bool_or(args: &Args<'_>, key: &str, default: bool) -> Result<bool, ParityError> {
    match args.value(key) {
        Value::Null => Ok(default),
        Value::Bool(flag) => Ok(*flag),
        _ => Err(ParityError::bad_arguments(format!(
            "{key} must be a boolean"
        ))),
    }
}

fn strings(value: &Value) -> Result<Vec<String>, ParityError> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| ParityError::bad_arguments("expected a list of strings"))
            })
            .collect(),
        _ => Err(ParityError::bad_arguments("expected a list of strings")),
    }
}

fn is_hex32(text: &str) -> bool {
    text.len() == 32 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// The options of a parity call, with callbacks that record into `env`.
fn options(args: &Args<'_>, env: &Arc<ScriptedEnv>) -> Result<ClientOptions, ParityError> {
    let mut options = ClientOptions::default();
    if bool_or(args, "callbacks", false)? {
        let record = |env: &Arc<ScriptedEnv>| {
            let env = Arc::clone(env);
            move |step: Value| env.record(step)
        };
        let step = record(env);
        options.on_vehicle_info = Some(Arc::new(move |vin, data| {
            step(json!({"on_vehicle_info": [vin, data.dump()]}));
        }));
        let step = record(env);
        options.on_mqtt_event = Some(Arc::new(move |event, vin, data| {
            step(json!({"on_mqtt_event": [event, vin, data]}));
        }));
        let step = record(env);
        options.on_command_ack = Some(Arc::new(move |ack| {
            step(json!({"on_command_ack": ack.dump()}));
        }));
        let step = record(env);
        options.on_command_lifecycle = Some(Arc::new(move |event| {
            step(json!({"on_command_lifecycle": event.dump()}));
        }));
        let step = record(env);
        options.on_mqtt_connect = Some(Arc::new(move || step(json!({"on_mqtt_connect": null}))));
    }
    if object(args, "options")?.is_none() {
        return Ok(options);
    }
    let given = Args(args.value("options"));
    options.command_ack_ttl_seconds = float_or(&given, "command_ack_ttl_seconds", 300.0)?;
    options.command_retries = int_or(&given, "command_retries", 3)?;
    let bypass = strings(given.value("gate_bypass"))?;
    if !bypass.is_empty() {
        options.gate_policy = Some(Arc::new(move |_vin, verdict| {
            verdict.supported || bypass.contains(&verdict.reason)
        }));
    }
    if bool_or(&given, "pin_passthrough", false)? {
        options.pin_resolver = Some(Arc::new(|given, config| match given {
            Some(pwd) if is_hex32(openbyd_protocol::coerce::py_strip(pwd)) => {
                openbyd_protocol::coerce::py_strip(pwd).to_owned()
            }
            other => super::resolve_command_pwd(other, config),
        }));
    }
    Ok(options)
}

/// Puts the client in the call's starting state, without any step.
fn install_state(client: &Client, args: &Args<'_>) -> Result<(), ParityError> {
    let Some(state) = object(args, "state")? else {
        return Ok(());
    };
    let _ = state;
    let given = Args(args.value("state"));
    let monotonic = client.env().monotonic();
    let session = match object(&given, "session")? {
        Some(kwargs) => Some(Arc::new(Session::from_kwargs(kwargs, monotonic)?)),
        None => None,
    };
    let mut inner = client.state();
    if let Some(guests) = object(&given, "guest_vins")? {
        for (vin, guest) in guests {
            let guest = guest
                .as_bool()
                .ok_or_else(|| ParityError::bad_arguments("guest_vins values are booleans"))?;
            match &session {
                Some(session) => session.set_guest(vin, guest),
                None => {
                    inner.guest_vins.insert(vin.clone(), guest);
                }
            }
        }
    }
    if let Some(session) = &session {
        for pair in given
            .value("user_type_settled")
            .as_array()
            .into_iter()
            .flatten()
        {
            match (pair[0].as_str(), pair[1].as_str()) {
                (Some(vin), Some(endpoint)) => session.settle_user_type(vin, endpoint),
                _ => return Err(ParityError::bad_arguments("user_type_settled holds pairs")),
            }
        }
        inner.session = Some(Arc::clone(session));
        inner.guest_owner = Some(Arc::clone(session));
    }
    inner.commands_enabled = bool_or(&given, "commands_enabled", false)?;
    for raw in given.value("vehicles").as_array().into_iter().flatten() {
        let vehicle: Vehicle = parse_model(raw)?;
        inner.put_vehicle(vehicle);
    }
    if let Some(capabilities) = object(&given, "capabilities")? {
        for (vin, raw) in capabilities {
            let latest: VehicleLatestConfig = parse_model(raw)?;
            inner.capabilities.insert(
                vin.clone(),
                VehicleCapabilities::from_latest_config(vin, &latest),
            );
        }
    }
    if bool_or(&given, "mqtt", false)? {
        inner.mqtt_started = true;
        let sink: std::sync::Weak<dyn crate::env::MqttSink> = client.inner.me.clone();
        client.env().install_mqtt(sink);
    }
    Ok(())
}

/// The client's state after the call, as the trace shows it.
fn state_trace(client: &Client) -> Value {
    let running = crate::env::ClientEnv::mqtt_running(client.env());
    let state = client.state();
    let session = state.session.as_ref().map(|session| {
        json!([
            session.user_id(),
            session.sign_token(),
            session.encry_token(),
            number(session.created_at()),
            number(session.ttl()),
        ])
    });
    let mut settled: Vec<(String, String)> = state
        .session
        .as_ref()
        .map(|session| session.user_type_settled().into_iter().collect())
        .unwrap_or_default();
    settled.sort();
    let guests: BTreeMap<String, bool> = state.guest_map().into_iter().collect();
    let mut capabilities: Vec<(String, String)> = state
        .capabilities
        .iter()
        .map(|(vin, capabilities)| (vin.clone(), capabilities.source.clone()))
        .collect();
    capabilities.sort();
    json!({
        "session": session,
        "guest_vins": guests,
        "user_type_settled": settled,
        "commands_enabled": state.commands_enabled,
        "vehicles": state.vehicles.iter().map(|(vin, _)| vin).collect::<Vec<_>>(),
        "capabilities": capabilities,
        "mqtt_running": state.mqtt_started && running,
        "waiters": state.waiters.len(),
        "pending": state.pending.iter().map(|pending| json!([
            pending.vin, pending.serial, pending.command, number(pending.created_at),
        ])).collect::<Vec<_>>(),
        "counters": [state.matched, state.expired, state.uncorrelated],
        "data_poll_serials": state.data_poll_serials.len(),
        "reauth": [number(state.reauth_at), state.decrypt_reauths],
        "recovery": [state.recovery_attempts, number(state.recovery_last_at)],
        "closed": state.closed,
    })
}

fn poll_settings(args: &Args<'_>) -> Result<PollSettings, ParityError> {
    let defaults = PollSettings::default();
    Ok(PollSettings {
        poll_attempts: int_or(args, "poll_attempts", defaults.poll_attempts)?,
        poll_interval: float_or(args, "poll_interval", defaults.poll_interval)?,
        mqtt_timeout: opt_float(args, "mqtt_timeout")?,
        signal_retries: int_or(args, "signal_retries", defaults.signal_retries)?,
    })
}

fn charge_settings(args: &Args<'_>) -> Result<ChargePollSettings, ParityError> {
    let defaults = ChargePollSettings::default();
    Ok(ChargePollSettings {
        mqtt_timeout: opt_float(args, "mqtt_timeout")?,
        poll_attempts: int_or(args, "poll_attempts", defaults.poll_attempts)?,
        poll_interval: float_or(args, "poll_interval", defaults.poll_interval)?,
    })
}

fn kwargs<'a>(args: &Args<'a>) -> Result<&'a Map<String, Value>, ParityError> {
    object(args, "params")?.ok_or_else(|| ParityError::bad_arguments("params must be an object"))
}

/// Runs one method; `Ok(Err(..))` is the method's own failure.
async fn run_method(
    client: &Client,
    method: &str,
    args: &Args<'_>,
) -> Result<Result<Value, Error>, ParityError> {
    let vin = || args.str("vin");
    let pwd = || args.opt_str("command_pwd");
    macro_rules! model {
        ($call:expr) => {
            $call.await.map(|model| model.dump())
        };
    }
    Ok(match method {
        "login" => client.login().await.map(|()| Value::Null),
        "ensure_session" => client
            .ensure_session()
            .await
            .map(|session| json!([session.user_id(), number(session.created_at())])),
        "invalidate_session" => {
            client.invalidate_session();
            Ok(Value::Null)
        }
        "async_start" => {
            client.async_start();
            Ok(Value::Null)
        }
        "async_close" => {
            client.async_close();
            Ok(Value::Null)
        }
        "commands_enabled" => Ok(json!(client.commands_enabled())),
        "get_command_ack_diagnostics" => Ok(client.get_command_ack_diagnostics().dump()),
        "on_mqtt_events" => Ok(Value::Null),
        "get_vehicles" => client
            .get_vehicles()
            .await
            .map(|vehicles| dump_all(&vehicles)),
        "get_latest_configs" => {
            let vins = strings(args.value("vins"))?;
            client.get_latest_configs(&vins).await.map(|configs| {
                Value::Object(
                    configs
                        .into_iter()
                        .map(|(vin, config)| (vin, config.dump()))
                        .collect(),
                )
            })
        }
        "get_latest_config" => model!(client.get_latest_config(vin()?)),
        "get_vehicle_capabilities" => {
            let force = bool_or(args, "force_refresh", false)?;
            Ok(client.get_vehicle_capabilities(vin()?, force).await.dump())
        }
        "get_vehicle_realtime" => model!(client.get_vehicle_realtime(vin()?, poll_settings(args)?)),
        "get_gps_info" => model!(client.get_gps_info(vin()?, poll_settings(args)?)),
        "get_hvac_status" => model!(client.get_hvac_status(vin()?)),
        "get_charging_status" => model!(client.get_charging_status(vin()?)),
        "get_charging_schedule" => model!(client.get_charging_schedule(vin()?)),
        "get_charging_homepage" => client
            .get_charging_homepage(vin()?)
            .await
            .map(|(status, schedule)| json!([status.dump(), schedule.dump()])),
        "get_ac_current_limit" => {
            let energy_type = match args.value("energy_type") {
                Value::Null => None,
                Value::String(text) => Some(EnergyTypeArg::Text(text.clone())),
                value => Some(EnergyTypeArg::Int(value.as_i64().ok_or_else(|| {
                    ParityError::bad_arguments("energy_type is an int or a string")
                })?)),
            };
            model!(client.get_ac_current_limit(vin()?, energy_type))
        }
        "get_energy_consumption" => model!(client.get_energy_consumption(vin()?)),
        "get_push_state" => model!(client.get_push_state(vin()?)),
        "set_push_state" => model!(client.set_push_state(
            vin()?,
            bool_or(args, "enable", false)?,
            int_or(args, "push_type", 701)?
        )),
        "get_notifications" => {
            let category = match args.value("category") {
                Value::Null => NotificationCategory::Vehicle,
                value => NotificationCategory::from_json(value)
                    .ok_or_else(|| ParityError::bad_arguments("unknown category"))?,
            };
            client
                .get_notifications(category)
                .await
                .map(|messages| dump_all(&messages))
        }
        "get_all_notifications" => client.get_all_notifications().await.map(|all| {
            Value::Object(
                all.into_iter()
                    .map(|(category, messages)| {
                        ((category as i64).to_string(), dump_all(&messages))
                    })
                    .collect(),
            )
        }),
        "rename_vehicle" => model!(client.rename_vehicle(vin()?, args.str("name")?)),
        "get_booking_list" => model!(client.get_booking_list(vin()?)),
        "verify_command_access" => model!(client.verify_command_access(vin()?, pwd()?)),
        "lock" => model!(client.lock(vin()?, pwd()?)),
        "unlock" => model!(client.unlock(vin()?, pwd()?)),
        "stop_climate" => model!(client.stop_climate(vin()?, pwd()?)),
        "flash_lights" => model!(client.flash_lights(vin()?, pwd()?)),
        "close_windows" => model!(client.close_windows(vin()?, pwd()?)),
        "open_windows" => model!(client.open_windows(vin()?, pwd()?)),
        "find_car" => model!(client.find_car(vin()?, pwd()?)),
        "open_trunk" => model!(client.open_trunk(vin()?, pwd()?)),
        "close_trunk" => model!(client.close_trunk(vin()?, pwd()?)),
        "start_climate" => match ClimateStartParams::from_kwargs(kwargs(args)?) {
            Ok(params) => model!(client.start_climate(vin()?, &params, pwd()?)),
            Err(error) => Err(model_error(error)),
        },
        "schedule_climate" => match ClimateScheduleParams::from_kwargs(kwargs(args)?) {
            Ok(params) => model!(client.schedule_climate(vin()?, &params, pwd()?)),
            Err(error) => Err(model_error(error)),
        },
        "set_seat_climate" => match SeatClimateParams::from_kwargs(kwargs(args)?) {
            Ok(params) => model!(client.set_seat_climate(vin()?, &params, pwd()?)),
            Err(error) => Err(model_error(error)),
        },
        "set_battery_heat" => match BatteryHeatParams::from_kwargs(kwargs(args)?) {
            Ok(params) => model!(client.set_battery_heat(vin()?, &params, pwd()?)),
            Err(error) => Err(model_error(error)),
        },
        "save_charging_schedule" => model!(client.save_charging_schedule(
            vin()?,
            args.str("start_charge_time")?,
            args.str("end_charge_time")?,
            args.str("charge_way")?,
            bool_or(args, "enabled", true)?,
            charge_settings(args)?
        )),
        "toggle_smart_charging" => {
            model!(client.toggle_smart_charging(vin()?, bool_or(args, "enable", false)?))
        }
        "start_charging" => model!(client.start_charging(vin()?, charge_settings(args)?)),
        "stop_charging" => model!(client.stop_charging(vin()?, charge_settings(args)?)),
        other => {
            return Err(ParityError::new(
                "UnknownCall",
                format!("no client method {other:?}"),
            ));
        }
    })
}

fn stalled() -> ParityError {
    ParityError::new(
        "Stalled",
        "the client waited on something the script cannot give",
    )
}

/// Runs the parity call `client.<method>`.
fn client_call(method: &str, raw: &Value) -> Outcome {
    let args = Args(raw);
    let replies = args
        .value("replies")
        .as_array()
        .ok_or_else(|| ParityError::bad_arguments("replies must be a list"))?
        .iter()
        .map(Reply::from_json)
        .collect::<Result<Vec<_>, _>>()?;
    let mqtt = ScriptedMqtt::from_json(args.value("mqtt"))?;
    let starts = args
        .value("mqtt_starts")
        .as_array()
        .map(|starts| {
            starts
                .iter()
                .map(|start| start.as_bool().unwrap_or(true))
                .collect()
        })
        .unwrap_or_default();
    let env = Arc::new(
        ScriptedEnv::new(args.i64("now_ms")?, args.str("seed")?, replies).with_mqtt(mqtt, starts),
    );
    let config = BydConfig::from_kwargs(
        object(&args, "config")?.ok_or_else(|| ParityError::bad_arguments("config is required"))?,
    )?;
    let options = options(&args, &env)?;
    let client: Client = BydClient::new(config, Arc::clone(&env), options);
    install_state(&client, &args)?;

    for step in args.value("setup").as_array().into_iter().flatten() {
        if let Some(seconds) = step.get("advance").and_then(Value::as_f64) {
            env.advance(seconds);
            continue;
        }
        let name = step
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| ParityError::bad_arguments("a setup step names a method"))?;
        let setup_args = step.get("args").cloned().unwrap_or_else(|| json!({}));
        let outcome =
            block_on(run_method(&client, name, &Args(&setup_args))).ok_or_else(stalled)??;
        if let Err(error) = outcome {
            env.record(json!({ "setup_error": error_trace(&error)["class"] }));
        }
    }

    let call_args = args.value("args").clone();
    let outcome =
        block_on(run_method(&client, method, &Args(&call_args))).ok_or_else(stalled)??;
    env.drain();
    if env.stalled() {
        return Err(stalled());
    }
    let mut trace = Map::new();
    trace.insert("steps".into(), Value::Array(env.steps()));
    match outcome {
        Ok(value) => trace.insert("result".into(), value),
        Err(error) => trace.insert("error".into(), error_trace(&error)),
    };
    trace.insert("state".into(), state_trace(&client));
    trace.insert("unused_replies".into(), json!(env.unused_replies()));
    trace.insert("unused_mqtt".into(), json!(env.unused_mqtt()));
    Ok(Value::Object(trace))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let method = name.strip_prefix("client.")?;
    Some(client_call(method, args))
}
