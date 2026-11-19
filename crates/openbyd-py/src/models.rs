//! pyBYD's reply models by class name, and the shadow comparison (`openbyd.models`, `openbyd.shadow`).
//!
//! `openbyd-protocol` has no list of its models, so [`MODELS`] names each
//! one here; a Python test checks it against every model pyBYD's parity
//! groups register.

use std::borrow::Cow;
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
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use serde_json::{Map, Value};

use crate::convert::{self, Mode};
use crate::{dump, errors};

/// What a shadow comparison returns when openbyd fails where pyBYD did not.
pub const SHADOW_ERROR: &str = "<error>";
/// What a shadow comparison returns for a class openbyd does not know.
pub const SHADOW_UNKNOWN_MODEL: &str = "<unknown model>";

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

/// A parsed model, kept as it is until the GIL is held, so its dump goes
/// straight into Python objects ([`dump::to_python`]).
trait Parsed: Send {
    /// `model_dump(mode="json")` as Python objects.
    fn dump<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>>;

    /// [`Parsed::dump`] with `raw` given as Python objects, for a model whose
    /// `raw` is the reply itself ([`BydModel::parse_reply`]). The model's own
    /// `raw` is left out of the dump instead of being converted.
    fn dump_with_raw<'py>(
        &mut self,
        py: Python<'py>,
        raw: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>>;

    /// Every `@property` of the Python class, by name.
    fn properties(&self) -> Map<String, Value>;

    /// The top-level fields of the dump other than `raw`, in order, or
    /// `None` if the dump is not an object. For the shadow comparison.
    fn fields(&self) -> Option<Vec<(Cow<'static, str>, Value)>>;
}

impl<M: BydModel + Send> Parsed for M {
    fn dump<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        dump::to_python(py, self)
    }

    fn dump_with_raw<'py>(
        &mut self,
        py: Python<'py>,
        raw: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let Some(slot) = self.raw_mut() else {
            return dump::to_python(py, &*self);
        };
        // Dump with `raw` empty, then put `raw` in its place: replacing the
        // value of a key keeps the dict's keys and their order.
        let own = std::mem::take(slot);
        let dumped = dump::to_python(py, &*self)?;
        if let Ok(fields) = dumped.cast::<PyDict>()
            && fields.contains(intern!(py, "raw"))?
        {
            fields.set_item(intern!(py, "raw"), raw)?;
            return Ok(dumped);
        }
        // No `raw` key in the dump: give the model its own back and dump that.
        if let Some(slot) = self.raw_mut() {
            *slot = own;
        }
        dump::to_python(py, &*self)
    }

    fn properties(&self) -> Map<String, Value> {
        BydModel::properties(self)
    }

    fn fields(&self) -> Option<Vec<(Cow<'static, str>, Value)>> {
        dump::fields_but_raw(self)
    }
}

/// pyBYD's `AuthToken`, which openbyd keeps outside the models.
struct Token(Value);

impl Parsed for Token {
    fn dump<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        dump::to_python(py, &self.0)
    }

    fn dump_with_raw<'py>(
        &mut self,
        py: Python<'py>,
        _raw: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.dump(py)
    }

    fn properties(&self) -> Map<String, Value> {
        Map::new()
    }

    fn fields(&self) -> Option<Vec<(Cow<'static, str>, Value)>> {
        dump::fields_but_raw(&self.0)
    }
}

/// A parsed model, and whether its `raw` is the reply itself.
type Run = fn(Value, &Value) -> Result<(Box<dyn Parsed>, bool), Failure>;

fn run<M: BydModel + Send + 'static>(
    raw: Value,
    context: &Value,
) -> Result<(Box<dyn Parsed>, bool), Failure> {
    let parsed = M::parse_reply(raw, context).map_err(Failure::Validation)?;
    Ok((Box::new(parsed.model), parsed.raw_is_reply))
}

fn run_auth_token(raw: Value, _context: &Value) -> Result<(Box<dyn Parsed>, bool), Failure> {
    let token = token::AuthToken::from_raw(&raw).map_err(Failure::Error)?;
    Ok((Box::new(Token(token.to_dump())), false))
}

