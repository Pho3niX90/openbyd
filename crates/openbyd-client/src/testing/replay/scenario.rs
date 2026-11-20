//! Scenario files (`bench/replay/SCENARIO.md`): loading, validation, and the
//! lookups the replay server answers from.
//!
//! A scenario is stored compactly: every car's `state` and `error` lines as
//! one time-sorted list found by binary search, and every `push` and `1005`
//! expiry in one time-sorted schedule.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::BufRead;
use std::path::Path;

use serde_json::{Map, Value};

/// The only scenario version this reader accepts.
pub const VERSION: i64 = 1;

/// The keys of a `state` line besides `type`, `vin` and `t`, in the order of
/// SCENARIO.md.
pub const STATE_KEYS: [&str; 13] = [
    "online",
    "soc",
    "odometer_km",
    "speed_kmh",
    "vehicle_on",
    "plugged",
    "charging",
    "connect_state",
    "charge_power_kw",
    "minutes_to_full",
    "hvac_on",
    "fuel_percent",
    "gps",
];

/// The state keys a `smartCharge` push may carry.
pub const SMART_CHARGE_KEYS: [&str; 4] = ["plugged", "charging", "connect_state", "soc"];

/// Why a scenario was refused: the line (from 1; 0 for the file as a whole)
/// and what is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioError {
    /// The line number, from 1; 0 when the problem is not on one line.
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "scenario: {}", self.message)
        } else {
            write!(f, "scenario line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for ScenarioError {}

fn error<T>(line: usize, message: impl Into<String>) -> Result<T, ScenarioError> {
    Err(ScenarioError {
        line,
        message: message.into(),
    })
}

/// Where a scenario came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Made by the generator.
    Synthetic,
    /// Exported from production, anonymised.
    Export,
}

/// The header line.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    /// The source.
    pub source: Source,
    /// The instant `t = 0` stands for, in seconds since the Unix epoch.
    pub start_unix: i64,
    /// The length in seconds.
    pub duration_s: u32,
    /// The generator's seed, for a synthetic scenario.
    pub seed: Option<String>,
    /// The free-form counts.
    pub stats: Value,
}

/// A car's powertrain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Energy {
    /// Battery electric.
    Ev,
    /// Plug-in hybrid.
    Hybrid,
}

/// An account: its name (`acct-<n>`) and its cars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioAccount {
    /// The account name; the login is `<name>@example.com` / `secret`.
    pub name: String,
    /// Its cars, as indices into [`Scenario::cars`], in the account line's order.
    pub cars: Vec<usize>,
}

impl ScenarioAccount {
    /// The login name.
    pub fn username(&self) -> String {
        format!("{}@example.com", self.name)
    }
}

/// The password of every scenario account.
pub const PASSWORD: &str = "secret";

/// A car.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioCar {
    /// The VIN.
    pub vin: String,
    /// The owning account, as an index into [`Scenario::accounts`].
    pub account: usize,
    /// The catalogue key, or `"unknown"`.
    pub model: String,
    /// The powertrain.
    pub energy: Energy,
}

/// The complete state of a car at a moment: a `state` line without its
/// `vin` and `t`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CarState {
    /// BYD can reach the car.
    pub online: bool,
    /// Battery, percent.
    pub soc: Option<f64>,
    /// Odometer, km.
    pub odometer_km: Option<f64>,
    /// Speed, km/h.
    pub speed_kmh: Option<f64>,
    /// Switched on.
    pub vehicle_on: Option<bool>,
    /// A charger is connected.
    pub plugged: bool,
    /// Charging now.
    pub charging: bool,
    /// BYD's charger connection state.
    pub connect_state: Option<i64>,
    /// Charge power at the battery, kW.
    pub charge_power_kw: Option<f64>,
    /// Remaining charge time, minutes.
    pub minutes_to_full: Option<i64>,
    /// Climate running.
    pub hvac_on: Option<bool>,
    /// Tank, percent (hybrids).
    pub fuel_percent: Option<f64>,
    /// Whether a location read succeeds.
    pub gps: bool,
}

impl CarState {
    /// The state of a car with no `state` line yet: asleep, zero odometer.
    pub const UNSEEN: CarState = CarState {
        online: false,
        soc: None,
        odometer_km: Some(0.0),
        speed_kmh: None,
        vehicle_on: None,
        plugged: false,
        charging: false,
        connect_state: None,
        charge_power_kw: None,
        minutes_to_full: None,
        hvac_on: None,
        fuel_percent: None,
        gps: true,
    };
}

/// The state keys a push carried, each `Some` when present (a nullable key
/// present as `null` is `Some(None)`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[allow(missing_docs)]
pub struct PartialState {
    pub online: Option<bool>,
    pub soc: Option<Option<f64>>,
    pub odometer_km: Option<Option<f64>>,
    pub speed_kmh: Option<Option<f64>>,
    pub vehicle_on: Option<Option<bool>>,
    pub plugged: Option<bool>,
    pub charging: Option<bool>,
    pub connect_state: Option<Option<i64>>,
    pub charge_power_kw: Option<Option<f64>>,
    pub minutes_to_full: Option<Option<i64>>,
    pub hvac_on: Option<Option<bool>>,
    pub fuel_percent: Option<Option<f64>>,
    pub gps: Option<bool>,
}

impl PartialState {
    /// `base` with the present keys laid over it.
    pub fn over(&self, base: &CarState) -> CarState {
        CarState {
            online: self.online.unwrap_or(base.online),
            soc: self.soc.unwrap_or(base.soc),
            odometer_km: self.odometer_km.unwrap_or(base.odometer_km),
            speed_kmh: self.speed_kmh.unwrap_or(base.speed_kmh),
            vehicle_on: self.vehicle_on.unwrap_or(base.vehicle_on),
            plugged: self.plugged.unwrap_or(base.plugged),
            charging: self.charging.unwrap_or(base.charging),
            connect_state: self.connect_state.unwrap_or(base.connect_state),
            charge_power_kw: self.charge_power_kw.unwrap_or(base.charge_power_kw),
            minutes_to_full: self.minutes_to_full.unwrap_or(base.minutes_to_full),
            hvac_on: self.hvac_on.unwrap_or(base.hvac_on),
            fuel_percent: self.fuel_percent.unwrap_or(base.fuel_percent),
            gps: self.gps.unwrap_or(base.gps),
        }
    }
}

/// The kind of a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushEvent {
    /// Realtime data.
    VehicleInfo,
    /// A remote-control result.
    RemoteControl,
    /// Charging state.
    SmartCharge,
    /// Energy consumption.
    EnergyConsumption,
}

