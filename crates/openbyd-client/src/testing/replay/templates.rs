//! Reply templates from `parity/corpus/` and how a car's state fills them.
//!
//! Each reply starts from a real (or realistic) payload of the corpus, so
//! that clients parse what BYD sends, and the scenario's values are written
//! into the raw fields pyBYD's models read them from. A missing corpus, file
//! or entry falls back to a small built-in payload: loading never fails.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::scenario::{CarState, Energy, PushEvent, ScenarioCar};

/// Radius of the circle a car drives on around its anchor, km.
pub const CIRCLE_KM: f64 = 5.0;
const KM_PER_DEGREE: f64 = 111.32;

/// The made-up position of car number `index` (its place in the file) with
/// `odometer_km` on the clock: `(latitude, longitude, heading in degrees)`.
///
/// The anchor is `52.0 + (index % 100) * 0.01`, `4.0 + (index / 100) * 0.01`;
/// the car sits on a circle of [`CIRCLE_KM`] around it at the angle
/// `odometer_km / 5` radians, so a parked car never moves and a driving car
/// covers its odometer distance.
pub fn position(index: usize, odometer_km: Option<f64>) -> (f64, f64, f64) {
    let anchor_lat = 52.0 + (index % 100) as f64 * 0.01;
    let anchor_lon = 4.0 + (index / 100) as f64 * 0.01;
    let angle = odometer_km.unwrap_or(0.0) / CIRCLE_KM;
    let lat = anchor_lat + CIRCLE_KM * angle.sin() / KM_PER_DEGREE;
    let lon =
        anchor_lon + CIRCLE_KM * angle.cos() / (KM_PER_DEGREE * anchor_lat.to_radians().cos());
    // Moving with growing angle: north by cos, east by -sin.
    let heading = (-angle.sin())
        .atan2(angle.cos())
        .to_degrees()
        .rem_euclid(360.0);
    let round = |value: f64| (value * 1e6).round() / 1e6;
    (round(lat), round(lon), round(heading))
}

/// The display name BYD's vehicle list gives a catalogue key: `sealion_7`
/// becomes `SEALION 7`.
pub fn model_name(model: &str) -> String {
    model.replace(['_', '-'], " ").to_uppercase()
}

/// A moment of the replay: scenario second `t` and the epoch it is counted
/// from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    /// Scenario seconds.
    pub t: u32,
    /// The instant `t = 0` stands for, in milliseconds since the Unix epoch.
    pub epoch_ms: i64,
}

impl Moment {
    /// The Unix time in seconds BYD writes into replies for this moment.
    pub fn unix(self) -> i64 {
        self.epoch_ms.div_euclid(1000) + i64::from(self.t)
    }
}

/// One corpus entry: its payload and the lowercase words of its source,
/// note and model-name fields, for matching.
#[derive(Debug, Clone)]
struct Entry {
    words: Vec<String>,
    note: String,
    payload: Value,
}

impl Entry {
    fn object(&self) -> Option<&Map<String, Value>> {
        self.payload.as_object()
    }

    fn mentions(&self, word: &str) -> bool {
        self.words.iter().any(|candidate| candidate == word)
    }

    fn hybrid(&self) -> bool {
        ["phev", "hybrid", "dm", "dmi"]
            .iter()
            .any(|word| self.mentions(word))
            || self
                .object()
                .and_then(|payload| payload.get("oilPercent"))
                .and_then(Value::as_f64)
                .is_some_and(|fuel| fuel > 0.0)
    }

    /// Whether the model's words appear in a row in the entry's words.
    fn names_model(&self, model: &str) -> bool {
        let tokens: Vec<&str> = model.split(['_', '-']).filter(|t| !t.is_empty()).collect();
        if model == "unknown" || tokens.is_empty() {
            return false;
        }
        self.words.windows(tokens.len()).any(|window| {
            window
                .iter()
                .zip(&tokens)
                .all(|(word, token)| word == token)
        })
    }
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn read_corpus(dir: Option<&Path>, file: &str) -> Vec<Entry> {
    let Some(text) = dir.and_then(|dir| std::fs::read_to_string(dir.join(file)).ok()) else {
        return Vec::new();
    };
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| {
            let Value::Object(mut entry) = entry else {
                return None;
            };
            let payload = entry.remove("payload")?;
            let text_of = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or("");
            let note = text_of("note").to_owned();
            let mut text = format!("{} {}", text_of("source"), note);
            if let Some(object) = payload.as_object() {
                for key in ["modelName", "autoModelName", "outModelType"] {
                    if let Some(name) = object.get(key).and_then(Value::as_str) {
                        text.push(' ');
                        text.push_str(name);
                    }
                }
            }
            Some(Entry {
                words: words(&text),
                note: note.to_lowercase(),
                payload,
            })
        })
        .collect()
}

