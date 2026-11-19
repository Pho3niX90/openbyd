//! Converting between `serde_json::Value` and Python objects, without JSON text in between.
//!
//! | Python | JSON value | Notes |
//! |---|---|---|
//! | `dict` (and subclasses) | object | insertion order is kept both ways; keys must be `str` |
//! | `list`, `tuple` (and subclasses) | array | a tuple comes back as a list |
//! | `str` (and subclasses, such as `StrEnum`) | string | |
//! | `int` (and subclasses, such as `IntEnum`) | number | from `-2**63` to `2**64 - 1`; anything wider raises `OverflowError` |
//! | `float` | number | NaN and the infinities raise `ValueError` in [`Mode::Strict`]; see [`Mode`] |
//! | `bool` | `true` / `false` | checked before `int`, so `True` stays a bool |
//! | `None` | `null` | |
//!
//! Any other type raises `TypeError`, and nesting deeper than [`MAX_DEPTH`]
//! (or a container that holds itself) raises `ValueError`.
//!
//! Going back, an integer that fits `i64` or `u64` becomes an `int` and any
//! other number a `float`.

use pyo3::exceptions::{PyOverflowError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use serde_json::{Map, Number, Value};

/// How deep containers may nest. `serde_json`'s own parser stops at 128.
pub const MAX_DEPTH: usize = 256;

/// What [`to_value`] does with a float JSON cannot hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// NaN and the infinities raise `ValueError`.
    Strict,
    /// NaN and the infinities become the strings `"NaN"`, `"Infinity"` and
    /// `"-Infinity"`, the form openbyd's models dump them in, and an
    /// integer too wide for 64 bits becomes its decimal text. For comparing
    /// a pyBYD dump with an openbyd one, where a value must never stop the
    /// comparison.
    Lenient,
}

/// Converts a Python object to a JSON value. See the module documentation.
pub fn to_value(object: &Bound<'_, PyAny>, mode: Mode) -> PyResult<Value> {
    convert(object, mode, 0)
}

fn convert(object: &Bound<'_, PyAny>, mode: Mode, depth: usize) -> PyResult<Value> {
    if depth > MAX_DEPTH {
        return Err(PyValueError::new_err(format!(
            "value nests deeper than {MAX_DEPTH} levels"
        )));
    }
    // Exact types first: they are the common case and cost one pointer compare each.
    if object.is_exact_instance_of::<PyString>() {
        return Ok(Value::String(
            object.cast::<PyString>()?.to_str()?.to_owned(),
        ));
    }
    if object.is_exact_instance_of::<PyDict>() {
        return dict(object.cast::<PyDict>()?, mode, depth);
    }
    if object.is_exact_instance_of::<PyInt>() {
        return int(object, mode);
    }
    if object.is_exact_instance_of::<PyFloat>() {
        return float(object.cast::<PyFloat>()?.value(), mode);
    }
    if object.is_exact_instance_of::<PyList>() {
        return list(object.cast::<PyList>()?.iter(), mode, depth);
    }
    if object.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(flag) = object.cast::<PyBool>() {
        return Ok(Value::Bool(flag.is_true()));
    }
    // Subclasses: enums, OrderedDict, tuples and the like.
    if let Ok(text) = object.cast::<PyString>() {
        return Ok(Value::String(text.to_str()?.to_owned()));
    }
    if object.is_instance_of::<PyInt>() {
        return int(object, mode);
    }
    if let Ok(number) = object.cast::<PyFloat>() {
        return float(number.value(), mode);
    }
    if let Ok(entries) = object.cast::<PyDict>() {
        return dict(entries, mode, depth);
    }
    if let Ok(items) = object.cast::<PyList>() {
        return list(items.iter(), mode, depth);
    }
    if let Ok(items) = object.cast::<PyTuple>() {
        return list(items.iter(), mode, depth);
    }
    Err(PyTypeError::new_err(format!(
        "{} is not a JSON value",
        object.get_type().name()?
    )))
}

