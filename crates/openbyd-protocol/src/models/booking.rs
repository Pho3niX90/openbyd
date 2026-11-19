//! Scheduled-climate booking list model (`pybyd/models/booking.py`).
//!
//! `/control/getBookingList` returns the vehicle's scheduled HVAC timers
//! (the `BOOKINGAIR` bookings), e.g.:
//!
//! ```json
//! {"allowSetting": 0, "configData": 0,
//!  "listInfo": [{"bookingTime": 1780938216, "mainSettingTemp": 23,
//!                "mainSettingTempNew": 23.0, "timeSpan": 1, "acSwitch": 0,
//!                "cycleMode": 2, "windLevel": 0, "windMode": 0,
//!                "airConditionTempRange": 7, "bookingId": 1216038691305533440}]}
//! ```
//!
//! Note: this endpoint has been observed to return an empty `{}` intermittently
//! even when a booking is set, so callers should treat an empty list as
//! "unknown / try again" rather than "no bookings".

use serde::Serialize;
use serde_json::{Map, Value};

use super::base::{self, BydModel, Fields, PyDateTime};
use crate::coerce::{self, ValidationError};
use crate::parity::Outcome;

/// A single scheduled-climate timer (one `listInfo` entry).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClimateBooking {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Server-assigned booking id (64-bit); required to modify/remove it.
    pub booking_id: Option<i64>,
    /// When the climate timer fires (parsed from epoch seconds).
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub booking_time: Option<PyDateTime>,
    /// Driver setpoint on BYD's raw scale.
    pub main_setting_temp: Option<i64>,
    /// Driver setpoint in degrees Celsius.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub main_setting_temp_new: Option<f64>,
    /// Run-duration code: 1=10min, 2=15min, 3=20min, 4=25min, 5=30min.
    pub time_span: Option<i64>,
    /// A/C on (1) / off (0) for the scheduled run.
    pub ac_switch: Option<i64>,
    /// Air recirculation: 1=fresh, 2=recirculate.
    pub cycle_mode: Option<i64>,
    /// Fan level.
    pub wind_level: Option<i64>,
    /// Fan mode.
    pub wind_mode: Option<i64>,
    /// Temperature range code.
    pub air_condition_temp_range: Option<i64>,
}

impl BydModel for ClimateBooking {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let values = base::clean_byd_values(raw, &[]);
        let mut fields = Fields::new(&values)?;
        let model = Self {
            raw: fields.raw(),
            booking_id: fields.opt("booking_id", coerce::int),
            booking_time: fields.get("booking_time", None, base::byd_timestamp),
            main_setting_temp: fields.opt("main_setting_temp", coerce::int),
            main_setting_temp_new: fields.opt("main_setting_temp_new", coerce::float),
            time_span: fields.opt("time_span", coerce::int),
            ac_switch: fields.opt("ac_switch", coerce::int),
            cycle_mode: fields.opt("cycle_mode", coerce::int),
            wind_level: fields.opt("wind_level", coerce::int),
            wind_mode: fields.opt("wind_mode", coerce::int),
            air_condition_temp_range: fields.opt("air_condition_temp_range", coerce::int),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Scheduled-climate bookings for a vehicle (`getBookingList` response).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BookingList {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// Whether the vehicle allows setting bookings.
    pub allow_setting: Option<i64>,
    /// Configuration data flag.
    pub config_data: Option<i64>,
    /// The scheduled timers (`listInfo` on the wire).
    pub bookings: Vec<ClimateBooking>,
}

/// `listInfo` is the wire name of `bookings`.
const KEY_ALIASES: &[(&str, &str)] = &[("listInfo", "bookings")];

impl BydModel for BookingList {
    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        let values = base::clean_byd_values(raw, KEY_ALIASES);
        let mut fields = Fields::new(&values)?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            allow_setting: fields.opt("allow_setting", coerce::int),
            config_data: fields.opt("config_data", coerce::int),
            bookings: fields.get("bookings", Vec::new(), |value| {
                coerce::list(value, |item| ClimateBooking::parse(item, context))
            }),
        };
        fields.finish()?;
        Ok(model)
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.booking.ClimateBooking" => base::model_call::<ClimateBooking>(args),
        "models.booking.BookingList" => base::model_call::<BookingList>(args),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn list_info_fills_bookings() {
        let list = BookingList::parse(
            &json!({"listInfo": [{"bookingId": 1216038691305533440_i64, "bookingTime": 1780938216}]}),
            &Value::Null,
        )
        .unwrap();
        assert_eq!(list.bookings.len(), 1);
        assert_eq!(list.bookings[0].booking_id, Some(1216038691305533440));
    }
}
