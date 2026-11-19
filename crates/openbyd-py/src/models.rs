//! pyBYD's reply models by class name, and the shadow comparison (`openbyd.models`, `openbyd.shadow`).
//!
//! `openbyd-protocol` has no list of its models, so [`MODELS`] names each
//! one here; a Python test checks it against every model pyBYD's parity
//! groups register.

use std::panic::{AssertUnwindSafe, catch_unwind};

use openbyd_protocol::coerce::ValidationError;
use openbyd_protocol::error::Error;
use openbyd_protocol::models::base::{BydBaseModel, BydModel};
use openbyd_protocol::models::{
    booking, charging, command_gating, control, energy, gps, hvac, latest_config, message_center,
    push_notification, realtime, smart_charging, token, vehicle,
};
use openbyd_protocol::mqtt;
use pyo3::exceptions::PyKeyError;
use pyo3::prelude::*;
use serde_json::{Map, Value};

use crate::convert::{self, Mode};
use crate::errors;

/// What a shadow comparison returns when openbyd fails where pyBYD did not.
pub const SHADOW_ERROR: &str = "<error>";
/// What a shadow comparison returns for a class openbyd does not know.
pub const SHADOW_UNKNOWN_MODEL: &str = "<unknown model>";

/// Which parts of a parsed model to compute.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Dump,
    Properties,
}

/// Why a model could not be parsed.
enum Failure {
    Validation(ValidationError),
    Error(Error),
}

impl Failure {
    fn into_py_err(self, py: Python<'_>) -> PyErr {
        match self {
            Failure::Validation(error) => errors::validation_to_py_err(py, &error),
            Failure::Error(error) => errors::to_py_err(py, &error),
        }
    }
}

type Run = fn(&Value, &Value, Part) -> Result<Value, Failure>;

fn run<M: BydModel>(raw: &Value, context: &Value, part: Part) -> Result<Value, Failure> {
    let model = M::parse(raw, context).map_err(Failure::Validation)?;
    Ok(match part {
        Part::Dump => model.dump(),
        Part::Properties => Value::Object(model.properties()),
    })
}

fn run_auth_token(raw: &Value, _context: &Value, part: Part) -> Result<Value, Failure> {
    let token = token::AuthToken::from_raw(raw).map_err(Failure::Error)?;
    Ok(match part {
        Part::Dump => token.to_dump(),
        Part::Properties => Value::Object(Map::new()),
    })
}

/// Every model openbyd parses, by pyBYD class name, sorted.
const MODELS: &[(&str, Run)] = &[
    ("AcCurrentLimit", run::<smart_charging::AcCurrentLimit>),
    ("AuthToken", run_auth_token),
    ("BookingList", run::<booking::BookingList>),
    ("BydBaseModel", run::<BydBaseModel>),
    ("ChargeChangeResult", run::<control::ChargeChangeResult>),
    ("ChargingStatus", run::<charging::ChargingStatus>),
    ("ClimateBooking", run::<booking::ClimateBooking>),
    ("CommandAck", run::<control::CommandAck>),
    (
        "CommandAckDiagnostics",
        run::<control::CommandAckDiagnostics>,
    ),
    ("CommandAckEvent", run::<control::CommandAckEvent>),
    ("CommandGateRule", run::<command_gating::CommandGateRule>),
    (
        "CommandGateVerdict",
        run::<command_gating::CommandGateVerdict>,
    ),
    (
        "CommandLifecycleEvent",
        run::<control::CommandLifecycleEvent>,
    ),
    (
        "CumulativeEnergyConsumption",
        run::<energy::CumulativeEnergyConsumption>,
    ),
    ("EmpowerRange", run::<vehicle::EmpowerRange>),
    ("EnergyConsumption", run::<energy::EnergyConsumption>),
    (
        "EnergyConsumptionGraph",
        run::<energy::EnergyConsumptionGraph>,
    ),
    ("GpsInfo", run::<gps::GpsInfo>),
    ("HvacStatus", run::<hvac::HvacStatus>),
    (
        "LatestConfigFunction",
        run::<latest_config::LatestConfigFunction>,
    ),
    ("MqttBootstrap", run::<mqtt::MqttBootstrap>),
    ("MqttEvent", run::<mqtt::MqttEvent>),
    (
        "NearestEnergyConsumption",
        run::<energy::NearestEnergyConsumption>,
    ),
    (
        "PushNotificationState",
        run::<push_notification::PushNotificationState>,
    ),
    ("PushSwitch", run::<push_notification::PushSwitch>),
    ("RemoteControlResult", run::<control::RemoteControlResult>),
    ("SmartChargeDto", run::<smart_charging::SmartChargeDto>),
    (
        "SmartChargingSchedule",
        run::<smart_charging::SmartChargingSchedule>,
    ),
    ("SmartJourneyDto", run::<smart_charging::SmartJourneyDto>),
    ("Vehicle", run::<vehicle::Vehicle>),
    (
        "VehicleCapabilities",
        run::<latest_config::VehicleCapabilities>,
    ),
    (
        "VehicleLatestConfig",
        run::<latest_config::VehicleLatestConfig>,
    ),
    (
        "VehicleMsgItemInfo",
        run::<message_center::VehicleMsgItemInfo>,
    ),
    (
        "VehicleMsgListResponse",
        run::<message_center::VehicleMsgListResponse>,
    ),
    ("VehicleRealtimeData", run::<realtime::VehicleRealtimeData>),
    (
        "VerifyControlPasswordResponse",
        run::<control::VerifyControlPasswordResponse>,
    ),
];

