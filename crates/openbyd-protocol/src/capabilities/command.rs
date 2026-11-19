//! What a capability command sends, and the helpers every capability shares.

use serde_json::{Value, json};

use crate::coerce::ValidationError;
use crate::error::{Error, ErrorKind};
use crate::models::control::{
    BatteryHeatParams, ClimateScheduleParams, ClimateStartParams, SeatClimateParams,
    SeatClimateState,
};
use crate::state::{Projected, ProjectionSpec, SectionModel, VehicleSnapshot};

/// The `BydEndpointNotSupportedError` a capability raises for a command the
/// vehicle does not support (`code="capability_unsupported"`).
pub fn unsupported(message: impl Into<String>, endpoint: &str) -> Error {
    Error::api(
        ErrorKind::BydEndpointNotSupportedError,
        message,
        "capability_unsupported",
        endpoint,
    )
}

/// A model validation failure as the error pyBYD raises: pydantic's
/// `ValidationError`, or the exception a validator let escape. An escaping
/// exception [`ErrorKind`] has no member for (`OverflowError`, `OSError`)
/// becomes a `ValidationError` whose message starts with its class name.
pub fn validation_error(error: ValidationError) -> Error {
    let kind = ErrorKind::from_class_name(error.exception).unwrap_or(ErrorKind::ValidationError);
    Error::new(kind, error.to_string())
}

/// The command a capability decided to send, before the current state is read.
#[derive(Debug, Clone, PartialEq)]
pub enum PlannedCommand {
    /// `client.lock(vin)`.
    Lock,
    /// `client.unlock(vin)`.
    Unlock,
    /// `client.start_climate(vin, params=...)`.
    StartClimate(ClimateStartParams),
    /// `client.stop_climate(vin)`.
    StopClimate,
    /// `client.schedule_climate(vin, params=...)`.
    ScheduleClimate(ClimateScheduleParams),
    /// `client.set_seat_climate(vin, params=...)`, the parameters built from
    /// the state at sending time with `param_key` changed to `value`.
    SeatClimate {
        /// The `SeatClimateParams` field to change (`"main_heat"`, `"steering_wheel_heat_state"`, ...).
        param_key: &'static str,
        /// Its value on BYD's command scale.
        value: i64,
    },
    /// `client.set_battery_heat(vin, params=...)`.
    BatteryHeat(BatteryHeatParams),
    /// `client.find_car(vin)`.
    FindCar,
    /// `client.flash_lights(vin)`.
    FlashLights,
    /// `client.close_windows(vin)`.
    CloseWindows,
    /// `client.open_windows(vin)`.
    OpenWindows,
    /// `client.open_trunk(vin)`.
    OpenTrunk,
    /// `client.close_trunk(vin)`.
    CloseTrunk,
}

/// A client call, ready to send.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandCall {
    /// `client.lock(vin)`.
    Lock,
    /// `client.unlock(vin)`.
    Unlock,
    /// `client.start_climate(vin, params=...)`.
    StartClimate(ClimateStartParams),
    /// `client.stop_climate(vin)`.
    StopClimate,
    /// `client.schedule_climate(vin, params=...)`.
    ScheduleClimate(ClimateScheduleParams),
    /// `client.set_seat_climate(vin, params=...)`.
    SetSeatClimate(SeatClimateParams),
    /// `client.set_battery_heat(vin, params=...)`.
    SetBatteryHeat(BatteryHeatParams),
    /// `client.find_car(vin)`.
    FindCar,
    /// `client.flash_lights(vin)`.
    FlashLights,
    /// `client.close_windows(vin)`.
    CloseWindows,
    /// `client.open_windows(vin)`.
    OpenWindows,
    /// `client.open_trunk(vin)`.
    OpenTrunk,
    /// `client.close_trunk(vin)`.
    CloseTrunk,
}

impl CommandCall {
    /// The name of the client method pyBYD calls.
    pub fn method(&self) -> &'static str {
        match self {
            Self::Lock => "lock",
            Self::Unlock => "unlock",
            Self::StartClimate(_) => "start_climate",
            Self::StopClimate => "stop_climate",
            Self::ScheduleClimate(_) => "schedule_climate",
            Self::SetSeatClimate(_) => "set_seat_climate",
            Self::SetBatteryHeat(_) => "set_battery_heat",
            Self::FindCar => "find_car",
            Self::FlashLights => "flash_lights",
            Self::CloseWindows => "close_windows",
            Self::OpenWindows => "open_windows",
            Self::OpenTrunk => "open_trunk",
            Self::CloseTrunk => "close_trunk",
        }
    }

    /// The call as the parity traces record it: the method, the VIN and,
    /// for a call with parameters, their `model_dump(mode="json")` and
    /// `to_control_params_map()`.
    pub fn to_json(&self, vin: &str) -> Value {
        let params = match self {
            Self::StartClimate(params) => {
                Some((params.model_dump(), params.to_control_params_map()))
            }
            Self::ScheduleClimate(params) => {
                Some((params.model_dump(), params.to_control_params_map()))
            }
            Self::SetSeatClimate(params) => {
                Some((params.model_dump(), params.to_control_params_map()))
            }
            Self::SetBatteryHeat(params) => {
                Some((params.model_dump(), params.to_control_params_map()))
            }
            _ => None,
        };
        match params {
            None => json!({"method": self.method(), "vin": vin}),
            Some((dump, control)) => json!({
                "method": self.method(),
                "vin": vin,
                "params": {"dump": dump, "control_params": control},
            }),
        }
    }
}

