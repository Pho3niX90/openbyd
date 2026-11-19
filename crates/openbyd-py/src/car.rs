//! `openbyd._native.Car`: a car kept by a client ([`ClientCar`]), wrapped by
//! `openbyd.Car` (`python/openbyd/car.py`), which documents it.
//!
//! The capability commands go through one entry point,
//! [`PyCar::command`], named by feature and action as pyBYD spells them
//! (`"lock"`, `"lock"`); the Python side gives each feature its namespace.

use openbyd_client::car::{ClimateStart, SeatLevel, SeatMode, SeatPosition};
use openbyd_client::client::{ClientCar, ProductionEnv};
use openbyd_protocol::coerce::StrEnum;
use openbyd_protocol::error::Error;
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::charging::ChargingStatus;
use openbyd_protocol::models::control::ClimateScheduleParams;
use openbyd_protocol::models::energy::EnergyConsumption;
use openbyd_protocol::models::realtime::VehicleRealtimeData;
use openbyd_protocol::models::vehicle::EnergyType;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use serde_json::{Map, Value, json};

use crate::bridge;
use crate::client::{Dump, object};
use crate::errors;
use crate::{convert, dump};

/// A car. See `openbyd.Car`.
#[pyclass(frozen, module = "openbyd._native", name = "Car")]
pub struct PyCar {
    car: ClientCar<ProductionEnv>,
}

impl PyCar {
    pub fn new(car: ClientCar<ProductionEnv>) -> Self {
        Self { car }
    }
}

fn seat_position(value: &Bound<'_, PyAny>) -> PyResult<SeatPosition> {
    value
        .extract::<String>()
        .ok()
        .and_then(|text| SeatPosition::from_value(&text))
        .ok_or_else(|| PyValueError::new_err(format!("{value} is not a valid SeatPosition")))
}

fn seat_level(value: &Bound<'_, PyAny>) -> PyResult<SeatLevel> {
    value
        .extract::<i64>()
        .ok()
        .and_then(SeatLevel::from_value)
        .ok_or_else(|| PyValueError::new_err(format!("{value} is not a valid SeatLevel")))
}

fn optional_int(args: &Map<String, Value>, key: &str) -> PyResult<Option<i64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| PyTypeError::new_err(format!("{key} must be an int"))),
    }
}

fn climate_start(args: &Map<String, Value>) -> PyResult<ClimateStart> {
    let defaults = ClimateStart::default();
    Ok(ClimateStart {
        temperature: match args.get("temperature") {
            None | Some(Value::Null) => defaults.temperature,
            Some(value) => value
                .as_f64()
                .ok_or_else(|| PyTypeError::new_err("temperature must be a number"))?,
        },
        duration: optional_int(args, "duration")?.unwrap_or(defaults.duration),
        cycle_mode: optional_int(args, "cycle_mode")?,
        fan_level: optional_int(args, "fan_level")?,
    })
}

/// What each feature offers: availability, and the climate scale.
fn features(car: &ClientCar<ProductionEnv>) -> Value {
    let (low, high) = car.hvac.temp_range();
    let mut seats = Map::new();
    for position in [
        SeatPosition::Driver,
        SeatPosition::Copilot,
        SeatPosition::RearLeft,
        SeatPosition::RearRight,
    ] {
        seats.insert(
            StrEnum::value(position).to_owned(),
            json!({
                "heat": car.seat.is_mode_available(position, SeatMode::Heat),
                "ventilation": car.seat.is_mode_available(position, SeatMode::Vent),
            }),
        );
    }
    json!({
        "lock": {"available": car.lock.available()},
        "hvac": {
            "available": car.hvac.available(),
            "temp_offset": car.hvac.temp_offset(),
            "temp_range": [low, high],
        },
        "seat": seats,
        "steering": {"available": car.steering.available()},
        "battery": {"available": car.battery.available()},
        "finder": {
            "find_available": car.finder.find_available(),
            "flash_available": car.finder.flash_available(),
        },
        "windows": {
            "close_available": car.windows.close_available(),
            "open_available": car.windows.open_available(),
        },
        "trunk": {
            "open_available": car.trunk.open_available(),
            "close_available": car.trunk.close_available(),
        },
    })
}

