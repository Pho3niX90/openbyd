//! `openbyd._native.Client`: the Rust [`BydClient`] for Python, wrapped by
//! `openbyd.Client` (`python/openbyd/client.py`), which documents it.
//!
//! Every client runs on [`ProductionEnv`] with the process's one HTTP pool
//! ([`crate::lowlevel::pool`]), so a client costs a few small allocations
//! and no thread or connection of its own until it is used. Its methods
//! return `asyncio` futures through [`bridge::spawn`]; results are
//! `model_dump(mode="json")` dicts. User callbacks go through
//! [`LoopHooks`], so they run on the client's event loop.

use std::collections::HashMap;
use std::sync::Arc;

use openbyd_client::client::{
    BydClient, ChargePollSettings, ClientOptions, GetCarOptions, PollSettings, ProductionEnv,
    ReloginPolicy, RequestOutcome, RequestRecord, RunPollSettings, SessionTokens,
    resolve_command_pwd,
};
use openbyd_client::http::HttpEnv;
use openbyd_protocol::coerce::{ValidationError, int_enum};
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::command_gating::CommandGateVerdict;
use openbyd_protocol::models::control::{
    BatteryHeatParams, ClimateScheduleParams, ClimateStartParams, SeatClimateParams,
};
use openbyd_protocol::models::message_center::NotificationCategory;
use openbyd_protocol::models::standby::{
    Repeat, StandbyPlan, StandbyPlanCommand, StandbySchedule, pydantic_datetime,
};
use openbyd_protocol::models::vehicle::{EnergyType, Vehicle};
use openbyd_protocol::state::StateChangedCallback;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::bridge::{self, IntoPython};
use crate::car::PyCar;
use crate::convert::{self, Mode};
use crate::dump::{self, DumpToPython};
use crate::errors;
use crate::hooks::LoopHooks;

/// The reasons the built-in gate policy `"allow_missing_data"` lets through:
/// the car's capability data is incomplete, not refusing.
const MISSING_DATA_REASONS: [&str; 2] = ["function_no_missing", "learn_info_missing"];

/// A model (or a list of them, or a JSON value) on its way to Python,
/// converted on the event loop's thread into what pyBYD's
/// `model_dump(mode="json")` gives, without a JSON value in between.
pub struct Dump(Box<dyn DumpToPython>);

impl Dump {
    /// Wraps a model, a `Vec` of models or a JSON value.
    pub fn new(value: impl DumpToPython + 'static) -> Self {
        Self(Box::new(value))
    }
}

impl IntoPython for Dump {
    fn into_python(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.0.dump_to_python(py).map(Bound::unbind)
    }
}

/// `(key, value)` pairs serialised as a map, in order.
struct Entries<T>(Vec<(String, T)>);

impl<T: Serialize> Serialize for Entries<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(key, value)| (key, value)))
    }
}

/// A dict argument as a JSON object.
pub fn object(what: &str, value: &Bound<'_, PyAny>) -> PyResult<Map<String, Value>> {
    let value = match value.getattr("model_dump") {
        // A pydantic model (such as pyBYD's own params): its fields.
        Ok(model_dump) if !value.is_instance_of::<PyDict>() => model_dump.call0()?,
        _ => value.clone(),
    };
    match convert::to_value(&value, Mode::Strict)? {
        Value::Object(map) => Ok(map),
        _ => Err(PyTypeError::new_err(format!("{what} must be a dict"))),
    }
}

/// A model argument given as its dict: the raw API dict or a
/// `model_dump(mode="json")` (pyBYD's models read both).
fn model<M: BydModel>(py: Python<'_>, what: &str, value: &Bound<'_, PyAny>) -> PyResult<M> {
    M::parse_owned(Value::Object(object(what, value)?), &Value::Null)
        .map_err(|error| errors::validation_to_py_err(py, &error))
}

/// The `schedule` argument: `None`, or `StandbySchedule`'s fields as a dict.
///
/// The Python package adds `_daily_after` to a daily schedule: the run one
/// calendar day after `at` in the caller's time zone, which only Python
/// can work out ([`StandbySchedule::with_daily_after`]).
fn schedule_argument(
    py: Python<'_>,
    schedule: Option<Bound<'_, PyAny>>,
) -> PyResult<Option<StandbySchedule>> {
    let Some(schedule) = schedule.filter(|schedule| !schedule.is_none()) else {
        return Ok(None);
    };
    let invalid = |error: ValidationError| errors::validation_to_py_err(py, &error);
    let mut fields = object("schedule", &schedule)?;
    let daily_after = fields.remove("_daily_after");
    let schedule = StandbySchedule::from_kwargs(&fields).map_err(invalid)?;
    if schedule.repeat() != Repeat::Daily {
        return Ok(Some(schedule));
    }
    Ok(Some(match daily_after {
        None | Some(Value::Null) => schedule,
        Some(following) => match pydantic_datetime(&following).map_err(invalid)?.aware() {
            Some(following) => schedule.with_daily_after(following),
            None => schedule,
        },
    }))
}

