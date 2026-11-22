//! Vehicle model (`pybyd/models/vehicle.py`).

use std::collections::BTreeSet;

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields, ParsedReply, Plan, PyDateTime};
use crate::coerce::{self, IntEnum, ValidationError};
use crate::parity::{Args, Outcome};
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
        static PLAN: Plan = Plan::new(EMPOWER_RANGE_KEY_ALIASES);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
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

/// Every permission `code` in an [`EmpowerRange`] tree, nested ones too
/// (pyBYD's `collect_permission_codes`). Empty codes are left out.
pub fn collect_permission_codes(ranges: &[EmpowerRange]) -> BTreeSet<String> {
    let mut codes = BTreeSet::new();
    let mut stack: Vec<&EmpowerRange> = ranges.iter().collect();
    while let Some(item) = stack.pop() {
        if !item.code.is_empty() {
            codes.insert(item.code.clone());
        }
        stack.extend(&item.children);
    }
    codes
}

// ---------------------------------------------------------------------------
// EmpowerDetail
// ---------------------------------------------------------------------------

/// A shared guest's grant in detail, from `empowerDetailByVin`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EmpowerDetail {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// Sharing grant identifier.
    pub empower_id: Option<i64>,
    /// Grant status code.
    pub empower_status: Option<i64>,
    /// Whether the guest may use the control PIN.
    pub empower_control_pwd_status: bool,
    /// The owner's account e-mail.
    pub owner_email: String,
    /// The guest's account e-mail.
    pub email: String,
    /// Grant period code.
    pub auth_period: Option<i64>,
    /// When the grant expires.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub expire_date: Option<PyDateTime>,
    /// The permission scopes granted.
    pub detail_list: Vec<EmpowerRange>,
}

impl EmpowerDetail {
    /// Every permission code granted in [`Self::detail_list`].
    pub fn granted_codes(&self) -> BTreeSet<String> {
        collect_permission_codes(&self.detail_list)
    }
}

impl BydModel for EmpowerDetail {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            empower_id: fields.opt("empower_id", coerce::int),
            empower_status: fields.opt("empower_status", coerce::int),
            empower_control_pwd_status: fields.get(
                "empower_control_pwd_status",
                false,
                coerce::boolean,
            ),
            owner_email: fields.get("owner_email", String::new(), coerce::string),
            email: fields.get("email", String::new(), coerce::string),
            auth_period: fields.opt("auth_period", coerce::int),
            expire_date: fields.get("expire_date", None, base::byd_timestamp),
            detail_list: fields.get("detail_list", Vec::new(), |value| {
                coerce::list(value, |item| EmpowerRange::parse(item, context))
            }),
        };
        fields.finish()?;
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([("granted_codes", json!(self.granted_codes()))])
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
    /// The "not now" switch of a pending OTA upgrade.
    pub not_upgrade_now_switch: String,
    /// Whether the car takes OTA upgrades at night (`"1"`).
    pub support_ota_night_upgrade: String,
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

    /// Every permission code granted to a shared guest through
    /// [`Self::range_detail_list`].
    pub fn granted_permission_codes(&self) -> BTreeSet<String> {
        collect_permission_codes(&self.range_detail_list)
    }

    /// Whether this account holds guest permission `code`. An owner, and a
    /// shared record without a `rangeDetailList`, fail open: `true`.
    pub fn has_permission(&self, code: &str) -> bool {
        if !self.is_shared() || self.range_detail_list.is_empty() {
            return true;
        }
        self.granted_permission_codes().contains(code)
    }

    /// Whether the car's learn flags say the app may run OTA upgrades
    /// (`otaUpgrade` is 1).
    pub fn ota_upgrade_supported(&self) -> bool {
        self.vehicle_fun_learn_info.get("otaUpgrade") == Some(&1)
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
    fn parse_reply(raw: Value, context: &Value) -> Result<ParsedReply<Self>, ValidationError> {
        Self::parse(&raw, context).map(ParsedReply::copied)
    }

    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
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
            not_upgrade_now_switch: fields.get(
                "not_upgrade_now_switch",
                String::new(),
                coerce::string,
            ),
            support_ota_night_upgrade: fields.get(
                "support_ota_night_upgrade",
                String::new(),
                coerce::string,
            ),
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
        base::properties([
            ("is_shared", json!(self.is_shared())),
            (
                "granted_permission_codes",
                json!(self.granted_permission_codes()),
            ),
            ("ota_upgrade_supported", json!(self.ota_upgrade_supported())),
        ])
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.vehicle.EnergyType" => enum_call::<EnergyType>(args),
        "models.vehicle.EmpowerRange" => base::model_call::<EmpowerRange>(args),
        "models.vehicle.Vehicle" => base::model_call::<Vehicle>(args),
        "models.vehicle.EmpowerDetail" => base::model_call::<EmpowerDetail>(args),
        "models.vehicle.collect_permission_codes" => (|| {
            let ranges = coerce::list(Args(args).value("ranges"), |item| {
                EmpowerRange::parse(item, &Value::Null)
            })?;
            Ok(json!(collect_permission_codes(&ranges)))
        })(),
        "models.vehicle.Vehicle.has_permission" => (|| {
            let args = Args(args);
            let vehicle = Vehicle::parse(args.value("raw"), &Value::Null)?;
            Ok(json!(vehicle.has_permission(args.str("code")?)))
        })(),
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

    #[test]
    fn guest_permissions_fail_open() {
        let parse = |raw: Value| Vehicle::parse(&raw, &Value::Null).unwrap();
        let owner = parse(json!({"empowerType": 0, "rangeDetailList": [{"code": "21"}]}));
        assert!(owner.has_permission("31"));
        let bare_guest = parse(json!({"empowerType": -1}));
        assert!(bare_guest.has_permission("31"));
        let guest = parse(json!({"empowerType": -1, "rangeDetailList": [
            {"code": "2", "children": [{"code": "21"}, {"code": ""}]}
        ]}));
        assert!(guest.has_permission("21"));
        assert!(!guest.has_permission("31"));
        assert_eq!(guest.granted_permission_codes().len(), 2);
        assert!(parse(json!({"vehicleFunLearnInfo": {"otaUpgrade": 1}})).ota_upgrade_supported());
        assert!(!parse(json!({"vehicleFunLearnInfo": {"otaUpgrade": 0}})).ota_upgrade_supported());
    }
}
