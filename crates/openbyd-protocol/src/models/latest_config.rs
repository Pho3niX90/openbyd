//! Vehicle latest-config models and normalised capabilities (`pybyd/models/latest_config.py`).
//!
//! `getLatestConfig` lists the features a car has as `functionNo` codes in
//! `cfFixedList` (with nested second-level lists). [`VehicleCapabilities`]
//! turns that into one tri-state flag per feature of [`FEATURE_REGISTRY`]:
//! `Some(true)` when the car lists one of the feature's codes, `Some(false)`
//! when the lookup ran and none was listed, `None` when capability
//! resolution itself failed ([`VehicleCapabilities::unknown`]).

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields, Plan};
use super::command_gating::known_command_function_nos;
use crate::coerce::{self, ValidationError};
use crate::parity::{Args, Outcome, ParityError};

/// One row in the capability registry: a capability flag, the BYD
/// `functionNo` values that signal it, and a label and category for reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FeatureSpec {
    /// The capability flag, also the [`VehicleCapabilities`] field name.
    pub key: &'static str,
    /// Human-readable label.
    pub name: &'static str,
    /// Any of these `functionNo` values means the feature is supported.
    pub function_nos: &'static [&'static str],
    /// `"command"`, `"sensor"` or `"metadata"`.
    pub category: &'static str,
    /// Longer description (empty for every current row).
    pub description: &'static str,
}