impl PushEvent {
    /// BYD's name for it.
    pub fn name(self) -> &'static str {
        match self {
            PushEvent::VehicleInfo => "vehicleInfo",
            PushEvent::RemoteControl => "remoteControl",
            PushEvent::SmartCharge => "smartCharge",
            PushEvent::EnergyConsumption => "energyConsumption",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        [
            PushEvent::VehicleInfo,
            PushEvent::RemoteControl,
            PushEvent::SmartCharge,
            PushEvent::EnergyConsumption,
        ]
        .into_iter()
        .find(|event| event.name() == name)
    }

    /// The state keys a push of this kind may carry.
    pub fn allowed_keys(self) -> &'static [&'static str] {
        match self {
            PushEvent::VehicleInfo => &STATE_KEYS,
            PushEvent::SmartCharge => &SMART_CHARGE_KEYS,
            PushEvent::RemoteControl | PushEvent::EnergyConsumption => &[],
        }
    }
}

/// A push line.
#[derive(Debug, Clone, PartialEq)]
pub struct Push {
    /// The car, as an index into [`Scenario::cars`].
    pub car: usize,
    /// The kind.
    pub event: PushEvent,
    /// The state keys it carried.
    pub fields: PartialState,
}

/// Something that happens at a moment rather than lasting.
#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    /// Every session of this account (index into [`Scenario::accounts`])
    /// expires: an `error` line with code `1005`.
    Expire(usize),
    /// A push.
    Push(Push),
}

/// One entry of the schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// When.
    pub t: u32,
    /// What.
    pub kind: EventKind,
}

const NO_STATE: u32 = u32::MAX;
const NO_ERROR: u32 = u32::MAX;

/// One `state` or (lasting) `error` line of a car.
#[derive(Debug, Clone, Copy)]
struct Entry {
    t: u32,
    /// The state that applies: this line's, or for an error the last state
    /// before it; [`NO_STATE`] when there is none.
    state: u32,
    /// The error code (index into `codes`), or [`NO_ERROR`].
    error: u32,
}

/// What applies to a car at a moment ([`Scenario::lookup`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lookup<'a> {
    /// The car's state: its last `state` line at or before the moment, or
    /// [`CarState::UNSEEN`].
    pub state: &'a CarState,
    /// The error BYD answers with now, if an `error` line is in force.
    pub error: Option<&'a str>,
}

/// A loaded, validated scenario.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// The header.
    pub header: Header,
    /// The accounts, in file order.
    pub accounts: Vec<ScenarioAccount>,
    /// The cars, in file order (a car's index places its made-up anchor).
    pub cars: Vec<ScenarioCar>,
    /// Pushes and session expiries, by time (ties in file order).
    pub schedule: Vec<Event>,
    states: Vec<CarState>,
    timelines: Vec<Vec<Entry>>,
    codes: Vec<String>,
    vins: HashMap<String, usize>,
    names: HashMap<String, usize>,
}

