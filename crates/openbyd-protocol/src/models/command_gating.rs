//! Command gating rules and verdicts for remote-control preflight checks (`pybyd/models/command_gating.py`).
//!
//! This is the command-level source of truth: each [`RemoteCommand`] maps to
//! the `functionNo` values (from `getLatestConfig`) that allow it. Gating is
//! strict and `functionNo`-only: a gate is supported when any of its values
//! is present. `OPENWINDOW` also needs `openWindowLearnInfo > 0` in the
//! vehicle's `vehicleFunLearnInfo` when the caller has that data.
//!
//! The client calls [`evaluate_command_gate`] before sending a command and
//! refuses it when the verdict is not [`CommandGateVerdict::supported`].

use std::collections::BTreeSet;
use std::sync::LazyLock;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields};
use super::control::RemoteCommand;
use super::latest_config::VehicleCapabilities;
use crate::coerce::{self, ValidationError};
use crate::parity::{Args, Outcome, ParityError};
use crate::pyjson::py_str;

/// Canonical command gate definition.
///
/// `required_function_nos` is the coarse gate from `cfFixedList`.
/// `required_learn_info_keys` is the optional finer gate from
/// `getVehicles.vehicleFunLearnInfo`: when set, at least one listed key must
/// have a positive value for the rule to be supported.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandGateRule {
    /// Original dict.
    pub raw: Map<String, Value>,
    /// Gate name, such as `"lock"` or `"seat_driver"`.
    pub gate_id: String,
    /// The command the gate applies to.
    pub command: RemoteCommand,
    /// Any one of these `functionNo` values allows the command.
    pub required_function_nos: Vec<String>,
    /// When not empty, one of these learn-info keys must be positive too.
    pub required_learn_info_keys: Vec<String>,
}

impl BydModel for CommandGateRule {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let mut fields = Fields::cleaned(raw, &[])?;
        let strings = |value: &Value| coerce::list(value, coerce::string);
        let raw = fields.raw();
        let gate_id = fields.required("gate_id", coerce::string);
        let command = fields.required("command", |value| coerce::str_enum(value).map(Some));
        let required_function_nos = fields.get("required_function_nos", Vec::new(), strings);
        let required_learn_info_keys = fields.get("required_learn_info_keys", Vec::new(), strings);
        fields.finish()?;
        Ok(Self {
            raw,
            gate_id,
            command: command.expect("a required field is set once validation passed"),
            required_function_nos,
            required_learn_info_keys,
        })
    }
}

/// Evaluated support verdict for a command gate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandGateVerdict {
    /// Original dict.
    pub raw: Map<String, Value>,
    /// The command evaluated.
    pub command: RemoteCommand,
    /// The gate that decided (`"seat_target"` when a seat command named no known seat).
    pub gate_id: String,
    /// Whether the command may be sent.
    pub supported: bool,
    /// `"supported"`, `"function_no_missing"`, `"learn_info_missing"` or `"seat_target_unknown"`.
    pub reason: String,
    /// The gate's `functionNo` values, any one of which allows the command.
    pub required_function_nos: Vec<String>,
    /// The required values the vehicle has, sorted.
    pub matched_function_nos: Vec<String>,
    /// The required values the vehicle lacks, sorted.
    pub counterpart_function_nos: Vec<String>,
    /// The gate's learn-info keys.
    pub required_learn_info_keys: Vec<String>,
    /// The learn-info keys with a positive value, sorted.
    pub matched_learn_info_keys: Vec<String>,
}

impl BydModel for CommandGateVerdict {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let mut fields = Fields::cleaned(raw, &[])?;
        let strings = |value: &Value| coerce::list(value, coerce::string);
        let raw = fields.raw();
        let command = fields.required("command", |value| coerce::str_enum(value).map(Some));
        let gate_id = fields.required("gate_id", coerce::string);
        let supported = fields.required("supported", coerce::boolean);
        let reason = fields.required("reason", coerce::string);
        let required_function_nos = fields.get("required_function_nos", Vec::new(), strings);
        let matched_function_nos = fields.get("matched_function_nos", Vec::new(), strings);
        let counterpart_function_nos = fields.get("counterpart_function_nos", Vec::new(), strings);
        let required_learn_info_keys = fields.get("required_learn_info_keys", Vec::new(), strings);
        let matched_learn_info_keys = fields.get("matched_learn_info_keys", Vec::new(), strings);
        fields.finish()?;
        Ok(Self {
            raw,
            command: command.expect("a required field is set once validation passed"),
            gate_id,
            supported,
            reason,
            required_function_nos,
            matched_function_nos,
            counterpart_function_nos,
            required_learn_info_keys,
            matched_learn_info_keys,
        })
    }
}