/// Whether a model's `raw` can be the reply itself, so that copying the
/// caller's objects for it before the parse may pay off. Only a speed hint:
/// the copy is used only when [`BydModel::parse_reply`] says so.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Raw {
    /// It can (it has a `raw` field filled by `Fields::raw`).
    MayBeReply,
    /// It never is: no `raw` field, a before-validator that always builds a
    /// new dict (`RemoteControlResult` and the command events), a parse of
    /// a cleaned copy (`BydBaseModel`) or a parse by reference (`Vehicle`).
    NeverReply,
}

use Raw::{MayBeReply, NeverReply};

/// Every model openbyd parses, by pyBYD class name, sorted.
const MODELS: &[(&str, Run, Raw)] = &[
    (
        "AcCurrentLimit",
        run::<smart_charging::AcCurrentLimit>,
        MayBeReply,
    ),
    ("AuthToken", run_auth_token, NeverReply),
    ("BookingList", run::<booking::BookingList>, MayBeReply),
    ("BydBaseModel", run::<BydBaseModel>, NeverReply),
    (
        "ChargeChangeResult",
        run::<control::ChargeChangeResult>,
        MayBeReply,
    ),
    (
        "ChargingStatus",
        run::<charging::ChargingStatus>,
        MayBeReply,
    ),
    ("ClimateBooking", run::<booking::ClimateBooking>, MayBeReply),
    ("CommandAck", run::<control::CommandAck>, MayBeReply),
    (
        "CommandAckDiagnostics",
        run::<control::CommandAckDiagnostics>,
        MayBeReply,
    ),
    (
        "CommandAckEvent",
        run::<control::CommandAckEvent>,
        NeverReply,
    ),
    (
        "CommandGateRule",
        run::<command_gating::CommandGateRule>,
        MayBeReply,
    ),
    (
        "CommandGateVerdict",
        run::<command_gating::CommandGateVerdict>,
        MayBeReply,
    ),
    (
        "CommandLifecycleEvent",
        run::<control::CommandLifecycleEvent>,
        NeverReply,
    ),
    (
        "CumulativeEnergyConsumption",
        run::<energy::CumulativeEnergyConsumption>,
        MayBeReply,
    ),
    ("EmpowerRange", run::<vehicle::EmpowerRange>, MayBeReply),
    (
        "EnergyConsumption",
        run::<energy::EnergyConsumption>,
        MayBeReply,
    ),
    (
        "EnergyConsumptionGraph",
        run::<energy::EnergyConsumptionGraph>,
        MayBeReply,
    ),
    ("GpsInfo", run::<gps::GpsInfo>, MayBeReply),
    ("HvacStatus", run::<hvac::HvacStatus>, MayBeReply),
    (
        "LatestConfigFunction",
        run::<latest_config::LatestConfigFunction>,
        MayBeReply,
    ),
    ("MqttBootstrap", run::<mqtt::MqttBootstrap>, NeverReply),
    ("MqttEvent", run::<mqtt::MqttEvent>, NeverReply),
    (
        "NearestEnergyConsumption",
        run::<energy::NearestEnergyConsumption>,
        MayBeReply,
    ),
    (
        "PushNotificationState",
        run::<push_notification::PushNotificationState>,
        MayBeReply,
    ),
    (
        "PushSwitch",
        run::<push_notification::PushSwitch>,
        MayBeReply,
    ),
    (
        "RemoteControlResult",
        run::<control::RemoteControlResult>,
        NeverReply,
    ),
    (
        "SmartChargeDto",
        run::<smart_charging::SmartChargeDto>,
        MayBeReply,
    ),
    (
        "SmartChargingSchedule",
        run::<smart_charging::SmartChargingSchedule>,
        MayBeReply,
    ),
    (
        "SmartJourneyDto",
        run::<smart_charging::SmartJourneyDto>,
        MayBeReply,
    ),
    ("Vehicle", run::<vehicle::Vehicle>, NeverReply),
    (
        "VehicleCapabilities",
        run::<latest_config::VehicleCapabilities>,
        MayBeReply,
    ),
    (
        "VehicleLatestConfig",
        run::<latest_config::VehicleLatestConfig>,
        MayBeReply,
    ),
    (
        "VehicleMsgItemInfo",
        run::<message_center::VehicleMsgItemInfo>,
        MayBeReply,
    ),
    (
        "VehicleMsgListResponse",
        run::<message_center::VehicleMsgListResponse>,
        MayBeReply,
    ),
    (
        "VehicleRealtimeData",
        run::<realtime::VehicleRealtimeData>,
        MayBeReply,
    ),
    (
        "VerifyControlPasswordResponse",
        run::<control::VerifyControlPasswordResponse>,
        MayBeReply,
    ),
];

