//! Quality filters for incoming vehicle telemetry (`pybyd/_validators.py`).
//!
//! This is the one place where incoming payloads are filtered before the
//! state engine keeps them. Two styles are used:
//!
//! - **Section-level filters** (GPS, HVAC) look at the whole model, because
//!   validity depends on several fields together (latitude and longitude).
//! - **Per-field filters** (realtime zero-drop, preserve-when-none) look at one
//!   field at a time and may keep the previous value when the incoming one is
//!   missing or not authoritative.
//!
//! The engine calls [`apply_realtime_filters`] for realtime payloads,
//! [`apply_gps_filters`] for GPS and [`apply_hvac_filters`] for HVAC. A filter
//! either keeps the incoming value or asks for a replacement (usually the last
//! known value). pyBYD's per-field filters return a `_MISSING` sentinel for
//! "no change"; here that is `None` from [`drop_zero_value_filter`] and
//! [`preserve_when_none_filter`].
//!
//! Adding a rule upstream means a new filter function, an entry in the field
//! table, and regression tests for three cases: the previous value is kept,
//! the incoming authoritative value is accepted, and the first payload (no
//! previous value). The parity cases in `parity/groups/state.py` follow the
//! same pattern.

use std::sync::OnceLock;

use serde_json::{Map, Value, json};

use crate::coerce::{self, IntEnum};
use crate::models::base::BydModel;
use crate::models::gps::GpsInfo;
use crate::models::hvac::HvacStatus;
use crate::models::realtime::{LockState, TirePressureUnit, VehicleRealtimeData};

type Realtime = VehicleRealtimeData;

/// Both coordinates closer to zero than this are BYD's "Null Island" artefact, not a fix.
pub const GPS_NULL_ISLAND_THRESHOLD: f64 = 0.1;

/// Realtime fields whose incoming `0` is not trusted: the previous value is kept.
///
/// BYD's HTTP `/vehicleRealTimeResult` answers a sleeping car with zeros in
/// place of the door locks, state of charge, range, tyre pressures and
/// odometer. A literal `0` there is a placeholder, so the last known value is
/// kept; on the first payload (no previous value) the `0` becomes `None`. A
/// lock reading `UNAVAILABLE` (0) counts as a zero; a lock missing from the
/// payload is not preserved.
pub const ZERO_DROP_FIELD_NAMES: [&str; 17] = [
    "left_front_door_lock",
    "right_front_door_lock",
    "left_rear_door_lock",
    "right_rear_door_lock",
    "sliding_door_lock",
    "elec_percent",
    "left_front_tire_pressure",
    "right_front_tire_pressure",
    "left_rear_tire_pressure",
    "right_rear_tire_pressure",
    "endurance_mileage",
    "ev_endurance",
    "endurance_mileage_v2",
    "total_mileage",
    "total_mileage_v2",
    "oil_endurance",
    "oil_percent",
];

/// Energy-consumption strings whose last known value is kept when the incoming one is `None`.
///
/// The HTTP `/getEnergyConsumption` endpoint returns the sentinel `"--"`
/// (stripped to `None`) where MQTT `vehicleInfo` events carry real values.
/// Keeping the last good value stops HTTP polls from clobbering the
/// MQTT-derived one.
pub const PRESERVE_WHEN_NONE_FIELD_NAMES: [&str; 4] = [
    "recent_50km_energy",
    "total_energy",
    "total_consumption",
    "total_consumption_en",
];

/// Suffixes of the per-leg fields parsed from each preserved string; see [`guard_consumption_legs`].
pub const LEG_SUFFIXES: [&str; 4] = ["ev", "ev_unit", "fuel", "fuel_unit"];

/// The four tyre pressures the [`guard_tire_press_unit`] rule looks at.
pub const TIRE_PRESSURE_FIELDS: [&str; 4] = [
    "left_front_tire_pressure",
    "right_front_tire_pressure",
    "left_rear_tire_pressure",
    "right_rear_tire_pressure",
];