/// A capability command, checked and built while the GIL is held.
enum Command {
    Lock,
    Unlock,
    HvacStart(ClimateStart),
    HvacStop,
    HvacSchedule(ClimateScheduleParams),
    SeatHeat(SeatPosition, SeatLevel),
    SeatVentilation(SeatPosition, SeatLevel),
    SteeringHeat(bool),
    BatteryHeat(bool),
    Find,
    FlashLights,
    WindowsClose,
    WindowsOpen,
    TrunkOpen,
    TrunkClose,
}

impl Command {
    fn parse(
        py: Python<'_>,
        feature: &str,
        action: &str,
        args: &Bound<'_, PyAny>,
    ) -> PyResult<Self> {
        let get = |key: &str| -> PyResult<Bound<'_, PyAny>> {
            args.get_item(key)
                .map_err(|_| PyTypeError::new_err(format!("{feature}.{action} needs {key}")))
        };
        Ok(match (feature, action) {
            ("lock", "lock") => Self::Lock,
            ("lock", "unlock") => Self::Unlock,
            ("hvac", "start") => Self::HvacStart(climate_start(&object("args", args)?)?),
            ("hvac", "stop") => Self::HvacStop,
            ("hvac", "schedule") => Self::HvacSchedule(
                ClimateScheduleParams::from_kwargs(&object("params", &get("params")?)?)
                    .map_err(|error| errors::validation_to_py_err(py, &error))?,
            ),
            ("seat", "heat") => Self::SeatHeat(
                seat_position(&get("position")?)?,
                seat_level(&get("level")?)?,
            ),
            ("seat", "ventilation") => Self::SeatVentilation(
                seat_position(&get("position")?)?,
                seat_level(&get("level")?)?,
            ),
            ("steering", "heat") => Self::SteeringHeat(get("on")?.is_truthy()?),
            ("battery", "heat") => Self::BatteryHeat(get("on")?.is_truthy()?),
            ("finder", "find") => Self::Find,
            ("finder", "flash_lights") => Self::FlashLights,
            ("windows", "close") => Self::WindowsClose,
            ("windows", "open") => Self::WindowsOpen,
            ("trunk", "open") => Self::TrunkOpen,
            ("trunk", "close") => Self::TrunkClose,
            _ => {
                return Err(PyValueError::new_err(format!(
                    "no capability command {feature}.{action}"
                )));
            }
        })
    }

    async fn run(self, car: &ClientCar<ProductionEnv>) -> Result<(), Error> {
        match self {
            Self::Lock => car.lock.lock().await,
            Self::Unlock => car.lock.unlock().await,
            Self::HvacStart(start) => car.hvac.start(start).await,
            Self::HvacStop => car.hvac.stop().await,
            Self::HvacSchedule(params) => car.hvac.schedule(params).await,
            Self::SeatHeat(position, level) => car.seat.heat(position, level).await,
            Self::SeatVentilation(position, level) => car.seat.ventilation(position, level).await,
            Self::SteeringHeat(on) => car.steering.heat(on).await,
            Self::BatteryHeat(on) => car.battery.heat(on).await,
            Self::Find => car.finder.find().await,
            Self::FlashLights => car.finder.flash_lights().await,
            Self::WindowsClose => car.windows.close().await,
            Self::WindowsOpen => car.windows.open().await,
            Self::TrunkOpen => car.trunk.open().await,
            Self::TrunkClose => car.trunk.close().await,
        }
    }
}

#[pymethods]
impl PyCar {
    #[getter]
    fn vin(&self) -> &str {
        self.car.vin()
    }

