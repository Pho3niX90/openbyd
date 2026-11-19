//! Battery heating (`pybyd/_capabilities/battery_heat.py`).

use serde_json::json;

use super::command::{CommandPlan, PlannedCommand, unsupported};
use crate::error::Error;
use crate::models::control::BatteryHeatParams;
use crate::state::ProjectionSpec;

/// Switching battery heating on and off, with `realtime.battery_heat_state` projected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatteryHeatCapability {
    vin: String,
    available: Option<bool>,
}

impl BatteryHeatCapability {
    /// The capability of `vin`; `available` is the capability flag (`None` when unknown).
    pub fn new(vin: impl Into<String>, available: Option<bool>) -> Self {
        Self {
            vin: vin.into(),
            available,
        }
    }

    /// Whether the vehicle supports battery heating (an unknown flag counts as no).
    pub fn available(&self) -> bool {
        self.available.unwrap_or(false)
    }

    /// Battery heating on or off: `client.set_battery_heat` with
    /// `BatteryHeatParams(on=on)`, `battery_heat_state` projected 1 or 0.
    pub fn heat(&self, on: bool) -> Result<CommandPlan, Error> {
        if !self.available() {
            return Err(unsupported(
                format!("Battery heat capability not supported for VIN {}", self.vin),
                "battery_heat",
            ));
        }
        Ok(CommandPlan {
            specs: vec![ProjectionSpec::new(
                "realtime",
                "battery_heat_state",
                json!(i64::from(on)),
            )],
            command: PlannedCommand::BatteryHeat(BatteryHeatParams::new(on)),
        })
    }
}
