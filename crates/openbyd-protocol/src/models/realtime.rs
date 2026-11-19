//! Vehicle realtime data model (`pybyd/models/realtime.py`).
//!
//! Enum values and field meanings are documented in pyBYD's `API_MAPPING.md`.

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::base::{
    self, BydModel, Fields, PyDateTime, PyEnum, enum_call, enum_field, py_equals_int, py_truthy,
};
use super::vehicle::EnergyType;

// The macro moved to `base`; this is its old path.
#[allow(unused_imports)]
pub(crate) use super::base::byd_enum;
use crate::coerce::{self, IntEnum, ValidationError, py_decimal, py_strip};
use crate::parity::{Args, Outcome, ParityError};
use crate::pyjson;

// ------------------------------------------------------------------
// Enums
// ------------------------------------------------------------------

byd_enum! {
    /// Vehicle online/offline state.
    OnlineState {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Online.
        Online = 1 => "ONLINE",
        /// Offline.
        Offline = 2 => "OFFLINE",
    }
}

byd_enum! {
    /// T-Box connection state.
    ConnectState {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Disconnected.
        Disconnected = 0 => "DISCONNECTED",
        /// Connected.
        Connected = 1 => "CONNECTED",
    }
}

byd_enum! {
    /// Vehicle power state.
    VehicleState {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Off.
        Off = 0 => "OFF",
        /// On (not confirmed).
        On = 2 => "ON",
    }
}

byd_enum! {
    /// Charging state indicator.
    ChargingState {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Not charging.
        NotCharging = 0 => "NOT_CHARGING",
        /// Charging.
        Charging = 1 => "CHARGING",
        /// Misnamed: 15 is the idle value, reported both unplugged and plugged
        /// in but not charging (Sealion 6, Sealion 7, ATTO 3 EVO; issue #20).
        Connected = 15 => "CONNECTED",
    }
}

byd_enum! {
    /// Sentry mode state.
    ///
    /// Confirmed via live toggle on a BYD Sealion 8 (AU region): ON observed
    /// as 2, OFF observed as 4. Only two states have been observed so far;
    /// any other raw value falls back to UNKNOWN via `_missing_`.
    SentryStatus {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Sentry mode on.
        On = 2 => "ON",
        /// Sentry mode off.
        Off = 4 => "OFF",
    }
}

byd_enum! {
    /// Unit used for tire pressure readings.
    TirePressureUnit {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Bar.
        Bar = 1 => "BAR",
        /// Pounds per square inch.
        Psi = 2 => "PSI",
        /// Kilopascal.
        Kpa = 3 => "KPA",
    }
}

byd_enum! {
    /// Door/trunk open/closed state.
    DoorOpenState {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Closed.
        Closed = 0 => "CLOSED",
        /// Open.
        Open = 1 => "OPEN",
    }
}

byd_enum! {
    /// Door lock state.
    ///
    /// `UNAVAILABLE` (0) is returned by the API when state is unknown.
    LockState {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// The API has no lock state.
        Unavailable = 0 => "UNAVAILABLE",
        /// Unlocked.
        Unlocked = 1 => "UNLOCKED",
        /// Locked.
        Locked = 2 => "LOCKED",
    }
}

byd_enum! {
    /// Window open/closed state.
    WindowState {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Closed.
        Closed = 1 => "CLOSED",
        /// Open.
        Open = 2 => "OPEN",
    }
}

byd_enum! {
    /// Vehicle power state.
    PowerGear {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Off.
        Off = 1 => "OFF",
        /// On.
        On = 3 => "ON",
    }
}

byd_enum! {
    /// Air circulation mode.
    AirCirculationMode {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Not available.
        Unavailable = 0 => "UNAVAILABLE",
        /// Outside air.
        External = 1 => "EXTERNAL",
        /// Recirculation.
        Internal = 2 => "INTERNAL",
    }
}

/// Steering wheel heating state.
///
/// Status values: `-1` = on, `1` = off, `0` = no data. Some models (ATTO 3
/// EVO) report on as `2` instead, on the seat-heat scale. Command values use
/// a different scale (see [`StearingWheelHeat::to_command_level`]).
///
/// `ON` occupies `-1`, so unmapped values resolve to `NO_DATA` rather than
/// the usual `UNKNOWN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StearingWheelHeat {
    /// Heating on.
    On = -1,
    /// No data.
    NoData = 0,
    /// Heating off.
    Off = 1,
}

impl StearingWheelHeat {
    /// The value to send in a seat-climate command. Command scale: `1` = on, `3` = off.
    pub fn to_command_level(self) -> i64 {
        if self == Self::On { 1 } else { 3 }
    }
}

impl IntEnum for StearingWheelHeat {
    const MEMBERS: &'static [Self] = &[Self::On, Self::NoData, Self::Off];
    /// Not used for fields, which go through [`steering_wheel_heat`]; see [`PyEnum::missing`].
    const MISSING: Option<Self> = Some(Self::NoData);
    fn value(self) -> i64 {
        self as i64
    }
}

impl PyEnum for StearingWheelHeat {
    fn name(self) -> &'static str {
        match self {
            Self::On => "ON",
            Self::NoData => "NO_DATA",
            Self::Off => "OFF",
        }
    }

    /// `_missing_`: `2` (the seat-heat scale's "on") is `ON`, anything else `NO_DATA`.
    fn missing(value: &Value) -> Self {
        if py_equals_int(value, 2) {
            Self::On
        } else {
            Self::NoData
        }
    }

    fn methods(self) -> Map<String, Value> {
        base::properties([("to_command_level", json!(self.to_command_level()))])
    }
}

/// A field typed `StearingWheelHeat`: pydantic reads the value as an `int`
/// and, when no member has it, calls `_missing_` with the *original* input,
/// so `2` and `2.0` are `ON` but `"2"` is `NO_DATA`.
pub fn steering_wheel_heat(value: &Value) -> Result<StearingWheelHeat, ValidationError> {
    let members: Vec<i64> = StearingWheelHeat::MEMBERS
        .iter()
        .map(|member| member.value())
        .collect();
    match coerce::int_enum_value(value, &members, None) {
        Ok(found) => Ok(StearingWheelHeat::call(&json!(found))),
        Err(error) if error.error_type == "enum" => Ok(StearingWheelHeat::missing(value)),
        Err(error) => Err(error),
    }
}

/// Seat heating / ventilation level.
///
/// `NO_DATA` (0) means the API has no information for this seat in the
/// current response; it does not indicate hardware absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeatHeatVentState {
    /// No mapped member.
    Unknown = -1,
    /// No data for this seat.
    NoData = 0,
    /// Off.
    Off = 1,
    /// Low.
    Low = 2,
    /// High.
    High = 3,
}

impl SeatHeatVentState {
    /// The value to send in a `set_seat_climate()` command.
    ///
    /// Command scale is inverted: HIGH=3 -> 1, LOW=2 -> 2, OFF=1 -> 3.
    /// `NO_DATA` and `UNKNOWN` map to `0` (no action).
    pub fn to_command_level(self) -> i64 {
        match self {
            Self::Unknown | Self::NoData => 0,
            Self::Off => 3,
            Self::Low => 2,
            Self::High => 1,
        }
    }
}

impl IntEnum for SeatHeatVentState {
    const MEMBERS: &'static [Self] = &[
        Self::Unknown,
        Self::NoData,
        Self::Off,
        Self::Low,
        Self::High,
    ];
    const MISSING: Option<Self> = Some(Self::Unknown);
    fn value(self) -> i64 {
        self as i64
    }
}

impl PyEnum for SeatHeatVentState {
    fn name(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::NoData => "NO_DATA",
            Self::Off => "OFF",
            Self::Low => "LOW",
            Self::High => "HIGH",
        }
    }

    fn methods(self) -> Map<String, Value> {
        base::properties([("to_command_level", json!(self.to_command_level()))])
    }
}

// ------------------------------------------------------------------
// Hybrid energy/consumption string splitting
// ------------------------------------------------------------------
//
// Several realtime fields combine EV and ICE data when the per-vehicle
// `EnergyType` is HYBRID. The shape varies per field:
//
//     EV                    ICE                  HYBRID (combined)
//     "6.1"                 "8.4"                "6.1+8.4"
//     "6.1kW·h/100km"       "3.5L/100km"         "6.1kW·h/100km+8.4L/100km"
//     "19.7kW·h/100km"      "3.5L/100km"         "(19.7kW·h+3.5L)/100km"
//     "10.1" (kW·h/100km)   "--"                 "10.1" (L/100km)  <- nearestEnergyConsumption
//
// `nearestEnergyConsumption` is special: even at HYBRID the cloud returns a
// single-leg value, with the unit field disambiguating which leg.

/// `(ev_value, ev_unit, fuel_value, fuel_unit)`.
type LegSplit = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

