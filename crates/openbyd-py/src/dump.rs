//! Serialising models straight into Python objects, without a `serde_json::Value` in between.
//!
//! [`to_python`] gives exactly what `convert::dump_to_python(py,
//! &serde_json::to_value(value)?)` gives, which is what pyBYD's
//! `model_dump(mode="json")` gives for a model, while building only the
//! Python objects:
//!
//! - numbers, strings, booleans, `None`, lists and dicts as
//!   `serde_json::to_value` would make them (a non-finite `f64` that a
//!   model serialises without `ser_float` is `None`, an `i128` must fit 64
//!   bits, map keys are written as `serde_json` writes them);
//! - the strings `"NaN"`, `"Infinity"` and `"-Infinity"` become floats,
//!   except anywhere under a `raw` key ([`crate::convert::dump_to_python`]).
//!
//! A struct's field names are made into Python strings once per thread and
//! reused.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Display;
use std::hash::{BuildHasherDefault, Hasher};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyList, PyString};
use serde::Serialize;
use serde::ser::{self, Impossible};

/// Something [`to_python`] can convert once the GIL is held: a model, a list
/// of models or a JSON value. Results of Rust futures carry one of these to
/// the event loop's thread.
pub trait DumpToPython: Send {
    /// The Python form of `self`; see the module documentation.
    fn dump_to_python<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>>;
}

impl<T: Serialize + Send> DumpToPython for T {
    fn dump_to_python<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        to_python(py, self)
    }
}

/// `value` serialised into Python objects; see the module documentation.
pub fn to_python<'py, T: Serialize + ?Sized>(
    py: Python<'py>,
    value: &T,
) -> PyResult<Bound<'py, PyAny>> {
    value
        .serialize(PySerializer { py, floats: true })
        .map_err(|Error(error)| error)
}

/// A conversion failure: a Python exception, or `ValueError` for what
/// `serde_json` itself would refuse.
#[derive(Debug)]
pub struct Error(PyErr);

impl Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for Error {}

impl ser::Error for Error {
    fn custom<T: Display>(message: T) -> Self {
        Self(PyValueError::new_err(message.to_string()))
    }
}

impl From<PyErr> for Error {
    fn from(error: PyErr) -> Self {
        Self(error)
    }
}

type Result<T> = std::result::Result<T, Error>;

/// Hashes the address and length of a `&'static str`; the keys are few and fixed.
#[derive(Default)]
struct AddressHasher(u64);

impl Hasher for AddressHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }

    fn write_usize(&mut self, word: usize) {
        self.write_u64(word as u64);
    }

    fn write_u64(&mut self, word: u64) {
        self.0 = (self.0 ^ word)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .rotate_left(29);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type Names = HashMap<(usize, usize), Py<PyString>, BuildHasherDefault<AddressHasher>>;

/// A struct field name as an interned Python string, made once per thread.
fn field_name<'py>(py: Python<'py>, name: &'static str) -> Bound<'py, PyString> {
    thread_local! {
        static NAMES: RefCell<Names> = RefCell::new(Names::default());
    }
    let key = (name.as_ptr() as usize, name.len());
    NAMES.with(|names| {
        if let Some(known) = names.borrow().get(&key) {
            return known.bind(py).clone();
        }
        let made = PyString::intern(py, name);
        names.borrow_mut().insert(key, made.clone().unbind());
        made
    })
}

/// A string as `dump_to_python` gives it: the three non-finite names become floats where `floats`.
fn text<'py>(py: Python<'py>, value: &str, floats: bool) -> Bound<'py, PyAny> {
    if floats {
        match value {
            "NaN" => return PyFloat::new(py, f64::NAN).into_any(),
            "Infinity" => return PyFloat::new(py, f64::INFINITY).into_any(),
            "-Infinity" => return PyFloat::new(py, f64::NEG_INFINITY).into_any(),
            _ => {}
        }
    }
    PyString::new(py, value).into_any()
}

#[derive(Clone, Copy)]
struct PySerializer<'py> {
    py: Python<'py>,
    /// Whether the non-finite names become floats here (`false` under a `raw` key).
    floats: bool,
}

impl<'py> PySerializer<'py> {
    /// The serializer for the value under `key`.
    fn under(self, key: &str) -> Self {
        Self {
            py: self.py,
            floats: self.floats && key != "raw",
        }
    }

