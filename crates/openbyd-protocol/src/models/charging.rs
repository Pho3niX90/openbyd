//! Charging status model (`pybyd/models/charging.py`).

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields, PyDateTime};
use crate::coerce::{self, ValidationError};
use crate::parity::Outcome;

/// Smart charging status for a vehicle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChargingStatus {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// State of charge (0-100 percent).
    pub soc: Option<i64>,
    /// Raw charging state; `1` means charging.
    pub charging_state: Option<i64>,
    /// Raw plug state; anything but `0` means a cable is connected.
    pub connect_state: Option<i64>,
    /// Raw wait (scheduled charging) status.
    pub wait_status: Option<i64>,
    /// Hours part of the time until full; `None` for BYD's negative sentinel.
    pub full_hour: Option<i64>,
    /// Minutes part of the time until full; `None` for BYD's negative sentinel.
    pub full_minute: Option<i64>,
    /// Charging power while charging, in [`Self::charge_power_unit`]. `None` when not charging.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub charge_power: Option<f64>,
    /// Unit for [`Self::charge_power`] (`"kW"` observed).
    pub charge_power_unit: Option<String>,
    /// Last data update timestamp (parsed to UTC datetime).
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub update_time: Option<PyDateTime>,
}

/// BYD sends the same value under different keys depending on the endpoint.
const KEY_ALIASES: &[(&str, &str)] = &[("elecPercent", "soc"), ("time", "updateTime")];

impl ChargingStatus {
    /// Whether a charging cable is connected (`connect_state` set and not 0).
    pub fn is_connected(&self) -> bool {
        self.connect_state.is_some_and(|state| state != 0)
    }

    /// Whether the car is charging (`charging_state == 1`).
    pub fn is_charging(&self) -> bool {
        self.charging_state == Some(1)
    }

    /// Whether both parts of the time until full are known.
    pub fn time_to_full_available(&self) -> bool {
        self.full_hour.is_some() && self.full_minute.is_some()
    }

    /// Total estimated minutes until fully charged.
    ///
    /// Combines `full_hour` and `full_minute` into a single value. Returns
    /// `None` when either component is unavailable, and also when the sum
    /// does not fit in `i64` (Python would return a larger integer).
    pub fn time_to_full_minutes(&self) -> Option<i64> {
        self.full_hour?
            .checked_mul(60)?
            .checked_add(self.full_minute?)
    }
}

impl BydModel for ChargingStatus {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let mut fields = Fields::cleaned(raw, KEY_ALIASES)?;
        let mut model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            soc: fields.opt("soc", coerce::int),
            charging_state: fields.opt("charging_state", coerce::int),
            connect_state: fields.opt("connect_state", coerce::int),
            wait_status: fields.opt("wait_status", coerce::int),
            full_hour: fields.opt("full_hour", coerce::int),
            full_minute: fields.opt("full_minute", coerce::int),
            charge_power: fields.opt("charge_power", coerce::float),
            charge_power_unit: fields.opt("charge_power_unit", coerce::string),
            update_time: fields.get("update_time", None, base::byd_timestamp),
        };
        fields.finish()?;
        // _SENTINEL_RULES
        base::clear_if(&mut model.full_hour, base::is_negative);
        base::clear_if(&mut model.full_minute, base::is_negative);
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([
            ("is_connected", json!(self.is_connected())),
            ("is_charging", json!(self.is_charging())),
            (
                "time_to_full_available",
                json!(self.time_to_full_available()),
            ),
            ("time_to_full_minutes", json!(self.time_to_full_minutes())),
        ])
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.charging.ChargingStatus" => base::model_call::<ChargingStatus>(args),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_to_full_does_not_overflow() {
        let status = ChargingStatus::parse(
            &json!({"fullHour": i64::MAX, "fullMinute": 1}),
            &Value::Null,
        )
        .unwrap();
        assert!(status.time_to_full_available());
        assert_eq!(status.time_to_full_minutes(), None);
    }
}