/// The HVAC fields that make up the asleep placeholder; see [`apply_hvac_filters`].
pub const HVAC_SENSOR_PLACEHOLDER_FIELDS: [&str; 3] = ["temp_in_car", "temp_out_car", "pm"];

// ---------------------------------------------------------------------------
// GPS
// ---------------------------------------------------------------------------

/// `_has_valid_coordinates`: whether `gps` carries a usable latitude/longitude pair.
///
/// The pair is invalid when either value is missing (a partial fix is
/// useless) or when both fall within [`GPS_NULL_ISLAND_THRESHOLD`] of zero (an
/// API artefact). A coordinate of exactly `0.1` is valid. NaN compares false
/// with the threshold, so a NaN pair counts as valid, as it does in pyBYD.
pub fn has_valid_coordinates(gps: &GpsInfo) -> bool {
    let (Some(lat), Some(lon)) = (gps.latitude, gps.longitude) else {
        return false;
    };
    !(lat.abs() < GPS_NULL_ISLAND_THRESHOLD && lon.abs() < GPS_NULL_ISLAND_THRESHOLD)
}

/// The best available GPS fix, preferring `incoming`.
///
/// Falls back to `previous` when `incoming` is `None`, has a coordinate
/// missing, or sits on "Null Island" (both coordinates near zero). On the
/// first payload (`previous` is `None`) the same checks apply, so the result
/// is `None` until a usable fix arrives. The result is one of the two
/// references passed in, never a copy.
pub fn guard_gps_coordinates<'a>(
    previous: Option<&'a GpsInfo>,
    incoming: Option<&'a GpsInfo>,
) -> Option<&'a GpsInfo> {
    match incoming {
        Some(incoming) if has_valid_coordinates(incoming) => Some(incoming),
        _ => previous,
    }
}

/// Every GPS filtering rule; today only [`guard_gps_coordinates`].
pub fn apply_gps_filters<'a>(
    previous: Option<&'a GpsInfo>,
    incoming: Option<&'a GpsInfo>,
) -> Option<&'a GpsInfo> {
    guard_gps_coordinates(previous, incoming)
}

// ---------------------------------------------------------------------------
// Realtime per-field filters
// ---------------------------------------------------------------------------

/// A value a realtime filter writes over the incoming one (pyBYD's `model_copy(update=...)`).
#[derive(Debug, Clone, PartialEq)]
pub enum RealtimeOverride {
    /// A door lock field.
    Lock(Option<LockState>),
    /// A numeric field.
    Float(Option<f64>),
    /// A text field: a consumption string or a leg's unit.
    Text(Option<String>),
    /// `tire_press_unit`.
    TireUnit(Option<TirePressureUnit>),
}

impl RealtimeOverride {
    /// The value as `model_dump(mode="json")` shows it.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Lock(value) => value.map_or(Value::Null, |lock| json!(lock.value())),
            Self::Float(value) => value.map_or(Value::Null, coerce::float_json),
            Self::Text(value) => value.as_ref().map_or(Value::Null, |text| json!(text)),
            Self::TireUnit(value) => value.map_or(Value::Null, |unit| json!(unit.value())),
        }
    }
}

/// Overrides by field name, in the order pyBYD collects them.
pub type RealtimeUpdates = Vec<(&'static str, RealtimeOverride)>;

/// A realtime field the filters read and write.
struct Slot<T: 'static> {
    name: &'static str,
    read: fn(&Realtime) -> &Option<T>,
    write: fn(&mut Realtime) -> &mut Option<T>,
}

/// A zero-drop field: door locks are enums, the rest are numbers.
enum ZeroDropSlot {
    Lock(Slot<LockState>),
    Float(Slot<f64>),
}

const fn lock(
    name: &'static str,
    read: fn(&Realtime) -> &Option<LockState>,
    write: fn(&mut Realtime) -> &mut Option<LockState>,
) -> ZeroDropSlot {
    ZeroDropSlot::Lock(Slot { name, read, write })
}