fn callable(what: &str, value: Option<Bound<'_, PyAny>>) -> PyResult<Option<Py<PyAny>>> {
    match value {
        None => Ok(None),
        Some(value) if value.is_none() => Ok(None),
        Some(value) if value.is_callable() => Ok(Some(value.unbind())),
        Some(_) => Err(PyTypeError::new_err(format!("{what} must be callable"))),
    }
}

fn to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    convert::dump_to_python(py, value)
}

fn outcome_dict<'py>(py: Python<'py>, record: &RequestRecord) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("endpoint", &record.endpoint)?;
    let (outcome, code, status, error): (&str, Option<&str>, Option<u16>, Option<&str>) =
        match &record.outcome {
            RequestOutcome::Reply { code } => ("reply", code.as_deref(), None, None),
            RequestOutcome::Transport { status } => ("transport", None, *status, None),
            RequestOutcome::Failed(kind) => ("failed", None, None, Some(kind.class_name())),
        };
    dict.set_item("outcome", outcome)?;
    dict.set_item("code", code)?;
    dict.set_item("status", status)?;
    dict.set_item("error", error)?;
    dict.set_item("duration", record.duration.as_secs_f64())?;
    Ok(dict)
}

/// The session as `export_session` gives it and `import_session` takes it.
fn tokens_to_dict<'py>(py: Python<'py>, tokens: &SessionTokens) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("user_id", &tokens.user_id)?;
    dict.set_item("sign_token", &tokens.sign_token)?;
    dict.set_item("encry_token", &tokens.encry_token)?;
    dict.set_item("refresh_token", &tokens.refresh_token)?;
    dict.set_item("ttl_remaining", tokens.ttl_remaining)?;
    let guests = PyDict::new(py);
    let mut vins: Vec<_> = tokens.guest_vins.iter().collect();
    vins.sort();
    for (vin, guest) in vins {
        guests.set_item(vin, guest)?;
    }
    dict.set_item("guest_vins", guests)?;
    Ok(dict)
}

fn tokens_from_value(value: &Value) -> Result<SessionTokens, String> {
    let Value::Object(map) = value else {
        return Err("a session is a dict".into());
    };
    let text = |key: &str| -> Result<String, String> {
        match map.get(key) {
            Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
            _ => Err(format!("session[{key:?}] must be a non-empty str")),
        }
    };
    let ttl_remaining = match map.get("ttl_remaining") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_f64()
                .ok_or("session['ttl_remaining'] must be a number or None")?,
        ),
    };
    // Optional: records saved before openbyd kept it have none.
    let refresh_token = match map.get("refresh_token") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(token)) => token.clone(),
        Some(_) => return Err("session['refresh_token'] must be a str or None".into()),
    };
    let mut guest_vins = HashMap::new();
    match map.get("guest_vins") {
        None | Some(Value::Null) => {}
        Some(Value::Object(guests)) => {
            for (vin, guest) in guests {
                let guest = guest
                    .as_bool()
                    .ok_or("session['guest_vins'] maps VINs to bools")?;
                guest_vins.insert(vin.clone(), guest);
            }
        }
        Some(_) => return Err("session['guest_vins'] must be a dict".into()),
    }
    Ok(SessionTokens {
        user_id: text("user_id")?,
        sign_token: text("sign_token")?,
        encry_token: text("encry_token")?,
        refresh_token,
        ttl_remaining,
        guest_vins,
    })
}

/// Parses a session dict for `import_session` (`ValueError` when malformed).
pub fn tokens_from_python(value: &Bound<'_, PyAny>) -> PyResult<SessionTokens> {
    tokens_from_value(&convert::to_value(value, Mode::Strict)?).map_err(PyValueError::new_err)
}

/// The options of a client, from the constructor's arguments.
struct Options<'py> {
    on_vehicle_info: Option<Bound<'py, PyAny>>,
    on_mqtt_event: Option<Bound<'py, PyAny>>,
    on_command_ack: Option<Bound<'py, PyAny>>,
    on_command_lifecycle: Option<Bound<'py, PyAny>>,
    on_mqtt_connect: Option<Bound<'py, PyAny>>,
    on_mqtt_disconnect: Option<Bound<'py, PyAny>>,
    on_mqtt_connack_refused: Option<Bound<'py, PyAny>>,
    command_ack_ttl_seconds: f64,
    gate_policy: Option<Bound<'py, PyAny>>,
    command_retries: i64,
    pin_resolver: Option<Bound<'py, PyAny>>,
    relogin: Option<Bound<'py, PyAny>>,
    request_observer: Option<Bound<'py, PyAny>>,
}

