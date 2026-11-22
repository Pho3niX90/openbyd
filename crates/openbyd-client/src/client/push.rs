//! The MQTT side of the client: waits, event dispatch, command
//! acknowledgements, and recovery by re-login (pyBYD's `_mqtt_wait`,
//! `_on_mqtt_event`, `_schedule_mqtt_reauth`, `_on_mqtt_connack_refused`
//! and the pending-command registry).

use openbyd_protocol::coerce::ValidationError;
use openbyd_protocol::error::Error;
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::charging::ChargingStatus;
use openbyd_protocol::models::control::{
    CommandAckDiagnostics, CommandAckEvent, CommandLifecycleEvent, CommandLifecycleStatus,
    RemoteControlResult,
};
use openbyd_protocol::models::energy::EnergyConsumption;
use openbyd_protocol::models::hvac::HvacStatus;
use openbyd_protocol::models::realtime::VehicleRealtimeData;
use openbyd_protocol::models::vehicle::EnergyType;
use openbyd_protocol::mqtt::{
    MqttEvent, MqttEventType, REALTIME_STATUS_PUSH_DTOS, VehicleStatusPushDto,
    flatten_vehicle_status_push,
};
use openbyd_protocol::pyjson::py_str;
use serde_json::{Map, Value, json};

use super::{BydClient, PendingCommand, Waiter, guard};
use crate::api::control::python_int;
use crate::env::{ClientEnv, Env};

/// Seconds between two decrypt-triggered re-logins.
pub const MQTT_REAUTH_COOLDOWN_S: f64 = 60.0;
/// Decrypt-triggered re-logins until a payload decrypts again: a fresh
/// login derives a fresh key, so failures that survive one are payloads of
/// another session on the account (hass-byd-vehicle#195).
pub const MQTT_DECRYPT_REAUTH_MAX: i64 = 1;
/// Refused CONNACKs in a row before a re-login.
pub const MQTT_CONNACK_REFUSAL_THRESHOLD: u32 = 3;
/// First backoff between CONNACK-recovery re-logins, in seconds.
pub const MQTT_RECOVERY_BACKOFF_BASE_S: f64 = 30.0;
/// Longest backoff between CONNACK-recovery re-logins, in seconds.
pub const MQTT_RECOVERY_BACKOFF_MAX_S: f64 = 1800.0;
/// Backoff growth per CONNACK-recovery attempt.
pub const MQTT_RECOVERY_BACKOFF_FACTOR: f64 = 2.0;
/// CONNACK-recovery re-logins before MQTT is stopped until the next login.
pub const MQTT_RECOVERY_MAX_ATTEMPTS: i64 = 6;

/// The identity of a JSON value as a Python dict or set key: numbers and
/// booleans that compare equal share a key, a list or an object has none
/// (Python raises `TypeError: unhashable type`).
pub(crate) fn hash_key(value: &Value) -> Option<String> {
    Some(match value {
        Value::Null => "none".to_owned(),
        Value::Bool(flag) => format!("n:{}", i64::from(*flag)),
        Value::Number(number) => match (number.as_i64(), number.as_u64(), number.as_f64()) {
            (Some(int), _, _) => format!("n:{int}"),
            (None, Some(int), _) => format!("n:{int}"),
            (_, _, Some(float)) if float.fract() == 0.0 && float.abs() < 9.2e18 => {
                format!("n:{}", float as i64)
            }
            (_, _, float) => format!("f:{float:?}"),
        },
        Value::String(text) => format!("s:{text}"),
        Value::Array(_) | Value::Object(_) => return None,
    })
}

/// Removes a wait from the list when the waiting future ends or is dropped.
struct WaitGuard<'a, E: ClientEnv> {
    client: &'a BydClient<E>,
    id: u64,
}

impl<E: ClientEnv> Drop for WaitGuard<'_, E> {
    fn drop(&mut self) {
        let id = self.id;
        self.client.state().waiters.retain(|waiter| waiter.id != id);
    }
}

