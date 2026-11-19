//! Find my car and flash lights (`pybyd/_capabilities/finder.py`).

use super::command::{CommandPlan, PlannedCommand, unsupported};
use crate::error::Error;

/// Find-my-car (horn and lights) and flashing the lights.
///
/// Both are fire-and-forget: nothing observable changes, so nothing is projected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinderCapability {
    vin: String,
    find_available: Option<bool>,
    flash_available: Option<bool>,
}

impl FinderCapability {
    /// The capability of `vin` with its two capability flags (`None` when unknown).
    pub fn new(
        vin: impl Into<String>,
        find_available: Option<bool>,
        flash_available: Option<bool>,
    ) -> Self {
        Self {
            vin: vin.into(),
            find_available,
            flash_available,
        }
    }

    /// Whether the vehicle supports find-my-car.
    pub fn find_available(&self) -> bool {
        self.find_available.unwrap_or(false)
    }

    /// Whether the vehicle supports flashing the lights.
    pub fn flash_available(&self) -> bool {
        self.flash_available.unwrap_or(false)
    }

    /// Find-my-car: `client.find_car`.
    pub fn find(&self) -> Result<CommandPlan, Error> {
        if !self.find_available() {
            return Err(unsupported(
                format!("Find-car capability not supported for VIN {}", self.vin),
                "finder.find",
            ));
        }
        Ok(CommandPlan::bare(PlannedCommand::FindCar))
    }

    /// Flash the lights: `client.flash_lights`.
    pub fn flash_lights(&self) -> Result<CommandPlan, Error> {
        if !self.flash_available() {
            return Err(unsupported(
                format!("Flash-lights capability not supported for VIN {}", self.vin),
                "finder.flash_lights",
            ));
        }
        Ok(CommandPlan::bare(PlannedCommand::FlashLights))
    }
}
