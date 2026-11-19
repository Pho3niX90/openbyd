//! The fake BYD server for Python tests and benchmarks (`openbyd.testing`, cargo feature `testing`).

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use openbyd_client::testing::{Account, FakeBydServer, Reply, Request};
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};
use serde_json::Value;

use crate::convert::{self, Mode};

/// What the fake server answers to one request. Build one with the static
/// methods; `delayed` and `with_cookie` wrap it.
#[pyclass(frozen, module = "openbyd._native", name = "Reply")]
#[derive(Clone)]
pub struct PyReply(Reply);

#[pymethods]
impl PyReply {
    /// Success: code `"0"` with `data` as the encrypted `respondData`.
    #[staticmethod]
    fn data(data: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(Self(Reply::Data(convert::to_value(data, Mode::Strict)?)))
    }

    /// An API error with this `code` and `message`.
    #[staticmethod]
    #[pyo3(signature = (code, message = String::new()))]
    fn code(code: String, message: String) -> Self {
        Self(Reply::Code { code, message })
    }

    /// This outer reply as it is, inside the envelope only.
    #[staticmethod]
    fn outer(outer: &Bound<'_, PyAny>) -> PyResult<Self> {
        Ok(Self(Reply::Outer(convert::to_value(outer, Mode::Strict)?)))
    }

    /// An HTTP status other than 200, with this body.
    #[staticmethod]
    #[pyo3(signature = (status, body = String::new()))]
    fn status(status: u16, body: String) -> Self {
        Self(Reply::Status(status, body))
    }

    /// Status 200 with these bytes as the body, not enveloped.
    #[staticmethod]
    fn raw(body: Vec<u8>) -> Self {
        Self(Reply::Raw(body))
    }

    /// Closes the connection without answering.
    #[staticmethod]
    #[pyo3(name = "drop")]
    fn close_connection() -> Self {
        Self(Reply::Drop)
    }

    /// What the server answers without a handler: a login for a login
    /// request, code 9004 otherwise.
    #[staticmethod]
    fn default() -> Self {
        Self(Reply::Default)
    }

    /// This reply after `seconds`.
    fn delayed(&self, seconds: f64) -> PyResult<Self> {
        let delay = Duration::try_from_secs_f64(seconds)
            .map_err(|_| PyValueError::new_err("seconds must be a finite, non-negative number"))?;
        Ok(Self(self.0.clone().delayed(delay)))
    }

    /// This reply with a `Set-Cookie` header.
    fn with_cookie(&self, header: &str) -> Self {
        Self(self.0.clone().with_cookie(header))
    }

    fn __repr__(&self) -> String {
        format!("Reply({:?})", self.0)
    }
}

/// An account given as `(username, password)` or as a dict with
/// `username`, `password` and optionally `user_id`, `guest_vins` and
/// `user_type_rule`.
fn account(value: &Bound<'_, PyAny>) -> PyResult<Account> {
    if let Ok((username, password)) = value.extract::<(String, String)>() {
        return Ok(Account::new(&username, &password));
    }
    let Ok(fields) = value.cast::<PyDict>() else {
        return Err(PyTypeError::new_err(
            "an account is (username, password) or a dict with username and password",
        ));
    };
    let get = |name: &str| fields.get_item(name);
    let required = |name: &str| -> PyResult<String> {
        get(name)?
            .ok_or_else(|| PyValueError::new_err(format!("account needs {name}")))?
            .extract()
    };
    let mut account = Account::new(&required("username")?, &required("password")?);
    if let Some(user_id) = get("user_id")?.filter(|value| !value.is_none()) {
        account = account.with_user_id(&user_id.extract::<String>()?);
    }
    if let Some(vins) = get("guest_vins")? {
        account.guest_vins = vins.extract::<Vec<String>>()?.into_iter().collect();
    }
    if let Some(rule) = get("user_type_rule")? {
        account.user_type_rule = rule.extract()?;
    }
    Ok(account)
}

fn request_to_python<'py>(py: Python<'py>, request: &Request) -> PyResult<Bound<'py, PyDict>> {
    let pairs = |pairs: &[(String, String)]| -> PyResult<Bound<'py, PyList>> {
        PyList::new(
            py,
            pairs
                .iter()
                .map(|(name, value)| PyTuple::new(py, [name, value]))
                .collect::<PyResult<Vec<_>>>()?,
        )
    };
    let optional = |value: Option<Value>| -> PyResult<Bound<'py, PyAny>> {
        match value {
            Some(value) => convert::to_python(py, &value),
            None => Ok(py.None().into_bound(py)),
        }
    };
    let dict = PyDict::new(py);
    dict.set_item("endpoint", &request.endpoint)?;
    dict.set_item("headers", pairs(&request.headers)?)?;
    dict.set_item("cookies", pairs(&request.cookies)?)?;
    dict.set_item("body", &request.body)?;
    dict.set_item("outer", optional(request.outer.clone().map(Value::Object))?)?;
    dict.set_item("inner", optional(request.inner.clone())?)?;
    dict.set_item("username", &request.username)?;
    dict.set_item("code", &request.code)?;
    dict.set_item("status", request.status)?;
    Ok(dict)
}