    /// The current state with active projections laid over it, by section.
    fn state<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::dump_to_python(py, &self.car.state().dump())
    }

    fn capabilities<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        dump::to_python(py, self.car.capabilities())
    }

    fn features<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        convert::dump_to_python(py, &features(&self.car))
    }

    /// Fetches one section (`realtime`, `hvac`, `gps`, `charging`, `energy`) and merges it.
    fn update<'py>(&self, py: Python<'py>, section: &str) -> PyResult<Bound<'py, PyAny>> {
        let car = self.car.clone();
        let section = section.to_owned();
        if !["realtime", "hvac", "gps", "charging", "energy"].contains(&section.as_str()) {
            return Err(PyValueError::new_err(format!("no section {section:?}")));
        }
        bridge::spawn(py, async move {
            Ok::<_, Error>(match section.as_str() {
                "realtime" => Dump::new(car.update_realtime().await?),
                "hvac" => Dump::new(car.update_hvac().await?),
                "gps" => Dump::new(car.update_gps().await?),
                "charging" => Dump::new(car.update_charging().await?),
                _ => Dump::new(car.update_energy().await?),
            })
        })
    }

    fn start_charging<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let car = self.car.clone();
        bridge::spawn(py, async move { car.start_charging().await.map(Dump::new) })
    }

    fn stop_charging<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let car = self.car.clone();
        bridge::spawn(py, async move { car.stop_charging().await.map(Dump::new) })
    }

    fn get_ac_current_limit<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let car = self.car.clone();
        bridge::spawn(py, async move {
            car.get_ac_current_limit().await.map(Dump::new)
        })
    }

    #[pyo3(signature = (*, start_charge_time, end_charge_time, charge_way, enabled = true))]
    fn save_charging_schedule<'py>(
        &self,
        py: Python<'py>,
        start_charge_time: String,
        end_charge_time: String,
        charge_way: String,
        enabled: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let car = self.car.clone();
        bridge::spawn(py, async move {
            car.save_charging_schedule(&start_charge_time, &end_charge_time, &charge_way, enabled)
                .await
                .map(Dump::new)
        })
    }

    /// Merges an MQTT push of `kind` (`realtime`, `charging`, `energy`): the
    /// raw `respondData`, validated as the client validates a push.
    fn handle_mqtt(&self, py: Python<'_>, kind: &str, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let raw = Value::Object(object("data", data)?);
        let _entered = bridge::handle()?.enter();
        let invalid = |error| errors::validation_to_py_err(py, &error);
        match kind {
            "realtime" => {
                let energy_type = match self.car.state().vehicle.energy_type {
                    EnergyType::Unknown => EnergyType::Ev,
                    known => known,
                };
                let context = json!({"energy_type": {"EnergyType": energy_type as i64}});
                let data = VehicleRealtimeData::parse(&raw, &context).map_err(invalid)?;
                self.car.handle_mqtt_realtime(data);
            }
            "charging" => {
                let data = ChargingStatus::parse(&raw, &Value::Null).map_err(invalid)?;
                self.car.handle_mqtt_charging(data);
            }
            "energy" => {
                let data = EnergyConsumption::parse(&raw, &Value::Null).map_err(invalid)?;
                self.car.handle_mqtt_energy(data);
            }
            other => return Err(PyValueError::new_err(format!("no push kind {other:?}"))),
        }
        Ok(())
    }

    /// Runs the capability command `feature.action` with `args` (a dict).
    fn command<'py>(
        &self,
        py: Python<'py>,
        feature: &str,
        action: &str,
        args: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let command = Command::parse(py, feature, action, args)?;
        let car = self.car.clone();
        bridge::spawn(py, async move { command.run(&car).await })
    }

    fn close(&self) {
        self.car.close();
    }

    /// Whether `other` is the same car (the one the client keeps).
    fn same_car(&self, other: PyRef<'_, PyCar>) -> bool {
        std::sync::Arc::ptr_eq(&self.car, &other.car)
    }

    fn __repr__(&self) -> String {
        format!("Car(vin={:?})", self.car.vin())
    }
}
