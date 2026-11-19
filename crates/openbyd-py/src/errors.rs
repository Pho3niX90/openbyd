//! Rust errors as the Python exceptions pyBYD raises (`python/openbyd/exceptions.py`).
//!
//! The classes are defined in Python, so that their constructors, attributes
//! and inheritance are exactly pyBYD's, and looked up here by name.

use openbyd_protocol::coerce::ValidationError;
use openbyd_protocol::error::{Error, ErrorKind};
use pyo3::exceptions::{
    PyAssertionError, PyAttributeError, PyOverflowError, PyRuntimeError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBytes, PyDict, PyTuple};

fn exceptions(py: Python<'_>) -> PyResult<&Bound<'_, PyModule>> {
    static MODULE: PyOnceLock<Py<PyModule>> = PyOnceLock::new();
    MODULE
        .get_or_try_init(py, || py.import("openbyd.exceptions").map(Bound::unbind))
        .map(|module| module.bind(py))
}

/// The class `name` from `openbyd.exceptions`.
pub fn exception_class<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyAny>> {
    exceptions(py)?.getattr(name)
}

/// Builds `class(message, **keywords)` as a `PyErr`; if that fails, the failure.
fn raise(class: PyResult<Bound<'_, PyAny>>, message: &str, keywords: &Bound<'_, PyDict>) -> PyErr {
    match class.and_then(|class| class.call((message,), Some(keywords))) {
        Ok(instance) => PyErr::from_value(instance),
        Err(error) => error,
    }
}

/// The Python exception for an openbyd [`Error`]: the class of the same
/// name, carrying `code` and `endpoint` (`BydApiError` and its subclasses)
/// or `status_code` and `endpoint` (`BydTransportError`).
///
/// Python's own classes that pyBYD lets through (`ValueError`, `TypeError`,
/// `json.JSONDecodeError`, ...) are raised as themselves; a pydantic
/// `ValidationError` becomes `openbyd.ValidationError`. The parity tests'
/// `ScriptExhausted` becomes a `RuntimeError`.
pub fn to_py_err(py: Python<'_>, error: &Error) -> PyErr {
    let message = error.message();
    match error.kind() {
        ErrorKind::ValueError => PyValueError::new_err(message.to_owned()),
        ErrorKind::TypeError => PyTypeError::new_err(message.to_owned()),
        ErrorKind::AttributeError => PyAttributeError::new_err(message.to_owned()),
        ErrorKind::AssertionError => PyAssertionError::new_err(message.to_owned()),
        ErrorKind::ScriptExhausted => {
            PyRuntimeError::new_err(format!("ScriptExhausted: {message}"))
        }
        ErrorKind::JsonDecodeError => {
            let class = py
                .import("json")
                .and_then(|json| json.getattr("JSONDecodeError"));
            match class.and_then(|class| class.call1((message, "", 0))) {
                Ok(instance) => PyErr::from_value(instance),
                Err(error) => error,
            }
        }
        ErrorKind::UnicodeDecodeError => {
            let class = py
                .import("builtins")
                .and_then(|builtins| builtins.getattr("UnicodeDecodeError"));
            let arguments = ("utf-8", PyBytes::new(py, b""), 0, 0, message);
            match class.and_then(|class| class.call1(arguments)) {
                Ok(instance) => PyErr::from_value(instance),
                Err(error) => error,
            }
        }
        kind => match attributes(py, error) {
            Ok(keywords) => raise(exception_class(py, kind.class_name()), message, &keywords),
            Err(error) => error,
        },
    }
}

/// The keyword arguments pyBYD's constructor of this error's class takes.
fn attributes<'py>(py: Python<'py>, error: &Error) -> PyResult<Bound<'py, PyDict>> {
    let keywords = PyDict::new(py);
    if error.is_a(ErrorKind::BydApiError) {
        keywords.set_item("code", error.code())?;
        keywords.set_item("endpoint", error.endpoint())?;
    } else if error.is_a(ErrorKind::BydTransportError) {
        keywords.set_item("status_code", error.status_code())?;
        keywords.set_item("endpoint", error.endpoint())?;
    }
    Ok(keywords)
}

/// The Python exception for a model validation failure.
///
/// A pydantic `ValidationError` becomes `openbyd.ValidationError` with
/// `error_type` and `loc`; an exception that escapes pydantic in pyBYD
/// (`TypeError`, `OverflowError`, ...) is raised as that class. A value
/// pydantic accepts but openbyd cannot hold (an integer outside 64 bits)
/// raises `OverflowError`.
pub fn validation_to_py_err(py: Python<'_>, error: &ValidationError) -> PyErr {
    let message = error.to_string();
    match error.exception {
        "ValidationError" => {
            let keywords = PyDict::new(py);
            let filled = keywords
                .set_item("error_type", error.error_type)
                .and_then(|()| keywords.set_item("loc", PyTuple::new(py, &error.loc)?));
            if let Err(error) = filled {
                return error;
            }
            raise(exception_class(py, "ValidationError"), &message, &keywords)
        }
        "Unsupported" => PyOverflowError::new_err(message),
        name => {
            let class = py
                .import("builtins")
                .and_then(|builtins| builtins.getattr(name))
                .or_else(|_| Ok(py.get_type::<PyValueError>().into_any()));
            raise(class, &message, &PyDict::new(py))
        }
    }
}

/// The Python exception for a failure of the envelope cipher (`BangcleError`).
pub fn crypto_to_py_err(py: Python<'_>, error: &openbyd_crypto::Error) -> PyErr {
    raise(
        exception_class(py, "BangcleError"),
        &error.to_string(),
        &PyDict::new(py),
    )
}