/// A fake BYD server on `127.0.0.1`, on a thread of its own.
#[pyclass(frozen, module = "openbyd._native", name = "FakeServer")]
pub struct PyFakeServer {
    server: Mutex<Option<FakeBydServer>>,
    url: String,
}

impl PyFakeServer {
    fn server(&self) -> PyResult<MutexGuard<'_, Option<FakeBydServer>>> {
        let guard = self.server.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            return Err(PyRuntimeError::new_err("the fake server has been stopped"));
        }
        Ok(guard)
    }

    fn with<T>(&self, action: impl FnOnce(&FakeBydServer) -> T) -> PyResult<T> {
        let guard = self.server()?;
        Ok(action(guard.as_ref().expect("checked by server()")))
    }
}

#[pymethods]
impl PyFakeServer {
    /// Starts a server that accepts logins for `accounts`.
    #[new]
    #[pyo3(signature = (accounts = None))]
    fn new(py: Python<'_>, accounts: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let mut parsed = Vec::new();
        if let Some(accounts) = accounts {
            for item in accounts.try_iter()? {
                parsed.push(account(&item?)?);
            }
        }
        let server = py
            .detach(|| FakeBydServer::start_standalone(parsed))
            .map_err(|error| {
                PyRuntimeError::new_err(format!("cannot start the fake server: {error}"))
            })?;
        Ok(Self {
            url: server.url(),
            server: Mutex::new(Some(server)),
        })
    }

    /// The base URL to give a client: `http://127.0.0.1:<port>`.
    #[getter]
    fn url(&self) -> &str {
        &self.url
    }

    /// Whether the server has not been stopped.
    #[getter]
    fn running(&self) -> bool {
        self.server().is_ok()
    }

    /// How many TCP connections the server has accepted.
    #[getter]
    fn connections(&self) -> PyResult<usize> {
        self.with(FakeBydServer::connections)
    }

    /// Accepts logins for one more account.
    #[pyo3(signature = (username, password, *, user_id = None, guest_vins = None, user_type_rule = true))]
    fn add_account(
        &self,
        username: &str,
        password: &str,
        user_id: Option<&str>,
        guest_vins: Option<Vec<String>>,
        user_type_rule: bool,
    ) -> PyResult<()> {
        let mut account = Account::new(username, password);
        if let Some(user_id) = user_id {
            account = account.with_user_id(user_id);
        }
        account.guest_vins = guest_vins.unwrap_or_default().into_iter().collect();
        account.user_type_rule = user_type_rule;
        self.with(|server| server.add_account(account))
    }

    /// The user id logins for `username` receive, or `None`.
    fn user_id(&self, username: &str) -> PyResult<Option<String>> {
        self.with(|server| server.user_id(username))
    }

    /// Answers every request to `endpoint` with `reply` (after any scripted replies).
    fn reply(&self, endpoint: &str, reply: PyRef<'_, PyReply>) -> PyResult<()> {
        let reply = reply.0.clone();
        self.with(|server| server.reply(endpoint, reply))
    }

    /// Queues replies for `endpoint`, one per request, ahead of its `reply`.
    fn script(&self, endpoint: &str, replies: Vec<PyRef<'_, PyReply>>) -> PyResult<()> {
        let replies: Vec<Reply> = replies.iter().map(|reply| reply.0.clone()).collect();
        self.with(|server| server.script(endpoint, replies))
    }

    /// Sends this `Set-Cookie` header with every reply.
    fn set_cookie(&self, header: &str) -> PyResult<()> {
        self.with(|server| server.set_cookie(header))
    }

    /// Expires every session of `username`: its requests get code 1005 until it logs in again.
    fn expire_sessions(&self, username: &str) -> PyResult<()> {
        self.with(|server| server.expire_sessions(username))
    }

    /// The requests received so far (for `endpoint` only, if given), decoded, as dicts with
    /// `endpoint`, `headers`, `cookies`, `body`, `outer`, `inner`, `username`, `code` and `status`.
    #[pyo3(signature = (endpoint = None))]
    fn requests<'py>(
        &self,
        py: Python<'py>,
        endpoint: Option<&str>,
    ) -> PyResult<Bound<'py, PyList>> {
        let requests = self.with(|server| match endpoint {
            Some(endpoint) => server.requests_to(endpoint),
            None => server.requests(),
        })?;
        PyList::new(
            py,
            requests
                .iter()
                .map(|request| request_to_python(py, request))
                .collect::<PyResult<Vec<_>>>()?,
        )
    }

    /// Forgets the recorded requests.
    fn clear_requests(&self) -> PyResult<()> {
        self.with(FakeBydServer::clear_requests)
    }

    /// Stops the server and waits until its socket is closed. Calling it again does nothing.
    fn stop(&self, py: Python<'_>) {
        let server = self
            .server
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(server) = server {
            py.detach(|| server.shutdown_blocking());
        }
    }

    /// Returns the server, for `with`.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Stops the server.
    fn __exit__(
        &self,
        py: Python<'_>,
        _kind: &Bound<'_, PyAny>,
        _value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> bool {
        self.stop(py);
        false
    }
}
