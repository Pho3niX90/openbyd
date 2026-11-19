//! Opening and closing the windows (`pybyd/_capabilities/windows.py`).

use super::command::{CommandPlan, PlannedCommand, unsupported};
use crate::error::Error;

/// Closing the windows and cracking them open.
///
/// Both are fire-and-forget: window state arrives later, so nothing is
/// projected. Opening cracks the windows to about 10 % (BYD's ventilation
/// mode, confirmed on a Sealion 7), not a full drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsCapability {
    vin: String,
    close_available: Option<bool>,
    open_available: Option<bool>,
    can_open: bool,
}

impl WindowsCapability {
    /// The capability of `vin` with its two capability flags (`None` when
    /// unknown). `can_open` says whether an open command exists at all
    /// (pyBYD's `open_fn is not None`).
    pub fn new(
        vin: impl Into<String>,
        close_available: Option<bool>,
        open_available: Option<bool>,
        can_open: bool,
    ) -> Self {
        Self {
            vin: vin.into(),
            close_available,
            open_available,
            can_open,
        }
    }

    /// Whether the vehicle can close its windows remotely.
    pub fn close_available(&self) -> bool {
        self.close_available.unwrap_or(false)
    }

    /// Whether the vehicle can crack its windows open remotely.
    pub fn open_available(&self) -> bool {
        self.open_available.unwrap_or(false) && self.can_open
    }

    /// Close all windows: `client.close_windows`.
    pub fn close(&self) -> Result<CommandPlan, Error> {
        if !self.close_available() {
            return Err(unsupported(
                format!(
                    "Close-windows capability not supported for VIN {}",
                    self.vin
                ),
                "windows.close",
            ));
        }
        Ok(CommandPlan::bare(PlannedCommand::CloseWindows))
    }

    /// Crack all windows open: `client.open_windows`.
    pub fn open(&self) -> Result<CommandPlan, Error> {
        if !self.open_available() {
            return Err(unsupported(
                format!("Open-windows capability not supported for VIN {}", self.vin),
                "windows.open",
            ));
        }
        Ok(CommandPlan::bare(PlannedCommand::OpenWindows))
    }
}