/// `_COMMAND_GATE_RULES` as data: gate id, command, `functionNo` values, learn-info keys.
type RuleRow = (
    &'static str,
    RemoteCommand,
    &'static [&'static str],
    &'static [&'static str],
);

const CLIMATE_FUNCTION_NOS: &[&str] = &["1001", "10300001", "1015"];

/// The canonical command → `functionNo` table, in pyBYD's order.
const RULE_ROWS: &[RuleRow] = &[
    ("lock", RemoteCommand::Lock, &["1005"], &[]),
    ("unlock", RemoteCommand::Unlock, &["1006"], &[]),
    (
        "climate",
        RemoteCommand::StartClimate,
        CLIMATE_FUNCTION_NOS,
        &[],
    ),
    (
        "climate",
        RemoteCommand::StopClimate,
        CLIMATE_FUNCTION_NOS,
        &[],
    ),
    (
        "climate_schedule",
        RemoteCommand::ScheduleClimate,
        CLIMATE_FUNCTION_NOS,
        &[],
    ),
    ("find_car", RemoteCommand::FindCar, &["1007"], &[]),
    ("flash_lights", RemoteCommand::FlashLights, &["1008"], &[]),
    ("close_windows", RemoteCommand::CloseWindows, &["1026"], &[]),
    // 1026 is listed by cars that can only close remotely too; only
    // openWindowLearnInfo = 1 marks the ones that can open (pyBYD #47, #20).
    (
        "open_windows",
        RemoteCommand::OpenWindows,
        &["1026"],
        &["openWindowLearnInfo"],
    ),
    ("open_trunk", RemoteCommand::OpenTrunk, &["1020"], &[]),
    ("close_trunk", RemoteCommand::CloseTrunk, &["1021"], &[]),
    (
        "seat_driver",
        RemoteCommand::SeatClimate,
        &["10030001", "10030002", "10300003"],
        &[],
    ),
    (
        "seat_passenger",
        RemoteCommand::SeatClimate,
        &["10030004", "10030005", "10300003"],
        &[],
    ),
    (
        "seat_rear_left",
        RemoteCommand::SeatClimate,
        &["10030006", "10030007"],
        &[],
    ),
    (
        "seat_rear_right",
        RemoteCommand::SeatClimate,
        &["10030008", "10030009"],
        &[],
    ),
    (
        "steering_wheel_heat",
        RemoteCommand::SeatClimate,
        &["10030010", "10300004"],
        &[],
    ),
    (
        "battery_heat",
        RemoteCommand::BatteryHeat,
        &["10300002", "10100001"],
        &[],
    ),
    // 1012 is the app's RESERVATIONCHARGING flag, which also gates /control/smartCharge/*.
    ("start_charge", RemoteCommand::StartCharge, &["1012"], &[]),
    ("stop_charge", RemoteCommand::StopCharge, &["1012"], &[]),
];

/// `_SEAT_GATE_BY_CHAIR_TYPE`: a seat command must name its target with `chairType`.
fn seat_gate_for_chair_type(chair_type: &str) -> Option<&'static str> {
    Some(match chair_type {
        "1" => "seat_driver",
        "2" => "seat_passenger",
        "3" => "seat_rear_left",
        "4" => "seat_rear_right",
        "5" => "steering_wheel_heat",
        _ => return None,
    })
}

const SEAT_GATES: [&str; 5] = [
    "seat_driver",
    "seat_passenger",
    "seat_rear_left",
    "seat_rear_right",
    "steering_wheel_heat",
];

static RULES: LazyLock<Vec<CommandGateRule>> = LazyLock::new(|| {
    RULE_ROWS
        .iter()
        .map(|&(gate_id, command, function_nos, learn_info_keys)| {
            let mut values = json!({
                "gateId": gate_id,
                "command": command.as_str(),
                "requiredFunctionNos": function_nos,
            });
            if !learn_info_keys.is_empty() {
                values["requiredLearnInfoKeys"] = json!(learn_info_keys);
            }
            CommandGateRule::parse(&values, &Value::Null).expect("the rule table is valid")
        })
        .collect()
});

