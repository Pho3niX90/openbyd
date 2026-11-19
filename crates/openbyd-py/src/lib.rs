//! Python bindings for openbyd. Imported as `openbyd._native`.

use openbyd_crypto::{Error, Tables};
use pyo3::create_exception;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

create_exception!(
    openbyd,
    BangcleError,
    PyValueError,
    "An envelope or cipher input could not be processed."
);

fn to_py_err(error: Error) -> PyErr {
    BangcleError::new_err(error.to_string())
}

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
        .map_err(to_py_err)?;
    Ok(PyBytes::new(py, &plain))
}

/// Encrypt whole 16-byte blocks in CBC mode with the built-in tables.
#[pyfunction]
fn encrypt_cbc<'py>(py: Python<'py>, data: &[u8], iv: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    let sealed = py
        .detach(|| openbyd_crypto::encrypt_cbc(Tables::embedded(), data, iv))
        .map_err(to_py_err)?;
    Ok(PyBytes::new(py, &sealed))
}

/// Decrypt whole 16-byte blocks in CBC mode with the built-in tables.
#[pyfunction]
fn decrypt_cbc<'py>(py: Python<'py>, data: &[u8], iv: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    let plain = py
        .detach(|| openbyd_crypto::decrypt_cbc(Tables::embedded(), data, iv))
        .map_err(to_py_err)?;
    Ok(PyBytes::new(py, &plain))
}

/// The built-in table file, in pyBYD's `bangcle_tables.bin` format.
#[pyfunction]
fn embedded_tables(py: Python<'_>) -> Bound<'_, PyBytes> {
    PyBytes::new(py, Tables::embedded_bytes())
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("BangcleError", module.py().get_type::<BangcleError>())?;
    module.add_function(wrap_pyfunction!(encode_envelope, module)?)?;
    module.add_function(wrap_pyfunction!(decode_envelope, module)?)?;
    module.add_function(wrap_pyfunction!(encrypt_cbc, module)?)?;
    module.add_function(wrap_pyfunction!(decrypt_cbc, module)?)?;
    module.add_function(wrap_pyfunction!(embedded_tables, module)?)?;
    Ok(())
}
