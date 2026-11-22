//! Merging a partial model onto an accepted section (`_build_section_patch`
//! in `pybyd/_state_engine.py`).
//!
//! An MQTT `vehicleStatusPush` carries only the sections of a car's state
//! that changed, so the models built from it are mostly empty. The engine
//! lays the fields such a partial model carries over the section it already
//! holds, instead of replacing the section as a full payload does.
//!
//! # Which fields a partial carries
//!
//! pyBYD walks the partial's `model_fields_set` (the fields the payload gave
//! after sentinel cleaning) and takes each one unless it is `raw` or `vin`,
//! its value is `None`, or it is a `BydEnum` member named `UNKNOWN` or
//! `NO_DATA`. A Rust model is a struct without a record of which keys the
//! payload had, and it does not need one: every field of the three patched
//! models defaults to `None`, to an enum's `UNKNOWN`, or is `raw` or `vin`,
//! so a field the payload did not give always holds a value the walk skips
//! anyway. "Carried" is therefore read off the value alone: it is not
//! `None` and not an `UNKNOWN` / `NO_DATA` member. (A model changed by hand
//! after parsing counts the same way, as pydantic's `model_copy(update=...)`
//! adds what it changes to the set.) The parity cases of
//! `state._build_section_patch` compare this with pyBYD over payloads with
//! fields at their defaults, sentinels, aliases and unknown keys.
//!
//! [`PatchSection::build_section_patch`] names every field of its model, so a
//! field added to a model does not compile until it is given a place here.

use serde_json::{Map, Value};

use super::engine::SectionModel;
use crate::coerce::{IntEnum, IntOrStr};
use crate::models::base::{PyDateTime, PyEnum};
use crate::models::charging::ChargingStatus;
use crate::models::hvac::HvacStatus;
use crate::models::realtime::VehicleRealtimeData;

/// A section of the snapshot a `vehicleStatusPush` can update in part.
pub trait PatchSection: SectionModel {
    /// pyBYD's `_build_section_patch(base, partial)`: `base` with the fields
    /// `partial` carries laid over it (see the [module documentation](self)),
    /// or `None` when `partial` carries nothing, so the section is left alone.
    ///
    /// The copy's `raw` is `base`'s with `partial`'s merged in: a key both
    /// have keeps its place and takes `partial`'s value, new keys follow.
    /// When a charging state other than `1` (charging) arrives without a
    /// charge power, the charge power and its unit are cleared: the car
    /// stops reporting them once it stops charging. Nothing is validated
    /// again (pyBYD's `model_copy(update=...)`).
    fn build_section_patch(base: &Self, partial: Self) -> Option<Self>;
}

/// The value of a field of a patched model.
trait PatchValue {
    /// Whether the value stands for "no reading": a `BydEnum` member named
    /// `UNKNOWN` or `NO_DATA`. Only enums have such values.
    fn is_placeholder(&self) -> bool {
        false
    }
}

impl PatchValue for f64 {}
impl PatchValue for i64 {}
impl PatchValue for bool {}
impl PatchValue for String {}
impl PatchValue for IntOrStr {}
impl PatchValue for PyDateTime {}

impl<E: PyEnum> PatchValue for E {
    fn is_placeholder(&self) -> bool {
        matches!(self.name(), "UNKNOWN" | "NO_DATA")
    }
}

/// Writes the fields a partial carries into the copy of the base (pyBYD's
/// `updates` dict, applied as it is collected).
#[derive(Default)]
struct Patch {
    /// Whether any field was written (`updates` is not empty).
    changed: bool,
}

impl Patch {
    /// A field that is `None` by default: written when the partial has a
    /// value that is not a placeholder.
    fn optional<T: PatchValue>(&mut self, base: &mut Option<T>, partial: Option<T>) {
        if let Some(value) = partial
            && !value.is_placeholder()
        {
            *base = Some(value);
            self.changed = true;
        }
    }