impl Options<'_> {
    #[allow(clippy::too_many_lines)]
    fn build(self, hooks: &Arc<LoopHooks>) -> PyResult<ClientOptions> {
        let mut options = ClientOptions::default();
        if !self.command_ack_ttl_seconds.is_finite() {
            return Err(PyValueError::new_err(
                "command_ack_ttl_seconds must be finite",
            ));
        }
        options.command_ack_ttl_seconds = self.command_ack_ttl_seconds;
        if self.command_retries < 1 {
            return Err(PyValueError::new_err("command_retries must be at least 1"));
        }
        options.command_retries = self.command_retries;

        if let Some(callback) = callable("on_vehicle_info", self.on_vehicle_info)? {
            let hooks = Arc::clone(hooks);
            options.on_vehicle_info = Some(Arc::new(move |vin, data| {
                hooks.post("on_vehicle_info", &callback, |py| {
                    PyTuple::new(
                        py,
                        [
                            vin.into_pyobject(py)?.into_any(),
                            dump::to_python(py, data)?,
                        ],
                    )
                });
            }));
        }
        if let Some(callback) = callable("on_mqtt_event", self.on_mqtt_event)? {
            let hooks = Arc::clone(hooks);
            options.on_mqtt_event = Some(Arc::new(move |event, vin, data| {
                let data = Value::Object(data.clone());
                hooks.post("on_mqtt_event", &callback, |py| {
                    PyTuple::new(
                        py,
                        [
                            event.into_pyobject(py)?.into_any(),
                            vin.into_pyobject(py)?.into_any(),
                            convert::to_python(py, &data)?,
                        ],
                    )
                });
            }));
        }
        if let Some(callback) = callable("on_command_ack", self.on_command_ack)? {
            let hooks = Arc::clone(hooks);
            options.on_command_ack = Some(Arc::new(move |ack| {
                hooks.post("on_command_ack", &callback, |py| {
                    PyTuple::new(py, [dump::to_python(py, ack)?])
                });
            }));
        }
        if let Some(callback) = callable("on_command_lifecycle", self.on_command_lifecycle)? {
            let hooks = Arc::clone(hooks);
            options.on_command_lifecycle = Some(Arc::new(move |event| {
                hooks.post("on_command_lifecycle", &callback, |py| {
                    PyTuple::new(py, [dump::to_python(py, event)?])
                });
            }));
        }
        if let Some(callback) = callable("on_mqtt_connect", self.on_mqtt_connect)? {
            let hooks = Arc::clone(hooks);
            options.on_mqtt_connect = Some(Arc::new(move || {
                hooks.post("on_mqtt_connect", &callback, |py| Ok(PyTuple::empty(py)));
            }));
        }
        if let Some(callback) = callable("on_mqtt_disconnect", self.on_mqtt_disconnect)? {
            let hooks = Arc::clone(hooks);
            options.on_mqtt_disconnect = Some(Arc::new(move |reason| {
                let reason = reason.to_owned();
                hooks.post("on_mqtt_disconnect", &callback, move |py| {
                    PyTuple::new(py, [reason.as_str()])
                });
            }));
        }
        if let Some(callback) = callable("on_mqtt_connack_refused", self.on_mqtt_connack_refused)? {
            let hooks = Arc::clone(hooks);
            options.on_mqtt_connack_refused = Some(Arc::new(move |consecutive| {
                hooks.post("on_mqtt_connack_refused", &callback, move |py| {
                    PyTuple::new(py, [consecutive])
                });
            }));
        }
        if let Some(callback) = callable("request_observer", self.request_observer)? {
            let hooks = Arc::clone(hooks);
            options.request_observer = Some(Arc::new(move |record| {
                hooks.post("request_observer", &callback, |py| {
                    PyTuple::new(py, [outcome_dict(py, record)?])
                });
            }));
        }
        options.gate_policy = gate_policy(self.gate_policy, hooks)?;
        if let Some(callback) = callable("pin_resolver", self.pin_resolver)? {
            let hooks = Arc::clone(hooks);
            options.pin_resolver = Some(Arc::new(move |given, config: &BydConfig| {
                let answer = hooks.decide("pin_resolver", &callback, |py| {
                    PyTuple::new(
                        py,
                        [
                            given.into_pyobject(py)?.into_any(),
                            config.control_pin.as_deref().into_pyobject(py)?.into_any(),
                        ],
                    )
                });
                let resolved = answer.and_then(|value| {
                    Python::try_attach(|py| value.extract::<String>(py).ok()).flatten()
                });
                resolved.unwrap_or_else(|| {
                    let _ = Python::try_attach(|py| {
                        crate::hooks::log(
                            py,
                            crate::hooks::WARNING,
                            "pin_resolver gave no str; using the default resolver",
                        );
                    });
                    resolve_command_pwd(given, config)
                })
            }));
        }
        options.relogin = relogin_policy(self.relogin, hooks)?;
        Ok(options)
    }
}