/// A capability command: the projections to register and the command to send.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandPlan {
    /// What the command should show until a payload confirms it. Empty for
    /// the fire-and-forget commands (finder, windows, trunk, schedule).
    pub specs: Vec<ProjectionSpec>,
    /// The command to send.
    pub command: PlannedCommand,
}

impl CommandPlan {
    /// A plan without projections.
    pub fn bare(command: PlannedCommand) -> Self {
        Self {
            specs: Vec::new(),
            command,
        }
    }

    /// The call to send, given the snapshot at sending time.
    ///
    /// pyBYD builds the seat parameters inside the command, after the
    /// command's own projections are registered: every other seat keeps the
    /// level the (projected) snapshot shows, read from the HVAC section first
    /// and the realtime section second.
    pub fn resolve(self, snapshot: &VehicleSnapshot) -> CommandCall {
        match self.command {
            PlannedCommand::Lock => CommandCall::Lock,
            PlannedCommand::Unlock => CommandCall::Unlock,
            PlannedCommand::StartClimate(params) => CommandCall::StartClimate(params),
            PlannedCommand::StopClimate => CommandCall::StopClimate,
            PlannedCommand::ScheduleClimate(params) => CommandCall::ScheduleClimate(params),
            PlannedCommand::SeatClimate { param_key, value } => {
                let hvac = snapshot.hvac.as_ref().map(projected_seat_state);
                let realtime = snapshot.realtime.as_ref().map(projected_seat_state);
                CommandCall::SetSeatClimate(
                    SeatClimateParams::from_current_state(hvac.as_ref(), realtime.as_ref())
                        .with_change(param_key, value),
                )
            }
            PlannedCommand::BatteryHeat(params) => CommandCall::SetBatteryHeat(params),
            PlannedCommand::FindCar => CommandCall::FindCar,
            PlannedCommand::FlashLights => CommandCall::FlashLights,
            PlannedCommand::CloseWindows => CommandCall::CloseWindows,
            PlannedCommand::OpenWindows => CommandCall::OpenWindows,
            PlannedCommand::OpenTrunk => CommandCall::OpenTrunk,
            PlannedCommand::CloseTrunk => CommandCall::CloseTrunk,
        }
    }
}

/// An overridden enum field as its integer value.
///
/// In a car, projections come only from the capabilities, which always
/// project enum members, so an override is an integer. Anything else stands
/// for a value that is not an enum member: `i64::MIN`, which maps to no
/// level, as pyBYD's `isinstance` checks do.
pub(crate) fn override_int(value: &Value) -> Option<i64> {
    match value {
        Value::Null => None,
        value => Some(value.as_i64().unwrap_or(i64::MIN)),
    }
}

/// The seat and steering wheel levels of a projected section, as
/// `SeatClimateParams.from_current_state` reads them from it.
pub fn projected_seat_state<M>(section: &Projected<M>) -> SeatClimateState
where
    M: SectionModel,
    for<'a> SeatClimateState: From<&'a M>,
{
    let mut state = SeatClimateState::from(section.base());
    for (name, value) in section.overrides() {
        let slot = match name.as_str() {
            "main_seat_heat_state" => &mut state.main_seat_heat_state,
            "main_seat_ventilation_state" => &mut state.main_seat_ventilation_state,
            "copilot_seat_heat_state" => &mut state.copilot_seat_heat_state,
            "copilot_seat_ventilation_state" => &mut state.copilot_seat_ventilation_state,
            "lr_seat_heat_state" => &mut state.lr_seat_heat_state,
            "lr_seat_ventilation_state" => &mut state.lr_seat_ventilation_state,
            "rr_seat_heat_state" => &mut state.rr_seat_heat_state,
            "rr_seat_ventilation_state" => &mut state.rr_seat_ventilation_state,
            "steering_wheel_heat_state" => &mut state.steering_wheel_heat_state,
            _ => continue,
        };
        *slot = override_int(value);
    }
    state
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::models::base::BydModel;
    use crate::models::hvac::HvacStatus;

    #[test]
    fn overrides_read_as_enum_values() {
        assert_eq!(override_int(&json!(2)), Some(2));
        assert_eq!(override_int(&Value::Null), None);
        assert_eq!(override_int(&json!("x")), Some(i64::MIN));
    }

    #[test]
    fn errors_carry_pybyd_attributes() {
        let error = unsupported("no", "lock");
        assert_eq!(error.kind(), ErrorKind::BydEndpointNotSupportedError);
        assert_eq!(error.code(), "capability_unsupported");
        assert_eq!(error.endpoint(), "lock");
        assert_eq!(
            validation_error(ValidationError::new("int_parsing")).kind(),
            ErrorKind::ValidationError
        );
        assert_eq!(
            validation_error(ValidationError::raised("ValueError", "x")).kind(),
            ErrorKind::ValueError
        );
        assert_eq!(
            validation_error(ValidationError::unsupported_int()).kind(),
            ErrorKind::ValidationError
        );
    }

    #[test]
    fn unknown_overrides_are_ignored() {
        let hvac = HvacStatus::parse(&json!({"mainSeatHeatState": 3}), &Value::Null).unwrap();
        let projected = Projected::new(Arc::new(hvac));
        let state = projected_seat_state(&projected);
        assert_eq!(state.main_seat_heat_state, Some(3));
    }
}