    fn int(self, value: i64) -> Result<Bound<'py, PyAny>> {
        let Ok(int) = value.into_pyobject(self.py);
        Ok(int.into_any())
    }

    fn uint(self, value: u64) -> Result<Bound<'py, PyAny>> {
        let Ok(int) = value.into_pyobject(self.py);
        Ok(int.into_any())
    }

    /// A one-entry dict `{variant: value}`, as `serde_json` writes an enum variant with data.
    fn variant(self, variant: &'static str, value: Bound<'py, PyAny>) -> Result<Bound<'py, PyAny>> {
        let dict = PyDict::new(self.py);
        dict.set_item(field_name(self.py, variant), value)?;
        Ok(dict.into_any())
    }
}

impl<'py> ser::Serializer for PySerializer<'py> {
    type Ok = Bound<'py, PyAny>;
    type Error = Error;
    type SerializeSeq = Seq<'py>;
    type SerializeTuple = Seq<'py>;
    type SerializeTupleStruct = Seq<'py>;
    type SerializeTupleVariant = Seq<'py>;
    type SerializeMap = Dict<'py>;
    type SerializeStruct = Dict<'py>;
    type SerializeStructVariant = Dict<'py>;

    fn serialize_bool(self, value: bool) -> Result<Self::Ok> {
        Ok(PyBool::new(self.py, value).to_owned().into_any())
    }

    fn serialize_i8(self, value: i8) -> Result<Self::Ok> {
        self.int(i64::from(value))
    }

    fn serialize_i16(self, value: i16) -> Result<Self::Ok> {
        self.int(i64::from(value))
    }

    fn serialize_i32(self, value: i32) -> Result<Self::Ok> {
        self.int(i64::from(value))
    }

    fn serialize_i64(self, value: i64) -> Result<Self::Ok> {
        self.int(value)
    }

    fn serialize_i128(self, value: i128) -> Result<Self::Ok> {
        match (u64::try_from(value), i64::try_from(value)) {
            (Ok(value), _) => self.uint(value),
            (_, Ok(value)) => self.int(value),
            _ => Err(ser::Error::custom("number out of range")),
        }
    }

    fn serialize_u8(self, value: u8) -> Result<Self::Ok> {
        self.uint(u64::from(value))
    }

    fn serialize_u16(self, value: u16) -> Result<Self::Ok> {
        self.uint(u64::from(value))
    }

    fn serialize_u32(self, value: u32) -> Result<Self::Ok> {
        self.uint(u64::from(value))
    }

    fn serialize_u64(self, value: u64) -> Result<Self::Ok> {
        self.uint(value)
    }

    fn serialize_u128(self, value: u128) -> Result<Self::Ok> {
        match u64::try_from(value) {
            Ok(value) => self.uint(value),
            Err(_) => Err(ser::Error::custom("number out of range")),
        }
    }

    fn serialize_f32(self, value: f32) -> Result<Self::Ok> {
        self.serialize_f64(f64::from(value))
    }

    fn serialize_f64(self, value: f64) -> Result<Self::Ok> {
        // `serde_json::to_value` makes a non-finite float `null`.
        if value.is_finite() {
            Ok(PyFloat::new(self.py, value).into_any())
        } else {
            self.serialize_unit()
        }
    }

    fn serialize_char(self, value: char) -> Result<Self::Ok> {
        Ok(text(self.py, value.encode_utf8(&mut [0; 4]), self.floats))
    }

    fn serialize_str(self, value: &str) -> Result<Self::Ok> {
        Ok(text(self.py, value, self.floats))
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<Self::Ok> {
        let items = value
            .iter()
            .map(|&byte| self.serialize_u8(byte))
            .collect::<Result<Vec<_>>>()?;
        Ok(PyList::new(self.py, items)?.into_any())
    }

    fn serialize_none(self) -> Result<Self::Ok> {
        self.serialize_unit()
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Self::Ok> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Self::Ok> {
        Ok(self.py.None().into_bound(self.py))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok> {
        let inner = value.serialize(self.under(variant))?;
        self.variant(variant, inner)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Seq<'py>> {
        Ok(Seq {
            serializer: self,
            items: Vec::with_capacity(len.unwrap_or(0)),
            variant: None,
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<Seq<'py>> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<Seq<'py>> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Seq<'py>> {
        Ok(Seq {
            serializer: self.under(variant),
            items: Vec::with_capacity(len),
            variant: Some((self, variant)),
        })
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Dict<'py>> {
        Ok(Dict {
            serializer: self,
            dict: PyDict::new(self.py),
            key: None,
            variant: None,
        })
    }

    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<Dict<'py>> {
        self.serialize_map(Some(len))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Dict<'py>> {
        let mut dict = self.under(variant).serialize_map(None)?;
        dict.variant = Some((self, variant));
        Ok(dict)
    }

    fn collect_str<T: Display + ?Sized>(self, value: &T) -> Result<Self::Ok> {
        self.serialize_str(&value.to_string())
    }
}

/// A list being built. `variant` is set for a tuple variant, which is
/// wrapped as `{variant: [...]}` at the end.
pub struct Seq<'py> {
    serializer: PySerializer<'py>,
    items: Vec<Bound<'py, PyAny>>,
    variant: Option<(PySerializer<'py>, &'static str)>,
}

impl<'py> Seq<'py> {
    fn push<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.items.push(value.serialize(self.serializer)?);
        Ok(())
    }

    fn finish(self) -> Result<Bound<'py, PyAny>> {
        let list = PyList::new(self.serializer.py, self.items)?.into_any();
        match self.variant {
            Some((outer, variant)) => outer.variant(variant, list),
            None => Ok(list),
        }
    }
}

impl<'py> ser::SerializeSeq for Seq<'py> {
    type Ok = Bound<'py, PyAny>;
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.push(value)
    }