fn int(object: &Bound<'_, PyAny>, mode: Mode) -> PyResult<Value> {
    if let Ok(number) = object.extract::<i64>() {
        return Ok(Value::Number(number.into()));
    }
    if let Ok(number) = object.extract::<u64>() {
        return Ok(Value::Number(number.into()));
    }
    match mode {
        Mode::Strict => Err(PyOverflowError::new_err(
            "integer does not fit in 64 bits (from -2**63 to 2**64 - 1)",
        )),
        Mode::Lenient => Ok(Value::String(object.str()?.to_str()?.to_owned())),
    }
}

fn float(number: f64, mode: Mode) -> PyResult<Value> {
    if let Some(number) = Number::from_f64(number) {
        return Ok(Value::Number(number));
    }
    match mode {
        Mode::Strict => Err(PyValueError::new_err(format!(
            "{number} cannot be held in JSON"
        ))),
        Mode::Lenient => Ok(Value::String(nonfinite_name(number).to_owned())),
    }
}

fn nonfinite_name(number: f64) -> &'static str {
    if number.is_nan() {
        "NaN"
    } else if number > 0.0 {
        "Infinity"
    } else {
        "-Infinity"
    }
}

/// A dict key as text. Keys must be `str` (or a subclass).
pub fn dict_key<'a>(key: &'a Bound<'_, PyAny>) -> PyResult<&'a str> {
    match key.cast::<PyString>() {
        Ok(key) => key.to_str(),
        Err(_) => Err(PyTypeError::new_err(format!(
            "dict keys must be str, not {}",
            key.get_type().name()?
        ))),
    }
}

fn dict(entries: &Bound<'_, PyDict>, mode: Mode, depth: usize) -> PyResult<Value> {
    let mut map = Map::with_capacity(entries.len());
    for (key, item) in entries.iter() {
        map.insert(dict_key(&key)?.to_owned(), convert(&item, mode, depth + 1)?);
    }
    Ok(Value::Object(map))
}

fn list<'py>(
    items: impl ExactSizeIterator<Item = Bound<'py, PyAny>>,
    mode: Mode,
    depth: usize,
) -> PyResult<Value> {
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        values.push(convert(&item, mode, depth + 1)?);
    }
    Ok(Value::Array(values))
}

/// [`to_value`] of a value found directly in a dict (one level deep, which
/// counts towards [`MAX_DEPTH`]).
pub fn item_to_value(object: &Bound<'_, PyAny>, mode: Mode) -> PyResult<Value> {
    convert(object, mode, 1)
}

/// Fails exactly where [`item_to_value`] would, without building the value.
pub fn check_item(object: &Bound<'_, PyAny>, mode: Mode) -> PyResult<()> {
    check(object, mode, 1)
}

/// [`convert`] without the result: the same checks in the same order.
fn check(object: &Bound<'_, PyAny>, mode: Mode, depth: usize) -> PyResult<()> {
    if depth > MAX_DEPTH {
        return Err(PyValueError::new_err(format!(
            "value nests deeper than {MAX_DEPTH} levels"
        )));
    }
    if object.is_exact_instance_of::<PyString>() {
        return object.cast::<PyString>()?.to_str().map(drop);
    }
    if object.is_exact_instance_of::<PyDict>() {
        return check_dict(object.cast::<PyDict>()?, mode, depth);
    }
    if object.is_exact_instance_of::<PyInt>() {
        return int(object, mode).map(drop);
    }
    if object.is_exact_instance_of::<PyFloat>() {
        return float(object.cast::<PyFloat>()?.value(), mode).map(drop);
    }
    if object.is_exact_instance_of::<PyList>() {
        return check_items(object.cast::<PyList>()?.iter(), mode, depth);
    }
    if object.is_none() || object.cast::<PyBool>().is_ok() {
        return Ok(());
    }
    if let Ok(text) = object.cast::<PyString>() {
        return text.to_str().map(drop);
    }
    if object.is_instance_of::<PyInt>() {
        return int(object, mode).map(drop);
    }
    if let Ok(number) = object.cast::<PyFloat>() {
        return float(number.value(), mode).map(drop);
    }
    if let Ok(entries) = object.cast::<PyDict>() {
        return check_dict(entries, mode, depth);
    }
    if let Ok(items) = object.cast::<PyList>() {
        return check_items(items.iter(), mode, depth);
    }
    if let Ok(items) = object.cast::<PyTuple>() {
        return check_items(items.iter(), mode, depth);
    }
    convert(object, mode, depth).map(drop)
}