const NO_LEGS: LegSplit = (None, None, None, None);

/// `_SENTINELS` of this module (narrower than the base model's).
fn is_leg_sentinel(text: &str) -> bool {
    text.is_empty() || text == "--"
}

/// `_NUM_RE.match(text)` (`-?\d+(?:\.\d+)?`, where `\d` is any Unicode
/// decimal digit): the byte length of the match and its value as `float()`
/// reads it. Overflowing digit runs give infinity, as in Python.
fn match_number(text: &str) -> Option<(usize, f64)> {
    let mut chars = text.char_indices().peekable();
    let mut end = 0;
    if let Some(&(_, '-')) = chars.peek() {
        chars.next();
        end = 1;
    }
    let digits = |chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>, end: &mut usize| {
        let mut count = 0;
        while let Some(&(index, c)) = chars.peek() {
            if py_decimal(c).is_none() {
                break;
            }
            *end = index + c.len_utf8();
            chars.next();
            count += 1;
        }
        count
    };
    if digits(&mut chars, &mut end) == 0 {
        return None;
    }
    let mut after_point = text[end..].chars();
    if after_point.next() == Some('.')
        && after_point.next().is_some_and(|c| py_decimal(c).is_some())
    {
        chars.next();
        end += 1;
        digits(&mut chars, &mut end);
    }
    let matched = &text[..end];
    let value = if matched.is_ascii() {
        matched.parse()
    } else {
        // Other Unicode digits read as their ASCII values.
        matched
            .chars()
            .map(|c| py_decimal(c).map_or(c, |digit| char::from(b'0' + digit as u8)))
            .collect::<String>()
            .parse()
    };
    Some((end, value.expect("digits with an optional point parse")))
}

/// `_first_num`: the signed decimal `text` starts with, as a float.
///
/// Anchored like [`extract_unit`]: a leg that carries a unit but no number
/// (`"kW·h/100km"`) must not read as the `100` of its unit.
fn first_num(text: Option<&str>) -> Option<f64> {
    let text = text.filter(|text| !text.is_empty())?;
    match_number(py_strip(text)).map(|(_, value)| value)
}

/// `_extract_unit`: the canonical unit trailing the leading signed decimal.
///
/// A `--` in place of the number is skipped, so `"--kW·h/100km"` still
/// reads as `kWh/100km`.
fn extract_unit(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let cleaned = py_strip(text);
    let tail = match match_number(cleaned) {
        Some((end, _)) => &cleaned[end..],
        None => cleaned.trim_start_matches('-'),
    };
    base::normalize_unit(Some(py_strip(tail))).unwrap_or_default()
}

/// `_quantity`: the quantity of a canonical unit: `kWh` (EV), `L` (fuel) or other.
fn quantity(unit: &str) -> &str {
    unit.split_once('/').map_or(unit, |(quantity, _)| quantity)
}

/// `_unit_or_default`: `unit`, completed from `default` when it names no quantity.
///
/// A numeric leg has no unit (`"6.1"`) or only the shared distance
/// (`"(6.1+8.4)/100km"` splits into `"6.1/100km"`).
fn unit_or_default(unit: &str, default: &str) -> String {
    if unit.starts_with('/') {
        format!("{}{unit}", quantity(default))
    } else if unit.is_empty() {
        default.to_owned()
    } else {
        unit.to_owned()
    }
}

/// `_classify_two_legs`: annotates two leg strings with their units, EV leg first.
///
/// A leg's unit beats its position: the legs are swapped when a unit shows
/// the fuel leg first (the first in L, or the second in kWh), unless the
/// other unit says they are already in place.
fn classify_two_legs(ev: String, fuel: String, default_ev: &str, default_fuel: &str) -> LegSplit {
    let (mut ev, mut fuel) = (ev, fuel);
    let mut ev_unit = extract_unit(&ev);
    let mut fuel_unit = extract_unit(&fuel);
    let (ev_quantity, fuel_quantity) = (quantity(&ev_unit), quantity(&fuel_unit));
    let fuel_first = ev_quantity == "L" || fuel_quantity == "kWh";
    let in_place = ev_quantity == "kWh" || fuel_quantity == "L";
    if fuel_first && !in_place {
        std::mem::swap(&mut ev, &mut fuel);
        std::mem::swap(&mut ev_unit, &mut fuel_unit);
    }
    (
        Some(ev),
        Some(unit_or_default(&ev_unit, default_ev)),
        Some(fuel),
        Some(unit_or_default(&fuel_unit, default_fuel)),
    )
}

/// `_split_combined_string`: splits a combined-form consumption string into
/// per-leg `(value, unit)` pairs, `(ev_value, ev_unit, fuel_value, fuel_unit)`.
///
/// Handles `"(EV+Fuel)/<shared-unit>"`, `"EV+Fuel"` with units inline or
/// numeric only, and a single leg classified by its unit or `energy_type`.
/// Units beat position and `energy_type`.
fn split_combined_string(
    raw: &Value,
    energy_type: EnergyType,
    default_ev: &str,
    default_fuel: &str,
) -> LegSplit {
    if raw.is_null() {
        return NO_LEGS;
    }
    let printed = pyjson::py_str(raw);
    let text = py_strip(&printed);
    if is_leg_sentinel(text) {
        return NO_LEGS;
    }
    if let Some(after_open) = text.strip_prefix('(')
        && let Some(close) = after_open.find(')')
    {
        let inside = &after_open[..close];
        let suffix = py_strip(py_strip(&after_open[close + 1..]).trim_start_matches('/'));
        if let Some((left, right)) = inside.split_once('+') {
            let (left, right) = (py_strip(left), py_strip(right));
            let leg = |part: &str| {
                if suffix.is_empty() {
                    part.to_owned()
                } else {
                    format!("{part}/{suffix}")
                }
            };
            return classify_two_legs(leg(left), leg(right), default_ev, default_fuel);
        }
    }
    if let Some((left, right)) = text.split_once('+') {
        return classify_two_legs(
            py_strip(left).to_owned(),
            py_strip(right).to_owned(),
            default_ev,
            default_fuel,
        );
    }
    let inline_unit = extract_unit(text);
    let text = text.to_owned();
    match quantity(&inline_unit) {
        "L" => (None, None, Some(text), Some(inline_unit)),
        "kWh" => (Some(text), Some(inline_unit), None, None),
        _ if energy_type == EnergyType::Ice => (
            None,
            None,
            Some(text),
            Some(unit_or_default(&inline_unit, default_fuel)),
        ),
        _ => (
            Some(text),
            Some(unit_or_default(&inline_unit, default_ev)),
            None,
            None,
        ),
    }
}

/// `_legacy_leg_alias`: the value for the legacy non-suffixed field.
///
/// The EV leg when present. For HYBRID where the response carries only the
/// petrol leg, the fuel leg, so legacy consumers still see some data.
/// Pure ICE deliberately does not fall back.
fn legacy_leg_alias<T>(ev: Option<T>, fuel: Option<T>, energy_type: EnergyType) -> Option<T> {
    if ev.is_some() {
        ev
    } else if energy_type == EnergyType::Hybrid {
        fuel
    } else {
        None
    }
}

/// `_split_nearest_energy_string`: resolves `nearestEnergyConsumption` with
/// its paired unit field, which is authoritative for which leg it is.
fn split_nearest_energy_string(
    value: Option<&Value>,
    unit: Option<&Value>,
    energy_type: EnergyType,
) -> LegSplit {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return NO_LEGS;
    };
    let printed = pyjson::py_str(value);
    let text = py_strip(&printed);
    if is_leg_sentinel(text) {
        return NO_LEGS;
    }
    let unit_printed = match unit {
        Some(unit) if py_truthy(unit) => pyjson::py_str(unit),
        _ => String::new(),
    };
    let mut unit_text = py_strip(&unit_printed);
    if is_leg_sentinel(unit_text) {
        unit_text = "";
    }
    let unit_text = base::normalize_unit(Some(unit_text)).unwrap_or_default();
    let text = Some(text.to_owned());
    match quantity(&unit_text) {
        "L" => (None, None, text, Some(unit_text)),
        "kWh" => (text, Some(unit_text), None, None),
        _ if energy_type == EnergyType::Ice => (None, None, text, Some(unit_text)),
        _ => (text, Some(unit_text), None, None),
    }
}

/// A float as the before-validator puts it into the input dict. JSON has no
/// infinity, so an overflowing number is stored as the text `"inf"`, which
/// every later step (sentinel stripping, `float` fields, `str()`) reads as
/// Python reads the float itself.
fn float_value(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or_else(
        || json!(if value > 0.0 { "inf" } else { "-inf" }),
        Value::Number,
    )
}