/// `YYYY-MM-DDTHH:MM:SSZ` as seconds since the Unix epoch.
fn parse_start(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return None;
    }
    let number = |from: usize, to: usize| -> Option<i64> {
        let part = &text[from..to];
        part.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day < 1 || day > days_in_month || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

fn valid_account_name(name: &str) -> bool {
    name.strip_prefix("acct-")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn valid_vin(vin: &str) -> bool {
    vin.strip_prefix("TESTVIN")
        .is_some_and(|digits| digits.len() == 10 && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn valid_model(model: &str) -> bool {
    (1..=40).contains(&model.len())
        && model
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// A `stats` key: lowercase words joined by underscores, 1 to 40 characters.
fn valid_stats_key(key: &str) -> bool {
    (1..=40).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// The bounds of the numeric state keys (SCENARIO.md, "State").
const PERCENT: (f64, f64) = (0.0, 100.0);
const ODOMETER: (f64, f64) = (0.0, 10_000_000.0);
const SPEED: (f64, f64) = (0.0, 400.0);
const CHARGE_POWER: (f64, f64) = (0.0, 1_000.0);
const MINUTES_TO_FULL: (i64, i64) = (0, 10_080);
const CONNECT_STATE: (i64, i64) = (-1, 255);

/// Reads typed fields of one line, naming the line in every error.
struct Line<'a> {
    number: usize,
    map: &'a Map<String, Value>,
}

impl Line<'_> {
    fn fail<T>(&self, message: impl Into<String>) -> Result<T, ScenarioError> {
        error(self.number, message)
    }

    fn keys(&self, required: &[&str], optional: &[&str]) -> Result<(), ScenarioError> {
        for key in required {
            if !self.map.contains_key(*key) {
                return self.fail(format!("missing key {key:?}"));
            }
        }
        for key in self.map.keys() {
            if key != "type"
                && !required.contains(&key.as_str())
                && !optional.contains(&key.as_str())
            {
                return self.fail(format!("unknown key {key:?}"));
            }
        }
        Ok(())
    }

    fn get(&self, key: &str) -> Result<&Value, ScenarioError> {
        match self.map.get(key) {
            Some(value) => Ok(value),
            None => self.fail(format!("missing key {key:?}")),
        }
    }

    fn string(&self, key: &str) -> Result<&str, ScenarioError> {
        match self.get(key)? {
            Value::String(text) => Ok(text),
            _ => self.fail(format!("{key:?} must be a string")),
        }
    }

    fn integer(&self, key: &str) -> Result<i64, ScenarioError> {
        match self.get(key)?.as_i64() {
            Some(value) => Ok(value),
            None => self.fail(format!("{key:?} must be an integer")),
        }
    }

    fn count(&self, key: &str) -> Result<u32, ScenarioError> {
        match u32::try_from(self.integer(key)?) {
            Ok(value) => Ok(value),
            Err(_) => self.fail(format!("{key:?} must be a non-negative integer")),
        }
    }

    fn boolean(&self, key: &str) -> Result<bool, ScenarioError> {
        match self.get(key)? {
            Value::Bool(value) => Ok(*value),
            _ => self.fail(format!("{key:?} must be true or false")),
        }
    }

    fn nullable<T>(
        &self,
        key: &str,
        kind: &str,
        read: impl Fn(&Value) -> Option<T>,
    ) -> Result<Option<T>, ScenarioError> {
        match self.get(key)? {
            Value::Null => Ok(None),
            value => match read(value) {
                Some(value) => Ok(Some(value)),
                None => self.fail(format!("{key:?} must be {kind} or null")),
            },
        }
    }

    fn number(&self, key: &str, (low, high): (f64, f64)) -> Result<Option<f64>, ScenarioError> {
        let value = self.nullable(key, "a number", Value::as_f64)?;
        match value {
            Some(number) if !(low..=high).contains(&number) => {
                self.fail(format!("{key:?} must be between {low} and {high}"))
            }
            _ => Ok(value),
        }
    }

    fn int(&self, key: &str, (low, high): (i64, i64)) -> Result<Option<i64>, ScenarioError> {
        let value = self.nullable(key, "an integer", Value::as_i64)?;
        match value {
            Some(number) if !(low..=high).contains(&number) => {
                self.fail(format!("{key:?} must be between {low} and {high}"))
            }
            _ => Ok(value),
        }
    }

    fn flag(&self, key: &str) -> Result<Option<bool>, ScenarioError> {
        self.nullable(key, "true or false", Value::as_bool)
    }

    /// The present keys of `allowed`, typed and bounded as in a `state`
    /// line, for a car with `energy` (only a hybrid has a fuel level).
    fn partial(&self, allowed: &[&str], energy: Energy) -> Result<PartialState, ScenarioError> {
        let mut fields = PartialState::default();
        for key in allowed {
            if !self.map.contains_key(*key) {
                continue;
            }
            match *key {
                "online" => fields.online = Some(self.boolean(key)?),
                "soc" => fields.soc = Some(self.number(key, PERCENT)?),
                "odometer_km" => fields.odometer_km = Some(self.number(key, ODOMETER)?),
                "speed_kmh" => fields.speed_kmh = Some(self.number(key, SPEED)?),
                "vehicle_on" => fields.vehicle_on = Some(self.flag(key)?),
                "plugged" => fields.plugged = Some(self.boolean(key)?),
                "charging" => fields.charging = Some(self.boolean(key)?),
                "connect_state" => fields.connect_state = Some(self.int(key, CONNECT_STATE)?),
                "charge_power_kw" => {
                    fields.charge_power_kw = Some(self.number(key, CHARGE_POWER)?);
                }
                "minutes_to_full" => {
                    fields.minutes_to_full = Some(self.int(key, MINUTES_TO_FULL)?);
                }
                "hvac_on" => fields.hvac_on = Some(self.flag(key)?),
                "fuel_percent" => {
                    let fuel = self.number(key, PERCENT)?;
                    if fuel.is_some() && energy != Energy::Hybrid {
                        return self
                            .fail("\"fuel_percent\" must be null on a car that is not a hybrid");
                    }
                    fields.fuel_percent = Some(fuel);
                }
                "gps" => fields.gps = Some(self.boolean(key)?),
                _ => unreachable!("every state key is handled"),
            }
        }
        Ok(fields)
    }
}

/// The rank of a line type in the required order.
fn rank(kind: &str) -> Option<u8> {
    match kind {
        "scenario" => Some(0),
        "account" => Some(1),
        "car" => Some(2),
        "state" | "error" | "push" => Some(3),
        _ => None,
    }
}

/// Builds a [`Scenario`] line by line.
struct Builder {
    /// The header, its line number and its account and car counts.
    header: Option<(Header, usize, u32, u32)>,
    rank: u8,
    last_t: u32,
    scenario: Scenario,
    /// The account each VIN is listed under, from the account lines.
    listed: HashMap<String, usize>,
    code_index: HashMap<String, u32>,
}

impl Builder {
    fn new() -> Self {
        Self {
            header: None,
            rank: 0,
            last_t: 0,
            scenario: Scenario {
                header: Header {
                    source: Source::Synthetic,
                    start_unix: 0,
                    duration_s: 0,
                    seed: None,
                    stats: Value::Null,
                },
                accounts: Vec::new(),
                cars: Vec::new(),
                schedule: Vec::new(),
                states: Vec::new(),
                timelines: Vec::new(),
                codes: Vec::new(),
                vins: HashMap::new(),
                names: HashMap::new(),
            },
            listed: HashMap::new(),
            code_index: HashMap::new(),
        }
    }

    fn line(&mut self, number: usize, text: &str) -> Result<(), ScenarioError> {
        let map = match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => map,
            Ok(_) => return error(number, "a line must be a JSON object"),
            Err(problem) => return error(number, format!("not JSON: {problem}")),
        };
        let line = Line { number, map: &map };
        let kind = line.string("type")?;
        let Some(rank) = rank(kind) else {
            return line.fail(format!("unknown type {kind:?}"));
        };
        if self.header.is_none() {
            if rank != 0 {
                return line.fail("the first line must be the scenario header");
            }
            return self.header(&line);
        }
        if rank == 0 {
            return line.fail("a second header");
        }
        if rank < self.rank {
            return line.fail(format!(
                "a {kind} line after the {} lines",
                ["header", "account", "car", "timeline"][usize::from(self.rank)]
            ));
        }
        if rank == 3 && self.rank < 3 {
            self.check_accounts_have_cars(number)?;
        }
        self.rank = rank;
        match kind {
            "account" => self.account(&line),
            "car" => self.car(&line),
            _ => self.timeline(&line, kind),
        }
    }

    fn header(&mut self, line: &Line<'_>) -> Result<(), ScenarioError> {
        line.keys(
            &[
                "version",
                "source",
                "start",
                "duration_s",
                "accounts",
                "cars",
            ],
            &["seed", "stats"],
        )?;
        if line.get("version")?.as_i64() != Some(VERSION) {
            return line.fail(format!(
                "version {} is not supported (only {VERSION})",
                line.get("version")?
            ));
        }
        let source = match line.string("source")? {
            "synthetic" => Source::Synthetic,
            "export" => Source::Export,
            other => return line.fail(format!("unknown source {other:?}")),
        };
        let Some(start_unix) = parse_start(line.string("start")?) else {
            return line.fail("\"start\" must be YYYY-MM-DDTHH:MM:SSZ");
        };
        let seed = match line.map.get("seed") {
            None | Some(Value::Null) => None,
            Some(_) if source != Source::Synthetic => {
                return line.fail("\"seed\" is only for a synthetic scenario");
            }
            Some(Value::String(seed)) if valid_model(seed) => Some(seed.clone()),
            Some(_) => return line.fail("\"seed\" must be written like a model key"),
        };
        let stats = line.map.get("stats").cloned().unwrap_or(Value::Null);
        match &stats {
            Value::Null => {}
            Value::Object(counts) => {
                for (key, value) in counts {
                    if !valid_stats_key(key) {
                        return line.fail(format!("stats key {key:?} must match [a-z_]{{1,40}}"));
                    }
                    if value.as_u64().is_none() {
                        return line.fail(format!(
                            "stats value {key:?} must be a non-negative whole number"
                        ));
                    }
                }
            }
            _ => return line.fail("\"stats\" must be an object"),
        }
        let header = Header {
            source,
            start_unix,
            duration_s: line.count("duration_s")?,
            seed,
            stats,
        };
        self.scenario.header = header.clone();
        self.header = Some((
            header,
            line.number,
            line.count("accounts")?,
            line.count("cars")?,
        ));
        Ok(())
    }

    fn account(&mut self, line: &Line<'_>) -> Result<(), ScenarioError> {
        line.keys(&["account", "cars"], &[])?;
        let name = line.string("account")?;
        if !valid_account_name(name) {
            return line.fail(format!("{name:?} is not an account name (acct-<number>)"));
        }
        if self.scenario.names.contains_key(name) {
            return line.fail(format!("account {name:?} appears twice"));
        }
        let Value::Array(vins) = line.get("cars")? else {
            return line.fail("\"cars\" must be a list of VINs");
        };
        let index = self.scenario.accounts.len();
        for vin in vins {
            let Some(vin) = vin.as_str().filter(|vin| valid_vin(vin)) else {
                return line.fail(format!("{vin} is not a VIN (TESTVIN and ten digits)"));
            };
            if self.listed.insert(vin.to_owned(), index).is_some() {
                return line.fail(format!("{vin} is listed twice"));
            }
        }
        self.scenario.names.insert(name.to_owned(), index);
        self.scenario.accounts.push(ScenarioAccount {
            name: name.to_owned(),
            cars: Vec::new(),
        });
        Ok(())
    }

    fn car(&mut self, line: &Line<'_>) -> Result<(), ScenarioError> {
        line.keys(&["vin", "account", "model", "energy"], &[])?;
        let vin = line.string("vin")?;
        if !valid_vin(vin) {
            return line.fail(format!("{vin:?} is not a VIN (TESTVIN and ten digits)"));
        }
        if self.scenario.vins.contains_key(vin) {
            return line.fail(format!("car {vin} appears twice"));
        }
        let name = line.string("account")?;
        let Some(&account) = self.scenario.names.get(name) else {
            return line.fail(format!("unknown account {name:?}"));
        };
        if self.listed.get(vin) != Some(&account) {
            return line.fail(format!("{vin} is not in the cars of {name}"));
        }
        let model = line.string("model")?;
        if !valid_model(model) {
            return line.fail(format!("{model:?} is not a catalogue key"));
        }
        let energy = match line.string("energy")? {
            "ev" => Energy::Ev,
            "hybrid" => Energy::Hybrid,
            other => return line.fail(format!("energy {other:?} is not \"ev\" or \"hybrid\"")),
        };
        let index = self.scenario.cars.len();
        self.scenario.vins.insert(vin.to_owned(), index);
        self.scenario.accounts[account].cars.push(index);
        self.scenario.cars.push(ScenarioCar {
            vin: vin.to_owned(),
            account,
            model: model.to_owned(),
            energy,
        });
        self.scenario.timelines.push(Vec::new());
        Ok(())
    }

    fn check_accounts_have_cars(&self, number: usize) -> Result<(), ScenarioError> {
        for (vin, account) in &self.listed {
            if !self.scenario.vins.contains_key(vin) {
                let name = &self.scenario.accounts[*account].name;
                return error(number, format!("{vin} of {name} has no car line"));
            }
        }
        Ok(())
    }

    fn timeline(&mut self, line: &Line<'_>, kind: &str) -> Result<(), ScenarioError> {
        let vin = line.string("vin")?;
        let Some(&car) = self.scenario.vins.get(vin) else {
            return line.fail(format!("unknown car {vin:?}"));
        };
        let t = line.count("t")?;
        let duration = self.scenario.header.duration_s;
        if t > duration {
            return line.fail(format!("t {t} is after the end ({duration})"));
        }
        if t < self.last_t {
            return line.fail(format!(
                "t {t} is before the previous line's {}",
                self.last_t
            ));
        }
        self.last_t = t;
        match kind {
            "state" => {
                line.keys(&[&["vin", "t"][..], &STATE_KEYS[..]].concat(), &[])?;
                let fields = line.partial(&STATE_KEYS, self.scenario.cars[car].energy)?;
                let state = fields.over(&CarState::UNSEEN);
                let index =
                    u32::try_from(self.scenario.states.len()).expect("fewer than 4G states");
                self.scenario.states.push(state);
                self.scenario.timelines[car].push(Entry {
                    t,
                    state: index,
                    error: NO_ERROR,
                });
            }
            "error" => {
                line.keys(&["vin", "t", "code"], &[])?;
                let code = line.string("code")?;
                if code.is_empty() || !code.bytes().all(|b| b.is_ascii_digit()) {
                    return line.fail(format!("code {code:?} must be digits"));
                }
                if code == "1005" {
                    let account = self.scenario.cars[car].account;
                    self.scenario.schedule.push(Event {
                        t,
                        kind: EventKind::Expire(account),
                    });
                    return Ok(());
                }
                let next = u32::try_from(self.scenario.codes.len()).expect("few codes");
                let code_index = *self.code_index.entry(code.to_owned()).or_insert(next);
                if code_index == next {
                    self.scenario.codes.push(code.to_owned());
                }
                let timeline = &mut self.scenario.timelines[car];
                let state = timeline.last().map_or(NO_STATE, |entry| entry.state);
                timeline.push(Entry {
                    t,
                    state,
                    error: code_index,
                });
            }
            _ => {
                let name = line.string("event")?;
                let Some(event) = PushEvent::parse(name) else {
                    return line.fail(format!("unknown push event {name:?}"));
                };
                line.keys(&["vin", "t", "event"], event.allowed_keys())?;
                let fields = line.partial(event.allowed_keys(), self.scenario.cars[car].energy)?;
                self.scenario.schedule.push(Event {
                    t,
                    kind: EventKind::Push(Push { car, event, fields }),
                });
            }
        }
        Ok(())
    }

    fn finish(mut self, lines: usize) -> Result<Scenario, ScenarioError> {
        let Some((_, header_line, accounts, cars)) = self.header.take() else {
            return error(0, "the file is empty");
        };
        if self.rank < 3 {
            self.check_accounts_have_cars(lines)?;
        }
        let scenario = self.scenario;
        if scenario.accounts.len() != accounts as usize {
            return error(
                header_line,
                format!(
                    "the header counts {accounts} accounts, the file has {}",
                    scenario.accounts.len()
                ),
            );
        }
        if scenario.cars.len() != cars as usize {
            return error(
                header_line,
                format!(
                    "the header counts {cars} cars, the file has {}",
                    scenario.cars.len()
                ),
            );
        }
        Ok(scenario)
    }
}

impl Scenario {
    /// Reads and validates a scenario from `reader`, line by line.
    pub fn from_reader(reader: impl BufRead) -> Result<Self, ScenarioError> {
        let mut builder = Builder::new();
        let mut number = 0;
        for line in reader.lines() {
            number += 1;
            let line = line.map_err(|problem| ScenarioError {
                line: number,
                message: format!("cannot read: {problem}"),
            })?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            builder.line(number, line)?;
        }
        builder.finish(number)
    }

    /// Reads and validates the scenario text `text`.
    pub fn parse(text: &str) -> Result<Self, ScenarioError> {
        Self::from_reader(text.as_bytes())
    }

    /// Reads and validates the scenario file at `path`.
    pub fn load(path: &Path) -> Result<Self, ScenarioError> {
        let file = std::fs::File::open(path).map_err(|problem| ScenarioError {
            line: 0,
            message: format!("cannot open {}: {problem}", path.display()),
        })?;
        Self::from_reader(std::io::BufReader::with_capacity(1 << 20, file))
    }

    /// The index of the car with `vin`.
    pub fn car_index(&self, vin: &str) -> Option<usize> {
        self.vins.get(vin).copied()
    }

    /// The index of the account whose login is `username`.
    pub fn account_for_username(&self, username: &str) -> Option<usize> {
        let name = username.strip_suffix("@example.com")?;
        self.names.get(name).copied()
    }

    /// What applies to car `car` at scenario second `t`: the last `state` or
    /// `error` line at or before `t` decides.
    pub fn lookup(&self, car: usize, t: u32) -> Lookup<'_> {
        let timeline = &self.timelines[car];
        let after = timeline.partition_point(|entry| entry.t <= t);
        let Some(entry) = after.checked_sub(1).map(|index| timeline[index]) else {
            return Lookup {
                state: &CarState::UNSEEN,
                error: None,
            };
        };
        Lookup {
            state: if entry.state == NO_STATE {
                &CarState::UNSEEN
            } else {
                &self.states[entry.state as usize]
            },
            error: (entry.error != NO_ERROR).then(|| self.codes[entry.error as usize].as_str()),
        }
    }

    /// How many `state` and lasting `error` lines the cars have.
    pub fn timeline_len(&self) -> usize {
        self.timelines.iter().map(Vec::len).sum()
    }

    /// The distinct `(model, energy)` pairs of the cars.
    pub fn models(&self) -> HashSet<(&str, Energy)> {
        self.cars
            .iter()
            .map(|car| (car.model.as_str(), car.energy))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 86400, "accounts": 2, "cars": 3, "seed": "demo", "stats": {"state": 3, "push": 0}}"#;
    const ACCOUNTS: &str = r#"{"type": "account", "account": "acct-0001", "cars": ["TESTVIN0000000001", "TESTVIN0000000002"]}
{"type": "account", "account": "acct-0002", "cars": ["TESTVIN0000000003"]}"#;
    const CARS: &str = r#"{"type": "car", "vin": "TESTVIN0000000001", "account": "acct-0001", "model": "seal", "energy": "ev"}
{"type": "car", "vin": "TESTVIN0000000002", "account": "acct-0001", "model": "unknown", "energy": "hybrid"}
{"type": "car", "vin": "TESTVIN0000000003", "account": "acct-0002", "model": "sealion_7", "energy": "ev"}"#;

    fn state(vin: &str, t: u32, soc: f64) -> String {
        format!(
            r#"{{"type": "state", "vin": "{vin}", "t": {t}, "online": true, "soc": {soc}, "odometer_km": 20431.4, "speed_kmh": 62.0, "vehicle_on": true, "plugged": false, "charging": false, "connect_state": 0, "charge_power_kw": null, "minutes_to_full": null, "hvac_on": false, "fuel_percent": null, "gps": true}}"#
        )
    }

    fn file(timeline: &[String]) -> String {
        let mut text = format!("{HEADER}\n{ACCOUNTS}\n{CARS}\n");
        for line in timeline {
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    fn good() -> Scenario {
        Scenario::parse(&file(&[
            state("TESTVIN0000000001", 100, 71.0),
            r#"{"type": "error", "vin": "TESTVIN0000000001", "t": 200, "code": "1008"}"#.into(),
            r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 200, "event": "vehicleInfo", "soc": 70.0, "online": true}"#.into(),
            r#"{"type": "error", "vin": "TESTVIN0000000001", "t": 250, "code": "1005"}"#.into(),
            r#"{"type": "error", "vin": "TESTVIN0000000002", "t": 260, "code": "6002"}"#.into(),
            state("TESTVIN0000000001", 300, 69.5),
            r#"{"type": "push", "vin": "TESTVIN0000000003", "t": 300, "event": "smartCharge", "charging": true}"#.into(),
            r#"{"type": "push", "vin": "TESTVIN0000000003", "t": 301, "event": "remoteControl"}"#.into(),
        ]))
        .expect("valid")
    }

    #[test]
    fn a_good_file_loads() {
        let scenario = good();
        assert_eq!(scenario.header.source, Source::Synthetic);
        assert_eq!(scenario.header.start_unix, 1_767_571_200);
        assert_eq!(scenario.header.duration_s, 86_400);
        assert_eq!(scenario.header.seed.as_deref(), Some("demo"));
        assert_eq!(scenario.accounts.len(), 2);
        assert_eq!(scenario.accounts[0].cars, vec![0, 1]);
        assert_eq!(scenario.accounts[0].username(), "acct-0001@example.com");
        assert_eq!(scenario.cars[2].model, "sealion_7");
        assert_eq!(scenario.cars[1].energy, Energy::Hybrid);
        assert_eq!(scenario.car_index("TESTVIN0000000003"), Some(2));
        assert_eq!(scenario.car_index("TESTVIN0000000009"), None);
        assert_eq!(
            scenario.account_for_username("acct-0002@example.com"),
            Some(1)
        );
        assert_eq!(scenario.account_for_username("acct-0002"), None);
        assert_eq!(scenario.timeline_len(), 4);
        assert_eq!(scenario.models().len(), 3);
        let kinds: Vec<(u32, String)> = scenario
            .schedule
            .iter()
            .map(|event| {
                let what = match &event.kind {
                    EventKind::Expire(account) => format!("expire {account}"),
                    EventKind::Push(push) => format!("{} {}", push.event.name(), push.car),
                };
                (event.t, what)
            })
            .collect();
        assert_eq!(
            kinds,
            [
                (200, "vehicleInfo 0".to_owned()),
                (250, "expire 0".to_owned()),
                (300, "smartCharge 2".to_owned()),
                (301, "remoteControl 2".to_owned()),
            ]
        );
        let EventKind::Push(push) = &scenario.schedule[0].kind else {
            panic!()
        };
        assert_eq!(push.fields.soc, Some(Some(70.0)));
        assert_eq!(push.fields.speed_kmh, None);
    }

    #[test]
    fn lookups_at_the_boundaries() {
        let scenario = good();
        // Before the first state: asleep with a zero odometer.
        let before = scenario.lookup(0, 99);
        assert_eq!(before.state, &CarState::UNSEEN);
        assert_eq!(before.error, None);
        assert!(!before.state.online);
        assert_eq!(before.state.odometer_km, Some(0.0));
        // Exactly at t, between, and up to the next line.
        for t in [100, 150, 199] {
            let at = scenario.lookup(0, t);
            assert_eq!((at.state.soc, at.error), (Some(71.0), None), "t={t}");
        }
        // An error keeps the previous state for the other reads.
        for t in [200, 250, 299] {
            let at = scenario.lookup(0, t);
            assert_eq!(
                (at.state.soc, at.error),
                (Some(71.0), Some("1008")),
                "t={t}"
            );
        }
        // Recovered, and after the end the last line holds.
        for t in [300, 86_400, u32::MAX] {
            let at = scenario.lookup(0, t);
            assert_eq!((at.state.soc, at.error), (Some(69.5), None), "t={t}");
        }
        // An error with no state before it.
        let unreachable = scenario.lookup(1, 260);
        assert_eq!(unreachable.error, Some("6002"));
        assert_eq!(unreachable.state, &CarState::UNSEEN);
        assert_eq!(scenario.lookup(2, 0).state, &CarState::UNSEEN);
    }

    #[test]
    fn partial_states_lay_over() {
        let base = *good().lookup(0, 100).state;
        let fields = PartialState {
            charging: Some(true),
            connect_state: Some(None),
            ..PartialState::default()
        };
        let over = fields.over(&base);
        assert!(over.charging);
        assert_eq!(over.connect_state, None);
        assert_eq!(over.soc, base.soc);
        assert_eq!(PartialState::default().over(&base), base);
    }

    #[test]
    fn start_times() {
        assert_eq!(parse_start("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_start("2000-02-29T23:59:59Z"), Some(951_868_799));
        assert_eq!(parse_start("2026-01-05T00:00:00Z"), Some(1_767_571_200));
        for bad in [
            "2026-01-05 00:00:00Z",
            "2026-01-05T00:00:00",
            "2026-13-05T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-01-00T00:00:00Z",
            "2026-01-05T24:00:00Z",
            "2026-01-05T00:60:00Z",
            "2026-01-05T00:00:60Z",
            "2026-01-05T00:00:00.5Z",
            "2026-0a-05T00:00:00Z",
            "+026-01-05T00:00:00Z",
        ] {
            assert_eq!(parse_start(bad), None, "{bad}");
        }
    }

    fn refused(text: &str) -> ScenarioError {
        Scenario::parse(text).expect_err("refused")
    }

    fn with_header(header: &str) -> String {
        file(&[]).replacen(HEADER, header, 1)
    }

    #[test]
    fn bad_headers() {
        let cases = [
            (
                r#"{"type": "scenario", "version": 2, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3}"#,
                "version 2",
            ),
            (
                r#"{"type": "scenario", "version": "1", "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3}"#,
                "version \"1\"",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "real", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3}"#,
                "source",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "export", "start": "yesterday", "duration_s": 10, "accounts": 2, "cars": 3}"#,
                "start",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "export", "start": "2026-01-05T00:00:00Z", "duration_s": -1, "accounts": 2, "cars": 3}"#,
                "non-negative",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "export", "start": "2026-01-05T00:00:00Z", "duration_s": 1.5, "accounts": 2, "cars": 3}"#,
                "integer",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "export", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2}"#,
                "missing key \"cars\"",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "export", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "owner": "x"}"#,
                "unknown key \"owner\"",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "export", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "seed": 5}"#,
                "seed",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "export", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "stats": {"note": "x"}}"#,
                "stats",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "export", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "stats": [1]}"#,
                "stats",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "export", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 1, "cars": 3}"#,
                "counts 1 accounts",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "export", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 4}"#,
                "counts 4 cars",
            ),
        ];
        for (header, expected) in cases {
            let problem = refused(&with_header(header));
            assert!(
                problem.to_string().contains(expected),
                "{problem} lacks {expected}"
            );
            assert_eq!(problem.line, 1, "{problem}");
        }
        let stricter = [
            // A seed only on a synthetic scenario, written like a model key.
            (
                r#"{"type": "scenario", "version": 1, "source": "export", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "seed": "demo"}"#,
                "only for a synthetic",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "seed": "Demo Run"}"#,
                "like a model key",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "seed": 5}"#,
                "like a model key",
            ),
            // Stats: flat, keys [a-z_]{1,40}, non-negative whole numbers.
            (
                r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "stats": {"events": {"state": 3}}}"#,
                "non-negative whole number",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "stats": {"n": 1.5}}"#,
                "non-negative whole number",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "stats": {"n": -1}}"#,
                "non-negative whole number",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "stats": {"n": "3"}}"#,
                "non-negative whole number",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "stats": {"Events": 3}}"#,
                "must match",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "stats": {"state2": 3}}"#,
                "must match",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "stats": {"": 3}}"#,
                "must match",
            ),
            (
                r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "stats": {"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": 3}}"#,
                "must match",
            ),
        ];
        for (header, expected) in stricter {
            let problem = refused(&with_header(header));
            assert!(
                problem.to_string().contains(expected),
                "{problem} lacks {expected}"
            );
            assert_eq!(problem.line, 1, "{problem}");
        }
        // A header count mismatch names the header's line, wherever it is.
        let four_cars = r#"{"type": "scenario", "version": 1, "source": "synthetic", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 4}"#;
        let late = format!("\n\n{}", with_header(four_cars));
        assert_eq!(refused(&late).line, 3);
        // Optional keys may be absent or null.
        let minimal = r#"{"type": "scenario", "version": 1, "source": "export", "start": "2026-01-05T00:00:00Z", "duration_s": 10, "accounts": 2, "cars": 3, "seed": null}"#;
        let scenario = Scenario::parse(&with_header(minimal)).unwrap();
        assert_eq!(scenario.header.source, Source::Export);
        assert_eq!(scenario.header.seed, None);
        assert_eq!(scenario.header.stats, Value::Null);
    }

    #[test]
    fn bad_structure() {
        let cases: Vec<(String, &str)> = vec![
            (String::new(), "empty"),
            ("\n  \n".into(), "empty"),
            ("not json\n".into(), "line 1: not JSON"),
            ("[1]\n".into(), "JSON object"),
            (r#"{"no": "type"}"#.into(), "missing key \"type\""),
            (r#"{"type": 3}"#.into(), "\"type\" must be a string"),
            (
                r#"{"type": "car"}"#.into(),
                "first line must be the scenario header",
            ),
            (format!("{HEADER}\n{HEADER}\n"), "second header"),
            (
                format!("{HEADER}\n{{\"type\": \"thing\"}}\n"),
                "unknown type",
            ),
            (
                format!("{HEADER}\n{ACCOUNTS}\n{CARS}\n{ACCOUNTS}\n"),
                "account line after the car lines",
            ),
            (
                format!(
                    "{HEADER}\n{ACCOUNTS}\n{}\n",
                    state("TESTVIN0000000001", 1, 1.0)
                ),
                "has no car line",
            ),
            (format!("{HEADER}\n{ACCOUNTS}\n"), "has no car line"),
        ];
        for (text, expected) in cases {
            let problem = refused(&text);
            assert!(
                problem.to_string().contains(expected),
                "{problem} lacks {expected}"
            );
        }
        assert_eq!(refused("not json").line, 1);
        assert_eq!(refused("").line, 0);
        assert!(refused("").to_string().starts_with("scenario: "));
    }

    #[test]
    fn bad_accounts_and_cars() {
        let account_cases = [
            (
                r#"{"type": "account", "account": "user-1", "cars": []}"#,
                "not an account name",
            ),
            (
                r#"{"type": "account", "account": "acct-", "cars": []}"#,
                "not an account name",
            ),
            (
                r#"{"type": "account", "account": "acct-0001", "cars": "TESTVIN0000000001"}"#,
                "list of VINs",
            ),
            (
                r#"{"type": "account", "account": "acct-0001", "cars": ["LSGJA52U07X000001"]}"#,
                "not a VIN",
            ),
            (
                r#"{"type": "account", "account": "acct-0001", "cars": [1]}"#,
                "not a VIN",
            ),
            (
                r#"{"type": "account", "account": "acct-0001", "cars": [], "email": "x"}"#,
                "unknown key",
            ),
        ];
        for (line, expected) in account_cases {
            let text = file(&[]).replacen(
                r#"{"type": "account", "account": "acct-0001", "cars": ["TESTVIN0000000001", "TESTVIN0000000002"]}"#,
                line,
                1,
            );
            let problem = refused(&text);
            assert!(
                problem.to_string().contains(expected),
                "{problem} lacks {expected}"
            );
        }
        let duplicate = file(&[]).replacen("acct-0002", "acct-0001", 2);
        assert!(refused(&duplicate).to_string().contains("appears twice"));
        let twice = file(&[]).replacen(r#"["TESTVIN0000000003"]"#, r#"["TESTVIN0000000001"]"#, 1);
        assert!(refused(&twice).to_string().contains("listed twice"));

        let car_cases = [
            (
                r#""vin": "TESTVIN0000000003", "account": "acct-0002", "model": "Seal", "energy": "ev""#,
                "catalogue key",
            ),
            (
                r#""vin": "TESTVIN0000000003", "account": "acct-0002", "model": "sealion_7", "energy": "petrol""#,
                "energy",
            ),
            (
                r#""vin": "TESTVIN0000000003", "account": "acct-0009", "model": "sealion_7", "energy": "ev""#,
                "unknown account",
            ),
            (
                r#""vin": "TESTVIN0000000003", "account": "acct-0001", "model": "sealion_7", "energy": "ev""#,
                "not in the cars of",
            ),
            (
                r#""vin": "TESTVIN0000000001", "account": "acct-0001", "model": "sealion_7", "energy": "ev""#,
                "appears twice",
            ),
            (
                r#""vin": "TESTVIN03", "account": "acct-0002", "model": "sealion_7", "energy": "ev""#,
                "not a VIN",
            ),
        ];
        for (fields, expected) in car_cases {
            let text = file(&[]).replacen(
                r#""vin": "TESTVIN0000000003", "account": "acct-0002", "model": "sealion_7", "energy": "ev""#,
                fields,
                1,
            );
            let problem = refused(&text);
            assert!(
                problem.to_string().contains(expected),
                "{problem} lacks {expected}"
            );
        }
        let long = format!("{:a<41}", "");
        let text = file(&[]).replacen("sealion_7", &long, 1);
        assert!(refused(&text).to_string().contains("catalogue key"));
    }

    #[test]
    fn bad_timelines() {
        let ok = state("TESTVIN0000000001", 10, 50.0);
        let cases: Vec<(String, &str)> = vec![
            (state("TESTVIN0000000009", 10, 50.0), "unknown car"),
            (state("TESTVIN0000000001", 86_401, 50.0), "after the end"),
            (ok.replace("\"t\": 10", "\"t\": -1"), "non-negative"),
            (ok.replace("\"t\": 10", "\"t\": 1.5"), "integer"),
            (ok.replace(", \"gps\": true", ""), "missing key \"gps\""),
            (ok.replace("\"gps\": true", "\"gps\": true, \"lat\": 52.1"), "unknown key \"lat\""),
            (ok.replace("\"online\": true", "\"online\": null"), "\"online\" must be true or false"),
            (ok.replace("\"soc\": 50", "\"soc\": \"50\""), "\"soc\" must be a number or null"),
            (ok.replace("\"connect_state\": 0", "\"connect_state\": 0.5"), "an integer or null"),
            (ok.replace("\"vehicle_on\": true", "\"vehicle_on\": 1"), "true or false or null"),
            (r#"{"type": "error", "vin": "TESTVIN0000000001", "t": 5, "code": 1008}"#.into(), "must be a string"),
            (r#"{"type": "error", "vin": "TESTVIN0000000001", "t": 5, "code": "busy"}"#.into(), "must be digits"),
            (r#"{"type": "error", "vin": "TESTVIN0000000001", "t": 5, "code": ""}"#.into(), "must be digits"),
            (r#"{"type": "error", "vin": "TESTVIN0000000001", "t": 5, "code": "1008", "message": "x"}"#.into(), "unknown key"),
            (r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 5, "event": "wake"}"#.into(), "unknown push event"),
            (r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 5, "event": "remoteControl", "soc": 5}"#.into(), "unknown key \"soc\""),
            (r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 5, "event": "smartCharge", "speed_kmh": 5}"#.into(), "unknown key \"speed_kmh\""),
            (r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 5, "event": "smartCharge", "plugged": null}"#.into(), "\"plugged\" must be true or false"),
            (r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 5, "event": "vehicleInfo", "gps": "yes"}"#.into(), "\"gps\" must be true or false"),
        ];
        for (line, expected) in cases {
            let problem = refused(&file(&[line]));
            assert!(
                problem.to_string().contains(expected),
                "{problem} lacks {expected}"
            );
            assert_eq!(problem.line, 7);
        }
        let unordered = file(&[
            state("TESTVIN0000000001", 10, 1.0),
            state("TESTVIN0000000002", 9, 1.0),
        ]);
        let problem = refused(&unordered);
        assert!(problem.to_string().contains("before the previous"));
        assert_eq!(problem.line, 8);
        // Equal times are fine, and every push kind and subset is accepted.
        Scenario::parse(&file(&[
            state("TESTVIN0000000001", 10, 1.0),
            state("TESTVIN0000000002", 10, 1.0),
            r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 10, "event": "smartCharge"}"#.into(),
            r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 10, "event": "smartCharge", "soc": null, "connect_state": 1, "plugged": true, "charging": false}"#.into(),
            r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 10, "event": "energyConsumption"}"#.into(),
            r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 10, "event": "vehicleInfo", "gps": false, "minutes_to_full": 30, "hvac_on": null}"#.into(),
        ]))
        .expect("valid");
    }

    #[test]
    fn values_are_bounded() {
        // (key, value, expected message), each on the parked EV of `state`.
        let cases = [
            ("soc", "-0.1", "\"soc\" must be between 0 and 100"),
            ("soc", "100.5", "\"soc\" must be between 0 and 100"),
            ("odometer_km", "-1", "between 0 and 10000000"),
            ("odometer_km", "10000000.5", "between 0 and 10000000"),
            ("speed_kmh", "-3", "between 0 and 400"),
            ("speed_kmh", "401", "between 0 and 400"),
            ("charge_power_kw", "1000.1", "between 0 and 1000"),
            ("charge_power_kw", "-7", "between 0 and 1000"),
            ("minutes_to_full", "10081", "between 0 and 10080"),
            ("minutes_to_full", "-1", "between 0 and 10080"),
            ("connect_state", "-2", "between -1 and 255"),
            ("connect_state", "256", "between -1 and 255"),
        ];
        for (key, value, expected) in cases {
            let line = state("TESTVIN0000000001", 10, 50.0);
            let start = line.find(&format!("\"{key}\": ")).unwrap() + key.len() + 4;
            let end = start + line[start..].find(',').unwrap();
            let line = format!("{}{value}{}", &line[..start], &line[end..]);
            let problem = refused(&file(std::slice::from_ref(&line)));
            assert!(
                problem.to_string().contains(expected),
                "{line}: {problem} lacks {expected}"
            );
            assert_eq!(problem.line, 7);
        }
        // The same bounds in a push.
        let push = r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 5, "event": "smartCharge", "connect_state": 300}"#;
        assert!(
            refused(&file(&[push.into()]))
                .to_string()
                .contains("between -1 and 255")
        );
        let push = r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 5, "event": "vehicleInfo", "speed_kmh": 999}"#;
        assert!(
            refused(&file(&[push.into()]))
                .to_string()
                .contains("between 0 and 400")
        );
        // A fuel level only on a hybrid (car 2), in a state and in a push.
        let ev_fuel = state("TESTVIN0000000001", 10, 50.0)
            .replace("\"fuel_percent\": null", "\"fuel_percent\": 40");
        let problem = refused(&file(&[ev_fuel]));
        assert!(problem.to_string().contains("not a hybrid"), "{problem}");
        assert_eq!(problem.line, 7);
        let push = r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 5, "event": "vehicleInfo", "fuel_percent": 40}"#;
        assert!(
            refused(&file(&[push.into()]))
                .to_string()
                .contains("not a hybrid")
        );
        let hybrid_fuel = state("TESTVIN0000000002", 10, 50.0)
            .replace("\"fuel_percent\": null", "\"fuel_percent\": 100");
        let hybrid_push = r#"{"type": "push", "vin": "TESTVIN0000000002", "t": 11, "event": "vehicleInfo", "fuel_percent": 0}"#;
        let ev_null = r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 11, "event": "vehicleInfo", "fuel_percent": null}"#;
        let edges = state("TESTVIN0000000003", 12, 100.0)
            .replace("\"odometer_km\": 20431.4", "\"odometer_km\": 10000000")
            .replace("\"speed_kmh\": 62.0", "\"speed_kmh\": 400")
            .replace("\"connect_state\": 0", "\"connect_state\": -1")
            .replace("\"charge_power_kw\": null", "\"charge_power_kw\": 1000")
            .replace("\"minutes_to_full\": null", "\"minutes_to_full\": 10080");
        let scenario = Scenario::parse(&file(&[
            hybrid_fuel,
            hybrid_push.into(),
            ev_null.into(),
            edges,
        ]))
        .expect("values at the bounds load");
        let edge = scenario.lookup(2, 12).state;
        assert_eq!(
            (edge.odometer_km, edge.speed_kmh, edge.connect_state),
            (Some(10_000_000.0), Some(400.0), Some(-1))
        );
        assert_eq!(
            (edge.charge_power_kw, edge.minutes_to_full),
            (Some(1000.0), Some(10_080))
        );
    }

    #[test]
    fn an_export_header_and_its_codes_load() {
        let header = r#"{"type":"scenario","version":1,"source":"export","start":"2026-10-30T00:00:00Z","duration_s":1123200,"accounts":2,"cars":3,"stats":{"dropped_fields":4406520,"state":231832,"push":577289,"error":55217,"unreported_fields":99135,"skipped_polls":18386,"dropped_errors":545,"dropped_events":151}}"#;
        let text = file(&[
            state("TESTVIN0000000001", 10, 50.0),
            r#"{"type": "error", "vin": "TESTVIN0000000001", "t": 20, "code": "8001"}"#.into(),
            r#"{"type": "error", "vin": "TESTVIN0000000002", "t": 20, "code": "500"}"#.into(),
            r#"{"type": "error", "vin": "TESTVIN0000000003", "t": 20, "code": "404"}"#.into(),
            r#"{"type": "push", "vin": "TESTVIN0000000001", "t": 25, "event": "vehicleInfo", "online": true, "soc": 49.0, "charge_power_kw": null}"#.into(),
        ])
        .replacen(HEADER, header, 1);
        let scenario = Scenario::parse(&text).expect("an export header loads");
        assert_eq!(scenario.header.source, Source::Export);
        assert_eq!(scenario.header.seed, None);
        assert_eq!(scenario.header.duration_s, 1_123_200);
        assert_eq!(scenario.header.stats["push"], 577_289);
        assert_eq!(scenario.lookup(0, 20).error, Some("8001"));
        assert_eq!(scenario.lookup(1, 20).error, Some("500"));
        assert_eq!(scenario.lookup(2, 20).error, Some("404"));
        let EventKind::Push(push) = &scenario.schedule[0].kind else {
            panic!("a push");
        };
        assert_eq!(
            push.fields.charge_power_kw,
            Some(None),
            "explicit null kept"
        );
    }

    #[test]
    fn readers_and_files() {
        // Windows line ends are fine. (Loading a real file is covered by the
        // integration tests, which have a directory of their own to write in.)
        let text = file(&[state("TESTVIN0000000001", 10, 1.0)]).replace('\n', "\r\n");
        assert_eq!(
            Scenario::from_reader(text.as_bytes())
                .unwrap()
                .timeline_len(),
            1
        );
        let missing = Scenario::load(Path::new("no such directory/missing.jsonl")).unwrap_err();
        assert_eq!(missing.line, 0);
        assert!(missing.to_string().contains("cannot open"));
        let bad_utf8: &[u8] = b"\xff\xfe\n";
        assert!(
            Scenario::from_reader(bad_utf8)
                .unwrap_err()
                .to_string()
                .contains("cannot read")
        );
    }
}
