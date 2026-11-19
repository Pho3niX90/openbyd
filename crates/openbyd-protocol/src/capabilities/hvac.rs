//! Climate control (`pybyd/_capabilities/hvac.py`).

use serde_json::{Value, json};

use super::command::{CommandPlan, PlannedCommand, override_int, unsupported, validation_error};
use crate::coerce::IntEnum;
use crate::constants::{DEFAULT_TEMP_OFFSET_C, minutes_to_time_span};
use crate::error::{Error, ErrorKind};
use crate::models::control::{ClimateScheduleParams, ClimateStartArgs, ClimateStartParams};
use crate::models::hvac::HvacStatus;
use crate::state::{Projected, ProjectionSpec};

/// `HvacOverallStatus.ON` and `.OFF`.
const HVAC_ON: i64 = 1;
const HVAC_OFF: i64 = 2;
/// `SeatHeatVentState.OFF`.
const SEAT_OFF: i64 = 1;
/// `StearingWheelHeat.OFF`.
const WHEEL_OFF: i64 = 1;
/// `AirCirculationMode.EXTERNAL` and `.INTERNAL`.
const EXTERNAL_AIR: i64 = 1;
const INTERNAL_AIR: i64 = 2;

/// The fields of an HVAC status that `HvacCapability.start` reads.
///
/// Enum fields are integer values. Build one from the projected snapshot
/// section ([`Self::from_projected`]) or, after a refresh, from the fetched
/// status itself ([`Self::from_status`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HvacReading {
    /// `temp_offset`: °C offset of the vehicle's 1-17 setpoint scale.
    pub temp_offset: f64,
    /// `status` (`HvacOverallStatus`).
    pub status: Option<i64>,
    /// `cycle_choice` (`AirCirculationMode`).
    pub cycle_choice: Option<i64>,
    /// `wind_position` (`HvacWindPosition`).
    pub wind_position: Option<i64>,
}

impl HvacReading {
    /// The reading of a status as fetched.
    pub fn from_status(status: &HvacStatus) -> Self {
        Self {
            temp_offset: status.temp_offset(),
            status: status.status.map(IntEnum::value),
            cycle_choice: status.cycle_choice.map(IntEnum::value),
            wind_position: status.wind_position.map(IntEnum::value),
        }
    }

    /// The reading of a projected snapshot section: projected values win.
    pub fn from_projected(section: &Projected<HvacStatus>) -> Self {
        let mut reading = Self::from_status(section.base());
        let overrides = section.overrides();
        let apply = |name: &str, slot: &mut Option<i64>| {
            if let Some(value) = overrides.get(name) {
                *slot = override_int(value);
            }
        };
        apply("status", &mut reading.status);
        apply("cycle_choice", &mut reading.cycle_choice);
        apply("wind_position", &mut reading.wind_position);
        reading
    }
}

/// The settings of a start request, as `HvacCapability.start` takes them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClimateStart {
    /// Target temperature in °C (15-31, or the vehicle's own range). pyBYD's default is 21.0.
    pub temperature: f64,
    /// Run time in minutes (10, 15, 20, 25 or 30). pyBYD's default is 20.
    pub duration: i64,
    /// Air source: 1 fresh, 2 recirculate. `None` keeps the running
    /// session's setting, or recirculates.
    pub cycle_mode: Option<i64>,
    /// Fan speed 1-7. `None` keeps the running session's speed, or lets the car pick.
    pub fan_level: Option<i64>,
}

impl Default for ClimateStart {
    fn default() -> Self {
        Self {
            temperature: 21.0,
            duration: 20,
            cycle_mode: None,
            fan_level: None,
        }
    }
}

/// Starting, stopping and scheduling climate control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HvacCapability {
    vin: String,
    available: Option<bool>,
}

impl HvacCapability {
    /// The capability of `vin`; `available` is the capability flag (`None` when unknown).
    pub fn new(vin: impl Into<String>, available: Option<bool>) -> Self {
        Self {
            vin: vin.into(),
            available,
        }
    }

    /// Whether the vehicle supports remote climate control (an unknown flag counts as no).
    pub fn available(&self) -> bool {
        self.available.unwrap_or(false)
    }

    /// °C offset of the 1-17 scale, from the HVAC status when one is known.
    pub fn temp_offset(hvac: Option<&HvacReading>) -> f64 {
        hvac.map_or(DEFAULT_TEMP_OFFSET_C, |hvac| hvac.temp_offset)
    }

    /// The settable temperature range in °C.
    pub fn temp_range(hvac: Option<&HvacReading>) -> (f64, f64) {
        let offset = Self::temp_offset(hvac);
        (offset + 1.0, offset + 17.0)
    }

