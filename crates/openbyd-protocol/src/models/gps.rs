//! GPS information model (`pybyd/models/gps.py`).

use serde::Serialize;
use serde_json::{Map, Value};

use super::base::{self, BydModel, Fields, PyDateTime, aliased};
use crate::coerce::{self, ValidationError};
use crate::parity::Outcome;

/// GPS location data for a vehicle.
///
/// The response nests fields under a `data` dict which is flattened automatically.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GpsInfo {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Latitude in degrees.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub latitude: Option<f64>,
    /// Longitude in degrees.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub longitude: Option<f64>,
    /// Speed as reported with the fix.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub speed: Option<f64>,
    /// Heading in degrees.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub direction: Option<f64>,
    /// When the fix was taken, from `gpsTimeStamp`; `None` while the fix is stale (BYD sends 0).
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub gps_timestamp: Option<PyDateTime>,
    /// Serial of the request that produced this reply, if any.
    pub request_serial: Option<String>,
}

/// `_flatten_nested_data`: merges the nested `data` dict the GPS endpoint wraps results in.
fn flatten_nested_data(values: &Value) -> Value {
    match values {
        Value::Object(outer) => match outer.get("data") {
            Some(Value::Object(nested)) => {
                let mut merged = outer.clone();
                merged.extend(nested.clone());
                Value::Object(merged)
            }
            _ => values.clone(),
        },
        _ => values.clone(),
    }
}

impl BydModel for GpsInfo {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let values = base::clean_byd_values(&flatten_nested_data(raw), &[]);
        let mut fields = Fields::new(&values)?;
        let model = Self {
            raw: fields.raw(),
            latitude: fields.opt("latitude", coerce::float),
            longitude: fields.opt("longitude", coerce::float),
            speed: fields.opt("speed", coerce::float),
            direction: fields.opt("direction", coerce::float),
            gps_timestamp: fields.get(
                aliased("gps_timestamp", &["gpsTimeStamp"]),
                None,
                base::byd_timestamp,
            ),
            request_serial: fields.opt("request_serial", coerce::string),
        };
        fields.finish()?;
        Ok(model)
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.gps.GpsInfo" => base::model_call::<GpsInfo>(args),
        _ => return None,
    })
}