const fn number(
    name: &'static str,
    read: fn(&Realtime) -> &Option<f64>,
    write: fn(&mut Realtime) -> &mut Option<f64>,
) -> ZeroDropSlot {
    ZeroDropSlot::Float(Slot { name, read, write })
}

/// [`ZERO_DROP_FIELD_NAMES`] with their accessors, in the same order.
const ZERO_DROP_SLOTS: [ZeroDropSlot; 17] = [
    lock(
        "left_front_door_lock",
        |v| &v.left_front_door_lock,
        |v| &mut v.left_front_door_lock,
    ),
    lock(
        "right_front_door_lock",
        |v| &v.right_front_door_lock,
        |v| &mut v.right_front_door_lock,
    ),
    lock(
        "left_rear_door_lock",
        |v| &v.left_rear_door_lock,
        |v| &mut v.left_rear_door_lock,
    ),
    lock(
        "right_rear_door_lock",
        |v| &v.right_rear_door_lock,
        |v| &mut v.right_rear_door_lock,
    ),
    lock(
        "sliding_door_lock",
        |v| &v.sliding_door_lock,
        |v| &mut v.sliding_door_lock,
    ),
    number("elec_percent", |v| &v.elec_percent, |v| &mut v.elec_percent),
    number(
        "left_front_tire_pressure",
        |v| &v.left_front_tire_pressure,
        |v| &mut v.left_front_tire_pressure,
    ),
    number(
        "right_front_tire_pressure",
        |v| &v.right_front_tire_pressure,
        |v| &mut v.right_front_tire_pressure,
    ),
    number(
        "left_rear_tire_pressure",
        |v| &v.left_rear_tire_pressure,
        |v| &mut v.left_rear_tire_pressure,
    ),
    number(
        "right_rear_tire_pressure",
        |v| &v.right_rear_tire_pressure,
        |v| &mut v.right_rear_tire_pressure,
    ),
    number(
        "endurance_mileage",
        |v| &v.endurance_mileage,
        |v| &mut v.endurance_mileage,
    ),
    number("ev_endurance", |v| &v.ev_endurance, |v| &mut v.ev_endurance),
    number(
        "endurance_mileage_v2",
        |v| &v.endurance_mileage_v2,
        |v| &mut v.endurance_mileage_v2,
    ),
    number(
        "total_mileage",
        |v| &v.total_mileage,
        |v| &mut v.total_mileage,
    ),
    number(
        "total_mileage_v2",
        |v| &v.total_mileage_v2,
        |v| &mut v.total_mileage_v2,
    ),
    number(
        "oil_endurance",
        |v| &v.oil_endurance,
        |v| &mut v.oil_endurance,
    ),
    number("oil_percent", |v| &v.oil_percent, |v| &mut v.oil_percent),
];

/// The per-leg fields parsed from one preserved consumption string.
struct LegSlots {
    ev: Slot<f64>,
    ev_unit: Slot<String>,
    fuel: Slot<f64>,
    fuel_unit: Slot<String>,
    /// The legacy string itself, kept by [`preserve_when_none_filter`].
    text: Slot<String>,
}