impl<E: ClientEnv> BydClient<E> {
    /// Waits for an MQTT event for `vin` (of `event_type`, with `serial`,
    /// when given) and returns its `respondData`; `None` when MQTT is not
    /// running, `timeout` (default `config.mqtt_timeout`) is not above 0,
    /// or nothing came in time. MQTT stopping meanwhile ends the wait the
    /// same way, so the caller falls back to polling (pyBYD's `_mqtt_wait`;
    /// before pyBYD #98 the wait was cancelled).
    pub(crate) async fn mqtt_wait(
        &self,
        vin: &str,
        event_type: Option<&str>,
        serial: Option<&Value>,
        timeout: Option<f64>,
    ) -> Result<Option<Map<String, Value>>, Error> {
        let timeout = timeout.unwrap_or(self.inner.config.mqtt_timeout);
        if timeout <= 0.0 {
            return Ok(None);
        }
        let (slot, receiver) = tokio::sync::oneshot::channel();
        // Checked and registered under one lock, which `stop_mqtt` drains
        // under: a stop either comes first (no wait at all) or ends this one.
        let id = {
            let mut state = self.state();
            if !(state.mqtt_started && self.inner.env.mqtt_running()) {
                return Ok(None);
            }
            state.next_waiter += 1;
            let id = state.next_waiter;
            state.waiters.push(Waiter {
                id,
                vin: vin.to_owned(),
                event_type: event_type.map(str::to_owned),
                serial: serial.cloned(),
                slot,
            });
            id
        };
        let _guard = WaitGuard { client: self, id };
        match self.inner.env.wait_for(Box::pin(receiver), timeout).await {
            None => Ok(None),
            Some(Ok(Some(data))) => Ok(Some(data)),
            Some(Ok(None) | Err(_)) => Ok(None),
        }
    }

    /// Handles a decrypted MQTT event (pyBYD's `_on_mqtt_event`): the
    /// generic callback, `vehicleInfo` and `vehicleStatusPush` parsing, the
    /// car layer, the waits, and the acknowledgement of remote commands.
    pub(crate) fn on_mqtt_event(&self, event: &MqttEvent) {
        self.state().decrypt_reauths = 0;
        let data = event.payload.get("data");
        let respond = match data {
            Some(Value::Object(data)) => data.get("respondData"),
            _ => None,
        };
        let respond_raw = match (data, respond) {
            (Some(Value::Object(_)), Some(Value::Object(respond))) => respond.clone(),
            (Some(Value::Object(_)), _) => match data {
                // Error payloads (code 6051 "no GPS") carry no respondData.
                Some(Value::Object(data)) if data.contains_key("code") => data.clone(),
                _ => return,
            },
            // No `data` object: the whole payload is the respondData.
            _ => event.payload.clone(),
        };
        let mut respond_data = respond_raw;
        let uuid = match data {
            Some(Value::Object(data)) => match data.get("uuid") {
                Some(Value::String(uuid)) if !uuid.is_empty() => Some(uuid.clone()),
                _ => None,
            },
            _ => None,
        };
        if let Some(uuid) = &uuid {
            respond_data
                .entry("requestSerial")
                .or_insert_with(|| json!(uuid));
        }
        let vin = event.vin.as_deref().filter(|vin| !vin.is_empty());
        let options = &self.inner.options;

        // 1) The generic callback.
        if let (Some(callback), Some(vin)) = (&options.on_mqtt_event, vin) {
            guard("on_mqtt_event", || {
                callback(&event.event, vin, &respond_data)
            });
        }

        // 2) Parsed pushes for the callbacks and the car layer.
        if let Some(vin) = vin {
            let envelope = match data {
                Some(Value::Object(data)) => Some(data),
                _ => None,
            };
            self.route_push(&event.event, vin, &respond_data, envelope);
        }

        // 3) The waits.
        let serial = match respond_data.get("requestSerial") {
            Some(Value::String(serial)) if !serial.is_empty() => Some(serial.clone()),
            _ => None,
        };
        let matched: Vec<Waiter> = {
            let mut state = self.state();
            let (matched, remaining) =
                std::mem::take(&mut state.waiters)
                    .into_iter()
                    .partition(|waiter| {
                        event.vin.as_deref() == Some(waiter.vin.as_str())
                            && waiter
                                .event_type
                                .as_ref()
                                .is_none_or(|kind| *kind == event.event)
                            && waiter.serial.as_ref().is_none_or(|wanted| {
                                serial
                                    .as_ref()
                                    .is_some_and(|serial| *wanted == json!(serial))
                            })
                    });
            state.waiters = remaining;
            matched
        };
        for waiter in matched {
            let _ = waiter.slot.send(Some(respond_data.clone()));
        }

        // 4) Remote-control acknowledgements (not the data polls' answers).
        let Some(vin) = vin else {
            return;
        };
        if event.event == MqttEventType::RemoteControlAck.as_str() {
            // The car has the command; its result follows as `remoteControl`.
            log::debug!(
                "MQTT remoteControlAck received serial={serial:?} res={:?} ackStatus={:?}",
                respond_data.get("res"),
                respond_data.get("ackStatus")
            );
        }
        if event.event != MqttEventType::RemoteControl.as_str() {
            return;
        }
        if let Some(serial) = &serial {
            let key = hash_key(&json!(serial));
            if self
                .state()
                .data_poll_serials
                .iter()
                .any(|known| hash_key(known) == key)
            {
                return;
            }
        }
        if self
            .acknowledge(vin, serial.as_deref(), uuid, &respond_data, event)
            .is_err()
        {
            log::debug!("on_command_ack callback failed");
        }
    }