fn gate_policy(
    value: Option<Bound<'_, PyAny>>,
    hooks: &Arc<LoopHooks>,
) -> PyResult<Option<openbyd_client::client::GatePolicy>> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Ok(None);
    };
    if let Ok(name) = value.extract::<String>() {
        return match name.as_str() {
            "allow_missing_data" => Ok(Some(Arc::new(|_vin, verdict: &CommandGateVerdict| {
                verdict.supported || MISSING_DATA_REASONS.contains(&verdict.reason.as_str())
            }))),
            "strict" => Ok(None),
            other => Err(PyValueError::new_err(format!(
                "gate_policy must be 'strict', 'allow_missing_data' or a callable, not {other:?}"
            ))),
        };
    }
    let callback = callable("gate_policy", Some(value))?.expect("not None");
    let hooks = Arc::clone(hooks);
    Ok(Some(Arc::new(move |vin, verdict: &CommandGateVerdict| {
        let answer = hooks.decide("gate_policy", &callback, |py| {
            PyTuple::new(
                py,
                [
                    vin.into_pyobject(py)?.into_any(),
                    dump::to_python(py, verdict)?,
                ],
            )
        });
        answer
            .and_then(|value| Python::try_attach(|py| value.bind(py).is_truthy().ok()).flatten())
            .unwrap_or(verdict.supported)
    })))
}

fn relogin_policy(
    value: Option<Bound<'_, PyAny>>,
    hooks: &Arc<LoopHooks>,
) -> PyResult<ReloginPolicy> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Ok(ReloginPolicy::Password);
    };
    if let Ok(name) = value.extract::<String>() {
        return match name.as_str() {
            "password" => Ok(ReloginPolicy::Password),
            "never" => Ok(ReloginPolicy::Never),
            other => Err(PyValueError::new_err(format!(
                "relogin must be 'password', 'never' or an async callable, not {other:?}"
            ))),
        };
    }
    let callback = callable("relogin", Some(value))?.expect("not None");
    let hooks = Arc::clone(hooks);
    Ok(ReloginPolicy::Handler(Arc::new(move || {
        let hooks = Arc::clone(&hooks);
        let callback = Python::attach(|py| callback.clone_ref(py));
        Box::pin(async move {
            let session = hooks.run_coroutine("relogin", callback).await?;
            let value = Python::try_attach(|py| convert::to_value(session.bind(py), Mode::Strict))
                .ok_or_else(|| Error::new(ErrorKind::BydError, "the interpreter is exiting"))?
                .map_err(|error| {
                    Error::new(
                        ErrorKind::BydError,
                        format!("relogin returned no session: {error}"),
                    )
                })?;
            tokens_from_value(&value).map_err(|message| {
                Error::new(
                    ErrorKind::BydError,
                    format!("relogin returned no session: {message}"),
                )
            })
        })
    })))
}

/// The polling keywords of `get_vehicle_realtime` and `get_gps_info`.
fn poll_settings(
    poll_attempts: i64,
    poll_interval: f64,
    mqtt_timeout: Option<f64>,
    signal_retries: i64,
) -> PollSettings {
    PollSettings {
        poll_attempts,
        poll_interval,
        mqtt_timeout,
        signal_retries,
    }
}

/// The client. See `openbyd.Client`.
#[pyclass(frozen, module = "openbyd._native", name = "Client")]
pub struct PyClient {
    client: BydClient<ProductionEnv>,
    hooks: Arc<LoopHooks>,
}

impl PyClient {
    fn handle(&self) -> BydClient<ProductionEnv> {
        self.client.clone()
    }
}

macro_rules! read {
    ($self:ident, $py:ident, |$client:ident| $call:expr) => {{
        let $client = $self.handle();
        bridge::spawn($py, async move { $call.await.map(Dump::new) })
    }};
}

macro_rules! command {
    ($self:ident, $py:ident, $vin:ident, $pwd:ident, $method:ident) => {{
        let client = $self.handle();
        bridge::spawn($py, async move {
            client.$method(&$vin, $pwd.as_deref()).await.map(Dump::new)
        })
    }};
}

macro_rules! with_params {
    ($self:ident, $py:ident, $vin:ident, $params:ident, $pwd:ident, $model:ty, $method:ident) => {{
        let params = <$model>::from_kwargs(&object("params", &$params)?)
            .map_err(|error| errors::validation_to_py_err($py, &error))?;
        let client = $self.handle();
        bridge::spawn($py, async move {
            client
                .$method(&$vin, &params, $pwd.as_deref())
                .await
                .map(Dump::new)
        })
    }};
}

