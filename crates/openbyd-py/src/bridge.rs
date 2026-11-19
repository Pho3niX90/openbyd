//! The asyncio bridge: Rust futures as Python awaitables.
//!
//! [`spawn`] runs a Rust future on the extension's shared tokio runtime and
//! returns an `asyncio.Future` of the event loop that called it. Exposing an
//! `async fn` takes a few lines:
//!
//! ```ignore
//! /// Fetches something. Returns an awaitable for the decoded reply.
//! #[pyfunction]
//! fn fetch<'py>(py: Python<'py>, client: PyRef<'py, Client>, vin: String) -> PyResult<Bound<'py, PyAny>> {
//!     let client = client.inner.clone(); // an Arc: the future must own what it uses
//!     bridge::spawn(py, async move { client.fetch(&vin).await })
//! }
//! ```
//!
//! The future's output is anything [`IntoPython`]: a `serde_json::Value`,
//! `()`, a plain number or string, `Result<T, openbyd_protocol::error::Error>`
//! (the error becomes the matching `openbyd.exceptions` class),
//! `PyResult<T>`, or [`WithPython`] for a conversion of your own. The
//! conversion runs on the event loop's thread, so it may build any Python
//! object.
//!
//! How it works:
//!
//! - The runtime starts on first use with [`configure`]'s number of worker
//!   threads (default [`DEFAULT_WORKER_THREADS`]). The future runs there
//!   without the GIL; nothing here starts a thread per call.
//! - When the future finishes, its output is queued on the event loop's
//!   [`Dispatcher`], and the dispatcher is woken with
//!   `loop.call_soon_threadsafe` unless a wake-up is already on its way. On
//!   the loop's thread the dispatcher converts every queued output and sets
//!   the futures' results. So a burst of completions costs one GIL
//!   acquisition and one loop wake-up, not one each, and results always
//!   arrive on the loop that awaited them, whichever thread it runs in.
//! - Cancelling the asyncio future aborts the tokio task, which drops the
//!   Rust future at its next scheduling point (at once if it is waiting).
//! - A panic in the future becomes a `RuntimeError` in Python.
//! - At interpreter exit (`atexit`) the runtime is shut down: pending
//!   futures are dropped and the worker threads stop, so none of them
//!   touches Python while it is being finalised.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use openbyd_protocol::error::Error;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyDict;
use serde_json::Value;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::AbortHandle;

use crate::{convert, errors};

/// Worker threads of the shared runtime unless [`configure`] says otherwise.
pub const DEFAULT_WORKER_THREADS: usize = 2;

/// The most worker threads [`configure`] accepts.
const MAX_WORKER_THREADS: usize = 256;

/// How long the exit hook waits for pending futures to be dropped.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// The runtime
// ---------------------------------------------------------------------------

struct RuntimeState {
    worker_threads: usize,
    runtime: Option<Runtime>,
}

static STATE: Mutex<RuntimeState> = Mutex::new(RuntimeState {
    worker_threads: DEFAULT_WORKER_THREADS,
    runtime: None,
});
static HANDLE: OnceLock<Handle> = OnceLock::new();
static SHUT_DOWN: AtomicBool = AtomicBool::new(false);

/// The shared runtime's handle, starting the runtime on first use.
pub fn handle() -> PyResult<Handle> {
    if SHUT_DOWN.load(Ordering::SeqCst) {
        return Err(PyRuntimeError::new_err(
            "openbyd's runtime has shut down (the interpreter is exiting)",
        ));
    }
    if let Some(handle) = HANDLE.get() {
        return Ok(handle.clone());
    }
    let mut state = lock(&STATE);
    if let Some(handle) = HANDLE.get() {
        return Ok(handle.clone());
    }
    let runtime = Builder::new_multi_thread()
        .worker_threads(state.worker_threads)
        // DNS lookups run on the blocking pool; one thread per lookup at most.
        .max_blocking_threads(64)
        .thread_name("openbyd-worker")
        .enable_all()
        .build()
        .map_err(|error| PyRuntimeError::new_err(format!("cannot start the runtime: {error}")))?;
    let handle = runtime.handle().clone();
    state.runtime = Some(runtime);
    let _ = HANDLE.set(handle.clone());
    Ok(handle)
}