    /// The validation context of a realtime push for `vin`: the energy type
    /// of the vehicle as last listed, or EV when it is not known.
    fn realtime_context(&self, vin: &str) -> Value {
        let energy_type = match self.state().vehicle(vin) {
            Some(vehicle) if vehicle.energy_type != EnergyType::Unknown => vehicle.energy_type,
            _ => EnergyType::Ev,
        };
        json!({"energy_type": {"EnergyType": energy_type as i64}})
    }

    /// Parses `vehicleInfo`, `vehicleStatusPush`, `smartCharge` and
    /// `energyConsumption` pushes for the callback, the [`super::CarRouter`]
    /// and the car kept for the VIN; a payload that does not parse is
    /// skipped. `envelope` is the event's `data` object, when it has one.
    fn route_push(
        &self,
        name: &str,
        vin: &str,
        respond_data: &Map<String, Value>,
        envelope: Option<&Map<String, Value>>,
    ) {
        let options = &self.inner.options;
        if name == MqttEventType::VehicleStatusPush.as_str() {
            self.route_status_push_event(vin, respond_data, envelope);
            return;
        }
        let raw = Value::Object(respond_data.clone());
        match name {
            "vehicleInfo" => {
                let context = self.realtime_context(vin);
                let Ok(parsed) = VehicleRealtimeData::parse_owned(raw, &context) else {
                    log::debug!("Failed to parse MQTT vehicleInfo");
                    return;
                };
                if let Some(callback) = &options.on_vehicle_info {
                    guard("on_vehicle_info", || callback(vin, &parsed));
                }
                if let Some(router) = &options.car_router {
                    router.handle_mqtt_realtime(vin, &parsed);
                }
                self.route_realtime(vin, &parsed);
            }
            "smartCharge" => match ChargingStatus::parse_owned(raw, &Value::Null) {
                Ok(parsed) => {
                    if let Some(router) = &options.car_router {
                        router.handle_mqtt_charging(vin, &parsed);
                    }
                    self.route_charging(vin, &parsed);
                }
                Err(_) => log::debug!("Failed to parse MQTT smartCharge"),
            },
            "energyConsumption" => match EnergyConsumption::parse_owned(raw, &Value::Null) {
                Ok(parsed) => {
                    if let Some(router) = &options.car_router {
                        router.handle_mqtt_energy(vin, &parsed);
                    }
                    self.route_energy(vin, &parsed);
                }
                Err(_) => log::debug!("Failed to parse MQTT energyConsumption"),
            },
            _ => {}
        }
    }