/// The first object payload whose note contains `note`, else the first
/// object payload, else `fallback`.
fn pick(entries: &[Entry], notes: &[&str], fallback: Value) -> Map<String, Value> {
    let objects = || {
        entries
            .iter()
            .filter_map(|entry| Some((entry, entry.object()?)))
    };
    for note in notes {
        if let Some((_, payload)) = objects().find(|(entry, _)| entry.note.contains(note)) {
            return payload.clone();
        }
    }
    objects()
        .next()
        .map(|(_, payload)| payload.clone())
        .or_else(|| fallback.as_object().cloned())
        .unwrap_or_default()
}

fn time_of(payload: &Map<String, Value>) -> Option<f64> {
    payload
        .get("time")
        .or_else(|| payload.get("timestamp"))
        .and_then(Value::as_f64)
}

fn is_awake(entry: &Entry) -> bool {
    let Some(payload) = entry.object() else {
        return false;
    };
    payload.get("onlineState").and_then(Value::as_i64) == Some(1)
        && time_of(payload) != Some(0.0)
        && !entry.note.contains("asleep")
        && !entry.note.contains("offline")
}

fn is_asleep(entry: &Entry) -> bool {
    let Some(payload) = entry.object() else {
        return false;
    };
    payload.contains_key("onlineState")
        && (payload.get("onlineState").and_then(Value::as_i64) != Some(1)
            || time_of(payload) == Some(0.0)
            || entry.note.contains("asleep")
            || entry.note.contains("offline"))
}

/// The realtime templates a car uses, as indices into the realtime entries
/// (`None`: the built-in one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CarTemplates {
    awake: Option<usize>,
    asleep: Option<usize>,
}

/// Every reply template.
#[derive(Debug, Clone)]
pub struct Templates {
    realtime: Vec<Entry>,
    vehicle: Map<String, Value>,
    charging: Map<String, Value>,
    hvac: Map<String, Value>,
    hvac_asleep: Map<String, Value>,
    gps: Map<String, Value>,
    energy_ev: Map<String, Value>,
    energy_hybrid: Map<String, Value>,
    latest_config: Map<String, Value>,
    booking: Value,
    push_state: Value,
    ac_limit: Value,
    messages: Value,
    builtin_awake: Map<String, Value>,
    builtin_asleep: Map<String, Value>,
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

impl Templates {
    /// Loads the templates from the corpus directory `dir` (the repository's
    /// `parity/corpus`), falling back to built-in payloads for anything
    /// missing. `None` uses only the built-in payloads.
    pub fn load(dir: Option<&Path>) -> Self {
        let file = |name: &str| read_corpus(dir, name);
        let energy = file("EnergyConsumption.json");
        let hvac = file("HvacStatus.json");
        let vehicle = file("Vehicle.json");
        let mut vehicle_base = pick(&vehicle, &["owned by the account"], json!({}));
        vehicle_base.shift_remove("empowerId");
        if let Some(learn) = vehicle
            .iter()
            .filter_map(Entry::object)
            .find_map(|payload| payload.get("vehicleFunLearnInfo"))
        {
            vehicle_base.insert("vehicleFunLearnInfo".into(), learn.clone());
        }
        let any = |name: &str, note: &str, fallback: Value| -> Value {
            let entries = file(name);
            entries
                .iter()
                .find(|entry| entry.note.contains(note))
                .or(entries.first())
                .map_or(fallback, |entry| entry.payload.clone())
        };
        Self {
            realtime: file("VehicleRealtimeData.json"),
            vehicle: vehicle_base,
            charging: pick(&file("ChargingStatus.json"), &[], json!({})),
            hvac: pick(
                &hvac,
                &["running climate"],
                json!({"tempInCar": 21.0, "tempOutCar": 15.0}),
            ),
            hvac_asleep: pick(
                &hvac,
                &["asleep"],
                json!({"tempInCar": 0, "tempOutCar": 0, "pm": 0, "acSwitch": 0, "status": 0}),
            ),
            gps: pick(&file("GpsInfo.json"), &["valid fix"], json!({})),
            energy_ev: pick(&energy, &["energy_type 0"], Self::builtin_energy()),
            energy_hybrid: pick(
                &energy,
                &["energy_type 2", "hybrid"],
                Self::builtin_energy(),
            ),
            latest_config: pick(
                &file("VehicleLatestConfig.json"),
                &["44 functionnos"],
                json!({"cfFixedList": [{"functionNo": "1001"}]}),
            ),
            booking: any("BookingList.json", "captured live", json!({})),
            // The corpus holds the model's form; the wire carries the bare list.
            push_state: {
                let state = any("PushNotificationState.json", "on", json!([]));
                match state.get("switches") {
                    Some(switches) if switches.is_array() => switches.clone(),
                    _ => state,
                }
            },
            ac_limit: any("AcCurrentLimit.json", "max selected", json!({})),
            messages: any("VehicleMsgListResponse.json", "", json!([])),
            builtin_awake: object(json!({
                "onlineState": 1, "connectState": -1, "vehicleState": 2, "elecPercent": 0,
                "powerBattery": 0, "enduranceMileage": 0, "totalMileage": 0, "speed": 0,
                "powerGear": 1, "chargeState": 15, "chargingState": -1, "tempInCar": 21,
                "oilPercent": -1, "oilEndurance": -1, "rate": -999, "gl": 0.0, "time": 0,
            })),
            builtin_asleep: object(json!({
                "onlineState": 0, "connectState": -1, "vehicleState": 0, "elecPercent": 0,
                "powerBattery": 0, "enduranceMileage": 0, "totalMileage": 0, "speed": 0,
                "powerGear": 0, "chargeState": -1, "chargingState": -1, "time": 0,
            })),
        }
    }