/// Single source of truth for "feature key → BYD functionNo(s) → human label".
pub const FEATURE_REGISTRY: &[FeatureSpec] = &[
    FeatureSpec {
        key: "lock",
        name: "Lock doors",
        function_nos: &["1005"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "unlock",
        name: "Unlock doors",
        function_nos: &["1006"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "climate",
        name: "Climate (HVAC)",
        function_nos: &["1001", "10300001", "1015"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "car_on",
        name: "Car on / one-tap prep",
        function_nos: &["1001", "10300001", "1015"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "battery_heat",
        name: "Battery heating",
        function_nos: &["10300002", "10100001", "10100002"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "steering_wheel_heat",
        name: "Steering-wheel heat",
        function_nos: &["10030010", "10300004"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "driver_seat_heat",
        name: "Driver seat heat",
        function_nos: &["10030002", "10300003"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "driver_seat_ventilation",
        name: "Driver seat ventilation",
        function_nos: &["10030001", "10300003"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "passenger_seat_heat",
        name: "Passenger seat heat",
        function_nos: &["10030005", "10300003"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "passenger_seat_ventilation",
        name: "Passenger seat ventilation",
        function_nos: &["10030004", "10300003"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "rear_left_seat_heat",
        name: "Rear left seat heat",
        function_nos: &["10030007"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "rear_right_seat_heat",
        name: "Rear right seat heat",
        function_nos: &["10030009"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "rear_left_seat_ventilation",
        name: "Rear left seat ventilation",
        function_nos: &["10030006"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "rear_right_seat_ventilation",
        name: "Rear right seat ventilation",
        function_nos: &["10030008"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "find_car",
        name: "Find car (horn + lights)",
        function_nos: &["1007"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "flash_lights",
        name: "Flash lights",
        function_nos: &["1008"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "close_windows",
        name: "Close windows",
        function_nos: &["1026"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "open_windows",
        name: "Open windows (vent)",
        function_nos: &["1026"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "location",
        name: "Vehicle location (GPS)",
        function_nos: &["1014"],
        category: "metadata",
        description: "",
    },
    FeatureSpec {
        key: "start_charge",
        name: "Start charging",
        function_nos: &["1012"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "stop_charge",
        name: "Stop charging",
        function_nos: &["1012"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "open_trunk",
        name: "Open trunk",
        function_nos: &["1020"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "close_trunk",
        name: "Close trunk",
        function_nos: &["1021"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "one_click_shutdown",
        name: "Shut car off",
        function_nos: &["1031"],
        category: "command",
        description: "",
    },
    FeatureSpec {
        key: "child_presence_detection",
        name: "Child presence detection (CPD)",
        function_nos: &["1009"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "digital_key",
        name: "Digital key",
        function_nos: &["1013"],
        category: "metadata",
        description: "",
    },
    FeatureSpec {
        key: "nfc_key",
        name: "NFC key",
        function_nos: &["10130001"],
        category: "metadata",
        description: "",
    },
    FeatureSpec {
        key: "nfc_key_3c",
        name: "NFC key (3C)",
        function_nos: &["10130002"],
        category: "metadata",
        description: "",
    },
    FeatureSpec {
        key: "uwb_key",
        name: "UWB key",
        function_nos: &["10130003"],
        category: "metadata",
        description: "",
    },
    FeatureSpec {
        key: "mobile_ota",
        name: "Mobile OTA",
        function_nos: &["1016"],
        category: "metadata",
        description: "",
    },
    FeatureSpec {
        key: "scheduled_departure",
        name: "Scheduled departure",
        function_nos: &["1029"],
        category: "metadata",
        description: "",
    },
    FeatureSpec {
        key: "energy_analysis",
        name: "Energy analysis",
        function_nos: &["1022"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "tpms",
        name: "Tire pressure monitoring (TPMS)",
        function_nos: &["1004"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "direct_tpms",
        name: "Direct tire pressure",
        function_nos: &["10040001"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "sentry",
        name: "Sentry mode",
        function_nos: &["1024"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "vehicle_health",
        name: "Vehicle health summary",
        function_nos: &["1023"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_tpms",
        name: "Health: tire pressure system",
        function_nos: &["10230001"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_steering",
        name: "Health: steering system",
        function_nos: &["10230002"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_srs",
        name: "Health: SRS (airbag)",
        function_nos: &["10230003"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_powertrain",
        name: "Health: powertrain",
        function_nos: &["10230004"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_battery",
        name: "Health: power battery",
        function_nos: &["10230005"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_oil_pressure",
        name: "Health: oil pressure system",
        function_nos: &["10230006"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_esp",
        name: "Health: ESP",
        function_nos: &["10230007"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_engine",
        name: "Health: engine status",
        function_nos: &["10230008"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_coolant",
        name: "Health: engine coolant temperature",
        function_nos: &["10230009"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_charging",
        name: "Health: charging system",
        function_nos: &["10230010"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_braking",
        name: "Health: parking brake",
        function_nos: &["10230011"],
        category: "sensor",
        description: "",
    },
    FeatureSpec {
        key: "health_abs",
        name: "Health: ABS",
        function_nos: &["10230012"],
        category: "sensor",
        description: "",
    },
];

/// Latest-config `functionNo` values that are known but are not command gates
/// or registry features (parent nodes, metadata, names seen in the app).
const OTHER_KNOWN_FUNCTION_NOS: &[&str] = &[
    "1002", "1003", "1010", "1014", "1030", "1031", "1018", "1019", "1024", "1025", "1032", "1033",
    "10020006", "10030012", "10030014", "10030015", "10030016", "10030017", "10020001", "10020002",
    "10020003", "10020004", "10020005", "10030011", "10030013",
];

/// All latest-config `functionNo` values pyBYD knows: the command gates'
/// ([`known_command_function_nos`]), the registry's, and the extra known
/// values. Anything else ends up in `unknown_function_nos`.
pub fn registered_latest_config_function_nos() -> BTreeSet<&'static str> {
    let mut known = known_command_function_nos();
    known.extend(
        FEATURE_REGISTRY
            .iter()
            .flat_map(|spec| spec.function_nos.iter().copied()),
    );
    known.extend(OTHER_KNOWN_FUNCTION_NOS.iter().copied());
    known
}

/// `_normalize_code`: Python's `str.upper()`, then only `A-Z` and `0-9` kept.
pub fn normalize_code(code: &str) -> String {
    code.to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        .collect()
}

/// One capability node from getLatestConfig (top-level or second-level).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LatestConfigFunction {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Feature code, such as `"Electric A/C"`.
    pub code: String,
    /// Feature name (often Chinese).
    pub function_name: String,
    /// BYD `functionNo`, such as `"1015"`.
    pub function_no: String,
    /// Display order.
    pub sort_num: Option<i64>,
    /// Nested second-level nodes.
    pub cf_fixed_second_level_list: Vec<LatestConfigFunction>,
}

impl LatestConfigFunction {
    /// This node followed by all nested second-level nodes, depth first.
    pub fn iter_flat(&self) -> Vec<&LatestConfigFunction> {
        let mut flattened = vec![self];
        for child in &self.cf_fixed_second_level_list {
            flattened.extend(child.iter_flat());
        }
        flattened
    }
}

impl BydModel for LatestConfigFunction {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    // No validator reads the context, so nested nodes get none.
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            code: fields.get("code", String::new(), coerce::string),
            function_name: fields.get("function_name", String::new(), coerce::string),
            function_no: fields.get("function_no", String::new(), coerce::string),
            sort_num: fields.opt("sort_num", coerce::int),
            cf_fixed_second_level_list: fields.get(
                "cf_fixed_second_level_list",
                Vec::new(),
                |value| {
                    coerce::list(value, |item| {
                        LatestConfigFunction::parse(item, &Value::Null)
                    })
                },
            ),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Latest per-vehicle feature configuration from BYD cloud.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VehicleLatestConfig {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Widget configuration id.
    pub widget_config_id: String,
    /// Configuration version.
    pub config_version: Option<i64>,
    /// App configuration version.
    pub app_config_version: Option<i64>,
    /// Style id.
    pub style_id: Option<i64>,
    /// Terminal type.
    pub terminal_type: Option<i64>,
    /// Top-level capability nodes.
    pub cf_fixed_list: Vec<LatestConfigFunction>,
}

impl VehicleLatestConfig {
    /// All capability nodes, including nested second-level ones.
    pub fn iter_functions(&self) -> Vec<&LatestConfigFunction> {
        self.cf_fixed_list
            .iter()
            .flat_map(LatestConfigFunction::iter_flat)
            .collect()
    }
}

impl BydModel for VehicleLatestConfig {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            widget_config_id: fields.get("widget_config_id", String::new(), coerce::string),
            config_version: fields.opt("config_version", coerce::int),
            app_config_version: fields.opt("app_config_version", coerce::int),
            style_id: fields.opt("style_id", coerce::int),
            terminal_type: fields.opt("terminal_type", coerce::int),
            cf_fixed_list: fields.get("cf_fixed_list", Vec::new(), |value| {
                coerce::list(value, |item| LatestConfigFunction::parse(item, context))
            }),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// One row of [`VehicleCapabilities::feature_report`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeatureReportRow {
    /// The capability flag.
    pub key: &'static str,
    /// Human-readable label.
    pub name: &'static str,
    /// `"command"`, `"sensor"` or `"metadata"`.
    pub category: &'static str,
    /// The `functionNo` values that signal the feature.
    pub function_nos: Vec<&'static str>,
    /// The flag's value.
    pub supported: Option<bool>,
}

/// Normalised vehicle capability availability used by integrations.
///
/// Each capability is `Some(true)` if the vehicle's `getLatestConfig` lists
/// one of the required `functionNo` values, `Some(false)` if the lookup ran
/// and none was listed, and `None` only when capability resolution failed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VehicleCapabilities {
    /// Original dict (the latest-config reply for [`Self::from_latest_config`]).
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// Where the flags came from: `"latest_config"` or `"unknown:<reason>"`.
    pub source: String,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub lock: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub unlock: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub climate: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub car_on: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub battery_heat: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub steering_wheel_heat: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub driver_seat_heat: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub driver_seat_ventilation: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub passenger_seat_heat: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub passenger_seat_ventilation: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub rear_left_seat_heat: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub rear_right_seat_heat: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub rear_left_seat_ventilation: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub rear_right_seat_ventilation: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub find_car: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub flash_lights: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub close_windows: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub open_windows: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub location: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub open_trunk: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub close_trunk: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub start_charge: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub stop_charge: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub one_click_shutdown: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub child_presence_detection: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub digital_key: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub nfc_key: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub nfc_key_3c: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub uwb_key: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub mobile_ota: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub scheduled_departure: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub energy_analysis: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub tpms: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub direct_tpms: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub sentry: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub vehicle_health: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_tpms: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_steering: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_srs: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_powertrain: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_battery: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_oil_pressure: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_esp: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_engine: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_coolant: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_charging: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_braking: Option<bool>,
    /// Capability flag (see [`FEATURE_REGISTRY`]).
    pub health_abs: Option<bool>,
    /// Every `functionNo` the vehicle listed, sorted.
    pub function_nos: Vec<String>,
    /// Every feature code, upper-cased and reduced to `A-Z0-9`, sorted.
    pub codes: Vec<String>,
    /// Listed `functionNo` values pyBYD does not know, sorted.
    pub unknown_function_nos: Vec<String>,
}

impl VehicleCapabilities {
    /// The flag of a [`FEATURE_REGISTRY`] key (`None` also for an unknown key).
    pub fn flag(&self, key: &str) -> Option<bool> {
        match key {
            "lock" => self.lock,
            "unlock" => self.unlock,
            "climate" => self.climate,
            "car_on" => self.car_on,
            "battery_heat" => self.battery_heat,
            "steering_wheel_heat" => self.steering_wheel_heat,
            "driver_seat_heat" => self.driver_seat_heat,
            "driver_seat_ventilation" => self.driver_seat_ventilation,
            "passenger_seat_heat" => self.passenger_seat_heat,
            "passenger_seat_ventilation" => self.passenger_seat_ventilation,
            "rear_left_seat_heat" => self.rear_left_seat_heat,
            "rear_right_seat_heat" => self.rear_right_seat_heat,
            "rear_left_seat_ventilation" => self.rear_left_seat_ventilation,
            "rear_right_seat_ventilation" => self.rear_right_seat_ventilation,
            "find_car" => self.find_car,
            "flash_lights" => self.flash_lights,
            "close_windows" => self.close_windows,
            "open_windows" => self.open_windows,
            "location" => self.location,
            "open_trunk" => self.open_trunk,
            "close_trunk" => self.close_trunk,
            "start_charge" => self.start_charge,
            "stop_charge" => self.stop_charge,
            "one_click_shutdown" => self.one_click_shutdown,
            "child_presence_detection" => self.child_presence_detection,
            "digital_key" => self.digital_key,
            "nfc_key" => self.nfc_key,
            "nfc_key_3c" => self.nfc_key_3c,
            "uwb_key" => self.uwb_key,
            "mobile_ota" => self.mobile_ota,
            "scheduled_departure" => self.scheduled_departure,
            "energy_analysis" => self.energy_analysis,
            "tpms" => self.tpms,
            "direct_tpms" => self.direct_tpms,
            "sentry" => self.sentry,
            "vehicle_health" => self.vehicle_health,
            "health_tpms" => self.health_tpms,
            "health_steering" => self.health_steering,
            "health_srs" => self.health_srs,
            "health_powertrain" => self.health_powertrain,
            "health_battery" => self.health_battery,
            "health_oil_pressure" => self.health_oil_pressure,
            "health_esp" => self.health_esp,
            "health_engine" => self.health_engine,
            "health_coolant" => self.health_coolant,
            "health_charging" => self.health_charging,
            "health_braking" => self.health_braking,
            "health_abs" => self.health_abs,
            _ => None,
        }
    }

    /// Builds the capability flags from a latest-config reply.
    ///
    /// Flags are derived from `functionNo` alone, one per [`FEATURE_REGISTRY`]
    /// row; `unknown_function_nos` lists what
    /// [`registered_latest_config_function_nos`] does not know.
    pub fn from_latest_config(vin: &str, latest: &VehicleLatestConfig) -> Self {
        let mut function_nos = BTreeSet::new();
        let mut codes = BTreeSet::new();
        for item in latest.iter_functions() {
            if !item.function_no.is_empty() {
                function_nos.insert(item.function_no.clone());
            }
            if !item.code.is_empty() {
                let normalized = normalize_code(&item.code);
                if !normalized.is_empty() {
                    codes.insert(normalized);
                }
            }
        }
        let known = registered_latest_config_function_nos();
        let unknown: Vec<&String> = function_nos
            .iter()
            .filter(|function_no| !known.contains(function_no.as_str()))
            .collect();
        let mut values = Map::new();
        values.insert("vin".to_owned(), json!(vin));
        values.insert("source".to_owned(), json!("latest_config"));
        for spec in FEATURE_REGISTRY {
            let supported = spec
                .function_nos
                .iter()
                .any(|function_no| function_nos.contains(*function_no));
            values.insert(spec.key.to_owned(), json!(supported));
        }
        values.insert("function_nos".to_owned(), json!(function_nos));
        values.insert("codes".to_owned(), json!(codes));
        values.insert("unknown_function_nos".to_owned(), json!(unknown));
        values.insert("raw".to_owned(), Value::Object(latest.raw.clone()));
        Self::parse(&Value::Object(values), &Value::Null).expect("built from valid values")
    }

    /// An unknown capability map (every flag `None`; strict consumers treat it as unavailable).
    pub fn unknown(vin: &str, reason: &str) -> Self {
        let values = json!({"vin": vin, "source": format!("unknown:{reason}")});
        Self::parse(&values, &Value::Null).expect("built from valid values")
    }

    /// One row per registered feature, with its supported state.
    pub fn feature_report(&self) -> Vec<FeatureReportRow> {
        FEATURE_REGISTRY
            .iter()
            .map(|spec| FeatureReportRow {
                key: spec.key,
                name: spec.name,
                category: spec.category,
                function_nos: spec.function_nos.to_vec(),
                supported: self.flag(spec.key),
            })
            .collect()
    }

    /// `(supported_count, total_count)` across all registry features.
    pub fn supported_summary(&self) -> (usize, usize) {
        let supported = FEATURE_REGISTRY
            .iter()
            .filter(|spec| self.flag(spec.key) == Some(true))
            .count();
        (supported, FEATURE_REGISTRY.len())
    }
}

impl BydModel for VehicleCapabilities {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let strings = |value: &Value| coerce::list(value, coerce::string);
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            source: fields.get("source", "latest_config".to_owned(), coerce::string),
            lock: fields.opt("lock", coerce::boolean),
            unlock: fields.opt("unlock", coerce::boolean),
            climate: fields.opt("climate", coerce::boolean),
            car_on: fields.opt("car_on", coerce::boolean),
            battery_heat: fields.opt("battery_heat", coerce::boolean),
            steering_wheel_heat: fields.opt("steering_wheel_heat", coerce::boolean),
            driver_seat_heat: fields.opt("driver_seat_heat", coerce::boolean),
            driver_seat_ventilation: fields.opt("driver_seat_ventilation", coerce::boolean),
            passenger_seat_heat: fields.opt("passenger_seat_heat", coerce::boolean),
            passenger_seat_ventilation: fields.opt("passenger_seat_ventilation", coerce::boolean),
            rear_left_seat_heat: fields.opt("rear_left_seat_heat", coerce::boolean),
            rear_right_seat_heat: fields.opt("rear_right_seat_heat", coerce::boolean),
            rear_left_seat_ventilation: fields.opt("rear_left_seat_ventilation", coerce::boolean),
            rear_right_seat_ventilation: fields.opt("rear_right_seat_ventilation", coerce::boolean),
            find_car: fields.opt("find_car", coerce::boolean),
            flash_lights: fields.opt("flash_lights", coerce::boolean),
            close_windows: fields.opt("close_windows", coerce::boolean),
            open_windows: fields.opt("open_windows", coerce::boolean),
            location: fields.opt("location", coerce::boolean),
            open_trunk: fields.opt("open_trunk", coerce::boolean),
            close_trunk: fields.opt("close_trunk", coerce::boolean),
            start_charge: fields.opt("start_charge", coerce::boolean),
            stop_charge: fields.opt("stop_charge", coerce::boolean),
            one_click_shutdown: fields.opt("one_click_shutdown", coerce::boolean),
            child_presence_detection: fields.opt("child_presence_detection", coerce::boolean),
            digital_key: fields.opt("digital_key", coerce::boolean),
            nfc_key: fields.opt("nfc_key", coerce::boolean),
            nfc_key_3c: fields.opt("nfc_key_3c", coerce::boolean),
            uwb_key: fields.opt("uwb_key", coerce::boolean),
            mobile_ota: fields.opt("mobile_ota", coerce::boolean),
            scheduled_departure: fields.opt("scheduled_departure", coerce::boolean),
            energy_analysis: fields.opt("energy_analysis", coerce::boolean),
            tpms: fields.opt("tpms", coerce::boolean),
            direct_tpms: fields.opt("direct_tpms", coerce::boolean),
            sentry: fields.opt("sentry", coerce::boolean),
            vehicle_health: fields.opt("vehicle_health", coerce::boolean),
            health_tpms: fields.opt("health_tpms", coerce::boolean),
            health_steering: fields.opt("health_steering", coerce::boolean),
            health_srs: fields.opt("health_srs", coerce::boolean),
            health_powertrain: fields.opt("health_powertrain", coerce::boolean),
            health_battery: fields.opt("health_battery", coerce::boolean),
            health_oil_pressure: fields.opt("health_oil_pressure", coerce::boolean),
            health_esp: fields.opt("health_esp", coerce::boolean),
            health_engine: fields.opt("health_engine", coerce::boolean),
            health_coolant: fields.opt("health_coolant", coerce::boolean),
            health_charging: fields.opt("health_charging", coerce::boolean),
            health_braking: fields.opt("health_braking", coerce::boolean),
            health_abs: fields.opt("health_abs", coerce::boolean),
            function_nos: fields.get("function_nos", Vec::new(), strings),
            codes: fields.get("codes", Vec::new(), strings),
            unknown_function_nos: fields.get("unknown_function_nos", Vec::new(), strings),
        };
        fields.finish()?;
        Ok(model)
    }
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

fn dumps<'a, M: BydModel + 'a>(models: impl IntoIterator<Item = &'a M>) -> Value {
    Value::Array(models.into_iter().map(BydModel::dump).collect())
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    let context = args.value("context");
    Some(match name {
        "models.latest_config.LatestConfigFunction" => {
            base::model_call::<LatestConfigFunction>(args.0)
        }
        "models.latest_config.VehicleLatestConfig" => {
            base::model_call::<VehicleLatestConfig>(args.0)
        }
        "models.latest_config.VehicleCapabilities" => {
            base::model_call::<VehicleCapabilities>(args.0)
        }
        "models.latest_config.registered_latest_config_function_nos" => {
            Ok(json!(registered_latest_config_function_nos()))
        }
        "models.latest_config.FEATURE_REGISTRY" => Ok(json!(FEATURE_REGISTRY)),
        "models.latest_config.normalize_code" => {
            args.str("code").map(|code| json!(normalize_code(code)))
        }
        "models.latest_config.LatestConfigFunction.iter_flat" => {
            LatestConfigFunction::parse(args.value("raw"), context)
                .map(|node| dumps(node.iter_flat()))
                .map_err(ParityError::from)
        }
        "models.latest_config.VehicleLatestConfig.iter_functions" => {
            VehicleLatestConfig::parse(args.value("raw"), context)
                .map(|latest| dumps(latest.iter_functions()))
                .map_err(ParityError::from)
        }
        "models.latest_config.VehicleCapabilities.from_latest_config" => {
            args.str("vin").and_then(|vin| {
                let latest = VehicleLatestConfig::parse(args.value("latest"), &Value::Null)?;
                let capabilities = VehicleCapabilities::from_latest_config(vin, &latest);
                Ok(json!({
                    "dump": capabilities.dump(),
                    "feature_report": capabilities.feature_report(),
                    "supported_summary": capabilities.supported_summary(),
                }))
            })
        }
        "models.latest_config.VehicleCapabilities.unknown" => {
            match (args.str("vin"), args.str("reason")) {
                (Ok(vin), Ok(reason)) => Ok(VehicleCapabilities::unknown(vin, reason).dump()),
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        }
        "models.latest_config.VehicleCapabilities.feature_report" => {
            VehicleCapabilities::parse(args.value("raw"), context)
                .map(|capabilities| {
                    json!({
                        "feature_report": capabilities.feature_report(),
                        "supported_summary": capabilities.supported_summary(),
                    })
                })
                .map_err(ParityError::from)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_and_flags_line_up() {
        let capabilities = VehicleCapabilities::unknown("TESTVIN0000000001", "unavailable");
        assert_eq!(capabilities.source, "unknown:unavailable");
        for spec in FEATURE_REGISTRY {
            assert_eq!(capabilities.flag(spec.key), None, "{}", spec.key);
        }
        let all: Map<String, Value> = FEATURE_REGISTRY
            .iter()
            .map(|spec| (spec.key.to_owned(), json!(true)))
            .collect();
        let capabilities = VehicleCapabilities::parse(&Value::Object(all), &Value::Null).unwrap();
        for spec in FEATURE_REGISTRY {
            assert_eq!(capabilities.flag(spec.key), Some(true), "{}", spec.key);
        }
        assert_eq!(capabilities.flag("nothing"), None);
        assert_eq!(
            capabilities.supported_summary(),
            (FEATURE_REGISTRY.len(), FEATURE_REGISTRY.len())
        );
    }

    #[test]
    fn bad_parity_arguments() {
        let call = |name: &str, args: Value| parity_call(name, &args).unwrap().unwrap_err().kind;
        assert_eq!(
            call("models.latest_config.normalize_code", json!({})),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.latest_config.VehicleCapabilities.unknown",
                json!({"vin": "x"})
            ),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.latest_config.VehicleCapabilities.from_latest_config",
                json!({"latest": {}})
            ),
            "BadArguments"
        );
        assert!(parity_call("models.latest_config.nothing", &json!({})).is_none());
    }
}