    /// A `vehicleStatusPush`: only the sections of the car's state that
    /// changed, each nested under its own key.
    ///
    /// The sections are looked for in `respond_data` and, when it has none,
    /// in the `data` envelope. Realtime data is built from every realtime
    /// section, HVAC only from `airStatusDto` and charging only from
    /// `chargeStatusDto`, so a door push neither touches those two nor leaks
    /// its keys into their `raw`. If any of the three does not validate the
    /// whole push is skipped. The partial models go to the car layer only:
    /// `on_vehicle_info` is for complete payloads and is not called.
    fn route_status_push_event(
        &self,
        vin: &str,
        respond_data: &Map<String, Value>,
        envelope: Option<&Map<String, Value>>,
    ) {
        let mut push_payload = respond_data;
        let mut flat_push = flatten_vehicle_status_push(push_payload, &REALTIME_STATUS_PUSH_DTOS);
        if flat_push.is_empty()
            && let Some(envelope) = envelope
        {
            push_payload = envelope;
            flat_push = flatten_vehicle_status_push(push_payload, &REALTIME_STATUS_PUSH_DTOS);
        }
        if flat_push.is_empty() {
            log::debug!("MQTT vehicleStatusPush had no recognised DTOs");
            return;
        }
        let air_push =
            flatten_vehicle_status_push(push_payload, &[VehicleStatusPushDto::AirStatus]);
        let charge_push =
            flatten_vehicle_status_push(push_payload, &[VehicleStatusPushDto::ChargeStatus]);
        let context = self.realtime_context(vin);
        let parsed = (|| -> Result<_, ValidationError> {
            let realtime = VehicleRealtimeData::parse_owned(Value::Object(flat_push), &context)?;
            let hvac = if air_push.is_empty() {
                None
            } else {
                Some(HvacStatus::parse_owned(
                    Value::Object(air_push),
                    &Value::Null,
                )?)
            };
            let charging = if charge_push.is_empty() {
                None
            } else {
                Some(ChargingStatus::parse_owned(
                    Value::Object(charge_push),
                    &Value::Null,
                )?)
            };
            Ok((realtime, hvac, charging))
        })();
        let Ok((realtime, hvac, charging)) = parsed else {
            log::debug!("Failed to parse MQTT vehicleStatusPush");
            return;
        };
        if let Some(router) = &self.inner.options.car_router {
            router.handle_mqtt_status_push(vin, &realtime, hvac.as_ref(), charging.as_ref());
        }
        self.route_status_push(vin, &realtime, hvac.as_ref(), charging.as_ref());
    }

    /// The `try` block of step 4 of `_on_mqtt_event`. `Err` where pyBYD's
    /// `except Exception` swallows a failure (a payload the models refuse).
    fn acknowledge(
        &self,
        vin: &str,
        serial: Option<&str>,
        raw_uuid: Option<String>,
        respond_data: &Map<String, Value>,
        event: &MqttEvent,
    ) -> Result<(), ()> {
        let mut timestamp = Value::Null;
        if let Some(value) = ["time", "timestamp"]
            .iter()
            .filter_map(|key| respond_data.get(*key))
            .find(|value| !value.is_null())
        {
            timestamp = match python_int(value) {
                // An int beyond i64 is a representation limit: the model refuses it.
                Ok(int) => json!(i64::try_from(int).map_err(|_| ())?),
                Err(_) => Value::Null,
            };
        }
        let result = ["result", "message", "msg"]
            .iter()
            .filter_map(|key| respond_data.get(*key))
            .find(|value| !value.is_null())
            .map(|value| match value {
                Value::String(text) => text.clone(),
                other => py_str(other),
            });
        let success =
            RemoteControlResult::parse(&Value::Object(respond_data.clone()), &Value::Null)
                .map_err(drop)?
                .success;
        let ack = CommandAckEvent::parse(
            &json!({
                "vin": vin,
                "requestSerial": serial,
                "raw_uuid": raw_uuid,
                "result": result,
                "success": success,
                "timestamp": timestamp,
                "raw": event.payload,
            }),
            &Value::Null,
        )
        .map_err(drop)?;

        self.expire_pending_commands();
        match serial {
            None => {
                self.state().uncorrelated += 1;
                self.emit_command_lifecycle(
                    CommandLifecycleStatus::Uncorrelated,
                    vin,
                    &Value::Null,
                    None,
                    Some(&ack),
                    Some("missing_request_serial"),
                );
            }
            Some(serial) => {
                let serial_value = json!(serial);
                let pending = {
                    let mut state = self.state();
                    let key = hash_key(&serial_value);
                    let found = state
                        .pending
                        .iter()
                        .position(|pending| pending.vin == vin && hash_key(&pending.serial) == key);
                    let pending = found.map(|index| state.pending.remove(index));
                    match pending {
                        Some(_) => state.matched += 1,
                        None => state.uncorrelated += 1,
                    }
                    pending
                };
                match pending {
                    None => self.emit_command_lifecycle(
                        CommandLifecycleStatus::Uncorrelated,
                        vin,
                        &serial_value,
                        None,
                        Some(&ack),
                        Some("pending_not_found"),
                    ),
                    Some(pending) => self.emit_command_lifecycle(
                        CommandLifecycleStatus::Matched,
                        vin,
                        &serial_value,
                        Some(&pending.command),
                        Some(&ack),
                        None,
                    ),
                }
            }
        }
        if let Some(callback) = &self.inner.options.on_command_ack {
            guard("on_command_ack", || callback(&ack));
        }
        Ok(())
    }

