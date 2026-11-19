//! Single requests over HTTP, through the asyncio bridge (`openbyd.lowlevel`).
//!
//! These prove the stack end to end (Python, the bridge, `HttpEnv`, the
//! ported flows) and are the pattern the full client follows: convert the
//! arguments while holding the GIL, move owned values into an `async move`
//! block, and hand it to [`bridge::spawn`].

use std::sync::OnceLock;

use openbyd_client::Env;
use openbyd_client::api::Context;
use openbyd_client::api::common::{self, ErrorCodes};
use openbyd_client::http::{HttpEnv, HttpPool};
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::envelope::{LOGIN_ENDPOINT, build_login_request, parse_login_response};
use openbyd_protocol::error::Error;
use openbyd_protocol::session::Session;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyDict;
use serde_json::{Map, Value};

use crate::bridge::{self, WithPython};
use crate::convert::{self, Mode};
use crate::errors;

/// The connection pool every request and every client in the process
/// shares. Made inside the runtime.
pub(crate) fn pool() -> &'static HttpPool {
    static POOL: OnceLock<HttpPool> = OnceLock::new();
    POOL.get_or_init(HttpPool::new)
}

fn monotonic(py: Python<'_>) -> PyResult<f64> {
    static MONOTONIC: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    MONOTONIC
        .import(py, "time", "monotonic")?
        .call0()?
        .extract()
}

/// A dict argument as a JSON object.
fn object(what: &str, value: &Bound<'_, PyAny>) -> PyResult<Map<String, Value>> {
    match convert::to_value(value, Mode::Strict)? {
        Value::Object(map) => Ok(map),
        _ => Err(PyTypeError::new_err(format!("{what} must be a dict"))),
    }
}

fn config(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<BydConfig> {
    BydConfig::from_kwargs(&object("config", value)?).map_err(|error| errors::to_py_err(py, &error))
}

/// Logs in with `config` (`BydConfig`'s fields as a dict) and returns an
/// awaitable for the session's fields: `user_id`, `sign_token`,
/// `encry_token`, `created_at` (`time.monotonic()`) and `ttl`, as pyBYD's
/// `BydClient.login` makes its `Session`.
#[pyfunction]
#[pyo3(signature = (config, base_url = None))]
pub fn login<'py>(
    py: Python<'py>,
    config: &Bound<'py, PyAny>,
    base_url: Option<String>,
) -> PyResult<Bound<'py, PyAny>> {
    let config = self::config(py, config)?;
    let base_url = base_url.unwrap_or_else(|| config.base_url.clone());
    bridge::spawn(py, async move {
        let env = HttpEnv::new(pool(), base_url);
        let outer = build_login_request(&config, env.now_ms(), &env.random_bytes(), env.now_ms());
        let reply = env.post_secure(LOGIN_ENDPOINT, &outer).await?;
        let token = parse_login_response(&reply, &config.password)?;
        let ttl = if config.session_ttl > 0.0 {
            config.session_ttl
        } else {
            f64::INFINITY
        };
        Ok::<_, Error>(WithPython(move |py: Python<'_>| {
            let session = PyDict::new(py);
            session.set_item("user_id", token.user_id.as_str())?;
            session.set_item("sign_token", token.sign_token.as_str())?;
            session.set_item("encry_token", token.encry_token.as_str())?;
            session.set_item("created_at", monotonic(py)?)?;
            session.set_item("ttl", ttl)?;
            Ok(session.into_any().unbind())
        }))
    })
}

/// Posts `inner` to `endpoint` in a token envelope and returns an awaitable
/// for the decrypted `respondData`, as pyBYD's `post_token_json`.
#[pyfunction]
#[pyo3(signature = (config, session, endpoint, inner, *, vin = None, user_type = None))]
pub fn post_token_json<'py>(
    py: Python<'py>,
    config: &Bound<'py, PyAny>,
    session: &Bound<'py, PyAny>,
    endpoint: String,
    inner: &Bound<'py, PyAny>,
    vin: Option<String>,
    user_type: Option<String>,
) -> PyResult<Bound<'py, PyAny>> {
    let config = self::config(py, config)?;
    let session = Session::from_kwargs(&object("session", session)?, monotonic(py)?)
        .map_err(|error| errors::to_py_err(py, &error))?;
    let inner = object("inner", inner)?;
    bridge::spawn(py, async move {
        let env = HttpEnv::for_config(pool(), &config);
        let cx = Context::new(&env, &config, &session);
        common::post_token_json(
            cx,
            &endpoint,
            &inner,
            None,
            vin.as_deref(),
            ErrorCodes::default(),
            user_type.as_deref(),
        )
        .await
    })
}
