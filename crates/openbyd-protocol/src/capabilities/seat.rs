//! Seat heating and ventilation (`pybyd/_capabilities/seat.py`).

use serde_json::json;

use super::command::{CommandPlan, PlannedCommand, unsupported};
use crate::coerce::StrEnum;
use crate::error::Error;
use crate::models::realtime::SeatHeatVentState;
use crate::state::ProjectionSpec;

/// `HvacOverallStatus.ON`.
const HVAC_ON: i64 = 1;

/// A seat a heat or ventilation command targets (a Python `StrEnum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeatPosition {
    /// `"driver"`.
    Driver,
    /// `"copilot"`: the front passenger.
    Copilot,
    /// `"rear_left"`.
    RearLeft,
    /// `"rear_right"`.
    RearRight,
}

impl SeatPosition {
    /// The member's Python name (`"DRIVER"`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::Driver => "DRIVER",
            Self::Copilot => "COPILOT",
            Self::RearLeft => "REAR_LEFT",
            Self::RearRight => "REAR_RIGHT",
        }
    }

    /// `SeatPosition(value)`: the member whose value is `value`.
    pub fn from_value(value: &str) -> Option<Self> {
        Self::MEMBERS
            .iter()
            .copied()
            .find(|member| member.value() == value)
    }

    fn index(self) -> usize {
        self as usize
    }
}

impl StrEnum for SeatPosition {
    const MEMBERS: &'static [Self] =
        &[Self::Driver, Self::Copilot, Self::RearLeft, Self::RearRight];

    fn value(self) -> &'static str {
        match self {
            Self::Driver => "driver",
            Self::Copilot => "copilot",
            Self::RearLeft => "rear_left",
            Self::RearRight => "rear_right",
        }
    }
}

/// Heat or ventilation intensity (a Python `IntEnum`: 0, 1, 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeatLevel {
    /// 0.
    Off = 0,
    /// 1.
    Low = 1,
    /// 2.
    High = 2,
}

impl SeatLevel {
    /// Every member, in definition order.
    pub const MEMBERS: [Self; 3] = [Self::Off, Self::Low, Self::High];

    /// The member's integer value.
    pub fn value(self) -> i64 {
        self as i64
    }

    /// The member's Python name (`"OFF"`, `"LOW"`, `"HIGH"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Low => "LOW",
            Self::High => "HIGH",
        }
    }

    /// `SeatLevel(value)`: the member whose value is `value`.
    pub fn from_value(value: i64) -> Option<Self> {
        Self::MEMBERS
            .into_iter()
            .find(|member| member.value() == value)
    }

    /// The level on BYD's command scale, which is inverted: HIGH→1, LOW→2, OFF→3.
    pub fn to_command_value(self) -> i64 {
        match self {
            Self::Off => 3,
            Self::Low => 2,
            Self::High => 1,
        }
    }

    /// The status reading the level shows as.
    pub fn to_status_value(self) -> SeatHeatVentState {
        match self {
            Self::Off => SeatHeatVentState::Off,
            Self::Low => SeatHeatVentState::Low,
            Self::High => SeatHeatVentState::High,
        }
    }
}

/// Heating or ventilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeatMode {
    /// `"heat"`.
    Heat,
    /// `"vent"`.
    Vent,
}