    /// A field that is never `None` (an enum that defaults to `UNKNOWN`):
    /// written unless the partial's value is a placeholder.
    fn required<T: PatchValue>(&mut self, base: &mut T, partial: T) {
        if !partial.is_placeholder() {
            *base = partial;
            self.changed = true;
        }
    }
}

/// Takes `$partial` apart, writing its `optional` and `required` fields into
/// `$patched` through `$patch` and leaving the `kept` ones as variables
/// (`name: _` for one that is not used).
///
/// The pattern names every field, so a field missing from the lists is a
/// compile error, and so is one in the wrong list (an `Option` is not a
/// [`PatchValue`]).
macro_rules! merge_fields {
    (
        $patch:ident, $patched:ident, $partial:ident: $model:ident,
        kept: [$($kept:ident $(: $ignored:pat)?),* $(,)?],
        required: [$($required:ident),* $(,)?],
        optional: [$($optional:ident),* $(,)?] $(,)?
    ) => {
        let $model { $($kept $(: $ignored)?,)* $($required,)* $($optional,)* } = $partial;
        $( $patch.required(&mut $patched.$required, $required); )*
        $( $patch.optional(&mut $patched.$optional, $optional); )*
    };
}

/// `{**base, **partial}`, written into `base`.
fn merge_raw(base: &mut Map<String, Value>, partial: Map<String, Value>) {
    for (key, value) in partial {
        base.insert(key, value);
    }
}

/// Whether a carried charging state says the car is not charging
/// (`int(state) != 1`); `None` when the partial carries no such state.
fn left_charging(state: Option<i64>) -> bool {
    state.is_some_and(|state| state != 1)
}

impl PatchSection for VehicleRealtimeData {
    fn build_section_patch(base: &Self, partial: Self) -> Option<Self> {
        // `charge_state` and `charging_state` are both looked at, each only
        // when the partial carries it.
        let carried = |state: crate::models::realtime::ChargingState| {
            (!state.is_placeholder()).then(|| state.value())
        };
        let clears_charge_power = partial.charge_power.is_none()
            && (left_charging(partial.charge_state.and_then(carried))
                || left_charging(carried(partial.charging_state)));

        let mut patched = base.clone();
        let mut patch = Patch::default();
        merge_fields!(
            patch, patched, partial: VehicleRealtimeData,
            kept: [raw],
            required: [online_state, connect_state, vehicle_state, charging_state, sentry_status],
            optional: [
                request_serial, elec_percent, power_battery, endurance_mileage, ev_endurance,
                endurance_mileage_v2, endurance_mileage_v2_unit, total_mileage, total_mileage_v2,
                total_mileage_v2_unit, speed, power_gear, temp_in_car, main_setting_temp,
                main_setting_temp_new, air_run_state, main_seat_heat_state,
                main_seat_ventilation_state, copilot_seat_heat_state,
                copilot_seat_ventilation_state, steering_wheel_heat_state, lr_seat_heat_state,
                lr_seat_ventilation_state, rr_seat_heat_state, rr_seat_ventilation_state,
                charge_state, wait_status, full_hour, full_minute, remaining_hours,
                remaining_minutes, booking_charge_state, booking_charging_hour,
                booking_charging_minute, charge_start_time_hour, charge_start_time_min,
                left_front_door, right_front_door, left_rear_door,
                right_rear_door, trunk_lid, trunk_lid_lower, sliding_door, forehold,
                left_front_door_lock, right_front_door_lock, left_rear_door_lock,
                right_rear_door_lock, sliding_door_lock, left_front_window, right_front_window,
                left_rear_window, right_rear_window, skylight, left_front_window_pct,
                right_front_window_pct, left_rear_window_pct, right_rear_window_pct,
                left_front_tire_pressure, right_front_tire_pressure, left_rear_tire_pressure,
                right_rear_tire_pressure, left_front_tire_status, right_front_tire_status,
                left_rear_tire_status, right_rear_tire_status, tire_press_unit,
                tirepressure_system, rapid_tire_leak, total_power, gl, total_energy,
                nearest_energy_consumption, nearest_energy_consumption_unit, recent_50km_energy,
                oil_endurance, oil_percent, total_oil, power_system, engine_status, epb, eps, esp,
                abs_warning, svs, srs, ect, ect_value, pwr, battery_heat_state, charge_heat_state,
                upgrade_status, lr_third_heat_state, lr_third_ventilation_state,
                rr_third_heat_state, rr_third_ventilation_state, rate, charge_power,
                charge_power_unit, less_one_min, energy_consumption, total_consumption,
                total_consumption_en, energy_consumption_ev, energy_consumption_ev_unit,
                energy_consumption_fuel, energy_consumption_fuel_unit,
                nearest_energy_consumption_ev, nearest_energy_consumption_ev_unit,
                nearest_energy_consumption_fuel, nearest_energy_consumption_fuel_unit,
                recent_50km_energy_ev, recent_50km_energy_ev_unit, recent_50km_energy_fuel,
                recent_50km_energy_fuel_unit, total_energy_ev, total_energy_ev_unit,
                total_energy_fuel, total_energy_fuel_unit, total_consumption_ev,
                total_consumption_ev_unit, total_consumption_fuel, total_consumption_fuel_unit,
                total_consumption_en_ev, total_consumption_en_ev_unit, total_consumption_en_fuel,
                total_consumption_en_fuel_unit, eq_consumption, eq_consumption_unit,
                oil_pressure_system, braking_system, charging_system, steering_system, ok_light,
                repair_mode_switch, vehicle_time_zone, power_battery_connection, ins, timestamp,
            ],
        );
        if !patch.changed {
            return None;
        }
        if clears_charge_power {
            patched.charge_power = None;
            patched.charge_power_unit = None;
        }
        merge_raw(&mut patched.raw, raw);
        Some(patched)
    }
}