#[pymethods]
impl PyClient {
    /// A closed client for `config` (`BydConfig`'s fields); `start` opens it.
    #[new]
    #[pyo3(signature = (
        config, *, on_vehicle_info = None, on_mqtt_event = None, on_command_ack = None,
        on_command_lifecycle = None, on_mqtt_connect = None, on_mqtt_disconnect = None,
        on_mqtt_connack_refused = None, command_ack_ttl_seconds = 300.0,
        gate_policy = None, command_retries = 1, pin_resolver = None, relogin = None,
        request_observer = None, commands_enabled = false, test_tls = false,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new<'py>(
        py: Python<'py>,
        config: &Bound<'py, PyAny>,
        on_vehicle_info: Option<Bound<'py, PyAny>>,
        on_mqtt_event: Option<Bound<'py, PyAny>>,
        on_command_ack: Option<Bound<'py, PyAny>>,
        on_command_lifecycle: Option<Bound<'py, PyAny>>,
        on_mqtt_connect: Option<Bound<'py, PyAny>>,
        on_mqtt_disconnect: Option<Bound<'py, PyAny>>,
        on_mqtt_connack_refused: Option<Bound<'py, PyAny>>,
        command_ack_ttl_seconds: f64,
        gate_policy: Option<Bound<'py, PyAny>>,
        command_retries: i64,
        pin_resolver: Option<Bound<'py, PyAny>>,
        relogin: Option<Bound<'py, PyAny>>,
        request_observer: Option<Bound<'py, PyAny>>,
        commands_enabled: bool,
        test_tls: bool,
    ) -> PyResult<Self> {
        let config = BydConfig::from_kwargs(&object("config", config)?)
            .map_err(|error| errors::to_py_err(py, &error))?;
        let hooks = Arc::new(LoopHooks::default());
        let options = Options {
            on_vehicle_info,
            on_mqtt_event,
            on_command_ack,
            on_command_lifecycle,
            on_mqtt_connect,
            on_mqtt_disconnect,
            on_mqtt_connack_refused,
            command_ack_ttl_seconds,
            gate_policy,
            command_retries,
            pin_resolver,
            relogin,
            request_observer,
        }
        .build(&hooks)?;
        let runtime = bridge::handle()?;
        let _entered = runtime.enter();
        let http = HttpEnv::for_config(crate::lowlevel::pool(), &config);
        let env = if test_tls {
            test_environment(http)?
        } else {
            ProductionEnv::new(http)
        };
        let client = BydClient::new(config, env, options);
        if commands_enabled {
            client.enable_commands();
        }
        // pyBYD's client is unusable until `async_start`.
        client.async_close();
        Ok(Self { client, hooks })
    }

    /// pyBYD's `async_start`, with the event loop callbacks run on.
    fn start(&self, event_loop: Py<PyAny>) {
        self.hooks.bind(event_loop);
        self.client.async_start();
    }

    /// pyBYD's `async_close`.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let _entered = bridge::handle()?.enter();
        py.detach(|| self.client.async_close());
        Ok(())
    }

    fn login<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.handle();
        bridge::spawn(py, async move { client.login().await })
    }

    fn ensure_session<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.handle();
        bridge::spawn(py, async move {
            let session = client.ensure_session().await?;
            Ok::<_, Error>(Dump::new(json!({
                "user_id": session.user_id(),
                "sign_token": session.sign_token(),
                "encry_token": session.encry_token(),
                "created_at": session.created_at(),
                "ttl": if session.ttl().is_finite() { json!(session.ttl()) } else { json!("Infinity") },
            })))
        })
    }

    fn invalidate_session(&self) {
        self.client.invalidate_session();
    }

    fn export_session<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        self.client
            .export_session()
            .map(|tokens| tokens_to_dict(py, &tokens))
            .transpose()
    }

    fn import_session<'py>(
        &self,
        py: Python<'py>,
        session: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let tokens = tokens_from_python(session)?;
        let client = self.handle();
        bridge::spawn(py, async move {
            client.import_session(tokens).await;
        })
    }

    #[getter]
    fn commands_enabled(&self) -> bool {
        self.client.commands_enabled()
    }

    fn enable_commands(&self) {
        self.client.enable_commands();
    }

    #[getter]
    fn guest_vins(&self) -> std::collections::HashMap<String, bool> {
        self.client.guest_vins()
    }

    fn stop_mqtt(&self, py: Python<'_>) -> PyResult<()> {
        let _entered = bridge::handle()?.enter();
        py.detach(|| self.client.stop_mqtt());
        Ok(())
    }

    #[getter]
    fn mqtt_is_running(&self) -> bool {
        self.client.mqtt_is_running()
    }

    fn start_mqtt<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.handle();
        bridge::spawn(py, async move { client.start_mqtt().await })
    }

    fn get_command_ack_diagnostics<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        dump::to_python(py, &self.client.get_command_ack_diagnostics())
    }

    #[pyo3(signature = (vin, energy_type, model_name = None))]
    fn seed_vehicle_metadata(
        &self,
        py: Python<'_>,
        vin: &str,
        energy_type: i64,
        model_name: Option<&str>,
    ) -> PyResult<()> {
        let energy_type: EnergyType = int_enum(&json!(energy_type))
            .map_err(|error| errors::validation_to_py_err(py, &error))?;
        self.client
            .seed_vehicle_metadata(vin, energy_type, model_name)
            .map_err(|error| errors::to_py_err(py, &error))
    }

    fn vehicle_metadata<'py>(
        &self,
        py: Python<'py>,
        vin: &str,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        self.client
            .vehicle_metadata(vin)
            .map(|vehicle| dump::to_python(py, &vehicle))
            .transpose()
    }

    fn get_vehicles<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.handle();
        bridge::spawn(
            py,
            async move { client.get_vehicles().await.map(Dump::new) },
        )
    }

    #[pyo3(signature = (vin, *, vehicle = None, on_state_changed = None, projection_ttl = 30.0))]
    fn get_car<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        vehicle: Option<Bound<'py, PyAny>>,
        on_state_changed: Option<Bound<'py, PyAny>>,
        projection_ttl: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let vehicle = match vehicle.filter(|vehicle| !vehicle.is_none()) {
            None => None,
            Some(raw) => Some(
                Vehicle::parse(&Value::Object(object("vehicle", &raw)?), &Value::Null)
                    .map_err(|error| errors::validation_to_py_err(py, &error))?,
            ),
        };
        let on_state_changed: Option<StateChangedCallback> =
            callable("on_state_changed", on_state_changed)?.map(|callback| {
                let hooks = Arc::clone(&self.hooks);
                Box::new(
                    move |vin: &str, snapshot: &openbyd_protocol::state::VehicleSnapshot| {
                        let state = snapshot.dump();
                        hooks.post("on_state_changed", &callback, |py| {
                            PyTuple::new(
                                py,
                                [vin.into_pyobject(py)?.into_any(), to_py(py, &state)?],
                            )
                        });
                    },
                ) as StateChangedCallback
            });
        let client = self.handle();
        bridge::spawn(py, async move {
            let options = GetCarOptions {
                vehicle,
                on_state_changed,
                projection_ttl,
            };
            let car = client.get_car(&vin, options).await?;
            Ok::<_, Error>(bridge::WithPython(move |py: Python<'_>| {
                Ok(Py::new(py, PyCar::new(car))?.into_any())
            }))
        })
    }

    fn get_latest_configs<'py>(
        &self,
        py: Python<'py>,
        vins: Vec<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.handle();
        bridge::spawn(py, async move {
            client
                .get_latest_configs(&vins)
                .await
                .map(|configs| Dump::new(Entries(configs)))
        })
    }

    fn get_latest_config<'py>(&self, py: Python<'py>, vin: String) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client.get_latest_config(&vin))
    }

    #[pyo3(signature = (vin, *, force_refresh = false))]
    fn get_vehicle_capabilities<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        force_refresh: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.handle();
        bridge::spawn(py, async move {
            Dump::new(client.get_vehicle_capabilities(&vin, force_refresh).await)
        })
    }

    #[pyo3(signature = (vin, *, poll_attempts = 10, poll_interval = 1.5, mqtt_timeout = None, signal_retries = 2))]
    fn get_vehicle_realtime<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        poll_attempts: i64,
        poll_interval: f64,
        mqtt_timeout: Option<f64>,
        signal_retries: i64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let settings = poll_settings(poll_attempts, poll_interval, mqtt_timeout, signal_retries);
        read!(self, py, |client| client
            .get_vehicle_realtime(&vin, settings))
    }

    #[pyo3(signature = (vin, *, poll_attempts = 10, poll_interval = 1.5, mqtt_timeout = None, signal_retries = 2))]
    fn get_gps_info<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        poll_attempts: i64,
        poll_interval: f64,
        mqtt_timeout: Option<f64>,
        signal_retries: i64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let settings = poll_settings(poll_attempts, poll_interval, mqtt_timeout, signal_retries);
        read!(self, py, |client| client.get_gps_info(&vin, settings))
    }

    fn get_hvac_status<'py>(&self, py: Python<'py>, vin: String) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client.get_hvac_status(&vin))
    }

    fn get_charging_status<'py>(
        &self,
        py: Python<'py>,
        vin: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client.get_charging_status(&vin))
    }

    fn get_charging_schedule<'py>(
        &self,
        py: Python<'py>,
        vin: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client.get_charging_schedule(&vin))
    }

    fn get_charging_homepage<'py>(
        &self,
        py: Python<'py>,
        vin: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.handle();
        bridge::spawn(py, async move {
            let (status, schedule) = client.get_charging_homepage(&vin).await?;
            Ok::<_, Error>(bridge::WithPython(move |py: Python<'_>| {
                let items = [
                    dump::to_python(py, &status)?,
                    dump::to_python(py, &schedule)?,
                ];
                Ok(PyTuple::new(py, items)?.into_any().unbind())
            }))
        })
    }

    #[pyo3(signature = (vin, *, energy_type = None))]
    fn get_ac_current_limit<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        energy_type: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        use openbyd_client::api::smart_charging::EnergyTypeArg;
        let energy_type = match energy_type.filter(|value| !value.is_none()) {
            None => None,
            Some(value) => Some(match value.extract::<i64>() {
                Ok(number) => EnergyTypeArg::Int(number),
                Err(_) => EnergyTypeArg::Text(value.extract::<String>().map_err(|_| {
                    PyTypeError::new_err("energy_type must be an int, a str or an EnergyType")
                })?),
            }),
        };
        read!(self, py, |client| client
            .get_ac_current_limit(&vin, energy_type))
    }

    fn get_energy_consumption<'py>(
        &self,
        py: Python<'py>,
        vin: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client.get_energy_consumption(&vin))
    }

    fn get_push_state<'py>(&self, py: Python<'py>, vin: String) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client.get_push_state(&vin))
    }

    #[pyo3(signature = (vin, *, enable, push_type = 701))]
    fn set_push_state<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        enable: bool,
        push_type: i64,
    ) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client
            .set_push_state(&vin, enable, push_type))
    }

    #[pyo3(signature = (category = 1))]
    fn get_notifications<'py>(
        &self,
        py: Python<'py>,
        category: i64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let category = NotificationCategory::from_json(&json!(category)).ok_or_else(|| {
            PyValueError::new_err(format!("{category} is not a valid NotificationCategory"))
        })?;
        let client = self.handle();
        bridge::spawn(py, async move {
            client.get_notifications(category).await.map(Dump::new)
        })
    }

    fn get_all_notifications<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.handle();
        bridge::spawn(py, async move {
            let all = client.get_all_notifications().await?;
            Ok::<_, Error>(bridge::WithPython(move |py: Python<'_>| {
                let dict = PyDict::new(py);
                for (category, messages) in &all {
                    dict.set_item(*category as i64, dump::to_python(py, messages)?)?;
                }
                Ok(dict.into_any().unbind())
            }))
        })
    }

    #[pyo3(signature = (vin, *, name))]
    fn rename_vehicle<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        name: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client.rename_vehicle(&vin, &name))
    }

    fn get_booking_list<'py>(&self, py: Python<'py>, vin: String) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client.get_booking_list(&vin))
    }

    fn get_ota_version<'py>(&self, py: Python<'py>, vin: String) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client.get_ota_version(&vin))
    }

    #[pyo3(signature = (vin, *, enabled, auto_upgrade_time = "2:00".to_owned(), command_pwd = None))]
    fn set_ota_auto_upgrade<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        enabled: bool,
        auto_upgrade_time: String,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client.set_ota_auto_upgrade(
            &vin,
            enabled,
            &auto_upgrade_time,
            command_pwd.as_deref()
        ))
    }

    #[pyo3(signature = (vin, *, empower_id = None))]
    fn get_empower_detail<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        empower_id: Option<i64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client
            .get_empower_detail(&vin, empower_id))
    }

    fn get_standby_plans<'py>(&self, py: Python<'py>, vin: String) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client.get_standby_plans(&vin))
    }

    #[pyo3(signature = (vin, plan, *, schedule = None))]
    fn save_standby_plan<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        plan: Bound<'py, PyAny>,
        schedule: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let plan: StandbyPlan = model(py, "plan", &plan)?;
        let schedule = schedule_argument(py, schedule)?;
        read!(self, py, |client| client.save_standby_plan(
            &vin,
            &plan,
            schedule.as_ref()
        ))
    }

    #[pyo3(signature = (vin, name, commands, *, schedule = None))]
    fn create_standby_plan<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        name: String,
        commands: Vec<Bound<'py, PyAny>>,
        schedule: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let commands = commands
            .iter()
            .map(|command| model::<StandbyPlanCommand>(py, "a command", command))
            .collect::<PyResult<Vec<_>>>()?;
        let schedule = schedule_argument(py, schedule)?;
        read!(self, py, |client| client.create_standby_plan(
            &vin,
            &name,
            &commands,
            schedule.as_ref()
        ))
    }

    fn set_default_standby_plan<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        plan_id: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client
            .set_default_standby_plan(&vin, &plan_id))
    }

    fn delete_standby_plan<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        plan_id: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client
            .delete_standby_plan(&vin, &plan_id))
    }

    #[pyo3(signature = (vin, plan_id, *, poll_attempts = 10, poll_interval = 1.5))]
    fn run_standby_plan<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        plan_id: String,
        poll_attempts: i64,
        poll_interval: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let settings = RunPollSettings {
            poll_attempts,
            poll_interval,
        };
        read!(self, py, |client| client
            .run_standby_plan(&vin, &plan_id, settings))
    }

    #[pyo3(signature = (vin, *, command_pwd = None))]
    fn verify_command_access<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        command!(self, py, vin, command_pwd, verify_command_access)
    }

    #[pyo3(signature = (vin, *, command_pwd = None))]
    fn lock<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        command!(self, py, vin, command_pwd, lock)
    }

    #[pyo3(signature = (vin, *, command_pwd = None))]
    fn unlock<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        command!(self, py, vin, command_pwd, unlock)
    }

    #[pyo3(signature = (vin, *, command_pwd = None))]
    fn stop_climate<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        command!(self, py, vin, command_pwd, stop_climate)
    }

    #[pyo3(signature = (vin, *, command_pwd = None))]
    fn flash_lights<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        command!(self, py, vin, command_pwd, flash_lights)
    }

    #[pyo3(signature = (vin, *, command_pwd = None))]
    fn close_windows<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        command!(self, py, vin, command_pwd, close_windows)
    }

    #[pyo3(signature = (vin, *, command_pwd = None))]
    fn open_windows<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        command!(self, py, vin, command_pwd, open_windows)
    }

    #[pyo3(signature = (vin, *, command_pwd = None))]
    fn find_car<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        command!(self, py, vin, command_pwd, find_car)
    }

    #[pyo3(signature = (vin, *, command_pwd = None))]
    fn open_trunk<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        command!(self, py, vin, command_pwd, open_trunk)
    }

    #[pyo3(signature = (vin, *, command_pwd = None))]
    fn close_trunk<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        command!(self, py, vin, command_pwd, close_trunk)
    }

    #[pyo3(signature = (vin, *, params, command_pwd = None))]
    fn start_climate<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        params: Bound<'py, PyAny>,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        with_params!(
            self,
            py,
            vin,
            params,
            command_pwd,
            ClimateStartParams,
            start_climate
        )
    }

    #[pyo3(signature = (vin, *, params, command_pwd = None))]
    fn schedule_climate<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        params: Bound<'py, PyAny>,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        with_params!(
            self,
            py,
            vin,
            params,
            command_pwd,
            ClimateScheduleParams,
            schedule_climate
        )
    }

    #[pyo3(signature = (vin, *, params, command_pwd = None))]
    fn set_seat_climate<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        params: Bound<'py, PyAny>,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        with_params!(
            self,
            py,
            vin,
            params,
            command_pwd,
            SeatClimateParams,
            set_seat_climate
        )
    }

    #[pyo3(signature = (vin, *, params, command_pwd = None))]
    fn set_battery_heat<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        params: Bound<'py, PyAny>,
        command_pwd: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        with_params!(
            self,
            py,
            vin,
            params,
            command_pwd,
            BatteryHeatParams,
            set_battery_heat
        )
    }

    #[pyo3(signature = (vin, *, start_charge_time, end_charge_time, charge_way, enabled = true, mqtt_timeout = None, poll_attempts = 10, poll_interval = None))]
    #[allow(clippy::too_many_arguments)]
    fn save_charging_schedule<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        start_charge_time: String,
        end_charge_time: String,
        charge_way: String,
        enabled: bool,
        mqtt_timeout: Option<f64>,
        poll_attempts: i64,
        poll_interval: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let settings = ChargePollSettings {
            mqtt_timeout,
            poll_attempts,
            poll_interval,
        };
        read!(self, py, |client| client.save_charging_schedule(
            &vin,
            &start_charge_time,
            &end_charge_time,
            &charge_way,
            enabled,
            settings
        ))
    }

    #[pyo3(signature = (vin, *, enable))]
    fn toggle_smart_charging<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        enable: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        read!(self, py, |client| client
            .toggle_smart_charging(&vin, enable))
    }

    #[pyo3(signature = (vin, *, mqtt_timeout = None, poll_attempts = 10, poll_interval = None))]
    fn start_charging<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        mqtt_timeout: Option<f64>,
        poll_attempts: i64,
        poll_interval: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let settings = ChargePollSettings {
            mqtt_timeout,
            poll_attempts,
            poll_interval,
        };
        read!(self, py, |client| client.start_charging(&vin, settings))
    }

    #[pyo3(signature = (vin, *, mqtt_timeout = None, poll_attempts = 10, poll_interval = None))]
    fn stop_charging<'py>(
        &self,
        py: Python<'py>,
        vin: String,
        mqtt_timeout: Option<f64>,
        poll_attempts: i64,
        poll_interval: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let settings = ChargePollSettings {
            mqtt_timeout,
            poll_attempts,
            poll_interval,
        };
        read!(self, py, |client| client.stop_charging(&vin, settings))
    }
}