/// Sets the number of worker threads of the shared runtime. Must be called
/// before the runtime starts (before the first awaitable is created).
#[pyfunction]
#[pyo3(signature = (*, worker_threads = None))]
pub fn configure(worker_threads: Option<usize>) -> PyResult<()> {
    let mut state = lock(&STATE);
    if state.runtime.is_some() || SHUT_DOWN.load(Ordering::SeqCst) {
        return Err(PyRuntimeError::new_err(
            "the runtime has already started; call configure() before the first awaitable",
        ));
    }
    if let Some(count) = worker_threads {
        if !(1..=MAX_WORKER_THREADS).contains(&count) {
            return Err(PyValueError::new_err(format!(
                "worker_threads must be from 1 to {MAX_WORKER_THREADS}"
            )));
        }
        state.worker_threads = count;
    }
    Ok(())
}

/// Shuts the runtime down: pending futures are dropped and the worker
/// threads stop. Registered with `atexit`; later calls raise `RuntimeError`.
#[pyfunction]
pub fn shutdown(py: Python<'_>) {
    SHUT_DOWN.store(true, Ordering::SeqCst);
    let runtime = lock(&STATE).runtime.take();
    if let Some(runtime) = runtime {
        // Without the GIL, so a worker waiting for it to deliver a result can finish.
        py.detach(|| runtime.shutdown_timeout(SHUTDOWN_TIMEOUT));
    }
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

static STARTED: AtomicU64 = AtomicU64::new(0);
static COMPLETED: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static WAKEUPS: AtomicU64 = AtomicU64::new(0);

/// Counts a bridged future from start to finish or drop.
struct Tracker {
    finished: bool,
}

impl Tracker {
    fn start() -> Self {
        STARTED.fetch_add(1, Ordering::Relaxed);
        Self { finished: false }
    }

    fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        let counter = if self.finished { &COMPLETED } else { &DROPPED };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// What the bridge has done so far: futures `started`, `completed` (ran to
/// the end, panics included) and `dropped` (cancelled before the end), the
/// number still `pending`, how many loop `wakeups` delivered them, the
/// runtime's `worker_threads` and whether it is `running`.
#[pyfunction]
pub fn stats(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let started = STARTED.load(Ordering::Relaxed);
    let completed = COMPLETED.load(Ordering::Relaxed);
    let dropped = DROPPED.load(Ordering::Relaxed);
    let state = lock(&STATE);
    let dict = PyDict::new(py);
    dict.set_item("started", started)?;
    dict.set_item("completed", completed)?;
    dict.set_item("dropped", dropped)?;
    dict.set_item("pending", started.saturating_sub(completed + dropped))?;
    dict.set_item("wakeups", WAKEUPS.load(Ordering::Relaxed))?;
    dict.set_item("worker_threads", state.worker_threads)?;
    dict.set_item("running", state.runtime.is_some())?;
    Ok(dict)
}

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

/// A future's output, converted to Python on the event loop's thread.
pub trait IntoPython: Send + 'static {
    /// The Python value of the output, or the exception to raise.
    fn into_python(self, py: Python<'_>) -> PyResult<Py<PyAny>>;
}

impl IntoPython for Value {
    fn into_python(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        convert::to_python(py, &self).map(Bound::unbind)
    }
}

impl IntoPython for () {
    fn into_python(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(py.None())
    }
}

impl IntoPython for Py<PyAny> {
    fn into_python(self, _py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self)
    }
}

macro_rules! plain_outputs {
    ($($type:ty),*) => {$(
        impl IntoPython for $type {
            fn into_python(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
                Ok(self.into_pyobject(py)?.into_any().unbind())
            }
        }
    )*};
}
plain_outputs!(i64, u64, f64, String);

impl IntoPython for bool {
    fn into_python(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(pyo3::types::PyBool::new(py, self)
            .to_owned()
            .into_any()
            .unbind())
    }
}

impl<T: IntoPython> IntoPython for Result<T, Error> {
    fn into_python(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match self {
            Ok(value) => value.into_python(py),
            Err(error) => Err(errors::to_py_err(py, &error)),
        }
    }
}

impl<T: IntoPython> IntoPython for PyResult<T> {
    fn into_python(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.and_then(|value| value.into_python(py))
    }
}

/// An output with a conversion of its own, run on the event loop's thread.
pub struct WithPython<F>(pub F);

impl<F> IntoPython for WithPython<F>
where
    F: FnOnce(Python<'_>) -> PyResult<Py<PyAny>> + Send + 'static,
{
    fn into_python(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        (self.0)(py)
    }
}

type Output = Box<dyn FnOnce(Python<'_>) -> PyResult<Py<PyAny>> + Send>;

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

/// A finished future waiting to be handed to its asyncio future.
struct Completion {
    future: Py<PyAny>,
    output: Output,
}

/// Delivers the results of one event loop's futures, on that loop's thread.
///
/// Worker threads queue completions here; the first to find no wake-up on
/// its way schedules one (`call_soon_threadsafe(dispatcher)`), and the call
/// on the loop thread hands out everything queued by then.
#[pyclass(frozen, module = "openbyd._native")]
pub struct Dispatcher {
    event_loop: Py<PyAny>,
    queue: Mutex<Vec<Completion>>,
    scheduled: AtomicBool,
    closed: AtomicBool,
}

#[pymethods]
impl Dispatcher {
    /// Sets the result or exception of every queued future that is still pending.
    fn __call__(&self, py: Python<'_>) {
        self.scheduled.store(false, Ordering::SeqCst);
        let batch = std::mem::take(&mut *lock(&self.queue));
        for completion in batch {
            let future = completion.future.bind(py);
            let pending = future
                .call_method0(intern!(py, "done"))
                .and_then(|done| done.is_truthy())
                .is_ok_and(|done| !done);
            if !pending {
                continue; // cancelled while the result was on its way
            }
            let outcome = match (completion.output)(py) {
                Ok(value) => future.call_method1(intern!(py, "set_result"), (value,)),
                Err(error) => {
                    future.call_method1(intern!(py, "set_exception"), (error.into_value(py),))
                }
            };
            if let Err(error) = outcome {
                error.write_unraisable(py, Some(future));
            }
        }
    }
}

impl Dispatcher {
    /// Queues a completion and wakes the loop if no wake-up is on its way.
    /// Runs on a worker thread, without the GIL.
    fn post(dispatcher: &Py<Dispatcher>, completion: Completion) {
        let this = dispatcher.get();
        if this.closed.load(Ordering::SeqCst) {
            return; // the loop is gone; the completion is dropped
        }
        lock(&this.queue).push(completion);
        if this.scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        WAKEUPS.fetch_add(1, Ordering::Relaxed);
        // `try_attach` gives up while the interpreter is being finalised.
        let _ = Python::try_attach(|py| {
            let woken = this.event_loop.bind(py).call_method1(
                intern!(py, "call_soon_threadsafe"),
                (dispatcher.clone_ref(py),),
            );
            if woken.is_err() {
                // The loop is closed: nothing queued for it can be delivered.
                this.closed.store(true, Ordering::SeqCst);
                drop(std::mem::take(&mut *lock(&this.queue)));
            }
        });
    }

    fn is_closed(&self, py: Python<'_>) -> bool {
        self.closed.load(Ordering::SeqCst)
            || self
                .event_loop
                .bind(py)
                .call_method0(intern!(py, "is_closed"))
                .and_then(|closed| closed.is_truthy())
                .unwrap_or(true)
    }
}

/// One dispatcher per event loop. A closed loop's entry is removed when the
/// next new loop registers.
static DISPATCHERS: Mutex<Vec<Py<Dispatcher>>> = Mutex::new(Vec::new());

fn dispatcher_for(py: Python<'_>, event_loop: &Bound<'_, PyAny>) -> PyResult<Py<Dispatcher>> {
    let known: Vec<Py<Dispatcher>> = lock(&DISPATCHERS)
        .iter()
        .map(|dispatcher| dispatcher.clone_ref(py))
        .collect();
    if let Some(found) = known
        .iter()
        .find(|dispatcher| dispatcher.get().event_loop.as_ptr() == event_loop.as_ptr())
    {
        return Ok(found.clone_ref(py));
    }
    // No lock is held while Python runs (`is_closed`), so another thread
    // cannot deadlock on it while waiting for the GIL.
    let closed: Vec<*mut pyo3::ffi::PyObject> = known
        .iter()
        .filter(|dispatcher| dispatcher.get().is_closed(py))
        .map(Py::as_ptr)
        .collect();
    let dispatcher = Py::new(
        py,
        Dispatcher {
            event_loop: event_loop.clone().unbind(),
            queue: Mutex::new(Vec::new()),
            scheduled: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        },
    )?;
    let mut registry = lock(&DISPATCHERS);
    registry.retain(|entry| !closed.contains(&entry.as_ptr()));
    registry.push(dispatcher.clone_ref(py));
    Ok(dispatcher)
}

/// Aborts the Rust task when its asyncio future is done (cancelled).
/// Aborting a task that has finished does nothing.
#[pyclass(frozen, module = "openbyd._native")]
struct Canceller(AbortHandle);

#[pymethods]
impl Canceller {
    fn __call__(&self, _future: &Bound<'_, PyAny>) {
        self.0.abort();
    }
}

/// Polls a future, turning a panic into an error message.
struct CatchPanic<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchPanic<F> {
    type Output = Result<F::Output, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = &mut self.get_mut().0;
        match catch_unwind(AssertUnwindSafe(|| inner.as_mut().poll(cx))) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_owned()))),
        }
    }
}

