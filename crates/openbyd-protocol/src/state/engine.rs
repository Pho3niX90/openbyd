//! The per-vehicle state engine (`pybyd/_state_engine.py`).
//!
//! - [`VehicleSnapshot`]: the composite of every known section of a vehicle's
//!   state, rebuilt on every accepted change.
//! - [`ProjectionSpec`]: what a command expects a field to read once it has
//!   worked (created by the capabilities).
//! - [`FieldProjection`]: a spec with its lifecycle: command id, TTL and
//!   creation time.
//! - [`VehicleStateEngine`]: merges incoming data section by section, runs
//!   the value-quality filters, manages projections and their guard window,
//!   and reports every rebuilt snapshot.
//!
//! pyBYD reads `time.monotonic()` for projection lifetimes. This crate never
//! reads the clock: every method that may create, expire or read a
//! projection takes `now`, the monotonic time in seconds.
//!
//! pyBYD serialises mutations behind an `asyncio.Lock` that callers also hold
//! while running a command. Here mutation needs `&mut self`, so the caller's
//! own lock (a `Mutex` around the engine) plays that part.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::patch::PatchSection;
use super::validators::{apply_gps_filters, apply_hvac_filters, apply_realtime_filters};
use crate::coerce;
use crate::models::base::BydModel;
use crate::models::charging::ChargingStatus;
use crate::models::energy::EnergyConsumption;
use crate::models::gps::GpsInfo;
use crate::models::hvac::HvacStatus;
use crate::models::realtime::VehicleRealtimeData;
use crate::models::smart_charging::SmartChargingSchedule;
use crate::models::vehicle::Vehicle;

/// The TTL of a projection unless the engine is given another (seconds).
pub const DEFAULT_PROJECTION_TTL: f64 = 30.0;

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

/// A model that is one section of the snapshot.
///
/// A projection writes a JSON value into a field without validation (pyBYD's
/// `model_copy(update=...)`), and `model_dump(mode="json")` then shows it as
/// pydantic's serializer does: a `float` field turns an integer or a boolean
/// into a float, an `int` field turns a boolean into an integer, and every
/// other field shows the value as it is. Comparing a model's field with a
/// projected value follows Python's `==`, under which a datetime or a nested
/// model never equals a JSON value. The field lists below carry the types
/// those rules need.
pub trait SectionModel: BydModel + Clone {
    /// The section's name in a [`ProjectionSpec`] (`"realtime"`, `"hvac"`, ...).
    const SECTION: &'static str;
    /// Fields annotated `float | None`.
    const FLOAT_FIELDS: &'static [&'static str];
    /// Fields annotated `int | None` or `int | str | None` (not enums): a
    /// `bool` written into one without validation is dumped as 0 or 1.
    const INT_FIELDS: &'static [&'static str];
    /// Fields holding a datetime or a nested model.
    const OPAQUE_FIELDS: &'static [&'static str];
}

impl SectionModel for VehicleRealtimeData {
    const SECTION: &'static str = "realtime";
    const FLOAT_FIELDS: &'static [&'static str] = &[
        "elec_percent",
        "power_battery",
        "endurance_mileage",
        "ev_endurance",
        "endurance_mileage_v2",
        "total_mileage",
        "total_mileage_v2",
        "speed",
        "temp_in_car",
        "main_setting_temp_new",
        "left_front_tire_pressure",
        "right_front_tire_pressure",
        "left_rear_tire_pressure",
        "right_rear_tire_pressure",
        "total_power",
        "gl",
        "oil_endurance",
        "oil_percent",
        "total_oil",
        "rate",
        "charge_power",
        "energy_consumption_ev",
        "energy_consumption_fuel",
        "nearest_energy_consumption_ev",
        "nearest_energy_consumption_fuel",
        "recent_50km_energy_ev",
        "recent_50km_energy_fuel",
        "total_energy_ev",
        "total_energy_fuel",
        "total_consumption_ev",
        "total_consumption_fuel",
        "total_consumption_en_ev",
        "total_consumption_en_fuel",
        "eq_consumption",
    ];
    const INT_FIELDS: &'static [&'static str] = &[
        "main_setting_temp",
        "air_run_state",
        "repair_mode_switch",
        "wait_status",
        "full_hour",
        "full_minute",
        "remaining_hours",
        "remaining_minutes",
        "booking_charge_state",
        "booking_charging_hour",
        "booking_charging_minute",
        "left_front_window_pct",
        "right_front_window_pct",
        "left_rear_window_pct",
        "right_rear_window_pct",
        "left_front_tire_status",
        "right_front_tire_status",
        "left_rear_tire_status",
        "right_rear_tire_status",
        "tirepressure_system",
        "rapid_tire_leak",
        "power_system",
        "engine_status",
        "epb",
        "eps",
        "esp",
        "abs_warning",
        "svs",
        "srs",
        "ect",
        "ect_value",
        "pwr",
        "battery_heat_state",
        "charge_heat_state",
        "upgrade_status",
        "oil_pressure_system",
        "braking_system",
        "charging_system",
        "steering_system",
        "ok_light",
        "power_battery_connection",
        "ins",
    ];
    const OPAQUE_FIELDS: &'static [&'static str] = &["timestamp"];
}

