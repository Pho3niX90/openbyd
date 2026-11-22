//! OTA firmware status and version models (`pybyd/models/ota.py`).

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields, Plan, PyDateTime, byd_enum, enum_call};
use crate::aes::is_python_space;
use crate::coerce::{self, ValidationError};
use crate::parity::Outcome;

byd_enum! {
    /// OTA firmware upgrade status reported by `/control/otaUpgrade/getOtaVersion`.
    OtaStatus {
        /// No mapped member.
        Unknown = -1 => "UNKNOWN",
        /// No upgrade in progress or waiting.
        Idle = 0 => "IDLE",
        /// A newer version can be installed.
        UpgradeAvailable = 1 => "UPGRADE_AVAILABLE",
        /// An installation is booked.
        UpgradeBooked = 2 => "UPGRADE_BOOKED",
        /// Installing.
        Upgrading = 3 => "UPGRADING",
        /// Downloaded and ready to install.
        UpgradeReady = 4 => "UPGRADE_READY",
        /// A booking is being made.
        Booking = 5 => "BOOKING",
        /// A booking is being cancelled.
        CancelBooking = 6 => "CANCEL_BOOKING",
        /// Waiting for the night auto-upgrade.
        NightSync = 100 => "NIGHT_SYNC",
    }
}

/// The upgrade result block nested in [`OtaUpgradeInfo`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OtaUpgradeResult {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Result status.
    pub status: String,
    /// Result status code.
    pub status_code: String,
    /// Result message.
    pub message: String,
}

impl BydModel for OtaUpgradeResult {
    fn raw_mut(&mut self) -> Option<&mut Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            status: fields.get("status", String::new(), coerce::string),
            status_code: fields.get("status_code", String::new(), coerce::string),
            message: fields.get("message", String::new(), coerce::string),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// A vehicle's OTA firmware version, release notes and night auto-upgrade settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OtaUpgradeInfo {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// The installed firmware version.
    pub current_version: String,
    /// The version on offer; empty when none.
    pub update_version: String,
    /// Where an upgrade stands.
    #[serde(serialize_with = "base::ser_int_enum")]
    pub upgrade_status: OtaStatus,
    /// `"1"` when the night auto-upgrade is on.
    pub auto_upgrade_switch: String,
    /// The night auto-upgrade time, `H:mm`.
    pub auto_upgrade_time: String,
    /// When a booked upgrade runs.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub booking_time_stamp: Option<PyDateTime>,
    /// When the current version was installed.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub current_update_time: Option<PyDateTime>,
    /// How long an upgrade is expected to take.
    pub estimate_upgrade_time: Option<i64>,
    /// The car head unit's time zone.
    pub pad_time_zone: String,
    /// Release notes: what is new.
    pub function_addition: String,
    /// Release notes: what is improved.
    pub function_optimization: String,
    /// Release notes: the description.
    pub function_description: String,
    /// Release notes: acknowledgements.
    pub acknowledgements: String,
    /// The last upgrade's result, when reported.
    pub upgrade_result: Option<OtaUpgradeResult>,
}

impl OtaUpgradeInfo {
    /// Whether the night auto-upgrade is on (`autoUpgradeSwitch` is `"1"`).
    pub fn auto_upgrade_enabled(&self) -> bool {
        self.auto_upgrade_switch == "1"
    }

    /// Whether a newer version is on offer (`update_version` is not blank).
    pub fn has_update(&self) -> bool {
        !self.update_version.trim_matches(is_python_space).is_empty()
    }
}

impl BydModel for OtaUpgradeInfo {
    fn raw_mut(&mut self) -> Option<&mut Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let text = |fields: &mut Fields, name: &'static str| {
            fields.get(name, String::new(), coerce::string)
        };
        let model = Self {
            raw: fields.raw(),
            vin: text(&mut fields, "vin"),
            current_version: text(&mut fields, "current_version"),
            update_version: text(&mut fields, "update_version"),
            upgrade_status: fields.get("upgrade_status", OtaStatus::Unknown, base::enum_field),
            auto_upgrade_switch: text(&mut fields, "auto_upgrade_switch"),
            auto_upgrade_time: text(&mut fields, "auto_upgrade_time"),
            booking_time_stamp: fields.get("booking_time_stamp", None, base::byd_timestamp),
            current_update_time: fields.get("current_update_time", None, base::byd_timestamp),
            estimate_upgrade_time: fields.opt("estimate_upgrade_time", coerce::int),
            pad_time_zone: text(&mut fields, "pad_time_zone"),
            function_addition: text(&mut fields, "function_addition"),
            function_optimization: text(&mut fields, "function_optimization"),
            function_description: text(&mut fields, "function_description"),
            acknowledgements: text(&mut fields, "acknowledgements"),
            upgrade_result: fields.opt("upgrade_result", |value| {
                OtaUpgradeResult::parse(value, context)
            }),
        };
        fields.finish()?;
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([
            ("auto_upgrade_enabled", json!(self.auto_upgrade_enabled())),
            ("has_update", json!(self.has_update())),
        ])
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.ota.OtaStatus" => enum_call::<OtaStatus>(args),
        "models.ota.OtaUpgradeResult" => base::model_call::<OtaUpgradeResult>(args),
        "models.ota.OtaUpgradeInfo" => base::model_call::<OtaUpgradeInfo>(args),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_and_updates() {
        let info = OtaUpgradeInfo::parse(
            &json!({"autoUpgradeSwitch": "1", "updateVersion": " \u{3000}", "upgradeStatus": 100}),
            &Value::Null,
        )
        .unwrap();
        assert!(info.auto_upgrade_enabled());
        assert!(!info.has_update());
        assert_eq!(info.upgrade_status, OtaStatus::NightSync);
        let info = OtaUpgradeInfo::parse(&json!({"updateVersion": "2.1"}), &Value::Null).unwrap();
        assert!(info.has_update());
        assert!(!info.auto_upgrade_enabled());
        assert_eq!(info.upgrade_status, OtaStatus::Unknown);
    }
}