/// The before-validator's working dict: the input with the keys it adds or
/// replaces kept apart, so the input is never copied.
struct Layered<'a> {
    input: &'a Map<String, Value>,
    changes: Vec<(&'static str, Value)>,
}

impl Layered<'_> {
    fn get(&self, key: &str) -> Option<&Value> {
        self.changes
            .iter()
            .find(|(changed, _)| *changed == key)
            .map(|(_, value)| value)
            .or_else(|| self.input.get(key))
    }

    fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    fn insert(&mut self, key: &'static str, value: Value) {
        match self.changes.iter_mut().find(|(changed, _)| *changed == key) {
            Some((_, slot)) => *slot = value,
            None => self.changes.push((key, value)),
        }
    }
}

/// `dict.setdefault(key, value)`.
fn set_default(values: &mut Layered<'_>, key: &'static str, value: Value) {
    if !values.contains_key(key) {
        values.insert(key, value);
    }
}

/// `(raw_key, ev_key, ev_unit_key, fuel_key, fuel_unit_key)`; the default
/// units are `kWh/100km` and `L/100km` for all of them.
const SPLITS: [(&str, &str, &str, &str, &str); 5] = [
    (
        "energyConsumption",
        "energy_consumption_ev",
        "energy_consumption_ev_unit",
        "energy_consumption_fuel",
        "energy_consumption_fuel_unit",
    ),
    (
        "recent50kmEnergy",
        "recent_50km_energy_ev",
        "recent_50km_energy_ev_unit",
        "recent_50km_energy_fuel",
        "recent_50km_energy_fuel_unit",
    ),
    (
        "totalEnergy",
        "total_energy_ev",
        "total_energy_ev_unit",
        "total_energy_fuel",
        "total_energy_fuel_unit",
    ),
    (
        "totalConsumption",
        "total_consumption_ev",
        "total_consumption_ev_unit",
        "total_consumption_fuel",
        "total_consumption_fuel_unit",
    ),
    (
        "totalConsumptionEn",
        "total_consumption_en_ev",
        "total_consumption_en_ev_unit",
        "total_consumption_en_fuel",
        "total_consumption_en_fuel_unit",
    ),
];

const DEFAULT_EV_UNIT: &str = "kWh/100km";
const DEFAULT_FUEL_UNIT: &str = "L/100km";

/// The `energy_type` the before-validator reads from the validation context.
///
/// pyBYD takes `info.context["energy_type"]` only when it is an `EnergyType`
/// member, and EV otherwise. JSON cannot carry a Python enum, so a parity
/// context spells a member as `{"energy_type": {"EnergyType": <value>}}`
/// (which the oracle turns into `EnergyType(<value>)`); any other value is
/// passed through as is and so reads as EV. A truthy context that is not a
/// dict makes pyBYD's `ctx.get` raise `AttributeError`.
pub fn context_energy_type(context: &Value) -> Result<EnergyType, ValidationError> {
    if !py_truthy(context) {
        return Ok(EnergyType::Ev);
    }
    let Value::Object(context) = context else {
        return Err(ValidationError::raised("AttributeError", "context_get"));
    };
    Ok(match context.get("energy_type") {
        Some(Value::Object(member)) if member.len() == 1 => match member.get("EnergyType") {
            Some(value) => EnergyType::call(value),
            None => EnergyType::Ev,
        },
        _ => EnergyType::Ev,
    })
}

/// `_split_hybrid_legs`, the model's before-validator.
///
/// Populates the `_ev`/`_ev_unit`/`_fuel`/`_fuel_unit` per-leg fields and
/// rebinds the original (non-suffixed) field to its EV-leg value. The
/// original combined string stays in `raw`, which is stashed here before
/// anything is rebound.
///
/// Returns the keys it adds or replaces; [`Fields::cleaned_with_changes`]
/// lays them over the input, which is also what `raw` gets.
fn split_hybrid_legs(
    original: &Map<String, Value>,
    context: &Value,
) -> Result<Vec<(&'static str, Value)>, ValidationError> {
    let mut values = Layered {
        input: original,
        changes: Vec::new(),
    };
    let energy_type = context_energy_type(context)?;

    for (raw_key, ev_key, ev_unit_key, fuel_key, fuel_unit_key) in SPLITS {
        let Some(raw) = values.get(raw_key).cloned() else {
            continue;
        };
        let (ev, ev_unit, fuel, fuel_unit) =
            split_combined_string(&raw, energy_type, DEFAULT_EV_UNIT, DEFAULT_FUEL_UNIT);
        let ev_number = first_num(ev.as_deref());
        let fuel_number = first_num(fuel.as_deref());
        if let Some(number) = ev_number {
            set_default(&mut values, ev_key, float_value(number));
            if let Some(unit) = ev_unit.filter(|unit| !unit.is_empty()) {
                set_default(&mut values, ev_unit_key, json!(unit));
            }
        }
        if let Some(number) = fuel_number {
            set_default(&mut values, fuel_key, float_value(number));
            if let Some(unit) = fuel_unit.filter(|unit| !unit.is_empty()) {
                set_default(&mut values, fuel_unit_key, json!(unit));
            }
        }
        // Backwards compatibility: the legacy field holds the EV leg (the
        // fuel leg for a petrol-only hybrid reply), or None when neither leg
        // has a number.
        let legacy = if ev_number.is_some() || fuel_number.is_some() {
            legacy_leg_alias(ev, fuel, energy_type)
        } else {
            None
        };
        values.insert(raw_key, json!(legacy));
    }

    if values.contains_key("nearestEnergyConsumption") {
        // The raw single-number "recent consumption" summary, captured
        // before the legacy field is rebound.
        let summary = values
            .get("nearestEnergyConsumption")
            .filter(|value| py_truthy(value))
            .map_or_else(String::new, pyjson::py_str);
        if let Some(number) = first_num(Some(summary.as_str())) {
            set_default(&mut values, "eq_consumption", float_value(number));
            if let Some(unit) = values
                .get("nearestEnergyConsumptionUnit")
                .filter(|unit| py_truthy(unit))
            {
                let unit = base::normalize_unit(Some(&pyjson::py_str(unit)));
                set_default(&mut values, "eq_consumption_unit", json!(unit));
            }
        }
        let (ev, ev_unit, fuel, fuel_unit) = split_nearest_energy_string(
            values.get("nearestEnergyConsumption"),
            values.get("nearestEnergyConsumptionUnit"),
            energy_type,
        );
        let (mut ev, mut fuel) = (ev, fuel);
        let mut ev_unit = ev_unit.map(Value::String);
        let mut fuel_unit = fuel_unit.map(Value::String);
        // At HYBRID the cloud repurposes nearestEnergyConsumption for the
        // equivalent-petrol consumption; the real nearest EV/fuel averages
        // are the energyConsumption legs split above.
        if energy_type == EnergyType::Hybrid {
            let present = |key: &str| values.get(key).filter(|value| !value.is_null());
            let unit_or = |key: &str, default: &str| {
                present(key)
                    .filter(|unit| py_truthy(unit))
                    .cloned()
                    .unwrap_or_else(|| json!(default))
            };
            if let Some(number) = present("energy_consumption_ev") {
                ev = Some(pyjson::py_str(number));
                ev_unit = Some(unit_or("energy_consumption_ev_unit", DEFAULT_EV_UNIT));
            }
            if let Some(number) = present("energy_consumption_fuel") {
                fuel = Some(pyjson::py_str(number));
                fuel_unit = Some(unit_or("energy_consumption_fuel_unit", DEFAULT_FUEL_UNIT));
            }
        }
        let ev_number = first_num(ev.as_deref());
        let fuel_number = first_num(fuel.as_deref());
        if let Some(number) = ev_number {
            set_default(
                &mut values,
                "nearest_energy_consumption_ev",
                float_value(number),
            );
            if let Some(unit) = ev_unit.clone().filter(py_truthy) {
                set_default(&mut values, "nearest_energy_consumption_ev_unit", unit);
            }
        }
        if let Some(number) = fuel_number {
            set_default(
                &mut values,
                "nearest_energy_consumption_fuel",
                float_value(number),
            );
            if let Some(unit) = fuel_unit.clone().filter(py_truthy) {
                set_default(&mut values, "nearest_energy_consumption_fuel_unit", unit);
            }
        }
        values.insert(
            "nearestEnergyConsumption",
            json!(legacy_leg_alias(ev, fuel, energy_type)),
        );
        values.insert(
            "nearestEnergyConsumptionUnit",
            legacy_leg_alias(ev_unit, fuel_unit, energy_type).unwrap_or(Value::Null),
        );
    }

    Ok(values.changes)
}

// ------------------------------------------------------------------
// The model
// ------------------------------------------------------------------