impl SectionModel for HvacStatus {
    const SECTION: &'static str = "hvac";
    const FLOAT_FIELDS: &'static [&'static str] = &[
        "main_setting_temp",
        "main_setting_temp_new",
        "copilot_setting_temp",
        "copilot_setting_temp_new",
        "temp_in_car",
        "temp_out_car",
        "vde_temp_range_min",
        "vde_temp_range_max",
        "pm",
        "pm25_state_out_car",
    ];
    const INT_FIELDS: &'static [&'static str] = &[
        "air_run_state",
        "whether_support_adjust_temp",
        "front_defrost_status",
        "electric_defrost_status",
        "wiper_heat_status",
        "rapid_increase_temp_state",
        "rapid_decrease_temp_state",
        "refrigerator_state",
        "refrigerator_door_state",
    ];
    const OPAQUE_FIELDS: &'static [&'static str] = &[];
}

impl SectionModel for GpsInfo {
    const SECTION: &'static str = "gps";
    const FLOAT_FIELDS: &'static [&'static str] = &["latitude", "longitude", "speed", "direction"];
    const INT_FIELDS: &'static [&'static str] = &[];
    const OPAQUE_FIELDS: &'static [&'static str] = &["gps_timestamp"];
}

impl SectionModel for ChargingStatus {
    const SECTION: &'static str = "charging";
    const FLOAT_FIELDS: &'static [&'static str] = &["charge_power"];
    const INT_FIELDS: &'static [&'static str] = &[
        "soc",
        "charging_state",
        "connect_state",
        "wait_status",
        "full_hour",
        "full_minute",
    ];
    const OPAQUE_FIELDS: &'static [&'static str] = &["update_time"];
}

impl SectionModel for EnergyConsumption {
    const SECTION: &'static str = "energy";
    const FLOAT_FIELDS: &'static [&'static str] = &[];
    const INT_FIELDS: &'static [&'static str] = &[];
    const OPAQUE_FIELDS: &'static [&'static str] = &[
        "self_graph",
        "cumulative_energy_consumption",
        "nearest_energy_consumption",
        "auto_model_graph",
        "timestamp",
    ];
}

impl SectionModel for SmartChargingSchedule {
    const SECTION: &'static str = "charging_schedule";
    const FLOAT_FIELDS: &'static [&'static str] = &[];
    const INT_FIELDS: &'static [&'static str] = &["soc"];
    const OPAQUE_FIELDS: &'static [&'static str] = &["charge", "journey", "update_time"];
}

/// How `model_dump(mode="json")` shows `value` written into `field` of `M` without validation.
pub fn dumped_override<M: SectionModel>(field: &str, value: &Value) -> Value {
    if M::FLOAT_FIELDS.contains(&field) {
        match value {
            Value::Bool(flag) => return json!(if *flag { 1.0 } else { 0.0 }),
            Value::Number(number) if !number.is_f64() => {
                return coerce::float_json(number.as_f64().unwrap_or(f64::NAN));
            }
            _ => {}
        }
    } else if let (true, Value::Bool(flag)) = (M::INT_FIELDS.contains(&field), value) {
        return json!(i64::from(*flag));
    }
    value.clone()
}