/// [`PRESERVE_WHEN_NONE_FIELD_NAMES`] with their legs, in the same order.
const LEG_SLOTS: [LegSlots; 4] = [
    LegSlots {
        ev: Slot {
            name: "recent_50km_energy_ev",
            read: |v| &v.recent_50km_energy_ev,
            write: |v| &mut v.recent_50km_energy_ev,
        },
        ev_unit: Slot {
            name: "recent_50km_energy_ev_unit",
            read: |v| &v.recent_50km_energy_ev_unit,
            write: |v| &mut v.recent_50km_energy_ev_unit,
        },
        fuel: Slot {
            name: "recent_50km_energy_fuel",
            read: |v| &v.recent_50km_energy_fuel,
            write: |v| &mut v.recent_50km_energy_fuel,
        },
        fuel_unit: Slot {
            name: "recent_50km_energy_fuel_unit",
            read: |v| &v.recent_50km_energy_fuel_unit,
            write: |v| &mut v.recent_50km_energy_fuel_unit,
        },
        text: Slot {
            name: "recent_50km_energy",
            read: |v| &v.recent_50km_energy,
            write: |v| &mut v.recent_50km_energy,
        },
    },
    LegSlots {
        ev: Slot {
            name: "total_energy_ev",
            read: |v| &v.total_energy_ev,
            write: |v| &mut v.total_energy_ev,
        },
        ev_unit: Slot {
            name: "total_energy_ev_unit",
            read: |v| &v.total_energy_ev_unit,
            write: |v| &mut v.total_energy_ev_unit,
        },
        fuel: Slot {
            name: "total_energy_fuel",
            read: |v| &v.total_energy_fuel,
            write: |v| &mut v.total_energy_fuel,
        },
        fuel_unit: Slot {
            name: "total_energy_fuel_unit",
            read: |v| &v.total_energy_fuel_unit,
            write: |v| &mut v.total_energy_fuel_unit,
        },
        text: Slot {
            name: "total_energy",
            read: |v| &v.total_energy,
            write: |v| &mut v.total_energy,
        },
    },
    LegSlots {
        ev: Slot {
            name: "total_consumption_ev",
            read: |v| &v.total_consumption_ev,
            write: |v| &mut v.total_consumption_ev,
        },
        ev_unit: Slot {
            name: "total_consumption_ev_unit",
            read: |v| &v.total_consumption_ev_unit,
            write: |v| &mut v.total_consumption_ev_unit,
        },
        fuel: Slot {
            name: "total_consumption_fuel",
            read: |v| &v.total_consumption_fuel,
            write: |v| &mut v.total_consumption_fuel,
        },
        fuel_unit: Slot {
            name: "total_consumption_fuel_unit",
            read: |v| &v.total_consumption_fuel_unit,
            write: |v| &mut v.total_consumption_fuel_unit,
        },
        text: Slot {
            name: "total_consumption",
            read: |v| &v.total_consumption,
            write: |v| &mut v.total_consumption,
        },
    },
    LegSlots {
        ev: Slot {
            name: "total_consumption_en_ev",
            read: |v| &v.total_consumption_en_ev,
            write: |v| &mut v.total_consumption_en_ev,
        },
        ev_unit: Slot {
            name: "total_consumption_en_ev_unit",
            read: |v| &v.total_consumption_en_ev_unit,
            write: |v| &mut v.total_consumption_en_ev_unit,
        },
        fuel: Slot {
            name: "total_consumption_en_fuel",
            read: |v| &v.total_consumption_en_fuel,
            write: |v| &mut v.total_consumption_en_fuel,
        },
        fuel_unit: Slot {
            name: "total_consumption_en_fuel_unit",
            read: |v| &v.total_consumption_en_fuel_unit,
            write: |v| &mut v.total_consumption_en_fuel_unit,
        },
        text: Slot {
            name: "total_consumption_en",
            read: |v| &v.total_consumption_en,
            write: |v| &mut v.total_consumption_en,
        },
    },
];

/// A value the zero-drop rule can compare with `0`.
pub trait ZeroComparable: Copy {
    /// Python's `value == 0`.
    fn is_zero(self) -> bool;
}

impl ZeroComparable for f64 {
    fn is_zero(self) -> bool {
        self == 0.0
    }
}

impl ZeroComparable for LockState {
    fn is_zero(self) -> bool {
        self.value() == 0
    }
}

/// `_drop_zero_value_filter`: keep the previous value when the incoming one is `0`.
///
/// Returns the replacement, or `None` (pyBYD's `_MISSING`) to leave the
/// incoming value as it is. `incoming_present` is whether the payload had
/// the field (pydantic's `model_fields_set`).
pub fn drop_zero_value_filter<T: ZeroComparable>(
    previous_value: Option<T>,
    incoming_value: Option<T>,
    incoming_present: bool,
) -> Option<Option<T>> {
    if !incoming_present {
        return None;
    }
    match incoming_value {
        Some(value) if value.is_zero() => Some(previous_value),
        _ => None,
    }
}