    fn builtin_energy() -> Value {
        json!({
            "cumulativeEnergyConsumption": {"totalMileage": "--", "avgEvConsumption": "--"},
            "nearestEnergyConsumption": {"avgEvConsumption": "--"},
        })
    }

    /// Picks the realtime templates for a car of `model` and `energy`: the
    /// entries that name the model and match the powertrain score highest,
    /// then the one with the most fields. Never fails: with no candidate
    /// the built-in payload is used.
    pub fn choose(&self, model: &str, energy: Energy) -> CarTemplates {
        let best = |accept: fn(&Entry) -> bool| {
            self.realtime
                .iter()
                .enumerate()
                .filter(|(_, entry)| accept(entry))
                .max_by_key(|(index, entry)| {
                    let mut score = 0;
                    if entry.names_model(model) {
                        score += 4;
                    }
                    if entry.hybrid() == (energy == Energy::Hybrid) {
                        score += 2;
                    }
                    let fields = entry.object().map_or(0, Map::len);
                    // The earliest entry wins a tie.
                    (score, fields, std::cmp::Reverse(*index))
                })
                .map(|(index, _)| index)
        };
        CarTemplates {
            awake: best(is_awake),
            asleep: best(is_asleep),
        }
    }

    /// The note of a car's awake realtime template, for diagnostics; `None`
    /// for the built-in one.
    pub fn awake_note(&self, choice: CarTemplates) -> Option<&str> {
        choice.awake.map(|index| self.realtime[index].note.as_str())
    }

    fn realtime_base(
        &self,
        index: Option<usize>,
        builtin: &Map<String, Value>,
    ) -> Map<String, Value> {
        index
            .and_then(|index| self.realtime[index].object())
            .unwrap_or(builtin)
            .clone()
    }

    /// The realtime reply (`vehicleRealTimeResult`, a `vehicleInfo` push):
    /// for an online car its state written into the awake template, for an
    /// offline one the placeholder-zero reply BYD sends (`onlineState` 0).
    pub fn realtime(
        &self,
        choice: CarTemplates,
        car: &ScenarioCar,
        state: &CarState,
        at: Moment,
        serial: Option<&str>,
    ) -> Value {
        let mut p;
        if state.online {
            p = self.realtime_base(choice.awake, &self.builtin_awake);
            p.shift_remove("timestamp");
            fill_realtime(&mut p, car, state, at);
        } else {
            p = self.realtime_base(choice.asleep, &self.builtin_asleep);
            p.shift_remove("timestamp");
            p.insert("onlineState".into(), json!(0));
            p.insert("time".into(), json!(0));
        }
        match serial {
            Some(serial) => p.insert("requestSerial".into(), json!(serial)),
            None => p.shift_remove("requestSerial"),
        };
        Value::Object(p)
    }

    /// The `/control/smartCharge/homePage` reply: the live charging fields
    /// (pyBYD's `ChargingStatus`) and a fixed schedule
    /// (`SmartChargingSchedule`). BYD answers it from the cloud, so it holds
    /// the last known values while the car sleeps.
    pub fn charging(&self, car: &ScenarioCar, state: &CarState, at: Moment) -> Value {
        let mut p = self.charging.clone();
        p.insert("vin".into(), json!(car.vin));
        match state.soc {
            Some(soc) => p.insert("elecPercent".into(), json!(soc.round() as i64)),
            None => p.shift_remove("elecPercent"),
        };
        p.insert("chargingState".into(), json!(i64::from(state.charging)));
        // "Connected" is read from connectState alone, so a plugged car whose
        // connection state was not recorded still has to say it is connected.
        match (state.connect_state, state.plugged) {
            (Some(connect), _) => p.insert("connectState".into(), json!(connect)),
            (None, true) => p.insert("connectState".into(), json!(1)),
            (None, false) => p.shift_remove("connectState"),
        };
        p.insert("waitStatus".into(), json!(0));
        let (hours, minutes) = split_minutes(state.minutes_to_full);
        p.insert("fullHour".into(), json!(hours));
        p.insert("fullMinute".into(), json!(minutes));
        set_charge_power(&mut p, state.charge_power_kw);
        p.insert("time".into(), json!(at.unix()));
        p.insert("vehicleTimeZone".into(), json!("UTC"));
        p.insert("type".into(), json!("1"));
        p.entry("smartChargeDto").or_insert_with(|| {
            json!({"startChargeTime": "22:00", "endChargeTime": "full", "chargeWay": "e", "status": "0"})
        });
        p.entry("smartJourneyDto").or_insert_with(|| {
            json!({"useVehicleTime": "07:30", "startDiscountPrice": "23:00", "endDiscountPrice": "06:00", "chargeWay": "e", "status": "0"})
        });
        Value::Object(p)
    }