/// A number as Python sees it for `==`: booleans are the integers 0 and 1.
#[derive(Clone, Copy)]
enum PyNumber {
    Int(i128),
    Float(f64),
}

fn py_number(value: &Value) -> Option<PyNumber> {
    match value {
        Value::Bool(flag) => Some(PyNumber::Int(i128::from(*flag))),
        Value::Number(number) => Some(if let Some(int) = number.as_i64() {
            PyNumber::Int(i128::from(int))
        } else if let Some(int) = number.as_u64() {
            PyNumber::Int(i128::from(int))
        } else {
            PyNumber::Float(number.as_f64().unwrap_or(f64::NAN))
        }),
        _ => None,
    }
}

fn float_equals_int(float: f64, int: i128) -> bool {
    // Python compares a float and an int exactly.
    float.is_finite() && float.fract() == 0.0 && float.abs() < 1.0e38 && float as i128 == int
}

/// Python's `a == b` for two JSON values (numbers compare by value, `True == 1`).
pub fn py_equals(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (py_number(a), py_number(b)) {
        return match (x, y) {
            (PyNumber::Int(x), PyNumber::Int(y)) => x == y,
            (PyNumber::Float(x), PyNumber::Float(y)) => x == y,
            (PyNumber::Int(int), PyNumber::Float(float))
            | (PyNumber::Float(float), PyNumber::Int(int)) => float_equals_int(float, int),
        };
    }
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| py_equals(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, x)| y.get(key).is_some_and(|y| py_equals(x, y)))
        }
        _ => false,
    }
}

/// A section's attributes as `getattr` sees them: its dumped fields, then its properties.
struct Attributes {
    dump: Map<String, Value>,
    properties: Map<String, Value>,
}

impl Attributes {
    fn of<M: SectionModel>(model: &M) -> Self {
        let dump = match model.dump() {
            Value::Object(dump) => dump,
            _ => Map::new(),
        };
        Self {
            dump,
            properties: model.properties(),
        }
    }

    /// `getattr(model, field) == expected`, or `None` when the model has no such attribute.
    ///
    /// A non-finite float dumps as a string (`"NaN"`) and equals no JSON
    /// value; no property returns a string, so a string there is such a float.
    fn confirms<M: SectionModel>(&self, field: &str, expected: &Value) -> Option<bool> {
        if let Some(actual) = self.dump.get(field) {
            if M::OPAQUE_FIELDS.contains(&field) {
                return Some(actual.is_null() && expected.is_null());
            }
            if M::FLOAT_FIELDS.contains(&field) && actual.is_string() {
                return Some(false);
            }
            return Some(py_equals(actual, expected));
        }
        let actual = self.properties.get(field)?;
        Some(!actual.is_string() && py_equals(actual, expected))
    }
}

/// One section of a [`VehicleSnapshot`]: the accepted model with the active projections over it.
///
/// pyBYD's projected section is `base.model_copy(update=overrides)`, whose
/// overridden fields hold whatever the command expects, valid for the field's
/// type or not. Here the accepted model stays typed and the overrides stay
/// JSON; [`Projected::field`] and [`Projected::dump`] show them together.
#[derive(Debug, Clone, PartialEq)]
pub struct Projected<M> {
    base: Arc<M>,
    overrides: Map<String, Value>,
}

impl<M: SectionModel> Projected<M> {
    /// A section with no projection over it.
    pub fn new(base: Arc<M>) -> Self {
        Self {
            base,
            overrides: Map::new(),
        }
    }

    /// The accepted (filtered) model, without projections.
    pub fn base(&self) -> &M {
        &self.base
    }

    /// The shared accepted model; the same `Arc` until the section is next updated.
    pub fn base_arc(&self) -> &Arc<M> {
        &self.base
    }

    /// The projected values by field name, as the commands gave them.
    ///
    /// A name that is not a field of the model is kept here but never shows
    /// in [`Projected::dump`], as in pyBYD.
    pub fn overrides(&self) -> &Map<String, Value> {
        &self.overrides
    }

    /// Whether any projection is laid over the accepted model.
    pub fn is_projected(&self) -> bool {
        !self.overrides.is_empty()
    }

    /// A field as `model_dump(mode="json")` of the projected section shows it.
    pub fn field(&self, name: &str) -> Option<Value> {
        self.dump().get(name).cloned()
    }