/// The canonical command gate rules.
pub fn command_gate_rules() -> &'static [CommandGateRule] {
    &RULES
}

/// Every `functionNo` referenced by the command rules.
///
/// The latest-config registry includes these, so a new command rule takes
/// part in the unknown-`functionNo` check automatically.
pub fn known_command_function_nos() -> BTreeSet<&'static str> {
    RULE_ROWS
        .iter()
        .flat_map(|(_, _, function_nos, _)| function_nos.iter().copied())
        .filter(|function_no| !function_no.is_empty())
        .collect()
}

/// `(learn_info.get(key) or 0) > 0` for one learn-info value.
///
/// Missing, `0`, `-1` and other non-positive numbers count as not present.
/// A non-empty string, list or dict cannot be compared with `0`: Python
/// raises `TypeError`, and so does this.
fn learn_info_positive(value: Option<&Value>) -> Result<bool, ValidationError> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(Value::Number(number)) => Ok(if let Some(int) = number.as_i64() {
            int > 0
        } else {
            number.as_u64().is_some() || number.as_f64().is_some_and(|float| float > 0.0)
        }),
        Some(Value::String(text)) if text.is_empty() => Ok(false),
        Some(Value::Array(items)) if items.is_empty() => Ok(false),
        Some(Value::Object(entries)) if entries.is_empty() => Ok(false),
        Some(_) => Err(ValidationError::raised("TypeError", "not_comparable")),
    }
}

/// `_learn_info_match`: the required keys whose learn-info value is positive, sorted.
fn learn_info_match(
    learn_info: Option<&Map<String, Value>>,
    required_keys: &[String],
) -> Result<Vec<String>, ValidationError> {
    let Some(learn_info) = learn_info.filter(|_| !required_keys.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut matched = Vec::new();
    for key in required_keys {
        if learn_info_positive(learn_info.get(key))? {
            matched.push(key.clone());
        }
    }
    matched.sort();
    Ok(matched)
}

/// `_evaluate_rule`: one rule against the vehicle's `functionNo` set and learn info.
///
/// The learn-info gate applies only when the rule declares keys *and* the
/// caller passed learn info; without learn info such a rule passes on its
/// `functionNo` alone.
pub fn evaluate_rule(
    rule: &CommandGateRule,
    function_nos: &BTreeSet<&str>,
    learn_info: Option<&Map<String, Value>>,
) -> Result<CommandGateVerdict, ValidationError> {
    let required = &rule.required_function_nos;
    let mut matched: Vec<&String> = required
        .iter()
        .filter(|function_no| function_nos.contains(function_no.as_str()))
        .collect();
    matched.sort();
    let mut counterpart: Vec<&String> = required
        .iter()
        .filter(|function_no| !matched.contains(function_no))
        .collect();
    counterpart.sort();
    let function_no_supported = !matched.is_empty();
    let matched_learn_info_keys = learn_info_match(learn_info, &rule.required_learn_info_keys)?;
    let learn_info_supported = rule.required_learn_info_keys.is_empty()
        || learn_info.is_none()
        || !matched_learn_info_keys.is_empty();
    let reason = if !function_no_supported {
        "function_no_missing"
    } else if !learn_info_supported {
        "learn_info_missing"
    } else {
        "supported"
    };
    let values = json!({
        "command": rule.command.as_str(),
        "gateId": rule.gate_id,
        "supported": function_no_supported && learn_info_supported,
        "reason": reason,
        "requiredFunctionNos": required,
        "matchedFunctionNos": matched,
        "counterpartFunctionNos": counterpart,
        "requiredLearnInfoKeys": rule.required_learn_info_keys,
        "matchedLearnInfoKeys": matched_learn_info_keys,
    });
    CommandGateVerdict::parse(&values, &Value::Null)
}

fn function_no_set(capabilities: &VehicleCapabilities) -> BTreeSet<&str> {
    capabilities
        .function_nos
        .iter()
        .map(String::as_str)
        .collect()
}

/// Every canonical gate, for reporting and diagnostics.
pub fn evaluate_all_command_gates(
    capabilities: &VehicleCapabilities,
    learn_info: Option<&Map<String, Value>>,
) -> Result<Vec<CommandGateVerdict>, ValidationError> {
    let function_nos = function_no_set(capabilities);
    command_gate_rules()
        .iter()
        .map(|rule| evaluate_rule(rule, &function_nos, learn_info))
        .collect()
}

/// Whether `command` may be sent to a vehicle with these capabilities.
///
/// A seat command ([`RemoteCommand::SeatClimate`]) is judged by the gate of
/// the seat `control_params["chairType"]` names (`"1"` driver, `"2"`
/// passenger, `"3"`/`"4"` rear left/right, `"5"` steering wheel; a
/// non-string value is compared as Python's `str()` of it); without a known
/// seat it is refused with `"seat_target_unknown"`. `learn_info` is the
/// vehicle's `vehicleFunLearnInfo`, or `None` when unknown.
///
/// Fails only when a learn-info value the gate reads is a non-empty string,
/// list or dict (pyBYD raises `TypeError` comparing it with 0).
pub fn evaluate_command_gate(
    command: RemoteCommand,
    capabilities: &VehicleCapabilities,
    control_params: Option<&Map<String, Value>>,
    learn_info: Option<&Map<String, Value>>,
) -> Result<CommandGateVerdict, ValidationError> {
    let function_nos = function_no_set(capabilities);
    let mut command_rules = command_gate_rules()
        .iter()
        .filter(|rule| rule.command == command);

    if command == RemoteCommand::SeatClimate {
        let chair_type = match control_params.and_then(|params| params.get("chairType")) {
            None | Some(Value::Null) => String::new(),
            Some(value) => py_str(value),
        };
        let Some(gate_id) = seat_gate_for_chair_type(&chair_type) else {
            let required: BTreeSet<&String> = command_rules
                .filter(|rule| SEAT_GATES.contains(&rule.gate_id.as_str()))
                .flat_map(|rule| &rule.required_function_nos)
                .collect();
            let values = json!({
                "command": command.as_str(),
                "gateId": "seat_target",
                "supported": false,
                "reason": "seat_target_unknown",
                "requiredFunctionNos": required,
                "matchedFunctionNos": [],
                "counterpartFunctionNos": required,
            });
            return CommandGateVerdict::parse(&values, &Value::Null);
        };
        let rule = command_rules
            .find(|rule| rule.gate_id == gate_id)
            .expect("every seat gate has a rule");
        return evaluate_rule(rule, &function_nos, learn_info);
    }

    let rule = command_rules.next().expect("every command has a rule");
    evaluate_rule(rule, &function_nos, learn_info)
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

/// A mapping argument: an object or null.
fn mapping_arg<'a>(
    args: &Args<'a>,
    key: &str,
) -> Result<Option<&'a Map<String, Value>>, ParityError> {
    match args.value(key) {
        Value::Null => Ok(None),
        Value::Object(entries) => Ok(Some(entries)),
        _ => Err(ParityError::bad_arguments(format!(
            "{key} must be an object or null"
        ))),
    }
}