    /// Tells the lifecycle callback, if any (pyBYD's `_emit_command_lifecycle`).
    fn emit_command_lifecycle(
        &self,
        status: CommandLifecycleStatus,
        vin: &str,
        request_serial: &Value,
        command: Option<&str>,
        ack: Option<&CommandAckEvent>,
        reason: Option<&str>,
    ) {
        let Some(callback) = &self.inner.options.on_command_lifecycle else {
            return;
        };
        let raw = json!({
            "status": status.as_str(),
            "vin": vin,
            "requestSerial": request_serial,
            "command": command,
            "timestamp": self.inner.env.now_ms(),
            "ack": ack.map(BydModel::dump),
            "reason": reason,
        });
        match CommandLifecycleEvent::parse(&raw, &Value::Null) {
            Ok(event) => guard("on_command_lifecycle", || callback(&event)),
            Err(_) => log::debug!("on_command_lifecycle callback failed"),
        }
    }

    /// Drops the pending commands older than the acknowledgement TTL, with
    /// an `expired` lifecycle event each (pyBYD's `_expire_pending_commands`).
    fn expire_pending_commands(&self) -> usize {
        let now = self.inner.env.monotonic();
        let ttl = self.ack_ttl();
        let expired: Vec<PendingCommand> = {
            let mut state = self.state();
            let (expired, kept) = std::mem::take(&mut state.pending)
                .into_iter()
                .partition(|pending| now - pending.created_at >= ttl);
            state.pending = kept;
            state.expired += i64::try_from(expired.len()).unwrap_or(i64::MAX);
            expired
        };
        for pending in &expired {
            self.emit_command_lifecycle(
                CommandLifecycleStatus::Expired,
                &pending.vin,
                &pending.serial,
                Some(&pending.command),
                None,
                Some("pending_ttl_exceeded"),
            );
        }
        expired.len()
    }

    fn ack_ttl(&self) -> f64 {
        let ttl = self.inner.options.command_ack_ttl_seconds;
        if ttl > 0.0 { ttl } else { 300.0 }
    }

    /// Records a sent command until its acknowledgement arrives (pyBYD's
    /// `_register_pending_command`). A serial that cannot be a dict key
    /// (a list or object) is not recorded, as pyBYD's `TypeError` is swallowed.
    pub(crate) fn register_pending_command(&self, vin: &str, serial: &Value, command: &str) {
        self.expire_pending_commands();
        let Some(key) = hash_key(serial) else {
            log::debug!("on_trigger_dispatched callback failed");
            return;
        };
        let created_at = self.inner.env.monotonic();
        {
            let mut state = self.state();
            let entry = PendingCommand {
                vin: vin.to_owned(),
                serial: serial.clone(),
                command: command.to_owned(),
                created_at,
            };
            match state.pending.iter_mut().find(|pending| {
                pending.vin == vin && hash_key(&pending.serial).as_deref() == Some(&key)
            }) {
                // A dict keeps the first key and its place, with the new value.
                Some(existing) => {
                    existing.command = entry.command;
                    existing.created_at = entry.created_at;
                }
                None => state.pending.push(entry),
            }
        }
        self.emit_command_lifecycle(
            CommandLifecycleStatus::Registered,
            vin,
            serial,
            Some(command),
            None,
            None,
        );
    }

    /// A snapshot of the command acknowledgement counters, after expiring
    /// stale pending commands (pyBYD's `get_command_ack_diagnostics`).
    pub fn get_command_ack_diagnostics(&self) -> CommandAckDiagnostics {
        self.expire_pending_commands();
        let state = self.state();
        let mut by_vin = Map::new();
        for pending in &state.pending {
            let count = by_vin
                .get(&pending.vin)
                .and_then(Value::as_i64)
                .unwrap_or(0);
            by_vin.insert(pending.vin.clone(), json!(count + 1));
        }
        let raw = json!({
            "pending": state.pending.len(),
            "matched": state.matched,
            "expired": state.expired,
            "uncorrelated": state.uncorrelated,
            "pending_by_vin": by_vin,
        });
        CommandAckDiagnostics::parse(&raw, &Value::Null).expect("the counters are integers")
    }

    // -----------------------------------------------------------------------
    // Recovery
    // -----------------------------------------------------------------------