/// Key aliases: BYD API key -> canonical camelCase key.
const KEY_ALIASES: &[(&str, &str)] = &[
    ("stearingWheelHeatState", "steeringWheelHeatState"),
    ("backCover", "trunkLid"),
    ("leftFrontTirepressure", "leftFrontTirePressure"),
    ("rightFrontTirepressure", "rightFrontTirePressure"),
    ("leftRearTirepressure", "leftRearTirePressure"),
    ("rightRearTirepressure", "rightRearTirePressure"),
    ("abs", "absWarning"),
    ("time", "timestamp"),
    ("recent50kmEnergy", "recent50KmEnergy"),
];

/// Realtime telemetry data for a vehicle.
///
/// Numeric fields are `None` when the value is absent or unparseable from
/// the API response. All original data is available in the `raw` dict.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VehicleRealtimeData {
    /// Original API response dict.
    pub raw: Map<String, Value>,

    // --- Connection & state ---
    /// Online/offline state.
    #[serde(serialize_with = "base::ser_int_enum")]
    pub online_state: OnlineState,
    /// T-Box connection state.
    #[serde(serialize_with = "base::ser_int_enum")]
    pub connect_state: ConnectState,
    /// Vehicle power state.
    #[serde(serialize_with = "base::ser_int_enum")]
    pub vehicle_state: VehicleState,
    /// Serial of the request that produced this reply, if any.
    pub request_serial: Option<String>,

    // --- Battery & range ---
    /// Battery state of charge (0-100 %).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub elec_percent: Option<f64>,
    /// Alternative battery percentage field.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub power_battery: Option<f64>,
    /// Estimated remaining EV range (km).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub endurance_mileage: Option<f64>,
    /// Alternative EV range field.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub ev_endurance: Option<f64>,
    /// EV range in [`Self::endurance_mileage_v2_unit`].
    #[serde(serialize_with = "base::ser_opt_float")]
    pub endurance_mileage_v2: Option<f64>,
    /// Unit of [`Self::endurance_mileage_v2`].
    pub endurance_mileage_v2_unit: Option<String>,
    /// Odometer reading (km).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_mileage: Option<f64>,
    /// Odometer reading in [`Self::total_mileage_v2_unit`].
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_mileage_v2: Option<f64>,
    /// Unit of [`Self::total_mileage_v2`].
    pub total_mileage_v2_unit: Option<String>,

    // --- Driving ---
    /// Current speed (km/h).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub speed: Option<f64>,
    /// Vehicle power state (off / on).
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub power_gear: Option<PowerGear>,

    // --- Climate ---
    /// Interior temperature (deg C). Sentinel `-129` is normalised to `None`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub temp_in_car: Option<f64>,
    /// Driver-side set temperature on BYD scale (1-17).
    pub main_setting_temp: Option<i64>,
    /// Driver-side set temperature (deg C, precise).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub main_setting_temp_new: Option<f64>,
    /// Air circulation mode.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub air_run_state: Option<AirCirculationMode>,

    // --- Seat heating/ventilation ---
    /// Driver seat heating.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub main_seat_heat_state: Option<SeatHeatVentState>,
    /// Driver seat ventilation.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub main_seat_ventilation_state: Option<SeatHeatVentState>,
    /// Passenger seat heating.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub copilot_seat_heat_state: Option<SeatHeatVentState>,
    /// Passenger seat ventilation.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub copilot_seat_ventilation_state: Option<SeatHeatVentState>,
    /// Steering wheel heating (BYD's key is spelled `stearingWheelHeatState`).
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub steering_wheel_heat_state: Option<StearingWheelHeat>,
    /// Rear left seat heating.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub lr_seat_heat_state: Option<SeatHeatVentState>,
    /// Rear left seat ventilation.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub lr_seat_ventilation_state: Option<SeatHeatVentState>,
    /// Rear right seat heating.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub rr_seat_heat_state: Option<SeatHeatVentState>,
    /// Rear right seat ventilation.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub rr_seat_ventilation_state: Option<SeatHeatVentState>,

    // --- Charging ---
    /// Charging field from the realtime payload.
    ///
    /// May remain `UNKNOWN` while `charge_state` carries the authoritative
    /// status. Prefer [`Self::effective_charging_state`].
    #[serde(serialize_with = "base::ser_int_enum")]
    pub charging_state: ChargingState,
    /// Authoritative realtime charging state.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub charge_state: Option<ChargingState>,
    /// Charge wait status.
    pub wait_status: Option<i64>,
    /// Estimated hours to full charge. Sentinel `-1` -> `None`.
    pub full_hour: Option<i64>,
    /// Estimated minutes to full charge. Sentinel `-1` -> `None`.
    pub full_minute: Option<i64>,
    /// Remaining hours component. Sentinel `-1` -> `None`.
    pub remaining_hours: Option<i64>,
    /// Remaining minutes component. Sentinel `-1` -> `None`.
    pub remaining_minutes: Option<i64>,
    /// Scheduled charging state (0=off).
    pub booking_charge_state: Option<i64>,
    /// Scheduled charge start hour.
    pub booking_charging_hour: Option<i64>,
    /// Scheduled charge start minute.
    pub booking_charging_minute: Option<i64>,

    // --- Doors ---
    /// Left front door.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub left_front_door: Option<DoorOpenState>,
    /// Right front door.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub right_front_door: Option<DoorOpenState>,
    /// Left rear door.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub left_rear_door: Option<DoorOpenState>,
    /// Right rear door.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub right_rear_door: Option<DoorOpenState>,
    /// Trunk lid (BYD also sends it as `backCover`).
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub trunk_lid: Option<DoorOpenState>,
    /// Sliding door.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub sliding_door: Option<DoorOpenState>,
    /// Front trunk / frunk.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub forehold: Option<DoorOpenState>,

    // --- Locks ---
    /// Left front door lock.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub left_front_door_lock: Option<LockState>,
    /// Right front door lock.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub right_front_door_lock: Option<LockState>,
    /// Left rear door lock.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub left_rear_door_lock: Option<LockState>,
    /// Right rear door lock.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub right_rear_door_lock: Option<LockState>,
    /// Sliding door lock.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub sliding_door_lock: Option<LockState>,

    // --- Windows ---
    /// Left front window.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub left_front_window: Option<WindowState>,
    /// Right front window.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub right_front_window: Option<WindowState>,
    /// Left rear window.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub left_rear_window: Option<WindowState>,
    /// Right rear window.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub right_rear_window: Option<WindowState>,
    /// Sunroof.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub skylight: Option<WindowState>,

    // --- Window opening percentage (0-100) ---
    // Sealion 7 EU reports 0 when closed and 10 after the OPEN_WINDOWS vent
    // command; full drop reports 100. Other trims may not populate.
    /// Left front window opening (0-100 %).
    pub left_front_window_pct: Option<i64>,
    /// Right front window opening (0-100 %).
    pub right_front_window_pct: Option<i64>,
    /// Left rear window opening (0-100 %).
    pub left_rear_window_pct: Option<i64>,
    /// Right rear window opening (0-100 %).
    pub right_rear_window_pct: Option<i64>,

    // --- Tire pressure ---
    /// Left front tire pressure, in [`Self::tire_press_unit`].
    #[serde(serialize_with = "base::ser_opt_float")]
    pub left_front_tire_pressure: Option<f64>,
    /// Right front tire pressure.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub right_front_tire_pressure: Option<f64>,
    /// Left rear tire pressure.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub left_rear_tire_pressure: Option<f64>,
    /// Right rear tire pressure.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub right_rear_tire_pressure: Option<f64>,
    /// Left front tire status. Sentinel `-1` -> `None`.
    pub left_front_tire_status: Option<i64>,
    /// Right front tire status. Sentinel `-1` -> `None`.
    pub right_front_tire_status: Option<i64>,
    /// Left rear tire status. Sentinel `-1` -> `None`.
    pub left_rear_tire_status: Option<i64>,
    /// Right rear tire status. Sentinel `-1` -> `None`.
    pub right_rear_tire_status: Option<i64>,
    /// Pressure unit: BAR (1), PSI (2), KPA (3).
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub tire_press_unit: Option<TirePressureUnit>,
    /// TPMS system state. 0=normal, >0=warning. Sentinel `-1` -> `None`.
    pub tirepressure_system: Option<i64>,
    /// Rapid tire leak indicator. 0=no leak, >0=leak. Sentinel `-1` -> `None`.
    pub rapid_tire_leak: Option<i64>,

    // --- Energy consumption ---
    /// Unmapped; semantics unconfirmed. Seen `0.0` while parked; not yet
    /// observed non-zero. Do not assume this is a cumulative energy counter.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_power: Option<f64>,
    /// Instantaneous battery power (W).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub gl: Option<f64>,
    /// Lifetime AVERAGE EV consumption rate, kWh/100km; despite the name
    /// not an absolute energy total. Split into [`Self::total_energy_ev`] /
    /// [`Self::total_energy_fuel`]; this field keeps the EV leg's text.
    pub total_energy: Option<String>,
    /// Nearest energy consumption (the EV leg's text; see the `_ev`/`_fuel` fields).
    pub nearest_energy_consumption: Option<String>,
    /// Unit of [`Self::nearest_energy_consumption`].
    pub nearest_energy_consumption_unit: Option<String>,
    /// AVERAGE EV consumption rate over the last 50km, kWh/100km; not the
    /// absolute energy used over that window.
    pub recent_50km_energy: Option<String>,

    // --- Fuel (hybrid vehicles) ---
    /// Fuel-based range (km). Sentinel `-1` -> `None`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub oil_endurance: Option<f64>,
    /// Fuel percentage. Sentinel `-1` -> `None`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub oil_percent: Option<f64>,
    /// Total fuel consumption.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_oil: Option<f64>,

    // --- System indicators ---
    /// Power system warning. 0=normal, >0=warning.
    pub power_system: Option<i64>,
    /// Engine status.
    pub engine_status: Option<i64>,
    /// Electronic parking brake. 0=released, 1=engaged.
    pub epb: Option<i64>,
    /// Electric power steering warning. 0=normal, >0=warning.
    pub eps: Option<i64>,
    /// Electronic stability program warning. 0=normal, >0=warning.
    pub esp: Option<i64>,
    /// ABS warning. 0=normal, >0=warning.
    pub abs_warning: Option<i64>,
    /// Service vehicle soon. 0=normal, >0=warning.
    pub svs: Option<i64>,
    /// Supplemental restraint system (airbag) warning. 0=normal, >0=warning.
    pub srs: Option<i64>,
    /// Engine coolant temperature warning. 0=normal, >0=warning.
    pub ect: Option<i64>,
    /// Engine coolant temperature (deg C).
    pub ect_value: Option<i64>,
    /// Power warning. 0=normal, >0=warning.
    pub pwr: Option<i64>,

    // --- Feature states ---
    /// Sentry/dashcam mode. See [`SentryStatus`].
    #[serde(serialize_with = "base::ser_int_enum")]
    pub sentry_status: SentryStatus,
    /// Battery heating state.
    pub battery_heat_state: Option<i64>,
    /// Charge heating state.
    pub charge_heat_state: Option<i64>,
    /// OTA upgrade status. 0=none, >0=active.
    pub upgrade_status: Option<i64>,

    // --- Third-row seats ---
    /// Third-row left seat heat state.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub lr_third_heat_state: Option<SeatHeatVentState>,
    /// Third-row left seat ventilation state.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub lr_third_ventilation_state: Option<SeatHeatVentState>,
    /// Third-row right seat heat state.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub rr_third_heat_state: Option<SeatHeatVentState>,
    /// Third-row right seat ventilation state.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub rr_third_ventilation_state: Option<SeatHeatVentState>,

    // --- Charging (extended) ---
    /// Battery power in kW, whole numbers. Negative while charging, positive
    /// while discharging. Sentinel `-999` (or below) -> `None`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub rate: Option<f64>,
    /// Charging power while charging, in [`Self::charge_power_unit`]. `None` when not charging.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub charge_power: Option<f64>,
    /// Unit for [`Self::charge_power`] (`"kW"` observed).
    pub charge_power_unit: Option<String>,
    /// Time-to-full is less than one minute.
    pub less_one_min: Option<bool>,

    // --- Energy (extended) ---
    // Every field here is a CONSUMPTION RATE (kWh/100km, or L/100km for the
    // fuel leg), never an accumulated amount.
    /// Recent EV consumption rate, kWh/100km (the EV leg's text).
    pub energy_consumption: Option<String>,
    /// Chinese-locale lifetime average consumption rate, kWh/100km.
    pub total_consumption: Option<String>,
    /// English-locale lifetime average consumption rate, kWh/100km (e.g. `16.6kW·h/100km`).
    pub total_consumption_en: Option<String>,

    // --- Hybrid leg breakouts (parsed from combined strings) ---
    // The *_unit fields hold the canonical spelling (`normalize_unit`).
    /// EV leg of `energyConsumption`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub energy_consumption_ev: Option<f64>,
    /// Unit of [`Self::energy_consumption_ev`].
    pub energy_consumption_ev_unit: Option<String>,
    /// Fuel leg of `energyConsumption`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub energy_consumption_fuel: Option<f64>,
    /// Unit of [`Self::energy_consumption_fuel`].
    pub energy_consumption_fuel_unit: Option<String>,

    /// EV leg of `nearestEnergyConsumption`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub nearest_energy_consumption_ev: Option<f64>,
    /// Unit of [`Self::nearest_energy_consumption_ev`].
    pub nearest_energy_consumption_ev_unit: Option<String>,
    /// Fuel leg of `nearestEnergyConsumption`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub nearest_energy_consumption_fuel: Option<f64>,
    /// Unit of [`Self::nearest_energy_consumption_fuel`].
    pub nearest_energy_consumption_fuel_unit: Option<String>,

    /// EV leg of `recent50kmEnergy`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub recent_50km_energy_ev: Option<f64>,
    /// Unit of [`Self::recent_50km_energy_ev`].
    pub recent_50km_energy_ev_unit: Option<String>,
    /// Fuel leg of `recent50kmEnergy`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub recent_50km_energy_fuel: Option<f64>,
    /// Unit of [`Self::recent_50km_energy_fuel`].
    pub recent_50km_energy_fuel_unit: Option<String>,

    /// EV leg of `totalEnergy`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_energy_ev: Option<f64>,
    /// Unit of [`Self::total_energy_ev`].
    pub total_energy_ev_unit: Option<String>,
    /// Fuel leg of `totalEnergy`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_energy_fuel: Option<f64>,
    /// Unit of [`Self::total_energy_fuel`].
    pub total_energy_fuel_unit: Option<String>,

    /// EV leg of `totalConsumption`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_consumption_ev: Option<f64>,
    /// Unit of [`Self::total_consumption_ev`].
    pub total_consumption_ev_unit: Option<String>,
    /// Fuel leg of `totalConsumption`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_consumption_fuel: Option<f64>,
    /// Unit of [`Self::total_consumption_fuel`].
    pub total_consumption_fuel_unit: Option<String>,

    /// EV leg of `totalConsumptionEn`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_consumption_en_ev: Option<f64>,
    /// Unit of [`Self::total_consumption_en_ev`].
    pub total_consumption_en_ev_unit: Option<String>,
    /// Fuel leg of `totalConsumptionEn`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_consumption_en_fuel: Option<f64>,
    /// Unit of [`Self::total_consumption_en_fuel`].
    pub total_consumption_en_fuel_unit: Option<String>,

    /// The raw single-number "recent consumption" summary from
    /// `nearestEnergyConsumption`, before the legacy field is rebound:
    /// average EV (pure EV), average petrol (ICE) or equivalent petrol (hybrid).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub eq_consumption: Option<f64>,
    /// Unit of [`Self::eq_consumption`].
    pub eq_consumption_unit: Option<String>,

    // --- Fuel (extended) ---
    /// Oil pressure warning. 0=normal.
    pub oil_pressure_system: Option<i64>,

    // --- Warnings (extended) ---
    /// Braking system warning. 0=normal.
    pub braking_system: Option<i64>,
    /// Charging system warning. 0=normal.
    pub charging_system: Option<i64>,
    /// Steering system warning. 0=normal.
    pub steering_system: Option<i64>,
    /// OK/ready indicator. 0=off.
    pub ok_light: Option<i64>,

    // --- Features (extended) ---
    /// Repair/service mode. `'0'`=off.
    pub repair_mode_switch: Option<String>,

    // --- Misc ---
    /// Vehicle timezone (e.g. `Europe/Rome`).
    pub vehicle_time_zone: Option<String>,
    /// Battery connectivity indicator. -1=unknown, 0=disconnected.
    pub power_battery_connection: Option<i64>,
    /// Unknown indicator field.
    pub ins: Option<i64>,

    // --- Metadata ---
    /// Data timestamp (parsed to UTC datetime).
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub timestamp: Option<PyDateTime>,
}