fn lookup(name: &str) -> Option<(Run, Raw)> {
    MODELS
        .binary_search_by(|(known, _, _)| (*known).cmp(name))
        .ok()
        .map(|index| (MODELS[index].1, MODELS[index].2))
}

fn require(name: &str) -> PyResult<(Run, Raw)> {
    lookup(name).ok_or_else(|| PyKeyError::new_err(format!("openbyd has no model named {name:?}")))
}

/// The pyBYD class names `parse_model` knows, sorted.
#[pyfunction]
pub fn model_names() -> Vec<&'static str> {
    MODELS.iter().map(|(name, _, _)| *name).collect()
}

/// Which part of a parsed model a call returns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Dump,
    Properties,
}

/// Pauses Python's cyclic garbage collector while it lives, so no finalizer
/// or weak reference callback can run between two reads of the same objects.
/// Holds the GIL's lifetime: it must be dropped before the GIL is released.
struct GcPaused<'py> {
    was_enabled: bool,
    _py: Python<'py>,
}

impl<'py> GcPaused<'py> {
    fn new(py: Python<'py>) -> Self {
        // SAFETY: `PyGC_Disable` only needs the GIL, which `py` proves is held.
        let was_enabled = unsafe { pyo3::ffi::PyGC_Disable() } != 0;
        Self {
            was_enabled,
            _py: py,
        }
    }
}

impl Drop for GcPaused<'_> {
    fn drop(&mut self) {
        if self.was_enabled {
            // SAFETY: the GIL is held for `'py`, which outlives this guard.
            unsafe { pyo3::ffi::PyGC_Enable() };
        }
    }
}

fn parse_part<'py>(
    py: Python<'py>,
    name: &str,
    raw: &Bound<'py, PyAny>,
    context: Option<&Bound<'py, PyAny>>,
    part: Part,
) -> PyResult<Bound<'py, PyAny>> {
    let (run, keeps) = require(name)?;
    let input = raw;
    // The dump's `raw` for a model that keeps the reply itself is built from
    // the caller's objects: cheaper than converting the parsed reply back.
    // It must be the snapshot the parse sees, so it is taken straight after
    // `to_value`, with the GIL held and before `detach` lets other threads
    // run. Neither conversion calls the input's own Python code (dicts are
    // read with `PyDict_Next`, numbers and text through the C API, never
    // `__iter__`, `__index__`, `__hash__` or `__eq__`). The one thing that
    // could still run in between is a garbage collection started by the
    // copy's allocations (Python 3.11 collects inside the allocation), whose
    // finalizers and weak reference callbacks are arbitrary code; the
    // collector is paused until both are done.
    let paused = GcPaused::new(py);
    let raw = convert::to_value(input, Mode::Strict)?;
    // A reply with its own `raw` key never is the model's `raw`; that is
    // read from the converted value, so no Python code runs here either.
    let own_raw = raw
        .as_object()
        .is_some_and(|reply| reply.contains_key("raw"));
    let copy = match (part, keeps) {
        (Part::Dump, MayBeReply) if !own_raw && input.is_instance_of::<PyDict>() => {
            Some(convert::round_tripped(input)?)
        }
        _ => None,
    };
    drop(paused);
    let context = match context {
        Some(context) => convert::to_value(context, Mode::Strict)?,
        None => Value::Null,
    };
    let (mut parsed, raw_is_reply) = py
        .detach(|| run(raw, &context))
        .map_err(|failure| failure.into_py_err(py))?;
    match (part, copy) {
        (Part::Dump, Some(copy)) if raw_is_reply => parsed.dump_with_raw(py, copy),
        (Part::Dump, _) => parsed.dump(py),
        (Part::Properties, _) => dump::to_python(py, &parsed.properties()),
    }
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
/// `model_dump(mode="json")`): fields `expected` has and openbyd lacks, and
/// fields both have with different values. A field only openbyd has is not
/// reported (an older pyBYD lacks it). Or one of
/// the markers [`SHADOW_UNKNOWN_MODEL`] and [`SHADOW_ERROR`].
///
/// Values are compared as JSON: `expected` is read as [`convert::to_value`]
/// reads it in [`Mode::Lenient`], so `1` differs from `1.0` and a NaN float
/// equals openbyd's `"NaN"`. `raw` is not compared, only checked to be
/// convertible, and openbyd's own `raw` is not dumped at all.
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
    let Some((run, _)) = lookup(name) else {
        return vec![SHADOW_UNKNOWN_MODEL.to_owned()];
    };
    let error = || vec![SHADOW_ERROR.to_owned()];
    let context = match context {
        Some(context) => convert::to_value(context, Mode::Strict),
        None => Ok(Value::Null),
    };
    let (Ok(raw), Ok(context), Ok(theirs)) = (
        convert::to_value(raw, Mode::Strict),
        context,
        expected_fields(expected),
    ) else {
        return error();
    };
    let ours = py.detach(|| {
        catch_unwind(AssertUnwindSafe(|| {
            run(raw, &context)
                .ok()
                .and_then(|(model, _)| model.fields())
        }))
        .ok()
        .flatten()
    });
    match (ours, theirs) {
        (Some(ours), Some(theirs)) => differing_fields(&ours, theirs),
        _ => error(),
    }
}

