//! Steering wheel heating (`pybyd/_capabilities/steering.py`).

use serde_json::json;

use super::command::{CommandPlan, PlannedCommand, unsupported};
use crate::error::Error;
use crate::state::ProjectionSpec;

/// `StearingWheelHeat.ON` and `.OFF`.
const WHEEL_ON: i64 = -1;
const WHEEL_OFF: i64 = 1;
/// `HvacOverallStatus.ON`.
const HVAC_ON: i64 = 1;

/// Switching steering wheel heating on and off.
///
/// The command is a seat-climate command (`VENTILATIONHEATING`) carrying
/// every seat's current level, built when it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringCapability {
    vin: String,
    available: Option<bool>,
}

impl SteeringCapability {
    /// The capability of `vin`; `available` is the capability flag (`None` when unknown).
    pub fn new(vin: impl Into<String>, available: Option<bool>) -> Self {
        Self {
            vin: vin.into(),
            available,
        }
    }

    /// Whether the vehicle supports steering wheel heating (an unknown flag counts as no).
    pub fn available(&self) -> bool {
        self.available.unwrap_or(false)
    }

    /// Steering wheel heating on or off (command value 1 or 3).
    ///
    /// Projects `steering_wheel_heat_state` on the HVAC and realtime
    /// sections, and HVAC status `ON` when switching on: the car turns its
    /// A/C on for it, but does not turn it off again.
    pub fn heat(&self, on: bool) -> Result<CommandPlan, Error> {
        if !self.available() {
            return Err(unsupported(
                format!(
                    "Steering heat capability not supported for VIN {}",
                    self.vin
                ),
                "steering",
            ));
        }
        let status = if on { WHEEL_ON } else { WHEEL_OFF };
        let mut specs = vec![
            ProjectionSpec::new("hvac", "steering_wheel_heat_state", json!(status)),
            ProjectionSpec::new("realtime", "steering_wheel_heat_state", json!(status)),
        ];
        if on {
            specs.push(ProjectionSpec::new("hvac", "status", json!(HVAC_ON)));
        }
        Ok(CommandPlan {
            specs,
            command: PlannedCommand::SeatClimate {
                param_key: "steering_wheel_heat_state",
                value: if on { 1 } else { 3 },
            },
        })
    }
}