fn check_dict(entries: &Bound<'_, PyDict>, mode: Mode, depth: usize) -> PyResult<()> {
    for (key, item) in entries.iter() {
        dict_key(&key)?;
        check(&item, mode, depth + 1)?;
    }
    Ok(())
}

fn check_items<'py>(
    items: impl Iterator<Item = Bound<'py, PyAny>>,
    mode: Mode,
    depth: usize,
) -> PyResult<()> {
    for item in items {
        check(&item, mode, depth + 1)?;
    }
    Ok(())
}

/// What `to_python(py, &to_value(object, Mode::Strict)?)` gives, built from
/// `object` itself instead of through a JSON value.
///
/// Containers are always new (a `dict` or `list`, whatever the input's
/// subclass; a tuple becomes a list), so the result shares no container with
/// `object`. Leaves of the exact types `str`, `int`, `float` and `bool`, and
/// `None`, are shared, since they cannot change. A subclass of a leaf type
/// (an `IntEnum`, a `StrEnum`, a `float` subclass) becomes its base type, and
/// a `str`-subclass key becomes a `str`; two keys with the same text then
/// collapse as `serde_json::Map::insert` collapses them: the first position,
/// the last value.
///
/// Call it only on an object [`to_value`] accepted in [`Mode::Strict`]; for
/// anything else it fails where [`to_value`] fails.
pub fn round_tripped<'py>(object: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    round_trip(object, 0)
}

/// Whether `object` is a leaf [`round_tripped`] may share: an exact `str`,
/// `int`, `float` or `bool`, or `None`.
fn shared_leaf(object: &Bound<'_, PyAny>) -> bool {
    object.is_exact_instance_of::<PyString>()
        || object.is_exact_instance_of::<PyInt>()
        || object.is_exact_instance_of::<PyFloat>()
        || object.is_exact_instance_of::<PyBool>()
        || object.is_none()
}

fn round_trip<'py>(object: &Bound<'py, PyAny>, depth: usize) -> PyResult<Bound<'py, PyAny>> {
    let py = object.py();
    if depth > MAX_DEPTH {
        return Err(PyValueError::new_err(format!(
            "value nests deeper than {MAX_DEPTH} levels"
        )));
    }
    if shared_leaf(object) {
        return Ok(object.clone());
    }
    if let Ok(entries) = object.cast::<PyDict>() {
        // The common case, a reply that is flat or nearly so: `dict.copy()`
        // shares the keys and values, which is right for every shared leaf,
        // and the few values that are not leaves are then replaced by their
        // own copies. Replacing a value keeps the key's place. This needs
        // exact `str` keys: no two of them have the same text.
        if object.is_exact_instance_of::<PyDict>() {
            let mut exact_keys = true;
            let mut nested = Vec::new();
            for (key, item) in entries.iter() {
                if !key.is_exact_instance_of::<PyString>() {
                    exact_keys = false;
                    break;
                }
                if !shared_leaf(&item) {
                    nested.push((key, item));
                }
            }
            if exact_keys {
                let copy = entries.copy()?;
                for (key, item) in nested {
                    copy.set_item(key, round_trip(&item, depth + 1)?)?;
                }
                return Ok(copy.into_any());
            }
        }
        let dict = PyDict::new(py);
        for (key, item) in entries.iter() {
            let key = if key.is_exact_instance_of::<PyString>() {
                key.cast_into::<PyString>()?
            } else {
                PyString::new(py, dict_key(&key)?)
            };
            // A later key with the same text replaces the value in place, as
            // `Map::insert` does.
            dict.set_item(key, round_trip(&item, depth + 1)?)?;
        }
        return Ok(dict.into_any());
    }
    if let Ok(items) = object.cast::<PyList>() {
        return round_trip_items(py, items.iter(), depth);
    }
    if let Ok(items) = object.cast::<PyTuple>() {
        return round_trip_items(py, items.iter(), depth);
    }
    // Leaf subclasses: through the same conversions `to_value` and
    // `to_python` use, so the value and type come out the same.
    if let Ok(flag) = object.cast::<PyBool>() {
        return Ok(PyBool::new(py, flag.is_true()).to_owned().into_any());
    }
    if let Ok(text) = object.cast::<PyString>() {
        return Ok(PyString::new(py, text.to_str()?).into_any());
    }
    if object.is_instance_of::<PyInt>() {
        return to_python(py, &int(object, Mode::Strict)?);
    }
    if let Ok(number) = object.cast::<PyFloat>() {
        return to_python(py, &float(number.value(), Mode::Strict)?);
    }
    // Not a JSON value: fail as `to_value` fails.
    to_python(py, &to_value(object, Mode::Strict)?)
}