    /// `model_dump(mode="json")` of the projected section.
    pub fn dump(&self) -> Value {
        let mut dump = self.base.dump();
        if let Value::Object(fields) = &mut dump {
            for (name, value) in &self.overrides {
                if let Some(slot) = fields.get_mut(name) {
                    *slot = dumped_override::<M>(name, value);
                }
            }
        }
        dump
    }
}

/// The composite of every known section of a vehicle's state.
///
/// A new one is built on every accepted change. A section is `None` until
/// its first payload has been accepted.
#[derive(Debug, Clone, PartialEq)]
pub struct VehicleSnapshot {
    /// The vehicle the state belongs to.
    pub vehicle: Arc<Vehicle>,
    /// Realtime data: doors, locks, charge, range, tyres, consumption.
    pub realtime: Option<Projected<VehicleRealtimeData>>,
    /// Climate control.
    pub hvac: Option<Projected<HvacStatus>>,
    /// Location.
    pub gps: Option<Projected<GpsInfo>>,
    /// Live charging state.
    pub charging: Option<Projected<ChargingStatus>>,
    /// Energy consumption.
    pub energy: Option<Projected<EnergyConsumption>>,
    /// The configured smart-charging schedule.
    pub charging_schedule: Option<Projected<SmartChargingSchedule>>,
}

fn dump_section<M: SectionModel>(section: Option<&Projected<M>>) -> Value {
    section.map_or(Value::Null, Projected::dump)
}

impl VehicleSnapshot {
    /// A snapshot with only the vehicle known.
    pub fn new(vehicle: Arc<Vehicle>) -> Self {
        Self {
            vehicle,
            realtime: None,
            hvac: None,
            gps: None,
            charging: None,
            energy: None,
            charging_schedule: None,
        }
    }

    /// Every section's `model_dump(mode="json")` (or `null`), keyed by section name.
    pub fn dump(&self) -> Value {
        json!({
            "vehicle": self.vehicle.dump(),
            "realtime": dump_section(self.realtime.as_ref()),
            "hvac": dump_section(self.hvac.as_ref()),
            "gps": dump_section(self.gps.as_ref()),
            "charging": dump_section(self.charging.as_ref()),
            "energy": dump_section(self.energy.as_ref()),
            "charging_schedule": dump_section(self.charging_schedule.as_ref()),
        })
    }
}

// ---------------------------------------------------------------------------
// Projections
// ---------------------------------------------------------------------------

/// What a command expects one field to read once it has worked.
///
/// Created by the capabilities; the engine wraps it in a [`FieldProjection`].
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionSpec {
    /// The section: `"realtime"`, `"hvac"`, `"gps"`, `"charging"`, `"energy"`
    /// or `"charging_schedule"`. Any other name is kept until it expires but
    /// never shows.
    pub section: String,
    /// The field within the section's model, for example `"left_front_door_lock"`.
    pub field_name: String,
    /// The value the field should have after the command succeeds.
    pub expected_value: Value,
}

impl ProjectionSpec {
    /// A spec for `section.field_name == expected_value`.
    pub fn new(
        section: impl Into<String>,
        field_name: impl Into<String>,
        expected_value: Value,
    ) -> Self {
        Self {
            section: section.into(),
            field_name: field_name.into(),
            expected_value,
        }
    }

    /// The spec as `dataclasses.asdict` gives it.
    pub fn to_json(&self) -> Value {
        json!({
            "section": self.section,
            "field_name": self.field_name,
            "expected_value": self.expected_value,
        })
    }
}

/// One field-level projection with its lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldProjection {
    /// See [`ProjectionSpec::section`].
    pub section: String,
    /// See [`ProjectionSpec::field_name`].
    pub field_name: String,
    /// See [`ProjectionSpec::expected_value`].
    pub expected_value: Value,
    /// The command that registered it (`"cmd-1"`, `"cmd-2"`, ...).
    pub command_id: String,
    /// Monotonic time of registration, in seconds.
    pub created_at: f64,
    /// Seconds it stays active.
    pub ttl: f64,
}

