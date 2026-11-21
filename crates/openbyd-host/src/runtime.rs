//! The one tokio runtime and the one HTTP pool every client in the process shares.
//!
//! A binding spawns each call here with [`spawn`] and hands the result to
//! its own language's future. The runtime starts on first use and lives for
//! the rest of the process.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use openbyd_client::http::HttpPool;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::JoinHandle;

use crate::error::HostError;

/// Worker threads of the shared runtime unless [`configure`] says otherwise.
pub const DEFAULT_WORKER_THREADS: usize = 2;

/// The most worker threads [`configure`] accepts.
pub const MAX_WORKER_THREADS: usize = 256;

static WORKER_THREADS: AtomicUsize = AtomicUsize::new(DEFAULT_WORKER_THREADS);
static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static STARTING: Mutex<()> = Mutex::new(());

/// Sets the number of worker threads. Fails once the runtime has started
/// (after the first client or the first call).
pub fn configure(worker_threads: usize) -> Result<(), HostError> {
    let _starting = STARTING.lock().unwrap_or_else(PoisonError::into_inner);
    if RUNTIME.get().is_some() {
        return Err(HostError::value(
            "the runtime has already started; configure it before the first client",
        ));
    }
    if !(1..=MAX_WORKER_THREADS).contains(&worker_threads) {
        return Err(HostError::value(format!(
            "worker_threads must be from 1 to {MAX_WORKER_THREADS}"
        )));
    }
    WORKER_THREADS.store(worker_threads, Ordering::SeqCst);
    Ok(())
}

/// The number of worker threads the runtime has, or will start with.
pub fn worker_threads() -> usize {
    WORKER_THREADS.load(Ordering::SeqCst)
}

/// Whether the runtime has started.
pub fn started() -> bool {
    RUNTIME.get().is_some()
}

fn runtime() -> &'static Runtime {
    if let Some(runtime) = RUNTIME.get() {
        return runtime;
    }
    let _starting = STARTING.lock().unwrap_or_else(PoisonError::into_inner);
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(worker_threads())
            // DNS lookups run on the blocking pool; one thread per lookup at most.
            .max_blocking_threads(64)
            .thread_name("openbyd-worker")
            .enable_all()
            .build()
            .expect("the openbyd runtime starts")
    })
}

/// The shared runtime's handle, starting the runtime on first use.
pub fn handle() -> Handle {
    runtime().handle().clone()
}

/// Runs `future` on the shared runtime.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().spawn(future)
}

/// Runs `future` on the shared runtime and waits for it on the calling
/// thread. Not for a runtime thread: a binding calls this from its own.
pub fn block_on<F: Future>(future: F) -> F::Output {
    runtime().block_on(future)
}

/// The connection pool every client in the process shares.
pub(crate) fn pool() -> &'static HttpPool {
    static POOL: OnceLock<HttpPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let _entered = runtime().enter();
        HttpPool::new()
    })
}
