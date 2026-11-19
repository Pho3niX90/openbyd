//! Python bindings for openbyd. Imported as `openbyd._native`.
//!
//! - [`bridge`]: Rust futures as asyncio awaitables, on a shared tokio runtime.
//! - [`convert`]: `serde_json::Value` to and from Python objects.
//! - [`errors`]: openbyd errors as pyBYD's exception classes.
//! - [`models`]: pyBYD's reply models by class name, and the shadow comparison.
//! - [`lowlevel`]: single requests over HTTP through the bridge.
//! - [`client`], [`car`]: the client and its cars (`openbyd.Client`,
//!   `openbyd.Car`), with [`hooks`] running user callbacks on the client's
//!   event loop.
//! - `testing` (cargo feature `testing`): the fake BYD server and the MQTT test broker.
//!
//! The Python package (`python/openbyd`) wraps these in documented modules.

mod bridge;
mod car;
mod client;
mod convert;
mod errors;
mod hooks;
mod lowlevel;
mod models;
#[cfg(feature = "testing")]
mod testing;

use openbyd_crypto::Tables;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

#[derive(FromPyObject)]
enum Plaintext {
    Text(String),
    Bytes(Vec<u8>),
}

/// Wrap plaintext (str or bytes) in a Bangcle envelope.
#[pyfunction]
fn encode_envelope(py: Python<'_>, plaintext: Plaintext) -> String {
    let data = match plaintext {
        Plaintext::Text(text) => text.into_bytes(),
        Plaintext::Bytes(bytes) => bytes,
    };
    py.detach(|| openbyd_crypto::encode_envelope(&data))
}

/// Open a Bangcle envelope and return the plaintext bytes.
#[pyfunction]
fn decode_envelope<'py>(py: Python<'py>, envelope: &str) -> PyResult<Bound<'py, PyBytes>> {
    let plain = py
        .detach(|| openbyd_crypto::decode_envelope(envelope))
        .map_err(|error| errors::crypto_to_py_err(py, &error))?;
    Ok(PyBytes::new(py, &plain))
}

/// Encrypt whole 16-byte blocks in CBC mode with the built-in tables.
#[pyfunction]
fn encrypt_cbc<'py>(py: Python<'py>, data: &[u8], iv: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    let sealed = py
        .detach(|| openbyd_crypto::encrypt_cbc(Tables::embedded(), data, iv))
        .map_err(|error| errors::crypto_to_py_err(py, &error))?;
    Ok(PyBytes::new(py, &sealed))
}

/// Decrypt whole 16-byte blocks in CBC mode with the built-in tables.
#[pyfunction]
fn decrypt_cbc<'py>(py: Python<'py>, data: &[u8], iv: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    let plain = py
        .detach(|| openbyd_crypto::decrypt_cbc(Tables::embedded(), data, iv))
        .map_err(|error| errors::crypto_to_py_err(py, &error))?;
    Ok(PyBytes::new(py, &plain))
}

/// Run one parity call. Takes the arguments as JSON text and returns JSON
/// text: `{"ok": value}` or `{"error": {"kind": ..., "message": ...}}`.
/// The protocol crate answers first; names it does not know go to the
/// client crate, which holds the ported `_api` flows.
///
/// This is the hook the parity tests use to compare openbyd with pyBYD. It
/// is not part of the public API.
#[pyfunction]
fn parity_call(py: Python<'_>, name: &str, args_json: &str) -> PyResult<String> {
    let args: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|error| PyValueError::new_err(format!("arguments are not JSON: {error}")))?;
    let reply = py.detach(|| match call(name, &args) {
        Ok(value) => serde_json::json!({ "ok": value }),
        Err(error) => {
            serde_json::json!({ "error": { "kind": error.kind, "message": error.message } })
        }
    });
    Ok(reply.to_string())
}

fn call(name: &str, args: &serde_json::Value) -> openbyd_protocol::parity::Outcome {
    match openbyd_protocol::parity::call(name, args) {
        Err(error) if error.kind == "UnknownCall" => openbyd_client::parity::call(name, args),
        answer => answer,
    }
}

/// The built-in table file, in pyBYD's `bangcle_tables.bin` format.
#[pyfunction]
fn embedded_tables(py: Python<'_>) -> Bound<'_, PyBytes> {
    PyBytes::new(py, Tables::embedded_bytes())
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    // The cipher's errors are pyBYD's `BangcleError`, defined in Python.
    module.add("BangcleError", errors::exception_class(py, "BangcleError")?)?;
    module.add_function(wrap_pyfunction!(encode_envelope, module)?)?;
    module.add_function(wrap_pyfunction!(decode_envelope, module)?)?;
    module.add_function(wrap_pyfunction!(encrypt_cbc, module)?)?;
    module.add_function(wrap_pyfunction!(decrypt_cbc, module)?)?;
    module.add_function(wrap_pyfunction!(embedded_tables, module)?)?;
    module.add_function(wrap_pyfunction!(parity_call, module)?)?;

    module.add("DEFAULT_WORKER_THREADS", bridge::DEFAULT_WORKER_THREADS)?;
    module.add_function(wrap_pyfunction!(bridge::configure, module)?)?;
    module.add_function(wrap_pyfunction!(bridge::stats, module)?)?;
    module.add_function(wrap_pyfunction!(bridge::sleep, module)?)?;
    module.add_function(wrap_pyfunction!(bridge::shutdown, module)?)?;
    module.add_function(wrap_pyfunction!(bridge::_fail, module)?)?;
    module.add_function(wrap_pyfunction!(bridge::_panic, module)?)?;
    module.add_class::<bridge::Dispatcher>()?;
    module.add_class::<convert::HeldValue>()?;

    module.add("SHADOW_ERROR", models::SHADOW_ERROR)?;
    module.add("SHADOW_UNKNOWN_MODEL", models::SHADOW_UNKNOWN_MODEL)?;
    module.add_function(wrap_pyfunction!(models::model_names, module)?)?;
    module.add_function(wrap_pyfunction!(models::parse_model, module)?)?;
    module.add_function(wrap_pyfunction!(models::model_properties, module)?)?;
    module.add_function(wrap_pyfunction!(models::shadow_compare, module)?)?;

    module.add_class::<client::PyClient>()?;
    module.add_class::<car::PyCar>()?;

    module.add_function(wrap_pyfunction!(lowlevel::login, module)?)?;
    module.add_function(wrap_pyfunction!(lowlevel::post_token_json, module)?)?;

    #[cfg(feature = "testing")]
    {
        module.add_class::<testing::PyFakeServer>()?;
        module.add_class::<testing::PyReply>()?;
        module.add_class::<testing::PyTestBroker>()?;
    }
    module.add("HAS_TESTING", cfg!(feature = "testing"))?;

    // Stop the runtime before the interpreter is finalised, so no worker
    // thread touches Python while it is being torn down.
    py.import("atexit")?
        .call_method1("register", (module.getattr("shutdown")?,))?;
    Ok(())
}