/// `ect_value`'s sentinel rule: the temperature sentinel `-129` or any negative.
fn is_ect_sentinel(value: i64) -> bool {
    base::is_temp_sentinel(value) || base::is_negative(value)
}

/// `rate`'s sentinel rule: `-999` (idle) or below.
fn is_rate_sentinel(value: f64) -> bool {
    value <= -999.0
}

/// Python's `float(value)` for a JSON value, as `is_ready_raw` uses it.
fn py_float(value: &Value) -> Result<f64, ValidationError> {
    match value {
        Value::Bool(flag) => Ok(f64::from(u8::from(*flag))),
        Value::Number(number) => Ok(number.as_f64().unwrap_or(f64::NAN)),
        Value::String(text) => pyjson::py_float_from_str(text)
            .ok_or_else(|| ValidationError::raised("ValueError", "float")),
        _ => Err(ValidationError::raised("TypeError", "float")),
    }
}

/// Whether Python's `int(value)` is positive, as `is_ready_raw` uses it.
fn py_int_is_positive(value: &Value) -> Result<bool, ValidationError> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        Value::Number(number) => Ok(match number.as_i64() {
            Some(int) => int > 0,
            None if number.is_u64() => true,
            None => number.as_f64().is_some_and(|float| float >= 1.0),
        }),
        Value::String(text) => match pyjson::py_int_from_str(text) {
            Some(int) => Ok(int > 0),
            // Beyond i128: still an integer if it has nothing but digits.
            None if py_strip(text)
                .chars()
                .all(|c| py_decimal(c).is_some() || matches!(c, '_' | '+' | '-')) =>
            {
                pyjson::py_float_from_str(text)
                    .map(|float| float > 0.0)
                    .ok_or_else(|| ValidationError::raised("ValueError", "int"))
            }
            None => Err(ValidationError::raised("ValueError", "int")),
        },
        _ => Err(ValidationError::raised("TypeError", "int")),
    }
}