    /// The `/control/getStatusNow` reply: `status` 1 for climate on, 2 off;
    /// the asleep placeholder while the car is offline.
    pub fn hvac(&self, state: &CarState) -> Value {
        if !state.online {
            return Value::Object(self.hvac_asleep.clone());
        }
        let mut p = self.hvac.clone();
        let (status, switch) = match state.hvac_on {
            Some(true) => (1, 2),
            Some(false) => (2, 1),
            None => (-1, -1),
        };
        p.insert("status".into(), json!(status));
        p.insert("acSwitch".into(), json!(switch));
        for (key, value) in [
            ("tempInCar", 21.0),
            ("tempOutCar", 15.0),
            ("mainSettingTempNew", 21.0),
        ] {
            p.entry(key).or_insert(json!(value));
        }
        Value::Object(p)
    }

    /// The GPS reply (`getGpsInfoResult`): the made-up position, or BYD's
    /// "location sharing off" (latitude and longitude 0) when `gps` is false.
    pub fn gps(&self, index: usize, state: &CarState, at: Moment, serial: Option<&str>) -> Value {
        let mut p = self.gps.clone();
        let speed = if state.online {
            state.speed_kmh.unwrap_or(0.0)
        } else {
            0.0
        };
        let (lat, lon, heading) = if state.gps {
            position(index, state.odometer_km)
        } else {
            (0.0, 0.0, 0.0)
        };
        p.insert("latitude".into(), json!(lat));
        p.insert("longitude".into(), json!(lon));
        p.insert("speed".into(), json!(if state.gps { speed } else { 0.0 }));
        p.insert("direction".into(), json!(heading));
        p.insert("gpsTimeStamp".into(), json!(at.unix()));
        if let Some(serial) = serial {
            p.insert("requestSerial".into(), json!(serial));
        }
        Value::Object(p)
    }

    /// The `/vehicleInfo/vehicle/getEnergyConsumption` reply, with the
    /// odometer as its cumulative mileage.
    pub fn energy(&self, car: &ScenarioCar, state: &CarState, at: Moment) -> Value {
        let mut p = match car.energy {
            Energy::Ev => self.energy_ev.clone(),
            Energy::Hybrid => self.energy_hybrid.clone(),
        };
        p.insert("time".into(), json!(at.unix()));
        let mileage = match state.odometer_km {
            Some(km) if km > 0.0 => format!("{km}"),
            _ => "--".to_owned(),
        };
        if let Value::Object(cumulative) = p
            .entry("cumulativeEnergyConsumption")
            .or_insert_with(|| json!({}))
        {
            cumulative.insert("totalMileage".into(), json!(mileage));
        }
        Value::Object(p)
    }

    /// A vehicle-list entry for `car`, with its VIN, model name and energy
    /// type (`"0"` EV, `"2"` hybrid) so the client resolves the powertrain.
    pub fn vehicle(&self, car: &ScenarioCar) -> Value {
        let mut p = self.vehicle.clone();
        p.insert("vin".into(), json!(car.vin));
        p.insert("modelName".into(), json!(model_name(&car.model)));
        let energy_type = match car.energy {
            Energy::Ev => "0",
            Energy::Hybrid => "2",
        };
        p.insert("energyType".into(), json!(energy_type));
        Value::Object(p)
    }

    /// The capability configuration of one car (`getLatestConfig` answers
    /// one per requested VIN).
    pub fn latest_config(&self) -> Value {
        Value::Object(self.latest_config.clone())
    }

    /// The other fixed replies, by endpoint. Write the car's VIN in with
    /// [`with_vin`].
    pub fn fixed(&self) -> HashMap<&'static str, Value> {
        HashMap::from([
            ("/control/getBookingList", self.booking.clone()),
            (
                "/vehicle/vehicleswitch/getPushSwitchState",
                self.push_state.clone(),
            ),
            (
                "/control/smartCharge/getAcCurrentLimit",
                self.ac_limit.clone(),
            ),
            (
                "/app/messageCenter/getUnReadMessageList",
                self.messages.clone(),
            ),
        ])
    }

    /// The body of a push of `event` for `car` (number `index` in the file)
    /// in `state`, as BYD publishes it before encryption.
    pub fn push(
        &self,
        event: PushEvent,
        choice: CarTemplates,
        car: &ScenarioCar,
        state: &CarState,
        at: Moment,
    ) -> Value {
        let data = match event {
            PushEvent::VehicleInfo => {
                json!({"respondData": self.realtime(choice, car, state, at, None)})
            }
            PushEvent::SmartCharge => json!({"respondData": self.charging(car, state, at)}),
            PushEvent::EnergyConsumption => json!({"respondData": self.energy(car, state, at)}),
            PushEvent::RemoteControl => {
                let uuid =
                    openbyd_protocol::hashing::md5_hex(&format!("{}:{}:push", car.vin, at.t));
                json!({"uuid": uuid.to_uppercase(), "respondData": {"res": 2}})
            }
        };
        json!({"event": event.name(), "vin": car.vin, "data": data})
    }
}

