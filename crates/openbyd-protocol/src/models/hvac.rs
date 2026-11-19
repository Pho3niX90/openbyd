//! HVAC / climate control status model (`pybyd/models/hvac.py`).

use std::borrow::Cow;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields, byd_enum, enum_call, enum_field};
use super::realtime::{
    AirCirculationMode, SeatHeatVentState, StearingWheelHeat, is_steering_wheel_heating,
    steering_wheel_heat,
};
use crate::coerce::{self, ValidationError};
use crate::constants;
use crate::parity::Outcome;

byd_enum! {
    /// A/C compressor switch, separate from the overall [`HvacOverallStatus`].
    AcSwitch {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Compressor off.
        Off = 1 => "OFF",
        /// Compressor on.
        On = 2 => "ON",
    }
}

byd_enum! {
    /// Overall HVAC on/off status.
    HvacOverallStatus {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// On.
        On = 1 => "ON",
        /// Off.
        Off = 2 => "OFF",
    }
}

byd_enum! {
    /// A/C control mode.
    AirConditioningMode {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Off.
        Off = 0 => "OFF",
        /// Automatic.
        Auto = 1 => "AUTO",
        /// Manual.
        Manual = 2 => "MANUAL",
    }
}

byd_enum! {
    /// Airflow direction (which vents blow).
    HvacWindMode {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Off.
        Off = 0 => "OFF",
        /// Face vents.
        Face = 1 => "FACE",
        /// Face and foot vents.
        FaceFoot = 2 => "FACE_FOOT",
        /// Foot vents.
        Foot = 3 => "FOOT",
        /// Foot vents and windscreen.
        FootDefrost = 4 => "FOOT_DEFROST",
        /// Windscreen.
        Defrost = 5 => "DEFROST",
    }
}

byd_enum! {
    /// Fan speed, 1 (lowest) to 7 (highest).
    HvacWindPosition {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// Fan off.
        Off = 0 => "OFF",
        /// Speed 1.
        Position1 = 1 => "POSITION_1",
        /// Speed 2.
        Position2 = 2 => "POSITION_2",
        /// Speed 3.
        Position3 = 3 => "POSITION_3",
        /// Speed 4.
        Position4 = 4 => "POSITION_4",
        /// Speed 5.
        Position5 = 5 => "POSITION_5",
        /// Speed 6.
        Position6 = 6 => "POSITION_6",
        /// Speed 7.
        Position7 = 7 => "POSITION_7",
    }
}

/// Current HVAC / climate control state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HvacStatus {
    /// Original API response dict (the cleaned `statusNow` dict when the reply nests one).
    pub raw: Map<String, Value>,

    // --- A/C state ---
    /// A/C switch state.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub ac_switch: Option<AcSwitch>,
    /// Overall HVAC status.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub status: Option<HvacOverallStatus>,
    /// A/C mode (auto / manual).
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub air_conditioning_mode: Option<AirConditioningMode>,
    /// Airflow direction.
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub wind_mode: Option<HvacWindMode>,
    /// Fan speed (1-7).
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub wind_position: Option<HvacWindPosition>,
    /// Air circulation mode (external / internal).
    #[serde(serialize_with = "base::ser_opt_int_enum")]
    pub cycle_choice: Option<AirCirculationMode>,

    // --- Temperature ---
    /// Driver-side set temperature on BYD scale (1-17).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub main_setting_temp: Option<f64>,
    /// Driver-side set temperature (deg C, precise).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub main_setting_temp_new: Option<f64>,
    /// Passenger-side set temperature on BYD scale (1-17).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub copilot_setting_temp: Option<f64>,
    /// Passenger-side set temperature (deg C, precise).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub copilot_setting_temp_new: Option<f64>,
    /// Interior temperature (deg C). Sentinel `-129` -> `None`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub temp_in_car: Option<f64>,
    /// Exterior temperature (deg C). Sentinel `-129` -> `None`.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub temp_out_car: Option<f64>,
    /// 1 = dual-zone temperature adjustment supported.
    pub whether_support_adjust_temp: Option<i64>,
    /// Lowest settable temperature (deg C), eg 17 on the HAN.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub vde_temp_range_min: Option<f64>,
    /// Highest settable temperature (deg C), eg 33 on the HAN.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub vde_temp_range_max: Option<f64>,

    // --- Defrost ---
    /// Front defrost status (0=off, 1=on).
    pub front_defrost_status: Option<i64>,
    /// Rear (electric) defrost status (0=off, 1=on).
    pub electric_defrost_status: Option<i64>,
    /// Wiper heater status (0=off).
    pub wiper_heat_status: Option<i64>,

    // --- Seat heating / ventilation ---
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

    // --- Rapid temperature changes ---
    /// Rapid heating state.
    pub rapid_increase_temp_state: Option<i64>,
    /// Rapid cooling state.
    pub rapid_decrease_temp_state: Option<i64>,

    // --- Refrigerator ---
    /// Refrigerator state.
    pub refrigerator_state: Option<i64>,
    /// Refrigerator door state.
    pub refrigerator_door_state: Option<i64>,

    // --- Air quality ---
    /// PM2.5 reading.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub pm: Option<f64>,
    /// Exterior PM2.5 level.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub pm25_state_out_car: Option<f64>,
}

/// `_KEY_ALIASES`: only the shared `stearing` spelling.
const KEY_ALIASES: &[(&str, &str)] = base::COMMON_KEY_ALIASES;

