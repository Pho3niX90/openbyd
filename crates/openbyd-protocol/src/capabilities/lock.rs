//! Lock and unlock (`pybyd/_capabilities/lock.py`).

use serde_json::json;

use super::command::{CommandPlan, PlannedCommand, unsupported};
use crate::error::Error;
use crate::state::ProjectionSpec;

/// `LockState.LOCKED` and `LockState.UNLOCKED`.
const LOCKED: i64 = 2;
const UNLOCKED: i64 = 1;

const DOOR_LOCKS: [&str; 4] = [
    "left_front_door_lock",
    "right_front_door_lock",
    "left_rear_door_lock",
    "right_rear_door_lock",
];

/// Locking and unlocking the doors, with the four door locks projected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockCapability {
    vin: String,
    available: Option<bool>,
}

impl LockCapability {
    /// The capability of `vin`; `available` is the capability flag (`None` when unknown).
    pub fn new(vin: impl Into<String>, available: Option<bool>) -> Self {
        Self {
            vin: vin.into(),
            available,
        }
    }

    /// Whether the vehicle supports locking (an unknown flag counts as no).
    pub fn available(&self) -> bool {
        self.available.unwrap_or(false)
    }

    fn plan(&self, state: i64, command: PlannedCommand) -> Result<CommandPlan, Error> {
        if !self.available() {
            return Err(unsupported(
                format!("Lock capability not supported for VIN {}", self.vin),
                "lock",
            ));
        }
        Ok(CommandPlan {
            specs: DOOR_LOCKS
                .iter()
                .map(|field| ProjectionSpec::new("realtime", *field, json!(state)))
                .collect(),
            command,
        })
    }

    /// Lock all doors: `client.lock`, the door locks projected `LOCKED`.
    pub fn lock(&self) -> Result<CommandPlan, Error> {
        self.plan(LOCKED, PlannedCommand::Lock)
    }

    /// Unlock all doors: `client.unlock`, the door locks projected `UNLOCKED`.
    pub fn unlock(&self) -> Result<CommandPlan, Error> {
        self.plan(UNLOCKED, PlannedCommand::Unlock)
    }
}