fn running_loop(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    static GET_RUNNING_LOOP: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    GET_RUNNING_LOOP
        .import(py, "asyncio", "get_running_loop")?
        .call0()
}

/// Runs `future` on the shared runtime and returns an `asyncio.Future` for
/// its output, created on the running event loop (`RuntimeError` when there
/// is none). See the module documentation.
pub fn spawn<'py, F>(py: Python<'py>, future: F) -> PyResult<Bound<'py, PyAny>>
where
    F: Future + Send + 'static,
    F::Output: IntoPython,
{
    let event_loop = running_loop(py)?;
    let handle = handle()?;
    let dispatcher = dispatcher_for(py, &event_loop)?;
    let awaitable = event_loop.call_method0(intern!(py, "create_future"))?;
    let target = awaitable.clone().unbind();
    let mut tracker = Tracker::start();
    let task = handle.spawn(async move {
        let outcome = CatchPanic(Box::pin(future)).await;
        tracker.finish();
        let output: Output = match outcome {
            Ok(value) => Box::new(move |py| value.into_python(py)),
            Err(message) => Box::new(move |_| {
                Err(PyRuntimeError::new_err(format!(
                    "openbyd: a Rust future panicked: {message}"
                )))
            }),
        };
        Dispatcher::post(
            &dispatcher,
            Completion {
                future: target,
                output,
            },
        );
    });
    let canceller = Canceller(task.abort_handle());
    if let Err(error) = awaitable.call_method1(intern!(py, "add_done_callback"), (canceller,)) {
        task.abort();
        return Err(error);
    }
    Ok(awaitable)
}