/// `_preserve_when_none_filter`: keep the previous value when the incoming one is absent or `None`.
///
/// Returns the replacement, or `None` (pyBYD's `_MISSING`) to leave the
/// incoming value as it is. Nothing is kept when there is no previous value.
pub fn preserve_when_none_filter<T: Clone>(
    previous_value: &Option<T>,
    incoming_value: &Option<T>,
    incoming_present: bool,
) -> Option<Option<T>> {
    previous_value.as_ref()?;
    if !incoming_present || incoming_value.is_none() {
        return Some(previous_value.clone());
    }
    None
}

/// The model `VehicleRealtimeData.model_validate({})`: every field at its default.
///
/// It stands in for the previous payload on the first one, so the zero-drop
/// rule turns a first `0` into `None`.
pub fn empty_realtime() -> &'static VehicleRealtimeData {
    static EMPTY: OnceLock<VehicleRealtimeData> = OnceLock::new();
    EMPTY.get_or_init(|| {
        VehicleRealtimeData::parse(&Value::Object(Map::new()), &Value::Null)
            .expect("an empty dict is a valid realtime payload")
    })
}

/// `_apply_model_field_filters` with the realtime filter table: the per-field overrides.
///
/// For each field of [`ZERO_DROP_FIELD_NAMES`] and then
/// [`PRESERVE_WHEN_NONE_FIELD_NAMES`] the field's filter runs, and its
/// replacement (if any) is collected.
///
/// pyBYD decides "present" from pydantic's `model_fields_set`. Every one of
/// these fields defaults to `None`, sentinel cleaning removes keys before
/// validation and the sentinel rules clear values to `None`, so a field that
/// was not in the payload always reads `None`, and both filters only act on
/// `0` (which must have come from the payload) or on `None` (where presence
/// does not change the answer). Presence is therefore taken as "the value is
/// not `None`", which gives the same overrides.
pub fn apply_model_field_filters(
    previous: &VehicleRealtimeData,
    incoming: &VehicleRealtimeData,
) -> RealtimeUpdates {
    let mut updates = RealtimeUpdates::new();
    for slot in &ZERO_DROP_SLOTS {
        match slot {
            ZeroDropSlot::Lock(slot) => {
                let value = *(slot.read)(incoming);
                if let Some(kept) =
                    drop_zero_value_filter(*(slot.read)(previous), value, value.is_some())
                {
                    updates.push((slot.name, RealtimeOverride::Lock(kept)));
                }
            }
            ZeroDropSlot::Float(slot) => {
                let value = *(slot.read)(incoming);
                if let Some(kept) =
                    drop_zero_value_filter(*(slot.read)(previous), value, value.is_some())
                {
                    updates.push((slot.name, RealtimeOverride::Float(kept)));
                }
            }
        }
    }
    for legs in &LEG_SLOTS {
        let value = (legs.text.read)(incoming);
        if let Some(kept) =
            preserve_when_none_filter((legs.text.read)(previous), value, value.is_some())
        {
            updates.push((legs.text.name, RealtimeOverride::Text(kept)));
        }
    }
    updates
}

/// `_guard_tire_press_unit`: keep the previous `tire_press_unit` when all four pressures read zero.
///
/// The HTTP `/vehicleRealTimeResult` endpoint returns the realtime block
/// with `tirePressUnit: 3` (kPa, the default) and every tyre pressure set to
/// `0` even when the car is configured for PSI. The zero-drop filters already
/// keep the previous numbers, but the unit had no such guard, so it flipped
/// PSI to kPa while the numbers stayed PSI-scale, and a consumer converting
/// units showed `pressure / 6.89476` until the next MQTT push restored the
/// real unit.
///
/// Returns the previous unit when every incoming pressure is `0` or `None`
/// (which is `None` on the first payload: the all-zero placeholder is
/// rejected wholesale, default unit included), or `None` (pyBYD's
/// `_MISSING`) to keep the incoming unit.
pub fn guard_tire_press_unit(
    previous: &VehicleRealtimeData,
    incoming: &VehicleRealtimeData,
) -> Option<Option<TirePressureUnit>> {
    let pressures = [
        incoming.left_front_tire_pressure,
        incoming.right_front_tire_pressure,
        incoming.left_rear_tire_pressure,
        incoming.right_rear_tire_pressure,
    ];
    if pressures
        .iter()
        .all(|pressure| pressure.is_none_or(|value| value == 0.0))
    {
        return Some(previous.tire_press_unit);
    }
    None
}