impl PatchSection for HvacStatus {
    fn build_section_patch(base: &Self, partial: Self) -> Option<Self> {
        let mut patched = base.clone();
        let mut patch = Patch::default();
        merge_fields!(
            patch, patched, partial: HvacStatus,
            kept: [raw],
            required: [],
            optional: [
                ac_switch, status, air_conditioning_mode, wind_mode, wind_position, cycle_choice,
                air_run_state, main_setting_temp, main_setting_temp_new, copilot_setting_temp,
                copilot_setting_temp_new, temp_in_car, temp_out_car, whether_support_adjust_temp,
                vde_temp_range_min, vde_temp_range_max, front_defrost_status,
                electric_defrost_status, wiper_heat_status, main_seat_heat_state,
                main_seat_ventilation_state, copilot_seat_heat_state,
                copilot_seat_ventilation_state, steering_wheel_heat_state, lr_seat_heat_state,
                lr_seat_ventilation_state, rr_seat_heat_state, rr_seat_ventilation_state,
                rapid_increase_temp_state, rapid_decrease_temp_state, refrigerator_state,
                refrigerator_door_state, pm, pm25_state_out_car,
            ],
        );
        if !patch.changed {
            return None;
        }
        merge_raw(&mut patched.raw, raw);
        Some(patched)
    }
}

impl PatchSection for ChargingStatus {
    fn build_section_patch(base: &Self, partial: Self) -> Option<Self> {
        let clears_charge_power =
            partial.charge_power.is_none() && left_charging(partial.charging_state);

        let mut patched = base.clone();
        let mut patch = Patch::default();
        merge_fields!(
            patch, patched, partial: ChargingStatus,
            // `vin` is never patched: a push names no VIN in its sections.
            kept: [raw, vin: _],
            required: [],
            optional: [
                soc, charging_state, connect_state, wait_status, full_hour, full_minute,
                charge_power, charge_power_unit, update_time,
            ],
        );
        if !patch.changed {
            return None;
        }
        if clears_charge_power {
            patched.charge_power = None;
            patched.charge_power_unit = None;
        }
        merge_raw(&mut patched.raw, raw);
        Some(patched)
    }
}
