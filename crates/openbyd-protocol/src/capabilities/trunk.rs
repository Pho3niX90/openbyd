//! Opening and closing the trunk (`pybyd/_capabilities/trunk.py`).

use super::command::{CommandPlan, PlannedCommand, unsupported};
use crate::error::Error;

/// Opening and closing the trunk remotely. Fire-and-forget: nothing is projected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrunkCapability {
    vin: String,
    open_available: Option<bool>,
    close_available: Option<bool>,
}

impl TrunkCapability {
    /// The capability of `vin` with its two capability flags (`None` when unknown).
    pub fn new(
        vin: impl Into<String>,
        open_available: Option<bool>,
        close_available: Option<bool>,
    ) -> Self {
        Self {
            vin: vin.into(),
            open_available,
            close_available,
        }
    }

    /// Whether the vehicle can release the trunk latch.
    pub fn open_available(&self) -> bool {
        self.open_available.unwrap_or(false)
    }

    /// Whether the vehicle can lower the trunk electrically.
    pub fn close_available(&self) -> bool {
        self.close_available.unwrap_or(false)
    }

    /// Release the trunk latch: `client.open_trunk`.
    pub fn open(&self) -> Result<CommandPlan, Error> {
        if !self.open_available() {
            return Err(unsupported(
                format!("Open-trunk capability not supported for VIN {}", self.vin),
                "trunk.open",
            ));
        }
        Ok(CommandPlan::bare(PlannedCommand::OpenTrunk))
    }

    /// Lower the trunk: `client.close_trunk`.
    pub fn close(&self) -> Result<CommandPlan, Error> {
        if !self.close_available() {
            return Err(unsupported(
                format!("Close-trunk capability not supported for VIN {}", self.vin),
                "trunk.close",
            ));
        }
        Ok(CommandPlan::bare(PlannedCommand::CloseTrunk))
    }
}