/// `_guard_consumption_legs`: carry the per-leg split of each preserved consumption string over.
///
/// [`PRESERVE_WHEN_NONE_FIELD_NAMES`] keeps the legacy strings when an HTTP
/// poll returns `--`, but the `<field>_ev` / `_ev_unit` / `_fuel` /
/// `_fuel_unit` fields parsed from them drop to `None`. Each group of four is
/// carried over as a whole, and only when all four arrive `None`: the payload
/// had no number for the field (`--`, a missing key or an unreadable value).
/// The legacy string is then `None` too, so its own filter keeps it in step.
///
/// Carrying the legs one by one would mix payloads, for example a hybrid's
/// fuel leg next to a newer EV-only reading.
///
/// Returns the overrides; empty when nothing needs carrying.
pub fn guard_consumption_legs(
    previous: &VehicleRealtimeData,
    incoming: &VehicleRealtimeData,
) -> RealtimeUpdates {
    let mut updates = RealtimeUpdates::new();
    for legs in &LEG_SLOTS {
        let arrived = (legs.ev.read)(incoming).is_some()
            || (legs.ev_unit.read)(incoming).is_some()
            || (legs.fuel.read)(incoming).is_some()
            || (legs.fuel_unit.read)(incoming).is_some();
        if arrived {
            continue;
        }
        let ev = *(legs.ev.read)(previous);
        let ev_unit = (legs.ev_unit.read)(previous).clone();
        let fuel = *(legs.fuel.read)(previous);
        let fuel_unit = (legs.fuel_unit.read)(previous).clone();
        if ev.is_some() || ev_unit.is_some() || fuel.is_some() || fuel_unit.is_some() {
            updates.push((legs.ev.name, RealtimeOverride::Float(ev)));
            updates.push((legs.ev_unit.name, RealtimeOverride::Text(ev_unit)));
            updates.push((legs.fuel.name, RealtimeOverride::Float(fuel)));
            updates.push((legs.fuel_unit.name, RealtimeOverride::Text(fuel_unit)));
        }
    }
    updates
}

/// Every override [`apply_realtime_filters`] writes, in pyBYD's dict order.
///
/// A name that appears twice keeps its first position and takes the later
/// value, as `dict.update` does (the rules never write the same field twice).
pub fn realtime_filter_updates(
    previous: Option<&VehicleRealtimeData>,
    incoming: &VehicleRealtimeData,
) -> RealtimeUpdates {
    let baseline = previous.unwrap_or_else(|| empty_realtime());
    let mut updates = apply_model_field_filters(baseline, incoming);
    if let Some(unit) = guard_tire_press_unit(baseline, incoming) {
        updates.push(("tire_press_unit", RealtimeOverride::TireUnit(unit)));
    }
    for (name, value) in guard_consumption_legs(baseline, incoming) {
        match updates.iter_mut().find(|(existing, _)| *existing == name) {
            Some(entry) => entry.1 = value,
            None => updates.push((name, value)),
        }
    }
    updates
}