/// `value or 0`.
fn or_zero(value: Option<&Value>) -> Value {
    value
        .filter(|value| py_truthy(value))
        .cloned()
        .unwrap_or(json!(0))
}

impl VehicleRealtimeData {
    /// `is_ready_raw`: whether a raw realtime payload appears to contain meaningful data.
    ///
    /// The error is the Python exception pyBYD lets escape (`ValueError` for
    /// text `float()`/`int()` cannot read, `TypeError` for a list or dict,
    /// `AttributeError` for a truthy payload that is not a dict).
    pub fn is_ready_raw(vehicle_info: &Value) -> Result<bool, ValidationError> {
        if !py_truthy(vehicle_info) {
            return Ok(false);
        }
        let Value::Object(info) = vehicle_info else {
            return Err(ValidationError::raised("AttributeError", "get"));
        };
        if info
            .get("onlineState")
            .is_some_and(|state| py_equals_int(state, OnlineState::Offline.value()))
        {
            return Ok(false);
        }
        for key in [
            "leftFrontTirepressure",
            "rightFrontTirepressure",
            "leftRearTirepressure",
            "rightRearTirepressure",
        ] {
            if py_float(&or_zero(info.get(key)))? > 0.0 {
                return Ok(true);
            }
        }
        if py_int_is_positive(&or_zero(info.get("time")))? {
            return Ok(true);
        }
        Ok(py_float(&or_zero(info.get("enduranceMileage")))? > 0.0)
    }

    /// Whether the vehicle is reporting as online.
    pub fn is_online(&self) -> bool {
        self.online_state == OnlineState::Online
    }

    /// Canonical charging state derived from the realtime payload.
    /// `charge_state` is the authoritative source.
    pub fn effective_charging_state(&self) -> ChargingState {
        self.charge_state.unwrap_or(ChargingState::Unknown)
    }

    /// Whether the vehicle is currently charging.
    pub fn is_charging(&self) -> bool {
        self.effective_charging_state() == ChargingState::Charging
    }

    /// Whether the charging gun is connected, as far as realtime can tell.
    ///
    /// `true` while charging, otherwise `None`: `charge_state` reads `15`
    /// both unplugged and plugged in but idle, and `connect_state` stays
    /// `-1`. Use `ChargingStatus::is_connected` for plug state.
    pub fn is_charger_connected(&self) -> Option<bool> {
        self.is_charging().then_some(true)
    }

    /// Total estimated minutes until fully charged.
    ///
    /// `None` when either component is unavailable, and also when the sum
    /// does not fit in `i64` (Python would return a larger integer).
    pub fn time_to_full_minutes(&self) -> Option<i64> {
        self.full_hour?
            .checked_mul(60)?
            .checked_add(self.full_minute?)
    }

    /// Whether the interior temperature reading is valid.
    pub fn interior_temp_available(&self) -> bool {
        self.temp_in_car.is_some()
    }

    /// Whether all doors are locked; `None` when no authoritative lock state is available.
    pub fn is_locked(&self) -> Option<bool> {
        let known: Vec<LockState> = [
            self.left_front_door_lock,
            self.right_front_door_lock,
            self.left_rear_door_lock,
            self.right_rear_door_lock,
        ]
        .into_iter()
        .flatten()
        .filter(|lock| !matches!(lock, LockState::Unknown | LockState::Unavailable))
        .collect();
        if known.is_empty() {
            return None;
        }
        Some(known.iter().all(|lock| *lock == LockState::Locked))
    }

    /// Whether any door/trunk/frunk is open.
    pub fn is_any_door_open(&self) -> bool {
        [
            self.left_front_door,
            self.right_front_door,
            self.left_rear_door,
            self.right_rear_door,
            self.trunk_lid,
            self.sliding_door,
            self.forehold,
        ]
        .contains(&Some(DoorOpenState::Open))
    }

    /// Whether any window is open.
    pub fn is_any_window_open(&self) -> bool {
        [
            self.left_front_window,
            self.right_front_window,
            self.left_rear_window,
            self.right_rear_window,
            self.skylight,
        ]
        .contains(&Some(WindowState::Open))
    }

    /// Whether the vehicle is powered on.
    pub fn is_vehicle_on(&self) -> bool {
        self.power_gear == Some(PowerGear::On)
    }

    /// Whether the battery heating system is active.
    pub fn is_battery_heating(&self) -> Option<bool> {
        self.battery_heat_state.map(|state| state != 0)
    }

    /// Whether steering wheel heating is active; `None` when the state is unknown.
    pub fn is_steering_wheel_heating(&self) -> Option<bool> {
        is_steering_wheel_heating(self.steering_wheel_heat_state)
    }
}

/// The shared `is_steering_wheel_heating` rule: `None` without data, else whether it is `ON`.
pub(crate) fn is_steering_wheel_heating(state: Option<StearingWheelHeat>) -> Option<bool> {
    match state {
        None | Some(StearingWheelHeat::NoData) => None,
        Some(state) => Some(state == StearingWheelHeat::On),
    }
}