/// `_unwrap_status_now`: when the reply nests the state under a `statusNow`
/// dict, that dict (key-aliased and sentinel-cleaned) replaces the whole
/// payload, so it is also what ends up in `raw`.
fn unwrap_status_now(values: &Value) -> Cow<'_, Value> {
    match values.get("statusNow") {
        Some(Value::Object(status_now)) if values.is_object() => {
            Cow::Owned(Value::Object(base::clean_dict(status_now, KEY_ALIASES)))
        }
        _ => Cow::Borrowed(values),
    }
}

impl HvacStatus {
    /// Whether the A/C is currently on.
    pub fn is_ac_on(&self) -> bool {
        self.status == Some(HvacOverallStatus::On)
    }

    /// Whether the A/C compressor is on, `None` when not reported.
    pub fn is_compressor_on(&self) -> Option<bool> {
        match self.ac_switch {
            None | Some(AcSwitch::Unknown) => None,
            Some(switch) => Some(switch == AcSwitch::On),
        }
    }

    /// °C offset of this vehicle's 1-17 temperature scale.
    pub fn temp_offset(&self) -> f64 {
        constants::temp_offset_from_range(self.vde_temp_range_min, self.vde_temp_range_max)
    }

    /// Whether the interior temperature reading is valid.
    pub fn interior_temp_available(&self) -> bool {
        self.temp_in_car.is_some()
    }

    /// Whether steering wheel heating is active; `None` when the state is unknown.
    pub fn is_steering_wheel_heating(&self) -> Option<bool> {
        is_steering_wheel_heating(self.steering_wheel_heat_state)
    }
}

impl BydModel for HvacStatus {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let values = unwrap_status_now(raw);
        let mut fields = Fields::cleaned(&values, KEY_ALIASES)?;
        let float = coerce::float;
        let int = coerce::int;
        let mut model = Self {
            raw: fields.raw(),
            ac_switch: fields.opt("ac_switch", enum_field),
            status: fields.opt("status", enum_field),
            air_conditioning_mode: fields.opt("air_conditioning_mode", enum_field),
            wind_mode: fields.opt("wind_mode", enum_field),
            wind_position: fields.opt("wind_position", enum_field),
            cycle_choice: fields.opt("cycle_choice", enum_field),
            main_setting_temp: fields.opt("main_setting_temp", float),
            main_setting_temp_new: fields.opt("main_setting_temp_new", float),
            copilot_setting_temp: fields.opt("copilot_setting_temp", float),
            copilot_setting_temp_new: fields.opt("copilot_setting_temp_new", float),
            temp_in_car: fields.opt("temp_in_car", float),
            temp_out_car: fields.opt("temp_out_car", float),
            whether_support_adjust_temp: fields.opt("whether_support_adjust_temp", int),
            vde_temp_range_min: fields.opt("vde_temp_range_min", float),
            vde_temp_range_max: fields.opt("vde_temp_range_max", float),
            front_defrost_status: fields.opt("front_defrost_status", int),
            electric_defrost_status: fields.opt("electric_defrost_status", int),
            wiper_heat_status: fields.opt("wiper_heat_status", int),
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
            rapid_increase_temp_state: fields.opt("rapid_increase_temp_state", int),
            rapid_decrease_temp_state: fields.opt("rapid_decrease_temp_state", int),
            refrigerator_state: fields.opt("refrigerator_state", int),
            refrigerator_door_state: fields.opt("refrigerator_door_state", int),
            pm: fields.opt("pm", float),
            pm25_state_out_car: fields.opt("pm25_state_out_car", float),
        };
        fields.finish()?;
        // _SENTINEL_RULES
        base::clear_if(&mut model.temp_in_car, base::is_temp_sentinel);
        base::clear_if(&mut model.temp_out_car, base::is_temp_sentinel);
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([
            ("is_ac_on", json!(self.is_ac_on())),
            ("is_compressor_on", json!(self.is_compressor_on())),
            ("temp_offset", coerce::float_json(self.temp_offset())),
            (
                "interior_temp_available",
                json!(self.interior_temp_available()),
            ),
            (
                "is_steering_wheel_heating",
                json!(self.is_steering_wheel_heating()),
            ),
        ])
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.hvac.AcSwitch" => enum_call::<AcSwitch>(args),
        "models.hvac.HvacOverallStatus" => enum_call::<HvacOverallStatus>(args),
        "models.hvac.AirConditioningMode" => enum_call::<AirConditioningMode>(args),
        "models.hvac.HvacWindMode" => enum_call::<HvacWindMode>(args),
        "models.hvac.HvacWindPosition" => enum_call::<HvacWindPosition>(args),
        "models.hvac.HvacStatus" => base::model_call::<HvacStatus>(args),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_now_is_unwrapped() {
        let status = HvacStatus::parse(
            &json!({"statusNow": {"status": 1, "acSwitch": 2, "stearingWheelHeatState": 2,
                                  "tempInCar": -129, "x": "--"}, "status": 2}),
            &Value::Null,
        )
        .unwrap();
        assert!(status.is_ac_on());
        assert_eq!(status.is_compressor_on(), Some(true));
        assert_eq!(status.is_steering_wheel_heating(), Some(true));
        assert!(!status.interior_temp_available());
        assert_eq!(status.temp_offset(), 14.0);
        assert!(!status.raw.contains_key("x"));
        assert_eq!(*unwrap_status_now(&json!([1])), json!([1]));
        assert_eq!(
            HvacStatus::parse(&json!({"acSwitch": -5}), &Value::Null)
                .unwrap()
                .is_compressor_on(),
            None
        );
    }
}