/// `RemoteCommand(value)`, which raises `ValueError` for anything else.
fn command_arg(args: &Args<'_>) -> Result<RemoteCommand, ParityError> {
    args.value("command")
        .as_str()
        .and_then(RemoteCommand::from_value)
        .ok_or_else(|| ParityError::new("ValueError", "not a valid RemoteCommand"))
}

fn capabilities_arg(args: &Args<'_>) -> Result<VehicleCapabilities, ParityError> {
    VehicleCapabilities::parse(args.value("capabilities"), &Value::Null).map_err(ParityError::from)
}

fn dump_all(verdicts: Vec<CommandGateVerdict>) -> Value {
    Value::Array(verdicts.iter().map(BydModel::dump).collect())
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "models.command_gating.CommandGateRule" => base::model_call::<CommandGateRule>(args.0),
        "models.command_gating.CommandGateVerdict" => {
            base::model_call::<CommandGateVerdict>(args.0)
        }
        "models.command_gating.command_gate_rules" => Ok(Value::Array(
            command_gate_rules().iter().map(BydModel::dump).collect(),
        )),
        "models.command_gating.known_command_function_nos" => {
            Ok(json!(known_command_function_nos()))
        }
        "models.command_gating.evaluate_command_gate" => (|| {
            let command = command_arg(&args)?;
            let capabilities = capabilities_arg(&args)?;
            let control_params = mapping_arg(&args, "control_params")?;
            let learn_info = mapping_arg(&args, "learn_info")?;
            let verdict =
                evaluate_command_gate(command, &capabilities, control_params, learn_info)?;
            Ok(verdict.dump())
        })(),
        "models.command_gating.evaluate_all_command_gates" => (|| {
            let capabilities = capabilities_arg(&args)?;
            let learn_info = mapping_arg(&args, "learn_info")?;
            Ok(dump_all(evaluate_all_command_gates(
                &capabilities,
                learn_info,
            )?))
        })(),
        "models.command_gating.evaluate_rule" => (|| {
            let rule = CommandGateRule::parse(args.value("rule"), &Value::Null)?;
            let function_nos: BTreeSet<&str> = match args.value("function_nos") {
                Value::Array(items) => items
                    .iter()
                    .map(|item| {
                        item.as_str().ok_or_else(|| {
                            ParityError::bad_arguments("function_nos must be strings")
                        })
                    })
                    .collect::<Result<_, _>>()?,
                _ => return Err(ParityError::bad_arguments("function_nos must be a list")),
            };
            let learn_info = mapping_arg(&args, "learn_info")?;
            Ok(evaluate_rule(&rule, &function_nos, learn_info)?.dump())
        })(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities(function_nos: &[&str]) -> VehicleCapabilities {
        VehicleCapabilities::parse(&json!({"functionNos": function_nos}), &Value::Null).unwrap()
    }

    #[test]
    fn every_command_has_a_gate() {
        let empty = capabilities(&[]);
        for command in RemoteCommand::ALL {
            let verdict = evaluate_command_gate(command, &empty, None, None).unwrap();
            assert!(!verdict.supported, "{command:?}");
        }
        let all: Vec<&str> = known_command_function_nos().into_iter().collect();
        let full = capabilities(&all);
        let seat = json!({"chairType": 5});
        for command in RemoteCommand::ALL {
            let verdict = evaluate_command_gate(command, &full, seat.as_object(), None).unwrap();
            assert!(verdict.supported, "{command:?}");
        }
        assert_eq!(
            evaluate_all_command_gates(&full, None).unwrap().len(),
            RULE_ROWS.len()
        );
    }

    #[test]
    fn open_windows_needs_learn_info() {
        let caps = capabilities(&["1026"]);
        let check = |learn_info: Value| {
            evaluate_command_gate(
                RemoteCommand::OpenWindows,
                &caps,
                None,
                learn_info.as_object(),
            )
            .map(|verdict| verdict.reason)
        };
        assert_eq!(
            check(json!({"openWindowLearnInfo": 1})).unwrap(),
            "supported"
        );
        assert_eq!(
            check(json!({"openWindowLearnInfo": 0, "openWindow499LearnInfo": 1})).unwrap(),
            "learn_info_missing"
        );
        assert_eq!(
            check(json!({"openWindowLearnInfo": u64::MAX})).unwrap(),
            "supported"
        );
        assert_eq!(
            check(json!({"openWindowLearnInfo": 0.5})).unwrap(),
            "supported"
        );
        assert_eq!(
            check(json!({"openWindowLearnInfo": ""})).unwrap(),
            "learn_info_missing"
        );
        assert_eq!(
            check(json!({"openWindowLearnInfo": []})).unwrap(),
            "learn_info_missing"
        );
        assert_eq!(
            check(json!({"openWindowLearnInfo": {}})).unwrap(),
            "learn_info_missing"
        );
        assert_eq!(
            check(json!({"openWindowLearnInfo": "1"}))
                .unwrap_err()
                .exception,
            "TypeError"
        );
        assert_eq!(check(Value::Null).unwrap(), "supported");
    }

    #[test]
    fn bad_parity_arguments() {
        let call = |name: &str, args: Value| parity_call(name, &args).unwrap().unwrap_err().kind;
        assert_eq!(
            call(
                "models.command_gating.evaluate_command_gate",
                json!({"command": "LOCKDOOR", "capabilities": {}, "learn_info": 1})
            ),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.command_gating.evaluate_rule",
                json!({"rule": {"gateId": "x", "command": "LOCKDOOR"}, "function_nos": 1})
            ),
            "BadArguments"
        );
        assert_eq!(
            call(
                "models.command_gating.evaluate_rule",
                json!({"rule": {"gateId": "x", "command": "LOCKDOOR"}, "function_nos": [1]})
            ),
            "BadArguments"
        );
        assert!(parity_call("models.command_gating.nothing", &json!({})).is_none());
    }
}