    fn end(self) -> Result<Self::Ok> {
        self.finish()
    }
}

impl<'py> ser::SerializeTuple for Seq<'py> {
    type Ok = Bound<'py, PyAny>;
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.push(value)
    }

    fn end(self) -> Result<Self::Ok> {
        self.finish()
    }
}

impl<'py> ser::SerializeTupleStruct for Seq<'py> {
    type Ok = Bound<'py, PyAny>;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.push(value)
    }

    fn end(self) -> Result<Self::Ok> {
        self.finish()
    }
}

impl<'py> ser::SerializeTupleVariant for Seq<'py> {
    type Ok = Bound<'py, PyAny>;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.push(value)
    }

    fn end(self) -> Result<Self::Ok> {
        self.finish()
    }
}

/// A dict being built. `variant` is set for a struct variant, which is
/// wrapped as `{variant: {...}}` at the end.
pub struct Dict<'py> {
    serializer: PySerializer<'py>,
    dict: Bound<'py, PyDict>,
    /// A map key waiting for its value, and whether it is `"raw"`.
    key: Option<(Bound<'py, PyAny>, bool)>,
    variant: Option<(PySerializer<'py>, &'static str)>,
}

impl<'py> Dict<'py> {
    fn finish(self) -> Result<Bound<'py, PyAny>> {
        let dict = self.dict.into_any();
        match self.variant {
            Some((outer, variant)) => outer.variant(variant, dict),
            None => Ok(dict),
        }
    }
}

impl<'py> ser::SerializeMap for Dict<'py> {
    type Ok = Bound<'py, PyAny>;
    type Error = Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<()> {
        self.key = Some(key.serialize(KeySerializer {
            py: self.serializer.py,
        })?);
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        let (key, is_raw) = self
            .key
            .take()
            .ok_or_else(|| <Error as ser::Error>::custom("a map value came before its key"))?;
        let serializer = PySerializer {
            py: self.serializer.py,
            floats: self.serializer.floats && !is_raw,
        };
        self.dict.set_item(key, value.serialize(serializer)?)?;
        Ok(())
    }

    fn end(self) -> Result<Self::Ok> {
        self.finish()
    }
}

impl<'py> ser::SerializeStruct for Dict<'py> {
    type Ok = Bound<'py, PyAny>;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        let item = value.serialize(self.serializer.under(key))?;
        self.dict
            .set_item(field_name(self.serializer.py, key), item)?;
        Ok(())
    }

    fn end(self) -> Result<Self::Ok> {
        self.finish()
    }
}

impl<'py> ser::SerializeStructVariant for Dict<'py> {
    type Ok = Bound<'py, PyAny>;
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        ser::SerializeStruct::serialize_field(self, key, value)
    }

    fn end(self) -> Result<Self::Ok> {
        self.finish()
    }
}

/// Serialises a map key the way `serde_json` does: as a string, whatever its
/// type. Gives the Python key and whether it is `"raw"`.
struct KeySerializer<'py> {
    py: Python<'py>,
}

type Key<'py> = (Bound<'py, PyAny>, bool);

impl<'py> KeySerializer<'py> {
    fn text(self, key: &str) -> Result<Key<'py>> {
        Ok((PyString::new(self.py, key).into_any(), key == "raw"))
    }

    /// A float key, written by `serde_json` itself (as the only key of a
    /// one-entry map), so it is spelled exactly as `serde_json` spells it.
    fn written_by_serde_json<T: Serialize>(self, key: T) -> Result<Key<'py>> {
        match serde_json::to_value(OneKey(key)) {
            Ok(serde_json::Value::Object(entries)) => match entries.keys().next() {
                Some(key) => self.text(key),
                None => self.refuse(),
            },
            Ok(_) => self.refuse(),
            Err(error) => Err(ser::Error::custom(error)),
        }
    }

    fn refuse<T>(self) -> Result<T> {
        Err(ser::Error::custom("key must be a string"))
    }
}

