//! Vehicle model (`pybyd/models/vehicle.py`).

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields, PyDateTime};
use crate::coerce::{self, IntEnum, ValidationError};
use crate::parity::Outcome;
use crate::pyjson;

// The enum helpers moved to `base`; these are their old paths.
pub use super::base::PyEnum;
#[allow(unused_imports)]
pub(crate) use super::base::{enum_call, py_equals_int, py_truthy};

// ---------------------------------------------------------------------------
// EnergyType
// ---------------------------------------------------------------------------

/// Vehicle powertrain type.
///
/// Maps to the BYD API's `energyType` (per-vehicle attribute) and
/// `powerType` (request field on `getEnergyConsumption`). The cloud
/// transports the value as a string-encoded integer (`"0"` / `"1"` /
/// `"2"`); call sites converting to the wire format use `str(int(value))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnergyType {
    /// Not reported, or a value with no member.
    Unknown = -1,
    /// Battery electric.
    Ev = 0,
    /// Combustion engine only.
    Ice = 1,
    /// Plug-in hybrid.
    Hybrid = 2,
}

impl IntEnum for EnergyType {
    const MEMBERS: &'static [Self] = &[Self::Unknown, Self::Ev, Self::Ice, Self::Hybrid];
    const MISSING: Option<Self> = Some(Self::Unknown);
    fn value(self) -> i64 {
        self as i64
    }
}

impl PyEnum for EnergyType {
    fn name(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::Ev => "EV",
            Self::Ice => "ICE",
            Self::Hybrid => "HYBRID",
        }
    }
}

// ---------------------------------------------------------------------------
// EmpowerRange
// ---------------------------------------------------------------------------

/// A permission scope granted to a shared user.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EmpowerRange {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Scope code.
    pub code: String,
    /// Scope name.
    pub name: String,
    /// Nested scopes.
    pub children: Vec<EmpowerRange>,
}

/// BYD sometimes sends `childList` instead of `children`.
const EMPOWER_RANGE_KEY_ALIASES: &[(&str, &str)] = &[("childList", "children")];

impl BydModel for EmpowerRange {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let mut fields = Fields::cleaned(raw, EMPOWER_RANGE_KEY_ALIASES)?;
        let model = Self {
            raw: fields.raw(),
            code: fields.get("code", String::new(), coerce::string),
            name: fields.get("name", String::new(), coerce::string),
            children: fields.get("children", Vec::new(), |value| {
                coerce::list(value, |item| Self::parse(item, _context))
            }),
        };
        fields.finish()?;
        Ok(model)
    }
}

// ---------------------------------------------------------------------------
// Vehicle
// ---------------------------------------------------------------------------

/// A vehicle associated with the user's account.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Vehicle {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// Model name, such as `"SEAL"`.
    pub model_name: String,
    /// Brand name.
    pub brand_name: String,
    /// Powertrain type.
    #[serde(serialize_with = "base::ser_int_enum")]
    pub energy_type: EnergyType,
    /// The owner's nickname for the car.
    pub auto_alias: String,
    /// Licence plate.
    pub auto_plate: String,
    /// Main picture URL (falls back to `cfPic.picMainUrl`).
    pub pic_main_url: String,
    /// Picture set URL (falls back to `cfPic.picSetUrl`).
    pub pic_set_url: String,
    /// External model type code.
    pub out_model_type: String,
    /// Odometer reading (km).
    #[serde(serialize_with = "base::ser_opt_float")]
    pub total_mileage: Option<f64>,
    /// Model identifier.
    pub model_id: Option<i64>,
    /// Car type code.
    pub car_type: Option<i64>,
    /// Whether this is the account's default car.
    pub default_car: bool,
    /// Sharing type; negative for a car shared with this account.
    pub empower_type: Option<i64>,
    /// Permission status code.
    pub permission_status: Option<i64>,
    /// T-Box firmware version.
    pub tbox_version: String,
    /// Vehicle state text.
    pub vehicle_state: String,
    /// When the car was bought.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub auto_bought_time: Option<PyDateTime>,
    /// When the cloud service was activated.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub yun_active_time: Option<PyDateTime>,
    /// Sharing grant identifier.
    pub empower_id: Option<i64>,
    /// Permission scopes granted to a shared user.
    pub range_detail_list: Vec<EmpowerRange>,
    /// Per-feature presence/variant flags returned at the top level of
    /// `getVehicles`. 46 keys observed on a Sealion 7 EU; values look
    /// tri-state: `0` absent / `1` present / `-1` not applicable. Used by
    /// command gating as a fine-grained second gate alongside the coarser
    /// `functionNo` list (Issue #47).
    pub vehicle_fun_learn_info: IndexMap<String, i64>,
}

impl Vehicle {
    /// Whether the car is shared with this account (`empower_type` below 0).
    pub fn is_shared(&self) -> bool {
        self.empower_type.is_some_and(|kind| kind < 0)
    }