impl SeatMode {
    /// pyBYD's spelling: `"heat"` or `"vent"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Heat => "heat",
            Self::Vent => "vent",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The `SeatClimateParams` field and the status field (for projections) of each seat and mode.
fn fields(position: SeatPosition, mode: SeatMode) -> (&'static str, &'static str) {
    match (position, mode) {
        (SeatPosition::Driver, SeatMode::Heat) => ("main_heat", "main_seat_heat_state"),
        (SeatPosition::Driver, SeatMode::Vent) => {
            ("main_ventilation", "main_seat_ventilation_state")
        }
        (SeatPosition::Copilot, SeatMode::Heat) => ("copilot_heat", "copilot_seat_heat_state"),
        (SeatPosition::Copilot, SeatMode::Vent) => {
            ("copilot_ventilation", "copilot_seat_ventilation_state")
        }
        (SeatPosition::RearLeft, SeatMode::Heat) => ("lr_seat_heat_state", "lr_seat_heat_state"),
        (SeatPosition::RearLeft, SeatMode::Vent) => {
            ("lr_seat_ventilation_state", "lr_seat_ventilation_state")
        }
        (SeatPosition::RearRight, SeatMode::Heat) => ("rr_seat_heat_state", "rr_seat_heat_state"),
        (SeatPosition::RearRight, SeatMode::Vent) => {
            ("rr_seat_ventilation_state", "rr_seat_ventilation_state")
        }
    }
}

/// The capability flags of every seat and mode (`None` when unknown).
///
/// [`Default`] gives pyBYD's constructor defaults: the front seats `true`,
/// the rear seats `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeatAvailability {
    /// Driver seat heating.
    pub driver_heat: Option<bool>,
    /// Driver seat ventilation.
    pub driver_ventilation: Option<bool>,
    /// Front passenger seat heating.
    pub passenger_heat: Option<bool>,
    /// Front passenger seat ventilation.
    pub passenger_ventilation: Option<bool>,
    /// Rear left seat heating.
    pub rear_left_heat: Option<bool>,
    /// Rear left seat ventilation.
    pub rear_left_ventilation: Option<bool>,
    /// Rear right seat heating.
    pub rear_right_heat: Option<bool>,
    /// Rear right seat ventilation.
    pub rear_right_ventilation: Option<bool>,
}

impl Default for SeatAvailability {
    fn default() -> Self {
        Self {
            driver_heat: Some(true),
            driver_ventilation: Some(true),
            passenger_heat: Some(true),
            passenger_ventilation: Some(true),
            rear_left_heat: None,
            rear_left_ventilation: None,
            rear_right_heat: None,
            rear_right_ventilation: None,
        }
    }
}

/// Seat heating and ventilation.
///
/// BYD needs every seat's value in every `VENTILATIONHEATING` command; the
/// caller names only the seat it changes, and the others are read from the
/// state when the command is sent (see [`super::CommandPlan::resolve`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatCapability {
    vin: String,
    available: [[Option<bool>; 2]; 4],
}

impl SeatCapability {
    /// The capability of `vin` with its flags.
    pub fn new(vin: impl Into<String>, flags: SeatAvailability) -> Self {
        Self {
            vin: vin.into(),
            available: [
                [flags.driver_heat, flags.driver_ventilation],
                [flags.passenger_heat, flags.passenger_ventilation],
                [flags.rear_left_heat, flags.rear_left_ventilation],
                [flags.rear_right_heat, flags.rear_right_ventilation],
            ],
        }
    }

    /// Whether the vehicle supports `mode` on the seat at `position`.
    pub fn is_mode_available(&self, position: SeatPosition, mode: SeatMode) -> bool {
        self.available[position.index()][mode.index()].unwrap_or(false)
    }

    /// Set the heating level of one seat.
    pub fn heat(&self, position: SeatPosition, level: SeatLevel) -> Result<CommandPlan, Error> {
        self.set_seat(position, SeatMode::Heat, level)
    }

    /// Set the ventilation level of one seat.
    pub fn ventilation(
        &self,
        position: SeatPosition,
        level: SeatLevel,
    ) -> Result<CommandPlan, Error> {
        self.set_seat(position, SeatMode::Vent, level)
    }

    /// Projects the seat's status field on the HVAC and realtime sections,
    /// and HVAC status `ON` for any level but `OFF`: the car turns its A/C on
    /// for a seat feature, but does not turn it off again.
    fn set_seat(
        &self,
        position: SeatPosition,
        mode: SeatMode,
        level: SeatLevel,
    ) -> Result<CommandPlan, Error> {
        if !self.is_mode_available(position, mode) {
            return Err(unsupported(
                format!(
                    "Seat {} {} capability not supported for VIN {}",
                    position.value(),
                    mode.as_str(),
                    self.vin
                ),
                &format!("seat.{}", mode.as_str()),
            ));
        }
        let (param_key, status_field) = fields(position, mode);
        let status = json!(level.to_status_value() as i64);
        let mut specs = vec![
            ProjectionSpec::new("hvac", status_field, status.clone()),
            ProjectionSpec::new("realtime", status_field, status),
        ];
        if level != SeatLevel::Off {
            specs.push(ProjectionSpec::new("hvac", "status", json!(HVAC_ON)));
        }
        Ok(CommandPlan {
            specs,
            command: PlannedCommand::SeatClimate {
                param_key,
                value: level.to_command_value(),
            },
        })
    }
}