impl BydModel for VehicleRealtimeData {
    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        let changes = match raw {
            Value::Object(input) => split_hybrid_legs(input, context)?,
            _ => Vec::new(),
        };
        let mut fields = Fields::cleaned_with_changes(raw, &changes, KEY_ALIASES)?;
        let float = coerce::float;
        let int = coerce::int;
        let string = coerce::string;
        let mut model = Self {
            raw: fields.raw(),
            online_state: fields.get("online_state", OnlineState::Unknown, enum_field),
            connect_state: fields.get("connect_state", ConnectState::Unknown, enum_field),
            vehicle_state: fields.get("vehicle_state", VehicleState::Unknown, enum_field),
            request_serial: fields.opt("request_serial", string),
            elec_percent: fields.opt("elec_percent", float),
            power_battery: fields.opt("power_battery", float),
            endurance_mileage: fields.opt("endurance_mileage", float),
            ev_endurance: fields.opt("ev_endurance", float),
            endurance_mileage_v2: fields.opt("endurance_mileage_v2", float),
            endurance_mileage_v2_unit: fields.opt("endurance_mileage_v2_unit", string),
            total_mileage: fields.opt("total_mileage", float),
            total_mileage_v2: fields.opt("total_mileage_v2", float),
            total_mileage_v2_unit: fields.opt("total_mileage_v2_unit", string),
            speed: fields.opt("speed", float),
            power_gear: fields.opt("power_gear", enum_field),
            temp_in_car: fields.opt("temp_in_car", float),
            main_setting_temp: fields.opt("main_setting_temp", int),
            main_setting_temp_new: fields.opt("main_setting_temp_new", float),
            air_run_state: fields.opt("air_run_state", enum_field),
            main_seat_heat_state: fields.opt("main_seat_heat_state", enum_field),
            main_seat_ventilation_state: fields.opt("main_seat_ventilation_state", enum_field),
            copilot_seat_heat_state: fields.opt("copilot_seat_heat_state", enum_field),
            copilot_seat_ventilation_state: fields
                .opt("copilot_seat_ventilation_state", enum_field),
            steering_wheel_heat_state: fields.opt("steering_wheel_heat_state", steering_wheel_heat),
            lr_seat_heat_state: fields.opt("lr_seat_heat_state", enum_field),
            lr_seat_ventilation_state: fields.opt("lr_seat_ventilation_state", enum_field),
            rr_seat_heat_state: fields.opt("rr_seat_heat_state", enum_field),
            rr_seat_ventilation_state: fields.opt("rr_seat_ventilation_state", enum_field),
            charging_state: fields.get("charging_state", ChargingState::Unknown, enum_field),
            charge_state: fields.opt("charge_state", enum_field),
            wait_status: fields.opt("wait_status", int),
            full_hour: fields.opt("full_hour", int),
            full_minute: fields.opt("full_minute", int),
            remaining_hours: fields.opt("remaining_hours", int),
            remaining_minutes: fields.opt("remaining_minutes", int),
            booking_charge_state: fields.opt("booking_charge_state", int),
            booking_charging_hour: fields.opt("booking_charging_hour", int),
            booking_charging_minute: fields.opt("booking_charging_minute", int),
            left_front_door: fields.opt("left_front_door", enum_field),
            right_front_door: fields.opt("right_front_door", enum_field),
            left_rear_door: fields.opt("left_rear_door", enum_field),
            right_rear_door: fields.opt("right_rear_door", enum_field),
            trunk_lid: fields.opt("trunk_lid", enum_field),
            sliding_door: fields.opt("sliding_door", enum_field),
            forehold: fields.opt("forehold", enum_field),
            left_front_door_lock: fields.opt("left_front_door_lock", enum_field),
            right_front_door_lock: fields.opt("right_front_door_lock", enum_field),
            left_rear_door_lock: fields.opt("left_rear_door_lock", enum_field),
            right_rear_door_lock: fields.opt("right_rear_door_lock", enum_field),
            sliding_door_lock: fields.opt("sliding_door_lock", enum_field),
            left_front_window: fields.opt("left_front_window", enum_field),
            right_front_window: fields.opt("right_front_window", enum_field),
            left_rear_window: fields.opt("left_rear_window", enum_field),
            right_rear_window: fields.opt("right_rear_window", enum_field),
            skylight: fields.opt("skylight", enum_field),
            left_front_window_pct: fields.opt("left_front_window_pct", int),
            right_front_window_pct: fields.opt("right_front_window_pct", int),
            left_rear_window_pct: fields.opt("left_rear_window_pct", int),
            right_rear_window_pct: fields.opt("right_rear_window_pct", int),
            left_front_tire_pressure: fields.opt("left_front_tire_pressure", float),
            right_front_tire_pressure: fields.opt("right_front_tire_pressure", float),
            left_rear_tire_pressure: fields.opt("left_rear_tire_pressure", float),
            right_rear_tire_pressure: fields.opt("right_rear_tire_pressure", float),
            left_front_tire_status: fields.opt("left_front_tire_status", int),
            right_front_tire_status: fields.opt("right_front_tire_status", int),
            left_rear_tire_status: fields.opt("left_rear_tire_status", int),
            right_rear_tire_status: fields.opt("right_rear_tire_status", int),
            tire_press_unit: fields.opt("tire_press_unit", enum_field),
            tirepressure_system: fields.opt("tirepressure_system", int),
            rapid_tire_leak: fields.opt("rapid_tire_leak", int),
            total_power: fields.opt("total_power", float),
            gl: fields.opt("gl", float),
            total_energy: fields.opt("total_energy", string),
            nearest_energy_consumption: fields.opt("nearest_energy_consumption", string),
            nearest_energy_consumption_unit: fields.opt("nearest_energy_consumption_unit", string),
            recent_50km_energy: fields.opt("recent_50km_energy", string),
            oil_endurance: fields.opt("oil_endurance", float),
            oil_percent: fields.opt("oil_percent", float),
            total_oil: fields.opt("total_oil", float),
            power_system: fields.opt("power_system", int),
            engine_status: fields.opt("engine_status", int),
            epb: fields.opt("epb", int),
            eps: fields.opt("eps", int),
            esp: fields.opt("esp", int),
            abs_warning: fields.opt("abs_warning", int),
            svs: fields.opt("svs", int),
            srs: fields.opt("srs", int),
            ect: fields.opt("ect", int),
            ect_value: fields.opt("ect_value", int),
            pwr: fields.opt("pwr", int),
            sentry_status: fields.get("sentry_status", SentryStatus::Unknown, enum_field),
            battery_heat_state: fields.opt("battery_heat_state", int),
            charge_heat_state: fields.opt("charge_heat_state", int),
            upgrade_status: fields.opt("upgrade_status", int),
            lr_third_heat_state: fields.opt("lr_third_heat_state", enum_field),
            lr_third_ventilation_state: fields.opt("lr_third_ventilation_state", enum_field),
            rr_third_heat_state: fields.opt("rr_third_heat_state", enum_field),
            rr_third_ventilation_state: fields.opt("rr_third_ventilation_state", enum_field),
            rate: fields.opt("rate", float),
            charge_power: fields.opt("charge_power", float),
            charge_power_unit: fields.opt("charge_power_unit", string),
            less_one_min: fields.opt("less_one_min", coerce::boolean),
            energy_consumption: fields.opt("energy_consumption", string),
            total_consumption: fields.opt("total_consumption", string),
            total_consumption_en: fields.opt("total_consumption_en", string),
            energy_consumption_ev: fields.opt("energy_consumption_ev", float),
            energy_consumption_ev_unit: fields.opt("energy_consumption_ev_unit", string),
            energy_consumption_fuel: fields.opt("energy_consumption_fuel", float),
            energy_consumption_fuel_unit: fields.opt("energy_consumption_fuel_unit", string),
            nearest_energy_consumption_ev: fields.opt("nearest_energy_consumption_ev", float),
            nearest_energy_consumption_ev_unit: fields
                .opt("nearest_energy_consumption_ev_unit", string),
            nearest_energy_consumption_fuel: fields.opt("nearest_energy_consumption_fuel", float),
            nearest_energy_consumption_fuel_unit: fields
                .opt("nearest_energy_consumption_fuel_unit", string),
            recent_50km_energy_ev: fields.opt("recent_50km_energy_ev", float),
            recent_50km_energy_ev_unit: fields.opt("recent_50km_energy_ev_unit", string),
            recent_50km_energy_fuel: fields.opt("recent_50km_energy_fuel", float),
            recent_50km_energy_fuel_unit: fields.opt("recent_50km_energy_fuel_unit", string),
            total_energy_ev: fields.opt("total_energy_ev", float),
            total_energy_ev_unit: fields.opt("total_energy_ev_unit", string),
            total_energy_fuel: fields.opt("total_energy_fuel", float),
            total_energy_fuel_unit: fields.opt("total_energy_fuel_unit", string),
            total_consumption_ev: fields.opt("total_consumption_ev", float),
            total_consumption_ev_unit: fields.opt("total_consumption_ev_unit", string),
            total_consumption_fuel: fields.opt("total_consumption_fuel", float),
            total_consumption_fuel_unit: fields.opt("total_consumption_fuel_unit", string),
            total_consumption_en_ev: fields.opt("total_consumption_en_ev", float),
            total_consumption_en_ev_unit: fields.opt("total_consumption_en_ev_unit", string),
            total_consumption_en_fuel: fields.opt("total_consumption_en_fuel", float),
            total_consumption_en_fuel_unit: fields.opt("total_consumption_en_fuel_unit", string),
            eq_consumption: fields.opt("eq_consumption", float),
            eq_consumption_unit: fields.opt("eq_consumption_unit", string),
            oil_pressure_system: fields.opt("oil_pressure_system", int),
            braking_system: fields.opt("braking_system", int),
            charging_system: fields.opt("charging_system", int),
            steering_system: fields.opt("steering_system", int),
            ok_light: fields.opt("ok_light", int),
            repair_mode_switch: fields.opt("repair_mode_switch", string),
            vehicle_time_zone: fields.opt("vehicle_time_zone", string),
            power_battery_connection: fields.opt("power_battery_connection", int),
            ins: fields.opt("ins", int),
            timestamp: fields.get("timestamp", None, base::byd_timestamp),
        };
        fields.finish()?;
        // _SENTINEL_RULES
        let m = &mut model;
        base::clear_if(&mut m.temp_in_car, base::is_temp_sentinel);
        for field in [
            &mut m.full_hour,
            &mut m.full_minute,
            &mut m.remaining_hours,
            &mut m.remaining_minutes,
            &mut m.ect,
            &mut m.abs_warning,
            &mut m.svs,
            &mut m.srs,
            &mut m.eps,
            &mut m.esp,
            &mut m.pwr,
            &mut m.power_system,
            &mut m.tirepressure_system,
            &mut m.rapid_tire_leak,
            &mut m.left_front_tire_status,
            &mut m.right_front_tire_status,
            &mut m.left_rear_tire_status,
            &mut m.right_rear_tire_status,
            &mut m.upgrade_status,
        ] {
            base::clear_if(field, base::is_negative);
        }
        base::clear_if(&mut m.ect_value, is_ect_sentinel);
        for field in [
            &mut m.oil_endurance,
            &mut m.oil_percent,
            &mut m.endurance_mileage,
            &mut m.endurance_mileage_v2,
            &mut m.elec_percent,
        ] {
            base::clear_if(field, base::is_negative);
        }
        base::clear_if(&mut m.rate, is_rate_sentinel);
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([
            ("is_online", json!(self.is_online())),
            (
                "effective_charging_state",
                json!(self.effective_charging_state().value()),
            ),
            ("is_charging", json!(self.is_charging())),
            ("is_charger_connected", json!(self.is_charger_connected())),
            ("time_to_full_minutes", json!(self.time_to_full_minutes())),
            (
                "interior_temp_available",
                json!(self.interior_temp_available()),
            ),
            ("is_locked", json!(self.is_locked())),
            ("is_any_door_open", json!(self.is_any_door_open())),
            ("is_any_window_open", json!(self.is_any_window_open())),
            ("is_vehicle_on", json!(self.is_vehicle_on())),
            ("is_battery_heating", json!(self.is_battery_heating())),
            (
                "is_steering_wheel_heating",
                json!(self.is_steering_wheel_heating()),
            ),
        ])
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.realtime.OnlineState" => enum_call::<OnlineState>(args),
        "models.realtime.ConnectState" => enum_call::<ConnectState>(args),
        "models.realtime.VehicleState" => enum_call::<VehicleState>(args),
        "models.realtime.ChargingState" => enum_call::<ChargingState>(args),
        "models.realtime.SentryStatus" => enum_call::<SentryStatus>(args),
        "models.realtime.TirePressureUnit" => enum_call::<TirePressureUnit>(args),
        "models.realtime.DoorOpenState" => enum_call::<DoorOpenState>(args),
        "models.realtime.LockState" => enum_call::<LockState>(args),
        "models.realtime.WindowState" => enum_call::<WindowState>(args),
        "models.realtime.PowerGear" => enum_call::<PowerGear>(args),
        "models.realtime.StearingWheelHeat" => enum_call::<StearingWheelHeat>(args),
        "models.realtime.SeatHeatVentState" => enum_call::<SeatHeatVentState>(args),
        "models.realtime.AirCirculationMode" => enum_call::<AirCirculationMode>(args),
        "models.realtime.VehicleRealtimeData" => base::model_call::<VehicleRealtimeData>(args),
        "models.realtime.VehicleRealtimeData.is_ready_raw" => {
            VehicleRealtimeData::is_ready_raw(Args(args).value("vehicle_info"))
                .map(|ready| json!(ready))
                .map_err(ParityError::from)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: Value, energy_type: i64) -> VehicleRealtimeData {
        let context = json!({"energy_type": {"EnergyType": energy_type}});
        VehicleRealtimeData::parse(&raw, &context).unwrap()
    }

