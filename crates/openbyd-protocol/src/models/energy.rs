//! Energy consumption data model (`pybyd/models/energy.py`).
//!
//! The `getEnergyConsumption` endpoint returns a four-section nested
//! object: two 7-day rolling graphs (`selfGraph`, `autoModelGraph`)
//! plus per-leg breakouts for lifetime totals (`cumulativeEnergyConsumption`)
//! and the last 50km (`nearestEnergyConsumption`). For hybrids
//! each leg is exposed in its own field, no string splitting required.

use serde::Serialize;
use serde_json::{Map, Value};

use super::base::{self, BydModel, Fields, PyDateTime};
use crate::coerce::{self, ValidationError};
use crate::parity::Outcome;

/// Python's `float(text)` for text already stripped of whitespace.
///
/// Differs from pydantic's string-to-float: any Unicode decimal digit counts
/// as its ASCII digit, and a single underscore is allowed between digits.
fn python_float(text: &str) -> Option<f64> {
    let mut ascii = String::with_capacity(text.len());
    let mut previous = '\0';
    for c in text.chars() {
        let c = match coerce::py_decimal(c) {
            Some(digit) => char::from_digit(digit, 10)?,
            None if c.is_ascii() => c,
            None => return None,
        };
        if c == '_' {
            if !previous.is_ascii_digit() {
                return None;
            }
        } else {
            if previous == '_' && !c.is_ascii_digit() {
                return None;
            }
            ascii.push(c);
        }
        previous = c;
    }
    if previous == '_' {
        return None;
    }
    ascii.parse().ok()
}