impl<'py> ser::Serializer for KeySerializer<'py> {
    type Ok = Key<'py>;
    type Error = Error;
    type SerializeSeq = Impossible<Key<'py>, Error>;
    type SerializeTuple = Impossible<Key<'py>, Error>;
    type SerializeTupleStruct = Impossible<Key<'py>, Error>;
    type SerializeTupleVariant = Impossible<Key<'py>, Error>;
    type SerializeMap = Impossible<Key<'py>, Error>;
    type SerializeStruct = Impossible<Key<'py>, Error>;
    type SerializeStructVariant = Impossible<Key<'py>, Error>;

    fn serialize_bool(self, value: bool) -> Result<Self::Ok> {
        self.text(if value { "true" } else { "false" })
    }

    fn serialize_i8(self, value: i8) -> Result<Self::Ok> {
        self.text(&value.to_string())
    }

    fn serialize_i16(self, value: i16) -> Result<Self::Ok> {
        self.text(&value.to_string())
    }

    fn serialize_i32(self, value: i32) -> Result<Self::Ok> {
        self.text(&value.to_string())
    }

    fn serialize_i64(self, value: i64) -> Result<Self::Ok> {
        self.text(&value.to_string())
    }

    fn serialize_i128(self, value: i128) -> Result<Self::Ok> {
        self.text(&value.to_string())
    }

    fn serialize_u8(self, value: u8) -> Result<Self::Ok> {
        self.text(&value.to_string())
    }

    fn serialize_u16(self, value: u16) -> Result<Self::Ok> {
        self.text(&value.to_string())
    }

    fn serialize_u32(self, value: u32) -> Result<Self::Ok> {
        self.text(&value.to_string())
    }

    fn serialize_u64(self, value: u64) -> Result<Self::Ok> {
        self.text(&value.to_string())
    }

    fn serialize_u128(self, value: u128) -> Result<Self::Ok> {
        self.text(&value.to_string())
    }

    fn serialize_f32(self, value: f32) -> Result<Self::Ok> {
        self.written_by_serde_json(value)
    }

    fn serialize_f64(self, value: f64) -> Result<Self::Ok> {
        self.written_by_serde_json(value)
    }

    fn serialize_char(self, value: char) -> Result<Self::Ok> {
        self.text(value.encode_utf8(&mut [0; 4]))
    }

