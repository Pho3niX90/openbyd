//! Push notification state model (`pybyd/models/push_notification.py`).
//!
//! The EU endpoint `/vehicle/vehicleswitch/getPushSwitchState` returns a LIST of
//! per-notification-type switches, e.g. `[{"state":1,"type":1}, ...]`, not a
//! single boolean. `type 701` is the vehicle real-time status push (mirrors the
//! `vehicleStatusPushLearnInfo` capability flag); when ON the cloud is expected
//! to relay car-originated status onto the MQTT result topic without a poll.

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields};
use crate::coerce::{self, ValidationError};
use crate::parity::Outcome;

/// Notification "type" code for the vehicle real-time status push.
pub const VEHICLE_STATUS_PUSH_TYPE: i64 = 701;

/// A single per-type push notification switch.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PushSwitch {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Notification type code.
    pub r#type: Option<i64>,
    /// Switch state (1 on, 0 off).
    pub state: Option<i64>,
}

impl BydModel for PushSwitch {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let values = base::clean_byd_values(raw, &[]);
        let mut fields = Fields::new(&values)?;
        let model = Self {
            raw: fields.raw(),
            r#type: fields.opt("type", coerce::int),
            state: fields.opt("state", coerce::int),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Per-type push notification switch states for a vehicle.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PushNotificationState {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// The per-type switches.
    pub switches: Vec<PushSwitch>,
}

impl PushNotificationState {
    /// Return the on/off state (1/0) for a notification `type`, or `None`.
    pub fn state_for(&self, push_type: i64) -> Option<i64> {
        self.switches
            .iter()
            .find(|switch| switch.r#type == Some(push_type))
            .and_then(|switch| switch.state)
    }

    /// Whether the vehicle real-time status push (type 701) is enabled.
    pub fn status_push_enabled(&self) -> bool {
        self.state_for(VEHICLE_STATUS_PUSH_TYPE) == Some(1)
    }

    /// Legacy alias: true when the vehicle status push (701) is on.
    pub fn is_enabled(&self) -> bool {
        self.status_push_enabled()
    }
}

impl BydModel for PushNotificationState {
    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        let values = base::clean_byd_values(raw, &[]);
        let mut fields = Fields::new(&values)?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            switches: fields.get("switches", Vec::new(), |value| {
                coerce::list(value, |item| PushSwitch::parse(item, context))
            }),
        };
        fields.finish()?;
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([
            ("status_push_enabled", json!(self.status_push_enabled())),
            ("is_enabled", json!(self.is_enabled())),
        ])
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.push_notification.PushSwitch" => base::model_call::<PushSwitch>(args),
        "models.push_notification.PushNotificationState" => {
            base::model_call::<PushNotificationState>(args)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(raw: Value) -> PushNotificationState {
        PushNotificationState::parse(&raw, &Value::Null).unwrap()
    }

    #[test]
    fn state_for_finds_the_first_switch_of_a_type() {
        let model = state(json!({"switches": [
            {"type": 1, "state": 0},
            {"type": 701, "state": 1},
            {"type": 701, "state": 0},
            {"state": 1},
        ]}));
        assert_eq!(model.state_for(1), Some(0));
        assert_eq!(model.state_for(701), Some(1));
        assert_eq!(model.state_for(5), None);
        assert!(model.status_push_enabled());
        assert!(model.is_enabled());
    }

    #[test]
    fn a_switch_without_state_or_a_missing_list_is_off() {
        assert!(!state(json!({"switches": [{"type": 701}]})).status_push_enabled());
        assert!(!state(json!({})).is_enabled());
    }

    #[test]
    fn a_bad_switch_is_a_validation_error() {
        let error = PushNotificationState::parse(&json!({"switches": [5]}), &Value::Null);
        assert!(error.unwrap_err().is_validation());
    }
}