    #[test]
    fn numbers_follow_the_python_regex() {
        assert_eq!(match_number("-12.5kW"), Some((5, -12.5)));
        assert_eq!(match_number("7.x"), Some((1, 7.0)));
        assert_eq!(match_number("\u{663}.\u{665}L"), Some((5, 3.5)));
        assert_eq!(match_number("-"), None);
        assert_eq!(match_number("x1"), None);
        assert_eq!(
            first_num(Some("9".repeat(400).as_str())),
            Some(f64::INFINITY)
        );
        assert_eq!(first_num(Some("")), None);
        assert_eq!(first_num(None), None);
        assert_eq!(float_value(f64::NEG_INFINITY), json!("-inf"));
        assert_eq!(extract_unit("--kW·h/100km"), "kWh/100km");
        assert_eq!(unit_or_default("/100km", "L/100km"), "L/100km");
    }

    #[test]
    fn hybrid_legs_are_split() {
        let data = parse(
            json!({
                "energyConsumption": "6.1kW·h/100km+8.4L/100km",
                "totalEnergy": "(19.7kW·h+3.5L)/100km",
                "nearestEnergyConsumption": "5.0",
                "nearestEnergyConsumptionUnit": "L/100km",
            }),
            2,
        );
        assert_eq!(data.energy_consumption_ev, Some(6.1));
        assert_eq!(
            data.energy_consumption_fuel_unit.as_deref(),
            Some("L/100km")
        );
        assert_eq!(data.total_energy_fuel, Some(3.5));
        assert_eq!(data.nearest_energy_consumption_ev, Some(6.1));
        assert_eq!(data.nearest_energy_consumption_fuel, Some(8.4));
        assert_eq!(data.eq_consumption, Some(5.0));
        assert_eq!(data.raw["totalEnergy"], json!("(19.7kW·h+3.5L)/100km"));
    }

    #[test]
    fn contexts() {
        assert_eq!(context_energy_type(&json!(null)).unwrap(), EnergyType::Ev);
        assert_eq!(
            context_energy_type(&json!({"energy_type": 1})).unwrap(),
            EnergyType::Ev
        );
        assert_eq!(
            context_energy_type(&json!({"energy_type": {"x": 1}})).unwrap(),
            EnergyType::Ev
        );
        assert_eq!(
            context_energy_type(&json!({"energy_type": {"EnergyType": 1}})).unwrap(),
            EnergyType::Ice
        );
        assert_eq!(
            context_energy_type(&json!("x")).unwrap_err().exception,
            "AttributeError"
        );
        assert!(VehicleRealtimeData::parse(&json!({}), &json!([1])).is_err());
        assert_eq!(
            VehicleRealtimeData::parse(&json!(5), &json!("x"))
                .unwrap_err()
                .error_type,
            "model_type"
        );
    }

    #[test]
    fn readiness() {
        let ready = |value: Value| VehicleRealtimeData::is_ready_raw(&value);
        assert!(!ready(json!(null)).unwrap());
        assert_eq!(ready(json!([1])).unwrap_err().exception, "AttributeError");
        assert!(!ready(json!({"onlineState": 2.0, "time": 5})).unwrap());
        assert!(ready(json!({"leftFrontTirepressure": "2.5"})).unwrap());
        assert!(ready(json!({"time": "9".repeat(50)})).unwrap());
        assert!(!ready(json!({"time": format!("-{}", "9".repeat(50))})).unwrap());
        assert_eq!(
            ready(json!({"time": "1.5"})).unwrap_err().exception,
            "ValueError"
        );
        assert_eq!(
            ready(json!({"time": [1]})).unwrap_err().exception,
            "TypeError"
        );
        assert!(ready(json!({"time": u64::MAX})).unwrap());
        assert!(!ready(json!({"time": 0.5, "enduranceMileage": false})).unwrap());
        assert!(ready(json!({"time": true})).unwrap());
        assert_eq!(
            ready(json!({"enduranceMileage": "x"}))
                .unwrap_err()
                .exception,
            "ValueError"
        );
        assert_eq!(
            ready(json!({"enduranceMileage": {"a": 1}}))
                .unwrap_err()
                .exception,
            "TypeError"
        );
        assert!(ready(json!({"enduranceMileage": true})).unwrap());
        assert_eq!(
            parity_call(
                "models.realtime.VehicleRealtimeData.is_ready_raw",
                &json!({})
            )
            .unwrap()
            .unwrap(),
            json!(false)
        );
    }

    #[test]
    fn properties_and_overflow() {
        let data = parse(
            json!({"fullHour": i64::MAX, "fullMinute": 1, "batteryHeatState": 2,
                   "leftFrontDoorLock": 0, "rightFrontDoorLock": 2}),
            0,
        );
        assert_eq!(data.time_to_full_minutes(), None);
        assert_eq!(data.is_battery_heating(), Some(true));
        assert_eq!(data.is_locked(), Some(true));
        assert_eq!(data.is_charger_connected(), None);
        assert_eq!(
            is_steering_wheel_heating(Some(StearingWheelHeat::Off)),
            Some(false)
        );
    }

    #[test]
    fn enum_extras() {
        assert_eq!(StearingWheelHeat::call(&json!(2.0)), StearingWheelHeat::On);
        assert_eq!(
            StearingWheelHeat::call(&json!("2")),
            StearingWheelHeat::NoData
        );
        assert_eq!(
            steering_wheel_heat(&json!(2)).unwrap(),
            StearingWheelHeat::On
        );
        assert_eq!(
            steering_wheel_heat(&json!(" 1 ")).unwrap(),
            StearingWheelHeat::Off
        );
        assert!(steering_wheel_heat(&json!(u64::MAX)).is_err());
        assert_eq!(SeatHeatVentState::High.to_command_level(), 1);
        assert_eq!(
            enum_call::<SeatHeatVentState>(&json!({"value": 2})).unwrap(),
            json!({"name": "LOW", "value": 2, "to_command_level": 2})
        );
        assert_eq!(ChargingState::call(&json!(15)).name(), "CONNECTED");
    }
}