    fn serialize_str(self, value: &str) -> Result<Self::Ok> {
        self.text(value)
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<Self::Ok> {
        self.refuse()
    }

    fn serialize_none(self) -> Result<Self::Ok> {
        self.refuse()
    }

    fn serialize_some<T: Serialize + ?Sized>(self, _value: &T) -> Result<Self::Ok> {
        self.refuse()
    }

    fn serialize_unit(self) -> Result<Self::Ok> {
        self.refuse()
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok> {
        self.refuse()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok> {
        self.text(variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<Self::Ok> {
        self.refuse()
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq> {
        self.refuse()
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple> {
        self.refuse()
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct> {
        self.refuse()
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant> {
        self.refuse()
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap> {
        self.refuse()
    }

    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Self::SerializeStruct> {
        self.refuse()
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant> {
        self.refuse()
    }

    fn collect_str<T: Display + ?Sized>(self, value: &T) -> Result<Self::Ok> {
        self.text(&value.to_string())
    }
}

/// A map with `.0` as its only key, for `serde_json` to spell the key.
struct OneKey<T>(T);

impl<T: Serialize> Serialize for OneKey<T> {
    fn serialize<S: ser::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use ser::SerializeMap as _;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(&self.0, &())?;
        map.end()
    }
}

// ---------------------------------------------------------------------------
// Top-level fields, for the shadow comparison
// ---------------------------------------------------------------------------

/// The top-level fields of `serde_json::to_value(value)` other than `raw`,
/// in order, without serialising `raw`; `None` if that value would not be
/// an object or could not be made.
pub fn fields_but_raw<T: Serialize + ?Sized>(
    value: &T,
) -> Option<Vec<(Cow<'static, str>, serde_json::Value)>> {
    value.serialize(TopLevel).ok()
}

type Fields = Vec<(Cow<'static, str>, serde_json::Value)>;

/// Serialises the top level of a model as its fields; refuses anything that is not an object.
struct TopLevel;

/// Anything [`TopLevel`] refuses.
#[derive(Debug)]
struct NotAnObject;

impl Display for NotAnObject {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("not an object")
    }
}

impl std::error::Error for NotAnObject {}

impl ser::Error for NotAnObject {
    fn custom<T: Display>(_message: T) -> Self {
        Self
    }
}

type Refused = std::result::Result<Fields, NotAnObject>;

/// Collects the fields of a struct or the entries of a map.
struct Collect {
    fields: Fields,
    key: Option<String>,
}

impl Collect {
    /// Adds a field. `unique`: the key cannot have come before (a struct's
    /// field); otherwise a repeated key keeps its place and takes the new
    /// value, as in a `serde_json::Map`.
    fn push<T: Serialize + ?Sized>(
        &mut self,
        key: Cow<'static, str>,
        value: &T,
        unique: bool,
    ) -> std::result::Result<(), NotAnObject> {
        if key == "raw" {
            return Ok(());
        }
        let value = serde_json::to_value(value).map_err(|_| NotAnObject)?;
        let known = if unique {
            None
        } else {
            self.fields.iter_mut().find(|(known, _)| *known == key)
        };
        match known {
            Some(slot) => slot.1 = value,
            None => self.fields.push((key, value)),
        }
        Ok(())
    }
}

impl ser::SerializeStruct for Collect {
    type Ok = Fields;
    type Error = NotAnObject;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> std::result::Result<(), NotAnObject> {
        self.push(Cow::Borrowed(key), value, true)
    }

    fn end(self) -> Refused {
        Ok(self.fields)
    }
}

impl ser::SerializeMap for Collect {
    type Ok = Fields;
    type Error = NotAnObject;

    fn serialize_key<T: Serialize + ?Sized>(
        &mut self,
        key: &T,
    ) -> std::result::Result<(), NotAnObject> {
        // `serde_json` spells the key; a one-entry map of it has it as its only key.
        let spelled = serde_json::to_value(OneKey(key)).map_err(|_| NotAnObject)?;
        let key = spelled
            .as_object()
            .and_then(|entries| entries.keys().next());
        self.key = Some(key.ok_or(NotAnObject)?.clone());
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> std::result::Result<(), NotAnObject> {
        let key = self.key.take().ok_or(NotAnObject)?;
        self.push(Cow::Owned(key), value, false)
    }

    fn end(self) -> Refused {
        Ok(self.fields)
    }
}

impl ser::Serializer for TopLevel {
    type Ok = Fields;
    type Error = NotAnObject;
    type SerializeSeq = Impossible<Fields, NotAnObject>;
    type SerializeTuple = Impossible<Fields, NotAnObject>;
    type SerializeTupleStruct = Impossible<Fields, NotAnObject>;
    type SerializeTupleVariant = Impossible<Fields, NotAnObject>;
    type SerializeMap = Collect;
    type SerializeStruct = Collect;
    type SerializeStructVariant = Impossible<Fields, NotAnObject>;

    fn serialize_map(self, len: Option<usize>) -> std::result::Result<Collect, NotAnObject> {
        Ok(Collect {
            fields: Vec::with_capacity(len.unwrap_or(0)),
            key: None,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> std::result::Result<Collect, NotAnObject> {
        self.serialize_map(Some(len))
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Refused {
        value.serialize(self)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Refused {
        value.serialize(self)
    }

    fn serialize_bool(self, _value: bool) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_i64(self, _value: i64) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_u64(self, _value: u64) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_i8(self, _value: i8) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_i16(self, _value: i16) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_i32(self, _value: i32) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_u8(self, _value: u8) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_u16(self, _value: u16) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_u32(self, _value: u32) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_f32(self, _value: f32) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_f64(self, _value: f64) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_char(self, _value: char) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_str(self, _value: &str) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_bytes(self, _value: &[u8]) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_none(self) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_unit(self) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
    ) -> Refused {
        Err(NotAnObject)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Refused {
        // `serde_json` makes this `{variant: value}`; no model serialises so.
        Err(NotAnObject)
    }

    fn serialize_seq(
        self,
        _len: Option<usize>,
    ) -> std::result::Result<Self::SerializeSeq, NotAnObject> {
        Err(NotAnObject)
    }

    fn serialize_tuple(
        self,
        _len: usize,
    ) -> std::result::Result<Self::SerializeTuple, NotAnObject> {
        Err(NotAnObject)
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> std::result::Result<Self::SerializeTupleStruct, NotAnObject> {
        Err(NotAnObject)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> std::result::Result<Self::SerializeTupleVariant, NotAnObject> {
        Err(NotAnObject)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> std::result::Result<Self::SerializeStructVariant, NotAnObject> {
        Err(NotAnObject)
    }
}