#[cfg(feature = "testing")]
fn test_environment(http: HttpEnv) -> PyResult<ProductionEnv> {
    Ok(ProductionEnv::with_mqtt_options(
        http,
        openbyd_client::testing::broker::runtime_options(),
    ))
}

#[cfg(not(feature = "testing"))]
fn test_environment(_http: HttpEnv) -> PyResult<ProductionEnv> {
    Err(PyValueError::new_err(
        "test_tls needs an extension built with the cargo feature 'testing'",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_dicts_are_checked() {
        let good = json!({"user_id": "1", "sign_token": "s", "encry_token": "e",
            "ttl_remaining": 5.0, "guest_vins": {"V": true}});
        let tokens = tokens_from_value(&good).unwrap();
        assert_eq!(tokens.ttl_remaining, Some(5.0));
        assert!(tokens.guest_vins["V"]);
        for bad in [
            json!([]),
            json!({"user_id": "", "sign_token": "s", "encry_token": "e"}),
            json!({"user_id": "1", "sign_token": "s", "encry_token": "e", "ttl_remaining": "x"}),
            json!({"user_id": "1", "sign_token": "s", "encry_token": "e", "guest_vins": []}),
            json!({"user_id": "1", "sign_token": "s", "encry_token": "e", "guest_vins": {"V": 1}}),
        ] {
            assert!(tokens_from_value(&bad).is_err(), "{bad}");
        }
    }
}