    /// `_fill_missing_pics`: fills empty picture URLs from the nested `cfPic` dict in `raw`.
    fn fill_missing_pics(&mut self) {
        if !self.pic_main_url.is_empty() && !self.pic_set_url.is_empty() {
            return;
        }
        let Some(Value::Object(nested)) = self.raw.get("cfPic") else {
            return;
        };
        let pick = |camel: &str, snake: &str| {
            [nested.get(camel), nested.get(snake)]
                .into_iter()
                .flatten()
                .find(|value| py_truthy(value))
                .map_or_else(String::new, pyjson::py_str)
        };
        if self.pic_main_url.is_empty() {
            self.pic_main_url = pick("picMainUrl", "pic_main_url");
        }
        if self.pic_set_url.is_empty() {
            self.pic_set_url = pick("picSetUrl", "pic_set_url");
        }
    }
}

impl BydModel for Vehicle {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    /// By reference: `_fill_missing_pics` reads `raw` while the model is
    /// built, so `raw` cannot be left for later.
    fn parse_owned(raw: Value, context: &Value) -> Result<Self, ValidationError> {
        Self::parse(&raw, context)
    }

    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        let mut fields = Fields::cleaned(raw, &[])?;
        let mut model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            model_name: fields.get("model_name", String::new(), coerce::string),
            brand_name: fields.get("brand_name", String::new(), coerce::string),
            energy_type: fields.get("energy_type", EnergyType::Unknown, coerce::int_enum),
            auto_alias: fields.get("auto_alias", String::new(), coerce::string),
            auto_plate: fields.get("auto_plate", String::new(), coerce::string),
            pic_main_url: fields.get("pic_main_url", String::new(), coerce::string),
            pic_set_url: fields.get("pic_set_url", String::new(), coerce::string),
            out_model_type: fields.get("out_model_type", String::new(), coerce::string),
            total_mileage: fields.opt("total_mileage", coerce::float),
            model_id: fields.opt("model_id", coerce::int),
            car_type: fields.opt("car_type", coerce::int),
            default_car: fields.get("default_car", false, coerce::boolean),
            empower_type: fields.opt("empower_type", coerce::int),
            permission_status: fields.opt("permission_status", coerce::int),
            tbox_version: fields.get("tbox_version", String::new(), coerce::string),
            vehicle_state: fields.get("vehicle_state", String::new(), coerce::string),
            auto_bought_time: fields.get("auto_bought_time", None, base::byd_timestamp),
            yun_active_time: fields.get("yun_active_time", None, base::byd_timestamp),
            empower_id: fields.opt("empower_id", coerce::int),
            range_detail_list: fields.get("range_detail_list", Vec::new(), |value| {
                coerce::list(value, |item| EmpowerRange::parse(item, context))
            }),
            vehicle_fun_learn_info: fields.get(
                "vehicle_fun_learn_info",
                IndexMap::new(),
                |value| coerce::dict(value, coerce::int),
            ),
        };
        fields.finish()?;
        model.fill_missing_pics();
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([("is_shared", json!(self.is_shared()))])
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.vehicle.EnergyType" => enum_call::<EnergyType>(args),
        "models.vehicle.EmpowerRange" => base::model_call::<EmpowerRange>(args),
        "models.vehicle.Vehicle" => base::model_call::<Vehicle>(args),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_calls_follow_python_equality() {
        assert_eq!(EnergyType::call(&json!(2)), EnergyType::Hybrid);
        assert_eq!(EnergyType::call(&json!(2.0)), EnergyType::Hybrid);
        assert_eq!(EnergyType::call(&json!(true)), EnergyType::Ice);
        assert_eq!(EnergyType::call(&json!("2")), EnergyType::Unknown);
        assert_eq!(EnergyType::call(&json!(u64::MAX)), EnergyType::Unknown);
        assert_eq!(EnergyType::call(&json!([2])), EnergyType::Unknown);
        assert_eq!(
            enum_call::<EnergyType>(&json!({"value": 0})).unwrap(),
            json!({"name": "EV", "value": 0})
        );
    }

    #[test]
    fn truthiness() {
        assert!(!py_truthy(&json!(null)));
        assert!(!py_truthy(&json!(0.0)));
        assert!(py_truthy(&json!(-1)));
        assert!(!py_truthy(&json!([])));
        assert!(py_truthy(&json!({"a": 1})));
        assert!(!py_truthy(&json!("")));
        assert!(py_truthy(&json!(true)));
    }

    #[test]
    fn pictures_come_from_cf_pic() {
        let vehicle = Vehicle::parse(
            &json!({"picMainUrl": "m", "cfPic": {"picSetUrl": 0, "pic_set_url": 5}}),
            &Value::Null,
        )
        .unwrap();
        assert_eq!(vehicle.pic_main_url, "m");
        assert_eq!(vehicle.pic_set_url, "5");
        assert!(!vehicle.is_shared());
    }
}