fn round_trip_items<'py>(
    py: Python<'py>,
    items: impl Iterator<Item = Bound<'py, PyAny>>,
    depth: usize,
) -> PyResult<Bound<'py, PyAny>> {
    let list = PyList::empty(py);
    for item in items {
        list.append(round_trip(&item, depth + 1)?)?;
    }
    Ok(list.into_any())
}

/// Converts a JSON value to Python objects. See the module documentation.
pub fn to_python<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    back(py, value, false)
}

/// Converts a model dump (or a property map) to what pyBYD's
/// `model_dump(mode="json")` gives.
///
/// openbyd's models write a non-finite float as the string `"NaN"`,
/// `"Infinity"` or `"-Infinity"`, because a JSON value cannot hold one;
/// pydantic gives the float itself. This turns those strings back into
/// floats everywhere except under a `raw` key, which holds the input as it
/// was. A `str` field whose text is exactly one of those three words would be
/// turned into a float too; no pyBYD string field is known to hold one.
pub fn dump_to_python<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    back(py, value, true)
}

fn back<'py>(py: Python<'py>, value: &Value, floats: bool) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        Value::Null => py.None().into_bound(py),
        Value::Bool(flag) => PyBool::new(py, *flag).to_owned().into_any(),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                int.into_pyobject(py)?.into_any()
            } else if let Some(int) = number.as_u64() {
                int.into_pyobject(py)?.into_any()
            } else {
                PyFloat::new(py, number.as_f64().unwrap_or(f64::NAN)).into_any()
            }
        }
        Value::String(text) => match (floats, text.as_str()) {
            (true, "NaN") => PyFloat::new(py, f64::NAN).into_any(),
            (true, "Infinity") => PyFloat::new(py, f64::INFINITY).into_any(),
            (true, "-Infinity") => PyFloat::new(py, f64::NEG_INFINITY).into_any(),
            _ => PyString::new(py, text).into_any(),
        },
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(back(py, item, floats)?)?;
            }
            list.into_any()
        }
        Value::Object(entries) => {
            let dict = PyDict::new(py);
            for (key, item) in entries {
                let floats = floats && key != "raw";
                dict.set_item(PyString::new(py, key), back(py, item, floats)?)?;
            }
            dict.into_any()
        }
    })
}

/// A JSON value held on the Rust side, for measuring the conversions.
///
/// Not part of the public API: `benchmarks` and the tests use it to compare
/// direct conversion with a trip through JSON text.
#[pyclass(frozen, module = "openbyd._native", name = "_Value")]
pub struct HeldValue(Value);

#[pymethods]
impl HeldValue {
    /// Converts `object` directly.
    #[new]
    fn new(object: &Bound<'_, PyAny>) -> PyResult<Self> {
        to_value(object, Mode::Strict).map(Self)
    }

    /// Parses JSON text.
    #[staticmethod]
    fn from_json(text: &str) -> PyResult<Self> {
        serde_json::from_str(text)
            .map(Self)
            .map_err(|error| PyValueError::new_err(error.to_string()))
    }

    /// Converts the value back to Python objects directly.
    fn to_python<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        to_python(py, &self.0)
    }

    /// Writes the value as JSON text.
    fn to_json(&self) -> String {
        self.0.to_string()
    }
}