    /// After a payload failed to decrypt: re-login in the background, at
    /// most once a minute and [`MQTT_DECRYPT_REAUTH_MAX`] times until a
    /// payload decrypts again (pyBYD's `_schedule_mqtt_reauth`).
    pub(crate) fn schedule_mqtt_reauth(&self) {
        let now = self.inner.env.monotonic();
        {
            let mut state = self.state();
            if now - state.reauth_at < MQTT_REAUTH_COOLDOWN_S {
                return;
            }
            if state.decrypt_reauths >= MQTT_DECRYPT_REAUTH_MAX {
                if state.decrypt_reauths == MQTT_DECRYPT_REAUTH_MAX {
                    log::info!(
                        "MQTT decrypt still failing after re-authentication - payloads are likely \
                         for another session on this account, not re-authenticating again"
                    );
                    state.decrypt_reauths += 1;
                }
                return;
            }
            state.reauth_at = now;
            state.decrypt_reauths += 1;
        }
        log::info!("MQTT decrypt failed - scheduling re-authentication");
        let client = self.clone();
        self.inner.env.spawn(Box::pin(async move {
            match client.relogin().await {
                Ok(_) => log::info!("MQTT re-auth succeeded - MQTT restarted with new key"),
                Err(error) => log::debug!("MQTT re-auth recovery failed: {error}"),
            }
        }));
    }

    /// A successful CONNACK: resets the recovery counters and tells the
    /// callback (pyBYD's `_on_mqtt_connected`).
    pub(crate) fn on_mqtt_connected(&self) {
        {
            let mut state = self.state();
            if state.recovery_attempts != 0 {
                log::info!("MQTT connected - recovery state reset");
            }
            state.recovery_attempts = 0;
            state.recovery_last_at = 0.0;
        }
        if let Some(callback) = &self.inner.options.on_mqtt_connect {
            guard("on_mqtt_connect", || callback());
        }
    }

    /// A refused CONNACK: after [`MQTT_CONNACK_REFUSAL_THRESHOLD`] in a row,
    /// re-login with exponential backoff and jitter, and stop MQTT after
    /// [`MQTT_RECOVERY_MAX_ATTEMPTS`] (pyBYD's `_on_mqtt_connack_refused`).
    /// A dropped MQTT connection: tells the callback (openbyd's addition).
    pub(crate) fn on_mqtt_disconnected(&self, reason: &str) {
        if let Some(callback) = &self.inner.options.on_mqtt_disconnect {
            guard("on_mqtt_disconnect", || callback(reason));
        }
    }

    pub(crate) fn on_mqtt_connack_refused(&self, consecutive: u32) {
        if let Some(callback) = &self.inner.options.on_mqtt_connack_refused {
            guard("on_mqtt_connack_refused", || callback(consecutive));
        }
        if consecutive < MQTT_CONNACK_REFUSAL_THRESHOLD {
            return;
        }
        let attempts = self.state().recovery_attempts;
        if attempts >= MQTT_RECOVERY_MAX_ATTEMPTS {
            log::warn!(
                "MQTT recovery ceiling reached ({attempts} attempts) - stopping push channel until the next login()"
            );
            self.stop_mqtt();
            return;
        }
        let now = self.inner.env.monotonic();
        let exponent = i32::try_from(attempts).unwrap_or(i32::MAX);
        let delay = (MQTT_RECOVERY_BACKOFF_BASE_S * MQTT_RECOVERY_BACKOFF_FACTOR.powi(exponent))
            .min(MQTT_RECOVERY_BACKOFF_MAX_S)
            * self.inner.env.uniform(0.8, 1.2);
        {
            let mut state = self.state();
            // 0.0 means no attempt yet.
            if state.recovery_last_at != 0.0 && now - state.recovery_last_at < delay {
                return;
            }
            state.recovery_last_at = now;
            state.recovery_attempts += 1;
            log::info!(
                "MQTT CONNACK refused x{consecutive} - scheduling re-login (attempt {}/{MQTT_RECOVERY_MAX_ATTEMPTS})",
                state.recovery_attempts
            );
        }
        let client = self.clone();
        self.inner.env.spawn(Box::pin(async move {
            client.stop_mqtt();
            match client.relogin().await {
                Ok(_) => log::info!("MQTT recovery: re-login done, push channel restarted"),
                Err(error) => log::warn!("MQTT recovery re-login failed: {error}"),
            }
        }));
    }
}