/// `_to_float`: coerce BYD numeric strings to `float`; sentinels and junk give `None`.
fn to_float(value: &Value) -> Option<f64> {
    match value {
        Value::Null => None,
        Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
        Value::Number(number) => number.as_f64(),
        Value::String(text) => {
            let text = coerce::py_strip(text);
            if matches!(text, "" | "--" | "nan" | "NaN") {
                return None;
            }
            python_float(text)
        }
        // `str()` of a list or dict never reads as a number.
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// `_to_int`: coerce BYD numeric strings to `int`; sentinels and junk give `None`.
///
/// Python's `int(float)` raises `ValueError` for NaN (which pydantic reports
/// as a `ValidationError`) and `OverflowError` for infinity (which escapes).
fn to_int(value: &Value) -> Result<Option<i64>, ValidationError> {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0; // 2^63
    let Some(number) = to_float(value) else {
        return Ok(None);
    };
    if number.is_nan() {
        Err(ValidationError::new("value_error"))
    } else if number.is_infinite() {
        Err(ValidationError::raised("OverflowError", "overflow"))
    } else if number <= -LIMIT || number >= LIMIT {
        // Python holds an arbitrarily large int; openbyd's integers are i64.
        Err(ValidationError::unsupported_int())
    } else {
        Ok(Some(number as i64))
    }
}

/// `_to_float_list`: coerce a list of numeric strings to `list[float]`, dropping sentinels.
fn to_float_list(value: &Value) -> Vec<f64> {
    match value {
        Value::Array(items) => items.iter().filter_map(to_float).collect(),
        _ => Vec::new(),
    }
}

/// `_canonical_unit`: canonicalise a unit string with `normalize_unit`, then read it as `str`.
///
/// `kW·h/100km` becomes `kWh/100km`. Applied to every unit field on this
/// endpoint so the realtime and energy models surface a single normalized form.
fn canonical_unit(value: &Value) -> Result<String, ValidationError> {
    match value {
        Value::String(text) => Ok(base::normalize_unit(Some(text)).unwrap_or_default()),
        other => coerce::string(other),
    }
}

/// 7-day rolling consumption series.
///
/// Used for both `selfGraph` (this vehicle) and `autoModelGraph`
/// (model-average comparison). The unit reflects whichever leg matches
/// the request's `powerType`: `kW·h/100km` for `"0"`, `L/100km`
/// for `"1"`/`"2"`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EnergyConsumptionGraph {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// The daily consumption values, sentinels dropped.
    #[serde(serialize_with = "base::ser_float_list")]
    pub energy_consumption: Vec<f64>,
    /// Canonical unit of the series.
    pub energy_consumption_unit: String,
}

impl BydModel for EnergyConsumptionGraph {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let values = base::clean_byd_values(raw, &[]);
        let mut fields = Fields::new(&values)?;
        let model = Self {
            raw: fields.raw(),
            energy_consumption: fields.get("energy_consumption", Vec::new(), |value| {
                Ok(to_float_list(value))
            }),
            energy_consumption_unit: fields.get(
                "energy_consumption_unit",
                String::new(),
                canonical_unit,
            ),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Lifetime AVERAGE consumption rate, per leg.
///
/// Named `cumulativeEnergyConsumption` by the API, but the fields it
/// carries are consumption rates (kWh/100km, L/100km) averaged over
/// the vehicle's lifetime, not an absolute cumulative energy total.
///
/// Both legs are populated for hybrids when `powerType="2"`; pure-EV
/// or pure-ICE responses leave the unused leg empty.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CumulativeEnergyConsumption {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Lifetime mileage.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_mileage: Option<f64>,
    /// Canonical unit of [`Self::total_mileage`].
    pub mileage_unit: String,
    /// Average EV consumption (kWh/100km).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub avg_ev_consumption: Option<f64>,
    /// Canonical unit of [`Self::avg_ev_consumption`].
    pub ev_unit: String,
    /// Average petrol consumption (L/100km).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub avg_oil_consumption: Option<f64>,
    /// Canonical unit of [`Self::avg_oil_consumption`].
    pub oil_unit: String,
}

impl BydModel for CumulativeEnergyConsumption {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let values = base::clean_byd_values(raw, &[]);
        let mut fields = Fields::new(&values)?;
        let model = Self {
            raw: fields.raw(),
            total_mileage: fields.get("total_mileage", None, |value| Ok(to_float(value))),
            mileage_unit: fields.get("mileage_unit", String::new(), canonical_unit),
            avg_ev_consumption: fields
                .get("avg_ev_consumption", None, |value| Ok(to_float(value))),
            ev_unit: fields.get("ev_unit", String::new(), canonical_unit),
            avg_oil_consumption: fields
                .get("avg_oil_consumption", None, |value| Ok(to_float(value))),
            oil_unit: fields.get("oil_unit", String::new(), canonical_unit),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Last-50km breakdown, per leg + driving-mode distribution.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NearestEnergyConsumption {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Average EV consumption over the last 50km (kWh/100km).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub avg_ev_consumption: Option<f64>,
    /// Total EV energy used over the last 50km (kWh).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub ev_consumption: Option<f64>,
    /// Canonical unit of the EV average.
    pub ev_unit: String,
    /// Canonical unit of the EV total.
    pub ev_value_unit: String,
    /// Average petrol consumption over the last 50km (L/100km).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub avg_oil_consumption: Option<f64>,
    /// Total petrol used over the last 50km (L).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub oil_consumption: Option<f64>,
    /// Canonical unit of the petrol average.
    pub oil_unit: String,
    /// Canonical unit of the petrol total.
    pub oil_value_unit: String,
    /// Equivalent petrol consumption: EV usage expressed in L/100km.
    #[serde(serialize_with = "base::ser_opt_float")]
    pub avg_eq_oil_consumption: Option<f64>,
    /// Percentage of the last 50km in normal drive mode.
    pub drive_distribution: Option<i64>,
    /// Percentage of the last 50km on electric power.
    pub elect_distribution: Option<i64>,
    /// Percentage of the last 50km with HVAC active.
    pub air_distribution: Option<i64>,
    /// Percentage of the last 50km in other modes.
    pub other_distribution: Option<i64>,
}

impl BydModel for NearestEnergyConsumption {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let values = base::clean_byd_values(raw, &[]);
        let mut fields = Fields::new(&values)?;
        let model = Self {
            raw: fields.raw(),
            avg_ev_consumption: fields
                .get("avg_ev_consumption", None, |value| Ok(to_float(value))),
            ev_consumption: fields.get("ev_consumption", None, |value| Ok(to_float(value))),
            ev_unit: fields.get("ev_unit", String::new(), canonical_unit),
            ev_value_unit: fields.get("ev_value_unit", String::new(), canonical_unit),
            avg_oil_consumption: fields
                .get("avg_oil_consumption", None, |value| Ok(to_float(value))),
            oil_consumption: fields.get("oil_consumption", None, |value| Ok(to_float(value))),
            oil_unit: fields.get("oil_unit", String::new(), canonical_unit),
            oil_value_unit: fields.get("oil_value_unit", String::new(), canonical_unit),
            avg_eq_oil_consumption: fields
                .get("avg_eq_oil_consumption", None, |value| Ok(to_float(value))),
            drive_distribution: fields.get("drive_distribution", None, to_int),
            elect_distribution: fields.get("elect_distribution", None, to_int),
            air_distribution: fields.get("air_distribution", None, to_int),
            other_distribution: fields.get("other_distribution", None, to_int),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Energy consumption data for a vehicle.
///
/// Mirrors the four-section response of
/// `/vehicleInfo/vehicle/getEnergyConsumption`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EnergyConsumption {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// This vehicle's 7-day series.
    pub self_graph: Option<EnergyConsumptionGraph>,
    /// Lifetime average consumption.
    pub cumulative_energy_consumption: Option<CumulativeEnergyConsumption>,
    /// Last-50km breakdown.
    pub nearest_energy_consumption: Option<NearestEnergyConsumption>,
    /// Model-average 7-day series.
    pub auto_model_graph: Option<EnergyConsumptionGraph>,
    /// When the data was produced (`time` on the wire).
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub timestamp: Option<PyDateTime>,
}

/// The API sometimes sends the timestamp as `time`.
const KEY_ALIASES: &[(&str, &str)] = &[("time", "timestamp")];

impl BydModel for EnergyConsumption {
    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        let values = base::clean_byd_values(raw, KEY_ALIASES);
        let mut fields = Fields::new(&values)?;
        let model = Self {
            raw: fields.raw(),
            self_graph: fields.opt("self_graph", |v| EnergyConsumptionGraph::parse(v, context)),
            cumulative_energy_consumption: fields.opt("cumulative_energy_consumption", |v| {
                CumulativeEnergyConsumption::parse(v, context)
            }),
            nearest_energy_consumption: fields.opt("nearest_energy_consumption", |v| {
                NearestEnergyConsumption::parse(v, context)
            }),
            auto_model_graph: fields.opt("auto_model_graph", |v| {
                EnergyConsumptionGraph::parse(v, context)
            }),
            timestamp: fields.get("timestamp", None, base::byd_timestamp),
        };
        fields.finish()?;
        Ok(model)
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.energy.EnergyConsumptionGraph" => base::model_call::<EnergyConsumptionGraph>(args),
        "models.energy.CumulativeEnergyConsumption" => {
            base::model_call::<CumulativeEnergyConsumption>(args)
        }
        "models.energy.NearestEnergyConsumption" => {
            base::model_call::<NearestEnergyConsumption>(args)
        }
        "models.energy.EnergyConsumption" => base::model_call::<EnergyConsumption>(args),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn python_floats_read_unicode_digits_and_underscores() {
        assert_eq!(python_float("١٢.٥"), Some(12.5));
        assert_eq!(python_float("1_000"), Some(1000.0));
        assert_eq!(python_float("1__0"), None);
        assert_eq!(python_float("_1"), None);
        assert_eq!(python_float("1_"), None);
        assert_eq!(python_float("1_.5"), None);
        assert_eq!(python_float("1e_5"), None);
        assert_eq!(python_float("é"), None);
        assert!(python_float("-INF").is_some_and(f64::is_infinite));
        assert!(python_float("NAN").is_some_and(f64::is_nan));
    }

    #[test]
    fn to_float_handles_every_json_type() {
        assert_eq!(to_float(&json!(null)), None);
        assert_eq!(to_float(&json!(true)), Some(1.0));
        assert_eq!(to_float(&json!(3)), Some(3.0));
        assert_eq!(to_float(&json!(" 2.5 ")), Some(2.5));
        assert_eq!(to_float(&json!("--")), None);
        assert_eq!(to_float(&json!([1])), None);
        assert_eq!(to_float(&json!({"a": 1})), None);
        assert_eq!(to_float(&json!("abc")), None);
    }

    #[test]
    fn to_int_follows_python_int_of_float() {
        assert_eq!(to_int(&json!("12.9")).unwrap(), Some(12));
        assert_eq!(to_int(&json!("-12.9")).unwrap(), Some(-12));
        assert_eq!(to_int(&json!("x")).unwrap(), None);
        assert!(to_int(&json!("NAN")).unwrap_err().is_validation());
        assert_eq!(to_int(&json!("inf")).unwrap_err().exception, "OverflowError");
        assert_eq!(to_int(&json!(1e300)).unwrap_err().exception, "Unsupported");
        assert_eq!(to_int(&json!(-1e300)).unwrap_err().exception, "Unsupported");
    }

    #[test]
    fn units_are_normalised_and_must_be_text() {
        assert_eq!(canonical_unit(&json!("kW·h/100km")).unwrap(), "kWh/100km");
        assert!(canonical_unit(&json!(5)).unwrap_err().is_validation());
    }

    #[test]
    fn the_series_drops_sentinels() {
        let graph = EnergyConsumptionGraph::parse(
            &json!({"energyConsumption": ["1.5", "--", 2, null, "x"], "energyConsumptionUnit": "kW·h/100km"}),
            &Value::Null,
        )
        .unwrap();
        assert_eq!(graph.energy_consumption, vec![1.5, 2.0]);
        assert_eq!(graph.energy_consumption_unit, "kWh/100km");
    }
}