/// Writes one override into `model` (pyBYD's `model_copy(update=...)` for one key).
///
/// Panics on a name no realtime filter writes; the names come from this module.
fn write_override(model: &mut VehicleRealtimeData, name: &str, value: RealtimeOverride) {
    if let ("tire_press_unit", RealtimeOverride::TireUnit(unit)) = (name, &value) {
        model.tire_press_unit = *unit;
        return;
    }
    for slot in &ZERO_DROP_SLOTS {
        match (slot, &value) {
            (ZeroDropSlot::Lock(slot), RealtimeOverride::Lock(lock)) if slot.name == name => {
                *(slot.write)(model) = *lock;
                return;
            }
            (ZeroDropSlot::Float(slot), RealtimeOverride::Float(number)) if slot.name == name => {
                *(slot.write)(model) = *number;
                return;
            }
            _ => {}
        }
    }
    for legs in &LEG_SLOTS {
        match &value {
            RealtimeOverride::Float(number) => {
                for slot in [&legs.ev, &legs.fuel] {
                    if slot.name == name {
                        *(slot.write)(model) = *number;
                        return;
                    }
                }
            }
            RealtimeOverride::Text(text) => {
                for slot in [&legs.ev_unit, &legs.fuel_unit, &legs.text] {
                    if slot.name == name {
                        (slot.write)(model).clone_from(text);
                        return;
                    }
                }
            }
            _ => {}
        }
    }
    panic!("no realtime filter writes {name} with {value:?}");
}

/// Every realtime filtering rule in one place.
///
/// The rules:
///
/// - zero-drop gating of selected fields (door locks, state of charge, range,
///   tyre pressures, odometer): see [`ZERO_DROP_FIELD_NAMES`];
/// - preserve-when-none for the energy-consumption strings: see
///   [`PRESERVE_WHEN_NONE_FIELD_NAMES`];
/// - the cross-field guard keeping `tire_press_unit` when all four incoming
///   pressures read zero: [`guard_tire_press_unit`];
/// - the cross-field guard carrying the per-leg split of each preserved
///   consumption string over as a group: [`guard_consumption_legs`].
///
/// Returns `incoming` itself when nothing needs overriding, otherwise
/// `incoming` with the overrides written in.
pub fn apply_realtime_filters(
    previous: Option<&VehicleRealtimeData>,
    incoming: VehicleRealtimeData,
) -> VehicleRealtimeData {
    let updates = realtime_filter_updates(previous, &incoming);
    let mut filtered = incoming;
    for (name, value) in updates {
        write_override(&mut filtered, name, value);
    }
    filtered
}

/// The JSON form of a list of overrides: `{field: value}` in order.
pub fn updates_to_json(updates: &RealtimeUpdates) -> Value {
    Value::Object(
        updates
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.to_json()))
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// HVAC
// ---------------------------------------------------------------------------

/// Drops the all-zero-or-missing placeholder HVAC payload.
///
/// BYD's HVAC HTTP endpoint has been seen returning a payload where every
/// sensor field ([`HVAC_SENSOR_PLACEHOLDER_FIELDS`]: `temp_in_car`,
/// `temp_out_car`, `pm`) is `0` or missing at once while the car's HVAC block
/// is asleep: a placeholder, not data. Then all three are cleared to `None`,
/// so the placeholder's literal `0` never surfaces as a reading: a poll with
/// no live sensor data gives no sensor values. Setpoints and state enums on
/// the same payload still update.
///
/// Previous values are deliberately not pinned over the placeholder (unlike
/// the realtime zero-drop family): temperatures and particulates keep
/// changing while the car sleeps, so a reading pinned across a multi-hour
/// sleep would look live while being hours old. `previous` stays in the
/// signature for the engine's call site.
///
/// A genuine 0 °C on one sensor survives as long as the same payload carries
/// another real sensor value (true of every capture seen so far). The
/// trade-off: a payload whose whole triad is genuinely 0-or-missing is
/// indistinguishable from the placeholder and gives `None` until the next
/// mixed payload. A NaN reading is neither `None` nor `0`, so it is not a
/// placeholder.
pub fn apply_hvac_filters(_previous: Option<&HvacStatus>, incoming: HvacStatus) -> HvacStatus {
    let triad = [incoming.temp_in_car, incoming.temp_out_car, incoming.pm];
    if !triad
        .iter()
        .all(|value| value.is_none_or(|value| value == 0.0))
    {
        return incoming;
    }
    let mut filtered = incoming;
    filtered.temp_in_car = None;
    filtered.temp_out_car = None;
    filtered.pm = None;
    filtered
}
