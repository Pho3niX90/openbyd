//! Python callbacks of a client, run on the client's event loop.
//!
//! The Rust client calls its hooks on tokio worker threads. None of them
//! calls Python there: each hands the call to the event loop the client was
//! started on (`loop.call_soon_threadsafe`), where `openbyd._callbacks` runs
//! it and logs (on the `openbyd` logger) any exception it raises, so a
//! failing callback never reaches the client.
//!
//! - Notifications (`on_vehicle_info`, the request observer, ...) are posted
//!   and not waited for. They arrive in the order they were posted.
//! - Decisions the client needs at once (the gate policy, the PIN resolver)
//!   are posted too; the worker waits for the answer with the GIL released,
//!   at most [`DECISION_TIMEOUT`], and takes the default on a failure or
//!   timeout.
//! - The re-login handler is a coroutine function: it runs as a task on the
//!   loop (`asyncio.run_coroutine_threadsafe`) and its result is awaited
//!   without blocking a worker.
//!
//! Before the client is started (no loop yet) and after its loop has closed,
//! notifications are dropped and decisions take their default.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use openbyd_protocol::error::{Error, ErrorKind};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyAny, PyTuple};

/// How long a worker waits for a decision callback before taking the default.
pub const DECISION_TIMEOUT: Duration = Duration::from_secs(30);

fn callbacks(py: Python<'_>) -> PyResult<&Bound<'_, PyModule>> {
    static MODULE: PyOnceLock<Py<PyModule>> = PyOnceLock::new();
    MODULE
        .get_or_try_init(py, || py.import("openbyd._callbacks").map(Bound::unbind))
        .map(|module| module.bind(py))
}

/// Logs `message` on Python's `openbyd` logger at `level` (`logging.DEBUG` is 10,
/// `logging.WARNING` 30).
pub fn log(py: Python<'_>, level: i32, message: &str) {
    let logged = py
        .import("logging")
        .and_then(|logging| logging.call_method1("getLogger", ("openbyd",)))
        .and_then(|logger| logger.call_method1("log", (level, message)));
    if let Err(error) = logged {
        error.write_unraisable(py, None);
    }
}

/// `logging.DEBUG`.
pub const DEBUG: i32 = 10;
/// `logging.WARNING`.
pub const WARNING: i32 = 30;

/// The event loop a client's callbacks run on.
#[derive(Default)]
pub struct LoopHooks {
    event_loop: Mutex<Option<Py<PyAny>>>,
}

impl LoopHooks {
    fn slot(&self) -> MutexGuard<'_, Option<Py<PyAny>>> {
        self.event_loop
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs the callbacks on `event_loop` from now on.
    pub fn bind(&self, event_loop: Py<PyAny>) {
        *self.slot() = Some(event_loop);
    }

    fn event_loop(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.slot()
            .as_ref()
            .map(|event_loop| event_loop.clone_ref(py))
    }

    /// Posts `callback(*args)` to the loop, with `args` built under the GIL.
    /// Never waits for it; a failure to post is logged and dropped.
    pub fn post(
        &self,
        name: &str,
        callback: &Py<PyAny>,
        args: impl FnOnce(Python<'_>) -> PyResult<Bound<'_, PyTuple>>,
    ) {
        let _ = Python::try_attach(|py| {
            let Some(event_loop) = self.event_loop(py) else {
                log(
                    py,
                    DEBUG,
                    &format!("{name} dropped: the client has no event loop yet"),
                );
                return;
            };
            let posted = args(py).and_then(|args| {
                let invoke = callbacks(py)?.getattr("invoke")?;
                event_loop
                    .bind(py)
                    .call_method1("call_soon_threadsafe", (invoke, name, callback, args))
            });
            if let Err(error) = posted {
                log(py, DEBUG, &format!("{name} not delivered: {error}"));
            }
        });
    }

    /// Runs `callback(*args)` on the loop and waits for its result, with
    /// the GIL released. `None` when it raised (logged), timed out, or could
    /// not be posted. Call it on a worker of a multi-thread runtime: the wait
    /// moves the worker's other tasks elsewhere.
    pub fn decide(
        &self,
        name: &str,
        callback: &Py<PyAny>,
        args: impl FnOnce(Python<'_>) -> PyResult<Bound<'_, PyTuple>>,
    ) -> Option<Py<PyAny>> {
        let wait = || {
            Python::try_attach(|py| -> PyResult<Option<Py<PyAny>>> {
                let Some(event_loop) = self.event_loop(py) else {
                    return Ok(None);
                };
                let module = callbacks(py)?;
                let future = py
                    .import("concurrent.futures")?
                    .getattr("Future")?
                    .call0()?;
                event_loop.bind(py).call_method1(
                    "call_soon_threadsafe",
                    (
                        module.getattr("decide")?,
                        future.clone(),
                        name,
                        callback,
                        args(py)?,
                    ),
                )?;
                // `result` waits with the GIL released.
                let (ok, value): (bool, Py<PyAny>) = future
                    .call_method1("result", (DECISION_TIMEOUT.as_secs_f64(),))?
                    .extract()?;
                Ok(ok.then_some(value))
            })
            .and_then(|outcome| match outcome {
                Ok(value) => value,
                Err(error) => {
                    let _ = Python::try_attach(|py| {
                        log(
                            py,
                            WARNING,
                            &format!("{name} gave no answer, using the default: {error}"),
                        );
                    });
                    None
                }
            })
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(wait)
            }
            _ => wait(),
        }
    }

    /// Runs the coroutine function `callback()` as a task on the loop and
    /// awaits its result without blocking a worker.
    pub async fn run_coroutine(&self, name: &str, callback: Py<PyAny>) -> Result<Py<PyAny>, Error> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let scheduled = Python::try_attach(|py| -> PyResult<()> {
            let event_loop = self.event_loop(py).ok_or_else(|| {
                pyo3::exceptions::PyRuntimeError::new_err("the client has no event loop")
            })?;
            let module = callbacks(py)?;
            let coroutine = module.getattr("run")?.call1((name, callback))?;
            let future = py
                .import("asyncio")?
                .getattr("run_coroutine_threadsafe")?
                .call1((coroutine, event_loop))?;
            future.call_method1("add_done_callback", (Delivery(Mutex::new(Some(sender))),))?;
            Ok(())
        });
        match scheduled {
            Some(Ok(())) => {}
            Some(Err(error)) => return Err(Error::new(ErrorKind::BydError, error.to_string())),
            None => {
                return Err(Error::new(
                    ErrorKind::BydError,
                    "the interpreter is exiting",
                ));
            }
        }
        receiver
            .await
            .unwrap_or_else(|_| Err(Error::new(ErrorKind::BydError, "the event loop went away")))
    }
}

type Slot = tokio::sync::oneshot::Sender<Result<Py<PyAny>, Error>>;

/// Hands a `concurrent.futures.Future`'s outcome to a waiting Rust future.
#[pyclass(frozen, module = "openbyd._native")]
struct Delivery(Mutex<Option<Slot>>);

#[pymethods]
impl Delivery {
    fn __call__(&self, future: &Bound<'_, PyAny>) {
        let outcome = match future.call_method0("result") {
            Ok(value) => Ok(value.unbind()),
            Err(error) => Err(error_from_python(future.py(), &error)),
        };
        if let Some(sender) = self.0.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = sender.send(outcome);
        }
    }
}