    fn ensure_available(&self) -> Result<(), Error> {
        if self.available() {
            return Ok(());
        }
        Err(unsupported(
            format!("HVAC capability not supported for VIN {}", self.vin),
            "hvac",
        ))
    }

    /// The checks `start` makes before it reads any state: availability, the
    /// fan level, and the duration. Returns the duration's `time_span` code.
    pub fn check_start(&self, start: &ClimateStart) -> Result<i64, Error> {
        self.ensure_available()?;
        if let Some(fan_level) = start.fan_level
            && !(1..=7).contains(&fan_level)
        {
            return Err(Error::new(
                ErrorKind::ValueError,
                format!("fan_level must be 1-7, got {fan_level}"),
            ));
        }
        minutes_to_time_span(start.duration)
    }

    /// Start climate control, or change the settings of a running session.
    ///
    /// `time_span` comes from [`Self::check_start`] and `hvac` is the
    /// current status: the projected snapshot's, or the freshly fetched one
    /// when the snapshot had none (pyBYD refreshes then, so the temperature
    /// scale matches the car's range). While climate runs, unset settings
    /// keep the session's own, and a change of the setpoint alone is sent as
    /// a setpoint adjustment (`airSet` `"14"`), which keeps the session and
    /// its auto-off timer. Projects HVAC status `ON`, the setpoint and the fan level.
    pub fn start(
        &self,
        start: &ClimateStart,
        time_span: i64,
        hvac: Option<&HvacReading>,
    ) -> Result<CommandPlan, Error> {
        let mut cycle_mode = start.cycle_mode;
        let mut fan_level = start.fan_level;
        let mut air_set = None;
        if let Some(hvac) = hvac
            && hvac.status == Some(HVAC_ON)
        {
            let current_cycle = hvac
                .cycle_choice
                .filter(|cycle| [EXTERNAL_AIR, INTERNAL_AIR].contains(cycle));
            let current_fan = hvac.wind_position.filter(|fan| (1..=7).contains(fan));
            let keeps =
                |asked: Option<i64>, current: Option<i64>| asked.is_none() || asked == current;
            if keeps(cycle_mode, current_cycle) && keeps(fan_level, current_fan) {
                air_set = Some("14".to_owned());
            }
            cycle_mode = cycle_mode.or(current_cycle);
            fan_level = fan_level.or(current_fan);
        }
        let params = ClimateStartParams::new(ClimateStartArgs {
            temperature: Some(start.temperature),
            time_span: Some(time_span),
            temp_offset: Self::temp_offset(hvac),
            cycle_mode: cycle_mode.unwrap_or(INTERNAL_AIR),
            wind_level: fan_level,
            air_set,
            ..ClimateStartArgs::default()
        })
        .map_err(validation_error)?;
        let mut specs = vec![
            ProjectionSpec::new("hvac", "status", json!(HVAC_ON)),
            ProjectionSpec::new("hvac", "main_setting_temp_new", json!(start.temperature)),
        ];
        if let Some(fan_level) = fan_level {
            specs.push(ProjectionSpec::new(
                "hvac",
                "wind_position",
                json!(fan_level),
            ));
        }
        Ok(CommandPlan {
            specs,
            command: PlannedCommand::StartClimate(params),
        })
    }

    /// Stop climate control, which also stops the front seats' heating and
    /// ventilation and the steering wheel heating: all projected off on both
    /// the HVAC and the realtime section.
    pub fn stop(&self) -> Result<CommandPlan, Error> {
        self.ensure_available()?;
        let mut specs = vec![ProjectionSpec::new("hvac", "status", json!(HVAC_OFF))];
        for section in ["hvac", "realtime"] {
            for (field, off) in [
                ("main_seat_heat_state", SEAT_OFF),
                ("copilot_seat_heat_state", SEAT_OFF),
                ("main_seat_ventilation_state", SEAT_OFF),
                ("copilot_seat_ventilation_state", SEAT_OFF),
                ("steering_wheel_heat_state", WHEEL_OFF),
            ] {
                specs.push(ProjectionSpec::new(section, field, Value::from(off)));
            }
        }
        Ok(CommandPlan {
            specs,
            command: PlannedCommand::StopClimate,
        })
    }

    /// Create, change or remove a climate schedule. Nothing changes on the
    /// car right away, so nothing is projected.
    pub fn schedule(&self, params: ClimateScheduleParams) -> Result<CommandPlan, Error> {
        self.ensure_available()?;
        Ok(CommandPlan::bare(PlannedCommand::ScheduleClimate(params)))
    }
}