/// `expected` as `(key, value)` pairs, converted as [`Mode::Lenient`]
/// converts them, except that `raw` is only checked (`None`). `Ok(None)`
/// if `expected` is not a dict; an error where the conversion would fail.
/// pyBYD's top-level fields by name; `None` for `raw`, which is only checked.
type ExpectedFields = Vec<(String, Option<Value>)>;

fn expected_fields(expected: &Bound<'_, PyAny>) -> PyResult<Option<ExpectedFields>> {
    let Ok(entries) = expected.cast::<PyDict>() else {
        // Not an object once converted, so the comparison fails whether it converts or not.
        return Ok(None);
    };
    let mut fields = Vec::with_capacity(entries.len());
    for (key, item) in entries.iter() {
        let key = convert::dict_key(&key)?.to_owned();
        let value = if key == "raw" {
            convert::check_item(&item, Mode::Lenient)?;
            None
        } else {
            Some(convert::item_to_value(&item, Mode::Lenient)?)
        };
        fields.push((key, value));
    }
    Ok(Some(fields))
}

/// The sorted top-level keys other than `raw` that pyBYD's dump has and
/// openbyd's lacks, or that both have with different values.
///
/// A key only openbyd has is not reported: it means the installed pyBYD is
/// older than the commit openbyd follows (which added the field), not that
/// the two parse differently.
///
/// `raw` is left out: it is the input echoed back, and openbyd only sees
/// `model.raw`, which is not always the input pyBYD was given (a model that
/// wraps a list reply keeps `{"list": [...]}` there).
fn differing_fields(
    ours: &[(Cow<'static, str>, Value)],
    mut theirs: ExpectedFields,
) -> Vec<String> {
    let mut names = Vec::new();
    let mut matched = vec![false; theirs.len()];
    for (index, (key, value)) in ours.iter().enumerate() {
        // Both sides are usually in the same order; search only when not.
        let position = match theirs.get(index) {
            Some((their_key, _)) if their_key == key => Some(index),
            _ => theirs.iter().position(|(their_key, _)| their_key == key),
        };
        if let Some(position) = position {
            matched[position] = true;
            if theirs[position].1.as_ref() != Some(value) {
                names.push(key.to_string());
            }
        }
    }
    for ((key, _), matched) in theirs.iter_mut().zip(matched) {
        if !matched && key != "raw" {
            names.push(std::mem::take(key));
        }
    }
    names.sort_unstable();
    names
}