/// `template` with `vin` written into its `vin` field, when it is an object
/// that has one.
pub fn with_vin(template: &Value, vin: &str) -> Value {
    let mut reply = template.clone();
    if let Some(field) = reply
        .as_object_mut()
        .and_then(|object| object.get_mut("vin"))
    {
        *field = json!(vin);
    }
    reply
}

fn split_minutes(minutes: Option<i64>) -> (i64, i64) {
    match minutes {
        Some(minutes) if minutes >= 0 => (minutes / 60, minutes % 60),
        _ => (-1, -1),
    }
}

fn set_charge_power(p: &mut Map<String, Value>, power: Option<f64>) {
    match power {
        Some(kw) => {
            p.insert("chargePower".into(), json!(kw));
            p.insert("chargePowerUnit".into(), json!("kW"));
        }
        None => {
            p.shift_remove("chargePower");
            p.shift_remove("chargePowerUnit");
        }
    }
}

/// Writes an online car's state into a realtime template: each scenario key
/// into the raw fields pyBYD's `VehicleRealtimeData` reads it from.
fn fill_realtime(p: &mut Map<String, Value>, car: &ScenarioCar, state: &CarState, at: Moment) {
    let mut set = |key: &str, value: Value| {
        p.insert(key.to_owned(), value);
    };
    set("onlineState", json!(1));
    set("time", json!(at.unix()));
    // State of charge, and a range that follows it.
    set("elecPercent", json!(state.soc.unwrap_or(-1.0)));
    let range = state.soc.map_or(-1.0, |soc| {
        let km_per_percent = match car.energy {
            Energy::Ev => 4.5,
            Energy::Hybrid => 0.75,
        };
        (soc * km_per_percent).round()
    });
    for key in ["enduranceMileage", "evEndurance", "enduranceMileageV2"] {
        set(key, json!(range));
    }
    if state.soc.is_none() {
        set("powerBattery", Value::Null);
    }
    let odometer = state.odometer_km.map_or(Value::Null, |km| json!(km));
    set("totalMileage", odometer.clone());
    set("totalMileageV2", odometer);
    set("totalMileageV2Unit", json!("km"));
    set(
        "speed",
        state.speed_kmh.map_or(Value::Null, |speed| json!(speed)),
    );
    let (gear, vehicle_state) = match state.vehicle_on {
        Some(true) => (3, 0),
        Some(false) => (1, 2),
        None => (-1, -1),
    };
    set("powerGear", json!(gear));
    set("vehicleState", json!(vehicle_state));
    // Charging: chargeState 1 while charging, 15 (idle) otherwise.
    set("chargeState", json!(if state.charging { 1 } else { 15 }));
    set("chargingState", json!(if state.charging { 1 } else { -1 }));
    set("connectState", json!(state.connect_state.unwrap_or(-1)));
    let (rate, pack) = match state.charge_power_kw {
        // BYD reports charging as a negative rate; no charge is a plain 0.
        Some(0.0) => (json!(0), json!(0.0)),
        Some(kw) => (json!(-kw), json!(kw * 1000.0)),
        None => (json!(-999), Value::Null),
    };
    set("rate", rate);
    set("gl", pack);
    let (hours, minutes) = split_minutes(state.minutes_to_full);
    for (key, value) in [
        ("remainingHours", hours),
        ("remainingMinutes", minutes),
        ("fullHour", hours),
        ("fullMinute", minutes),
    ] {
        set(key, json!(value));
    }
    let (fuel, fuel_range) = match (car.energy, state.fuel_percent) {
        (Energy::Hybrid, Some(fuel)) => (json!(fuel), json!((fuel * 8.0).round())),
        _ => (json!(-1), json!(-1)),
    };
    set("oilPercent", fuel);
    set("oilEndurance", fuel_range);
    set_charge_power(p, state.charge_power_kw);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parity/corpus")
    }

    fn car(model: &str, energy: Energy) -> ScenarioCar {
        ScenarioCar {
            vin: "TESTVIN0000000001".into(),
            account: 0,
            model: model.into(),
            energy,
        }
    }

    const AT: Moment = Moment {
        t: 3600,
        epoch_ms: 1_767_571_200_000,
    };

    fn driving() -> CarState {
        CarState {
            online: true,
            soc: Some(71.0),
            odometer_km: Some(20431.4),
            speed_kmh: Some(62.0),
            vehicle_on: Some(true),
            plugged: false,
            charging: false,
            connect_state: Some(0),
            charge_power_kw: None,
            minutes_to_full: None,
            hvac_on: Some(false),
            fuel_percent: None,
            gps: true,
        }
    }

    fn charging() -> CarState {
        CarState {
            speed_kmh: Some(0.0),
            vehicle_on: Some(false),
            plugged: true,
            charging: true,
            connect_state: Some(1),
            charge_power_kw: Some(6.3),
            minutes_to_full: Some(125),
            hvac_on: Some(true),
            ..driving()
        }
    }

    #[test]
    fn positions_follow_the_odometer() {
        let (lat, lon, heading) = position(0, Some(0.0));
        assert_eq!((lat, heading), (52.0, 0.0));
        assert!((lon - (4.0 + 5.0 / (111.32 * 52f64.to_radians().cos()))).abs() < 1e-6);
        assert_eq!(position(0, None), position(0, Some(0.0)));
        // Anchors by index.
        let (lat, lon, _) = position(234, Some(0.0));
        assert!((lat - 52.34).abs() < 1e-9 && lon > 4.02, "{lat} {lon}");
        // A quarter turn is 5 * pi / 2 km: due north of the anchor, heading west.
        let (lat, lon, heading) = position(0, Some(5.0 * std::f64::consts::FRAC_PI_2));
        assert!((lat - (52.0 + 5.0 / 111.32)).abs() < 1e-6);
        assert!((lon - 4.0).abs() < 1e-6);
        assert!((heading - 270.0).abs() < 1e-6);
        // Distance travelled along the circle equals the odometer delta.
        let (a, b) = (position(7, Some(1000.0)), position(7, Some(1000.1)));
        let dy = (b.0 - a.0) * 111.32;
        let dx = (b.1 - a.1) * 111.32 * (52.07f64).to_radians().cos();
        assert!(((dx * dx + dy * dy).sqrt() - 0.1).abs() < 0.002);
    }

    #[test]
    fn model_names() {
        assert_eq!(model_name("sealion_7"), "SEALION 7");
        assert_eq!(model_name("seal-u"), "SEAL U");
        assert_eq!(model_name("unknown"), "UNKNOWN");
        assert_eq!(AT.unix(), 1_767_571_200 + 3600);
        assert_eq!(
            Moment {
                t: 1,
                epoch_ms: -1500
            }
            .unix(),
            -1
        );
    }

    #[test]
    fn templates_are_chosen_by_model_and_energy() {
        let templates = Templates::load(Some(&corpus()));
        // Whatever the corpus holds, every car gets an awake and an asleep template.
        for (model, energy) in [
            ("seal", Energy::Ev),
            ("unknown", Energy::Hybrid),
            ("x", Energy::Ev),
        ] {
            let choice = templates.choose(model, energy);
            assert!(choice.awake.is_some() && choice.asleep.is_some(), "{model}");
        }
        // Synthetic entries teach the matcher.
        let entry = |note: &str, payload: Value| Entry {
            words: words(note),
            note: note.to_lowercase(),
            payload,
        };
        let awake = json!({"onlineState": 1, "time": 5});
        let mut small = Templates::load(None);
        small.realtime = vec![
            entry("Seal BEV, awake", awake.clone()),
            entry("Seal 6 DM-i PHEV, awake", awake.clone()),
            entry(
                "Sealion 7, awake",
                json!({"onlineState": 1, "time": 5, "extra": 1}),
            ),
            entry("Sealion 7, asleep", json!({"onlineState": 1, "time": 0})),
            entry("not a realtime payload", json!([1])),
            entry("excerpt", json!({"elecPercent": 1})),
        ];
        let choose = |model: &str, energy| small.choose(model, energy);
        assert_eq!(choose("seal", Energy::Ev).awake, Some(0));
        assert_eq!(choose("seal-6", Energy::Hybrid).awake, Some(1));
        assert_eq!(choose("seal", Energy::Hybrid).awake, Some(1));
        assert_eq!(choose("sealion_7", Energy::Ev).awake, Some(2));
        assert_eq!(choose("unknown", Energy::Ev).awake, Some(2), "most fields");
        assert_eq!(choose("seal", Energy::Ev).asleep, Some(3));
        assert_eq!(
            small.awake_note(choose("seal", Energy::Ev)),
            Some("seal bev, awake")
        );
        // An empty corpus uses the built-in payloads.
        let empty = Templates::load(None);
        let choice = empty.choose("seal", Energy::Ev);
        assert_eq!(choice, CarTemplates::default());
        assert_eq!(empty.awake_note(choice), None);
        let reply = empty.realtime(choice, &car("seal", Energy::Ev), &driving(), AT, Some("S1"));
        assert_eq!(reply["elecPercent"], json!(71.0));
        assert_eq!(reply["requestSerial"], json!("S1"));
        let asleep = CarState {
            online: false,
            ..driving()
        };
        let reply = empty.realtime(choice, &car("seal", Energy::Ev), &asleep, AT, None);
        assert_eq!(
            (reply["onlineState"].clone(), reply["time"].clone()),
            (json!(0), json!(0))
        );
        assert!(reply.get("requestSerial").is_none());
    }

    #[test]
    fn a_missing_or_broken_corpus_falls_back() {
        let missing = Templates::load(Some(Path::new("no such corpus")));
        assert!(missing.realtime.is_empty());
        assert_eq!(
            missing.latest_config()["cfFixedList"][0]["functionNo"],
            json!("1001")
        );
        assert_eq!(missing.fixed().len(), 4);
        assert_eq!(
            with_vin(&json!({"vin": "A", "x": 1}), "B"),
            json!({"vin": "B", "x": 1})
        );
        assert_eq!(with_vin(&json!({"x": 1}), "B"), json!({"x": 1}));
        assert_eq!(with_vin(&json!([1]), "B"), json!([1]));
        let entries = [Entry {
            words: vec![],
            note: String::new(),
            payload: json!([1]),
        }];
        assert_eq!(pick(&entries, &["x"], json!({"a": 1}))["a"], json!(1));
        assert!(pick(&entries, &[], json!(null)).is_empty());
    }

    #[test]
    fn realtime_carries_the_state() {
        let templates = Templates::load(Some(&corpus()));
        let ev = car("seal", Energy::Ev);
        let choice = templates.choose("seal", Energy::Ev);
        let reply = templates.realtime(choice, &ev, &driving(), AT, Some("SERIAL"));
        assert_eq!(reply["onlineState"], json!(1));
        assert_eq!(reply["time"], json!(AT.unix()));
        assert!(reply.get("timestamp").is_none());
        assert_eq!(reply["elecPercent"], json!(71.0));
        assert_eq!(reply["totalMileage"], json!(20431.4));
        assert_eq!(reply["totalMileageV2"], json!(20431.4));
        assert_eq!(reply["speed"], json!(62.0));
        assert_eq!(
            (reply["powerGear"].clone(), reply["vehicleState"].clone()),
            (json!(3), json!(0))
        );
        assert_eq!(reply["chargeState"], json!(15));
        assert_eq!(reply["connectState"], json!(0));
        assert_eq!(
            (reply["rate"].clone(), reply["gl"].clone()),
            (json!(-999), Value::Null)
        );
        assert!(reply.get("chargePower").is_none());
        assert_eq!(reply["remainingHours"], json!(-1));
        assert_eq!(
            (reply["oilPercent"].clone(), reply["oilEndurance"].clone()),
            (json!(-1), json!(-1))
        );
        assert_eq!(reply["requestSerial"], json!("SERIAL"));

        let reply = templates.realtime(choice, &ev, &charging(), AT, None);
        assert_eq!(
            (reply["chargeState"].clone(), reply["chargingState"].clone()),
            (json!(1), json!(1))
        );
        assert_eq!(reply["chargePower"], json!(6.3));
        assert_eq!(reply["chargePowerUnit"], json!("kW"));
        assert_eq!(
            (reply["rate"].clone(), reply["gl"].clone()),
            (json!(-6.3), json!(6300.0))
        );
        assert_eq!(
            (
                reply["remainingHours"].clone(),
                reply["remainingMinutes"].clone()
            ),
            (json!(2), json!(5))
        );
        assert_eq!(
            (reply["fullHour"].clone(), reply["fullMinute"].clone()),
            (json!(2), json!(5))
        );
        assert_eq!(reply["powerGear"], json!(1));

        let unknown = CarState {
            soc: None,
            odometer_km: None,
            speed_kmh: None,
            vehicle_on: None,
            connect_state: None,
            minutes_to_full: Some(-5),
            ..driving()
        };
        let reply = templates.realtime(choice, &ev, &unknown, AT, None);
        assert_eq!(reply["elecPercent"], json!(-1.0));
        assert_eq!(reply["enduranceMileage"], json!(-1.0));
        assert_eq!(reply["powerBattery"], Value::Null);
        assert_eq!(reply["totalMileage"], Value::Null);
        assert_eq!(reply["speed"], Value::Null);
        assert_eq!(
            (reply["powerGear"].clone(), reply["vehicleState"].clone()),
            (json!(-1), json!(-1))
        );
        assert_eq!(reply["connectState"], json!(-1));
        assert_eq!(reply["fullHour"], json!(-1));

        let hybrid = car("seal-6", Energy::Hybrid);
        let fuelled = CarState {
            fuel_percent: Some(95.0),
            ..driving()
        };
        let choice = templates.choose("seal-6", Energy::Hybrid);
        let reply = templates.realtime(choice, &hybrid, &fuelled, AT, None);
        assert_eq!(reply["oilPercent"], json!(95.0));
        assert_eq!(reply["oilEndurance"], json!(760.0));
        assert_eq!(reply["enduranceMileage"], json!(53.0));
        // A fuel level on an EV is not reported.
        let reply = templates.realtime(choice, &ev, &fuelled, AT, None);
        assert_eq!(reply["oilPercent"], json!(-1));
    }

    #[test]
    fn the_other_replies_carry_the_state() {
        let templates = Templates::load(Some(&corpus()));
        let ev = car("seal", Energy::Ev);
        let reply = templates.charging(&ev, &charging(), AT);
        assert_eq!(reply["vin"], json!(ev.vin));
        assert_eq!(reply["elecPercent"], json!(71));
        assert_eq!(
            (
                reply["chargingState"].clone(),
                reply["connectState"].clone()
            ),
            (json!(1), json!(1))
        );
        assert_eq!(
            (reply["fullHour"].clone(), reply["fullMinute"].clone()),
            (json!(2), json!(5))
        );
        assert_eq!(reply["chargePower"], json!(6.3));
        assert_eq!(reply["time"], json!(AT.unix()));
        assert!(reply["smartChargeDto"].is_object() && reply["smartJourneyDto"].is_object());
        let idle = CarState {
            soc: None,
            connect_state: None,
            ..driving()
        };
        let reply = templates.charging(&ev, &idle, AT);
        assert!(reply.get("elecPercent").is_none() && reply.get("connectState").is_none());
        assert!(reply.get("chargePower").is_none());
        assert_eq!(
            (reply["chargingState"].clone(), reply["fullHour"].clone()),
            (json!(0), json!(-1))
        );
        let plugged = CarState {
            plugged: true,
            ..idle
        };
        assert_eq!(
            templates.charging(&ev, &plugged, AT)["connectState"],
            json!(1)
        );

        assert_eq!(templates.hvac(&charging())["status"], json!(1));
        assert_eq!(templates.hvac(&charging())["acSwitch"], json!(2));
        assert_eq!(templates.hvac(&driving())["status"], json!(2));
        let unknown = CarState {
            hvac_on: None,
            ..driving()
        };
        assert_eq!(templates.hvac(&unknown)["status"], json!(-1));
        let asleep = CarState {
            online: false,
            ..charging()
        };
        assert_eq!(templates.hvac(&asleep)["status"], json!(0));

        let reply = templates.gps(3, &driving(), AT, Some("G1"));
        let (lat, lon, heading) = position(3, Some(20431.4));
        assert_eq!(
            (reply["latitude"].clone(), reply["longitude"].clone()),
            (json!(lat), json!(lon))
        );
        assert_eq!(
            (reply["speed"].clone(), reply["direction"].clone()),
            (json!(62.0), json!(heading))
        );
        assert_eq!(reply["requestSerial"], json!("G1"));
        assert_eq!(reply["gpsTimeStamp"], json!(AT.unix()));
        let off = CarState {
            gps: false,
            ..driving()
        };
        let reply = templates.gps(3, &off, AT, None);
        assert_eq!(
            (reply["latitude"].clone(), reply["longitude"].clone()),
            (json!(0.0), json!(0.0))
        );
        assert!(reply.get("requestSerial").is_none());
        let asleep = CarState {
            online: false,
            ..driving()
        };
        assert_eq!(templates.gps(3, &asleep, AT, None)["speed"], json!(0.0));

        let reply = templates.energy(&ev, &driving(), AT);
        assert_eq!(
            reply["cumulativeEnergyConsumption"]["totalMileage"],
            json!("20431.4")
        );
        assert_eq!(reply["time"], json!(AT.unix()));
        let hybrid = car("x", Energy::Hybrid);
        let reply = templates.energy(&hybrid, &CarState::UNSEEN, AT);
        assert_eq!(
            reply["cumulativeEnergyConsumption"]["totalMileage"],
            json!("--")
        );
        assert_eq!(
            reply["cumulativeEnergyConsumption"]["oilUnit"],
            json!("L/100km")
        );

        let reply = templates.vehicle(&hybrid);
        assert_eq!(
            (reply["vin"].clone(), reply["modelName"].clone()),
            (json!(hybrid.vin), json!("X"))
        );
        assert_eq!(reply["energyType"], json!("2"));
        assert!(reply.get("empowerId").is_none());
        assert_eq!(templates.vehicle(&ev)["energyType"], json!("0"));
        assert!(
            templates.latest_config()["cfFixedList"]
                .as_array()
                .unwrap()
                .len()
                > 10
        );
    }

    #[test]
    fn push_bodies() {
        let templates = Templates::load(Some(&corpus()));
        let ev = car("seal", Energy::Ev);
        let choice = templates.choose("seal", Energy::Ev);
        let body = templates.push(PushEvent::VehicleInfo, choice, &ev, &driving(), AT);
        assert_eq!(
            (body["event"].clone(), body["vin"].clone()),
            (json!("vehicleInfo"), json!(ev.vin))
        );
        assert_eq!(body["data"]["respondData"]["elecPercent"], json!(71.0));
        let body = templates.push(PushEvent::SmartCharge, choice, &ev, &charging(), AT);
        assert_eq!(body["data"]["respondData"]["chargingState"], json!(1));
        let body = templates.push(PushEvent::EnergyConsumption, choice, &ev, &driving(), AT);
        assert!(body["data"]["respondData"]["cumulativeEnergyConsumption"].is_object());
        let body = templates.push(PushEvent::RemoteControl, choice, &ev, &driving(), AT);
        assert_eq!(body["data"]["respondData"], json!({"res": 2}));
        assert_eq!(body["data"]["uuid"].as_str().unwrap().len(), 32);
    }
}