// ---------------------------------------------------------------------------
// Awaitables for checking the bridge
// ---------------------------------------------------------------------------

/// Waits `seconds` on the Rust runtime, then returns `result`.
#[pyfunction]
#[pyo3(signature = (seconds, result = None))]
pub fn sleep<'py>(
    py: Python<'py>,
    seconds: f64,
    result: Option<Py<PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    if seconds.is_nan() {
        return Err(PyValueError::new_err("seconds must be a number"));
    }
    let duration = Duration::try_from_secs_f64(seconds.max(0.0)).unwrap_or(Duration::MAX);
    spawn(py, async move {
        if !duration.is_zero() {
            tokio::time::sleep(duration).await;
        }
        WithPython(move |py: Python<'_>| Ok(result.unwrap_or_else(|| py.None())))
    })
}

/// Fails with the openbyd error of class `kind` after `delay` seconds.
/// For testing the exception mapping through the bridge.
#[pyfunction]
#[pyo3(signature = (kind, message = String::new(), code = String::new(), endpoint = String::new(), status_code = None, delay = 0.0))]
pub fn _fail<'py>(
    py: Python<'py>,
    kind: &str,
    message: String,
    code: String,
    endpoint: String,
    status_code: Option<u16>,
    delay: f64,
) -> PyResult<Bound<'py, PyAny>> {
    use openbyd_protocol::error::ErrorKind;
    let kind = ErrorKind::from_class_name(kind)
        .ok_or_else(|| PyValueError::new_err(format!("unknown error class {kind}")))?;
    let error = if kind.is_a(ErrorKind::BydTransportError) {
        Error::transport(message, status_code, endpoint)
    } else if kind.is_a(ErrorKind::BydApiError) {
        Error::api(kind, message, code, endpoint)
    } else {
        Error::new(kind, message)
    };
    let delay = Duration::from_secs_f64(delay.clamp(0.0, 60.0));
    spawn(py, async move {
        tokio::time::sleep(delay).await;
        Err::<(), Error>(error)
    })
}

/// Panics on the Rust runtime. For testing that a panic becomes an exception.
#[pyfunction]
pub fn _panic(py: Python<'_>, message: String) -> PyResult<Bound<'_, PyAny>> {
    spawn(py, async move {
        tokio::task::yield_now().await;
        if !message.is_empty() {
            panic!("{message}");
        }
    })
}