/// An openbyd [`Error`] for a Python exception: an `openbyd.exceptions`
/// class keeps its class, `code`, `endpoint` and `status_code`; anything
/// else becomes a `BydError` with the exception's text.
pub fn error_from_python(py: Python<'_>, error: &PyErr) -> Error {
    let value = error.value(py);
    let class = value
        .get_type()
        .name()
        .map(|name| name.to_string())
        .unwrap_or_default();
    let message = value.str().map(|text| text.to_string()).unwrap_or_default();
    let text = |name: &str| -> String {
        value
            .getattr(name)
            .ok()
            .filter(|attribute| !attribute.is_none())
            .and_then(|attribute| attribute.extract::<String>().ok())
            .unwrap_or_default()
    };
    let from_openbyd = value
        .get_type()
        .getattr("__module__")
        .and_then(|module| module.extract::<String>())
        .is_ok_and(|module| module == "openbyd.exceptions");
    let kind = ErrorKind::from_class_name(&class).filter(|_| from_openbyd);
    match kind {
        Some(kind) if kind.is_a(ErrorKind::BydTransportError) => Error::transport(
            message,
            value
                .getattr("status_code")
                .ok()
                .and_then(|status| status.extract::<u16>().ok()),
            text("endpoint"),
        ),
        Some(kind) if kind.is_a(ErrorKind::BydApiError) => {
            Error::api(kind, message, text("code"), text("endpoint"))
        }
        Some(kind) => Error::new(kind, message),
        None => Error::new(ErrorKind::BydError, format!("{class}: {message}")),
    }
}