fn lookup(name: &str) -> Option<Run> {
    MODELS
        .binary_search_by(|(known, _)| (*known).cmp(name))
        .ok()
        .map(|index| MODELS[index].1)
}

fn require(name: &str) -> PyResult<Run> {
    lookup(name).ok_or_else(|| PyKeyError::new_err(format!("openbyd has no model named {name:?}")))
}

/// The pyBYD class names `parse_model` knows, sorted.
#[pyfunction]
pub fn model_names() -> Vec<&'static str> {
    MODELS.iter().map(|(name, _)| *name).collect()
}

fn parse_part<'py>(
    py: Python<'py>,
    name: &str,
    raw: &Bound<'py, PyAny>,
    context: Option<&Bound<'py, PyAny>>,
    part: Part,
) -> PyResult<Bound<'py, PyAny>> {
    let run = require(name)?;
    let raw = convert::to_value(raw, Mode::Strict)?;
    let context = match context {
        Some(context) => convert::to_value(context, Mode::Strict)?,
        None => Value::Null,
    };
    let parsed = py
        .detach(|| run(&raw, &context, part))
        .map_err(|failure| failure.into_py_err(py))?;
    convert::dump_to_python(py, &parsed)
}

/// `Model.model_validate(raw, context=context).model_dump(mode="json")` for
/// the pyBYD model class `name`, parsed without the GIL.
#[pyfunction]
#[pyo3(signature = (name, raw, context = None))]
pub fn parse_model<'py>(
    py: Python<'py>,
    name: &str,
    raw: &Bound<'py, PyAny>,
    context: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    parse_part(py, name, raw, context, Part::Dump)
}

/// The value of every `@property` of the pyBYD model class `name` for
/// `raw`, by property name.
#[pyfunction]
#[pyo3(signature = (name, raw, context = None))]
pub fn model_properties<'py>(
    py: Python<'py>,
    name: &str,
    raw: &Bound<'py, PyAny>,
    context: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    parse_part(py, name, raw, context, Part::Properties)
}

/// The sorted names of the top-level fields (other than `raw`) where
/// openbyd's dump of `raw` differs from `expected` (pyBYD's
/// `model_dump(mode="json")`), or one of
/// the markers [`SHADOW_UNKNOWN_MODEL`] and [`SHADOW_ERROR`].
///
/// Never raises for bad values: anything openbyd cannot convert or parse is
/// [`SHADOW_ERROR`]. Values never appear in the answer.
#[pyfunction]
#[pyo3(signature = (name, raw, expected, context = None))]
pub fn shadow_compare(
    py: Python<'_>,
    name: &str,
    raw: &Bound<'_, PyAny>,
    expected: &Bound<'_, PyAny>,
    context: Option<&Bound<'_, PyAny>>,
) -> Vec<String> {
    let Some(run) = lookup(name) else {
        return vec![SHADOW_UNKNOWN_MODEL.to_owned()];
    };
    let error = || vec![SHADOW_ERROR.to_owned()];
    let context = match context {
        Some(context) => convert::to_value(context, Mode::Strict),
        None => Ok(Value::Null),
    };
    let (Ok(raw), Ok(expected), Ok(context)) = (
        convert::to_value(raw, Mode::Strict),
        convert::to_value(expected, Mode::Lenient),
        context,
    ) else {
        return error();
    };
    py.detach(|| {
        catch_unwind(AssertUnwindSafe(|| {
            run(&raw, &context, Part::Dump)
                .ok()
                .and_then(|ours| differing_fields(&ours, &expected))
        }))
        .ok()
        .flatten()
        .unwrap_or_else(error)
    })
}

/// The sorted top-level keys whose values differ, or `None` if either side
/// is not an object.
///
/// `raw` is left out: it is the input echoed back, and openbyd only sees
/// `model.raw`, which is not always the input pyBYD was given (a model that
/// wraps a list reply keeps `{"list": [...]}` there).
fn differing_fields(ours: &Value, theirs: &Value) -> Option<Vec<String>> {
    let (ours, theirs) = (ours.as_object()?, theirs.as_object()?);
    let mut names: Vec<String> = ours
        .keys()
        .chain(theirs.keys().filter(|key| !ours.contains_key(*key)))
        .filter(|key| *key != "raw" && ours.get(*key) != theirs.get(*key))
        .cloned()
        .collect();
    names.sort_unstable();
    Some(names)
}