impl FieldProjection {
    /// Whether the projection has outlived its TTL at monotonic time `now`.
    ///
    /// Strictly greater: a projection is still active exactly `ttl` seconds after registration.
    pub fn is_expired(&self, now: f64) -> bool {
        (now - self.created_at) > self.ttl
    }

    /// The projection as `dataclasses.asdict` gives it.
    pub fn to_json(&self) -> Value {
        json!({
            "section": self.section,
            "field_name": self.field_name,
            "expected_value": self.expected_value,
            "command_id": self.command_id,
            "created_at": coerce::float_json(self.created_at),
            "ttl": coerce::float_json(self.ttl),
        })
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Called with the VIN and the new snapshot every time the snapshot is rebuilt.
pub type StateChangedCallback = Box<dyn FnMut(&str, &VehicleSnapshot) + Send>;

/// The state of one vehicle, merged from HTTP polls and MQTT pushes.
///
/// It manages:
///
/// - **partial merges**: each `update_*` replaces one section, and
///   `patch_status_push` lays a partial push over the sections held;
/// - **value-quality filters**: GPS Null Island, realtime zero-drop and
///   friends, the HVAC placeholder (see [`super::validators`]);
/// - **command projections**: the state a command expects, laid over the
///   accepted data;
/// - **guard windows**: while a projection is active, data that contradicts
///   it does not show through;
/// - **rollback**: a failed command's projections are removed.
///
/// Every rebuild of the snapshot calls the `on_state_changed` callback, if
/// any. pyBYD swallows exceptions from the callback; a Rust callback that
/// panics is not caught.
pub struct VehicleStateEngine {
    vin: String,
    vehicle: Arc<Vehicle>,
    on_state_changed: Option<StateChangedCallback>,
    projection_ttl: f64,
    next_command_id: u64,
    base_realtime: Option<Arc<VehicleRealtimeData>>,
    base_hvac: Option<Arc<HvacStatus>>,
    base_gps: Option<Arc<GpsInfo>>,
    base_charging: Option<Arc<ChargingStatus>>,
    base_energy: Option<Arc<EnergyConsumption>>,
    base_charging_schedule: Option<Arc<SmartChargingSchedule>>,
    projections: Vec<FieldProjection>,
    snapshot: VehicleSnapshot,
}

impl VehicleStateEngine {
    /// An engine for `vin` that knows nothing but the vehicle yet.
    ///
    /// `projection_ttl` is how long new projections stay active, in seconds
    /// ([`DEFAULT_PROJECTION_TTL`] in pyBYD).
    pub fn new(vin: impl Into<String>, vehicle: Vehicle, projection_ttl: f64) -> Self {
        let vehicle = Arc::new(vehicle);
        Self {
            vin: vin.into(),
            snapshot: VehicleSnapshot::new(Arc::clone(&vehicle)),
            vehicle,
            on_state_changed: None,
            projection_ttl,
            next_command_id: 0,
            base_realtime: None,
            base_hvac: None,
            base_gps: None,
            base_charging: None,
            base_energy: None,
            base_charging_schedule: None,
            projections: Vec::new(),
        }
    }

    /// Sets the callback run after every snapshot rebuild (pyBYD's `on_state_changed`).
    pub fn set_on_state_changed(&mut self, callback: Option<StateChangedCallback>) {
        self.on_state_changed = callback;
    }

    /// The VIN the engine was made for.
    pub fn vin(&self) -> &str {
        &self.vin
    }

    /// The current projected snapshot.
    pub fn snapshot(&self) -> &VehicleSnapshot {
        &self.snapshot
    }

    /// The TTL given to new projections, in seconds.
    pub fn projection_ttl(&self) -> f64 {
        self.projection_ttl
    }

    /// The projections still active at monotonic time `now` (a copy).
    pub fn active_projections(&self, now: f64) -> Vec<FieldProjection> {
        self.projections
            .iter()
            .filter(|projection| !projection.is_expired(now))
            .cloned()
            .collect()
    }

    /// Every projection the engine holds, including expired ones not yet cleaned
    /// out (they go at the next rebuild).
    pub fn held_projections(&self) -> &[FieldProjection] {
        &self.projections
    }

    /// Registers the projections of one command and rebuilds the snapshot.
    ///
    /// Returns the command id grouping them. Only the latest intent per field
    /// is projected: projections of earlier commands on the same
    /// `(section, field)` are dropped first, otherwise an older projection
    /// (ON) would survive once a newer one (OFF) is confirmed. pyBYD requires
    /// the engine lock to be held; `&mut self` does that here.
    pub fn register_projections(&mut self, specs: &[ProjectionSpec], now: f64) -> String {
        let command_id = self.generate_command_id();
        self.projections.retain(|projection| {
            !specs.iter().any(|spec| {
                spec.section == projection.section && spec.field_name == projection.field_name
            })
        });
        for spec in specs {
            self.projections.push(FieldProjection {
                section: spec.section.clone(),
                field_name: spec.field_name.clone(),
                expected_value: spec.expected_value.clone(),
                command_id: command_id.clone(),
                created_at: now,
                ttl: self.projection_ttl,
            });
        }
        self.rebuild_snapshot(now);
        command_id
    }

    /// Removes every projection of `command_id` (a failed command) and rebuilds the snapshot.
    pub fn rollback_projections(&mut self, command_id: &str, now: f64) {
        self.projections
            .retain(|projection| projection.command_id != command_id);
        self.rebuild_snapshot(now);
    }

    /// Merges realtime data (HTTP poll or MQTT `vehicleInfo` push) through the
    /// realtime filters and the guard window.
    pub fn update_realtime(&mut self, data: VehicleRealtimeData, now: f64) {
        let validated = Arc::new(apply_realtime_filters(self.base_realtime.as_deref(), data));
        self.reconcile_projections(validated.as_ref(), now);
        self.base_realtime = Some(validated);
        self.rebuild_snapshot(now);
    }

    /// Lays the fields a partial realtime model carries over the realtime
    /// data already held ([`Self::patch_status_push`] with only realtime).
    pub fn patch_realtime(&mut self, partial: VehicleRealtimeData, now: f64) {
        self.patch_status_push(Some(partial), None, None, now);
    }

    /// Merges the partial models of one MQTT `vehicleStatusPush` into the
    /// sections already held, with one snapshot rebuild for all of them.
    ///
    /// A section that has not had a full payload yet is skipped, so a push
    /// never shows a half-empty section. Values a partial does not carry
    /// (`None`, an enum's `UNKNOWN` or `NO_DATA`) leave the known reading in
    /// place, and `raw` is merged, not replaced: see
    /// [`PatchSection::build_section_patch`]. The patched realtime and HVAC
    /// sections then go through the same filters as a full payload, with the
    /// section before the patch as the previous value, and every patched
    /// section through the guard window.
    ///
    /// When no section changes (none is held yet, or the partials carry
    /// nothing) the snapshot is not rebuilt and the callback is not called.
    pub fn patch_status_push(
        &mut self,
        realtime: Option<VehicleRealtimeData>,
        hvac: Option<HvacStatus>,
        charging: Option<ChargingStatus>,
        now: f64,
    ) {
        let mut changed = false;

        if let (Some(partial), Some(base)) = (realtime, self.base_realtime.clone())
            && let Some(patched) = VehicleRealtimeData::build_section_patch(&base, partial)
        {
            let validated = Arc::new(apply_realtime_filters(Some(&base), patched));
            self.reconcile_projections(validated.as_ref(), now);
            self.base_realtime = Some(validated);
            changed = true;
        }

        if let (Some(partial), Some(base)) = (hvac, self.base_hvac.clone())
            && let Some(patched) = HvacStatus::build_section_patch(&base, partial)
        {
            let validated = Arc::new(apply_hvac_filters(Some(&base), patched));
            self.reconcile_projections(validated.as_ref(), now);
            self.base_hvac = Some(validated);
            changed = true;
        }

        if let (Some(partial), Some(base)) = (charging, self.base_charging.clone())
            && let Some(patched) = ChargingStatus::build_section_patch(&base, partial)
        {
            self.reconcile_projections(&patched, now);
            self.base_charging = Some(Arc::new(patched));
            changed = true;
        }

        if changed {
            self.rebuild_snapshot(now);
        }
    }

    /// Merges HVAC data through the HVAC placeholder filter and the guard window.
    pub fn update_hvac(&mut self, data: HvacStatus, now: f64) {
        let validated = Arc::new(apply_hvac_filters(self.base_hvac.as_deref(), data));
        self.reconcile_projections(validated.as_ref(), now);
        self.base_hvac = Some(validated);
        self.rebuild_snapshot(now);
    }

    /// Merges GPS data through the coordinate guard and the guard window.
    ///
    /// A fix without usable coordinates leaves the previous one in place (and
    /// nothing at all before the first usable fix); projections are then
    /// reconciled against whichever fix was kept.
    pub fn update_gps(&mut self, data: GpsInfo, now: f64) {
        let keep_incoming = apply_gps_filters(self.base_gps.as_deref(), Some(&data))
            .is_some_and(|kept| std::ptr::eq(kept, &data));
        let validated = if keep_incoming {
            Some(Arc::new(data))
        } else {
            self.base_gps.clone()
        };
        if let Some(validated) = &validated {
            self.reconcile_projections(validated.as_ref(), now);
        }
        self.base_gps = validated;
        self.rebuild_snapshot(now);
    }

    /// Merges live charging data (HTTP or MQTT `smartCharge`) through the guard window.
    pub fn update_charging(&mut self, data: ChargingStatus, now: f64) {
        self.reconcile_projections(&data, now);
        self.base_charging = Some(Arc::new(data));
        self.rebuild_snapshot(now);
    }

    /// Merges energy consumption data (HTTP or MQTT `energyConsumption`).
    pub fn update_energy(&mut self, data: EnergyConsumption, now: f64) {
        self.reconcile_projections(&data, now);
        self.base_energy = Some(Arc::new(data));
        self.rebuild_snapshot(now);
    }

    /// Merges the smart-charging schedule. Only HTTP refreshes carry it, so an
    /// MQTT charging push can never clobber it.
    pub fn update_charging_schedule(&mut self, data: SmartChargingSchedule, now: f64) {
        self.reconcile_projections(&data, now);
        self.base_charging_schedule = Some(Arc::new(data));
        self.rebuild_snapshot(now);
    }

    fn generate_command_id(&mut self) -> String {
        self.next_command_id += 1;
        format!("cmd-{}", self.next_command_id)
    }

    /// Drops projections confirmed by `data` and every expired one.
    ///
    /// A projection is confirmed when `data`'s field equals the expected
    /// value (Python `==`). A contradicting projection, or one whose field the
    /// model does not have, stays: that is the guard window, during which the
    /// projected value keeps overlaying the base data.
    fn reconcile_projections<M: SectionModel>(&mut self, data: &M, now: f64) {
        let mut attributes: Option<Attributes> = None;
        self.projections.retain(|projection| {
            if projection.is_expired(now) {
                return false;
            }
            if projection.section != M::SECTION {
                return true;
            }
            let attributes = attributes.get_or_insert_with(|| Attributes::of(data));
            let confirmed = attributes
                .confirms::<M>(&projection.field_name, &projection.expected_value)
                .unwrap_or(false);
            !confirmed
        });
    }

    fn project<M: SectionModel>(&self, base: Option<&Arc<M>>, now: f64) -> Option<Projected<M>> {
        let mut projected = Projected::new(Arc::clone(base?));
        for projection in &self.projections {
            if projection.section == M::SECTION && !projection.is_expired(now) {
                projected.overrides.insert(
                    projection.field_name.clone(),
                    projection.expected_value.clone(),
                );
            }
        }
        Some(projected)
    }

    /// Cleans out expired projections and rebuilds the snapshot from the base
    /// state and the active projections.
    fn rebuild_snapshot(&mut self, now: f64) {
        self.projections
            .retain(|projection| !projection.is_expired(now));
        self.snapshot = VehicleSnapshot {
            vehicle: Arc::clone(&self.vehicle),
            realtime: self.project(self.base_realtime.as_ref(), now),
            hvac: self.project(self.base_hvac.as_ref(), now),
            gps: self.project(self.base_gps.as_ref(), now),
            charging: self.project(self.base_charging.as_ref(), now),
            energy: self.project(self.base_energy.as_ref(), now),
            charging_schedule: self.project(self.base_charging_schedule.as_ref(), now),
        };
        if let Some(callback) = self.on_state_changed.as_mut() {
            callback(&self.vin, &self.snapshot);
        }
    }
}
