//! Message center (account inbox) models (`pybyd/models/message_center.py`).
//!
//! `/app/messageCenter/getUnReadMessageList` returns a bare list of
//! messages (charging started/stopped, status alerts, ...), e.g.:
//!
//! ```json
//! [{"pushId": 211902416, "pushTime": 1790966443550, "messageGroup": 1,
//!   "titleId": 1, "title": "Empezar carga",
//!   "content": "2026-10-02 20:40,BYD ATTO 3 EVO El coche comience a cargar.",
//!   "jumpUrl": ""}]
//! ```
//!
//! The EU host identifies messages by `pushId`; `messageId` is accepted
//! as a fallback.

use std::borrow::Cow;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::base::{self, BydModel, Fields, Plan, PyDateTime, aliased};
use crate::coerce::{self, IntOrStr, ValidationError};
use crate::parity::{Args, Outcome, ParityError};

/// Keys under which the message list may be wrapped in a dict response.
const LIST_KEYS: [&str; 4] = ["list", "messageList", "rows", "records"];

/// Inbox category, the value is the `messageGroup` sent to the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationCategory {
    /// Vehicle messages (`1`).
    Vehicle = 1,
    /// System messages (`2`).
    System = 2,
}

impl NotificationCategory {
    /// The member's name as Python spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Vehicle => "VEHICLE",
            Self::System => "SYSTEM",
        }
    }

    /// `NotificationCategory(value)`: the member for a JSON value, or `None`
    /// where Python raises `ValueError`. Python looks members up by equality,
    /// so `1.0` and `true` find `VEHICLE` just as `1` does.
    pub fn from_json(value: &Value) -> Option<Self> {
        let number = match value {
            Value::Bool(flag) => f64::from(u8::from(*flag)),
            Value::Number(number) => number.as_f64()?,
            _ => return None,
        };
        <Self as coerce::IntEnum>::MEMBERS
            .iter()
            .copied()
            .find(|member| (coerce::IntEnum::value(*member) as f64) == number)
    }
}

impl coerce::IntEnum for NotificationCategory {
    const MEMBERS: &'static [Self] = &[Self::Vehicle, Self::System];
    const MISSING: Option<Self> = None;
    fn value(self) -> i64 {
        self as i64
    }
}

/// A single message from the account inbox.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VehicleMsgItemInfo {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// Message id, from `pushId` or `messageId`; digit strings become integers.
    pub message_id: IntOrStr,
    /// Message title, from `title` or `messageTitle`.
    pub title: Option<String>,
    /// Title id.
    pub title_id: Option<i64>,
    /// Message body, from `content` or `messageContent`.
    pub content: Option<String>,
    /// When the message was pushed.
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub push_time: Option<PyDateTime>,
    /// Inbox category (see [`NotificationCategory`]).
    pub message_group: Option<i64>,
    /// Link the message points at.
    pub jump_url: Option<String>,
}

/// `_normalise_id`: a string of decimal digits becomes an `int`.
fn normalise_id(value: &Value) -> Result<Value, ValidationError> {
    let Value::String(text) = value else {
        return Ok(value.clone());
    };
    if text.is_empty() || !text.chars().all(|c| coerce::py_decimal(c).is_some()) {
        return Ok(value.clone());
    }
    let digits: String = text
        .chars()
        .filter_map(coerce::py_decimal)
        .filter_map(|digit| char::from_digit(digit, 10))
        .collect();
    // CPython refuses to turn more than 4300 digits into an int.
    if digits.chars().count() > 4300 {
        return Err(ValidationError::new("value_error"));
    }
    match digits.parse::<u64>() {
        Ok(number) => Ok(json!(number)),
        // Python holds an arbitrarily large int; openbyd's integers are i64.
        Err(_) => Err(ValidationError::unsupported_int()),
    }
}

impl BydModel for VehicleMsgItemInfo {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            // `required` needs a `Default`; an error is reported by `finish`, so the 0 never escapes.
            message_id: fields
                .required(aliased("message_id", &["pushId", "messageId"]), |value| {
                    coerce::int_or_str(&normalise_id(value)?).map(Some)
                })
                .unwrap_or(IntOrStr::Int(0)),
            title: fields.opt(aliased("title", &["title", "messageTitle"]), coerce::string),
            title_id: fields.opt("title_id", coerce::int),
            content: fields.opt(
                aliased("content", &["content", "messageContent"]),
                coerce::string,
            ),
            push_time: fields.get("push_time", None, base::byd_timestamp),
            message_group: fields.opt("message_group", coerce::int),
            jump_url: fields.opt("jump_url", coerce::string),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Decoded response of `/app/messageCenter/getUnReadMessageList`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VehicleMsgListResponse {
    /// Original API response dict.
    pub raw: Map<String, Value>,
    /// The inbox messages.
    pub messages: Vec<VehicleMsgItemInfo>,
}

/// `_unwrap_list`: accept a bare list (observed) or a dict wrapping the list.
fn unwrap_list(values: &Value) -> Cow<'_, Value> {
    match values {
        Value::Array(items) => Cow::Owned(json!({"messages": items, "raw": {"list": items}})),
        Value::Object(map) if !map.contains_key("messages") => {
            for key in LIST_KEYS {
                if let Some(items @ Value::Array(_)) = map.get(key) {
                    let mut merged = map.clone();
                    merged.insert("messages".to_owned(), items.clone());
                    return Cow::Owned(Value::Object(merged));
                }
            }
            Cow::Borrowed(values)
        }
        _ => Cow::Borrowed(values),
    }
}

impl BydModel for VehicleMsgListResponse {
    fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        let values = unwrap_list(raw);
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, &values, &[])?;
        let model = Self {
            raw: fields.raw(),
            messages: fields.get("messages", Vec::new(), |value| {
                coerce::list(value, |item| VehicleMsgItemInfo::parse(item, context))
            }),
        };
        fields.finish()?;
        Ok(model)
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.message_center.NotificationCategory" => {
            match NotificationCategory::from_json(Args(args).value("value")) {
                Some(member) => Ok(json!({"name": member.name(), "value": member as i64})),
                None => Err(ParityError::new("ValueError", "not a NotificationCategory")),
            }
        }
        "models.message_center.VehicleMsgItemInfo" => base::model_call::<VehicleMsgItemInfo>(args),
        "models.message_center.VehicleMsgListResponse" => {
            base::model_call::<VehicleMsgListResponse>(args)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_are_found_by_equality() {
        use NotificationCategory::{System, Vehicle};
        assert_eq!(NotificationCategory::from_json(&json!(1)), Some(Vehicle));
        assert_eq!(NotificationCategory::from_json(&json!(2.0)), Some(System));
        assert_eq!(NotificationCategory::from_json(&json!(true)), Some(Vehicle));
        assert_eq!(NotificationCategory::from_json(&json!(false)), None);
        assert_eq!(NotificationCategory::from_json(&json!("1")), None);
        assert_eq!(NotificationCategory::from_json(&json!(3)), None);
        assert_eq!(Vehicle.name(), "VEHICLE");
        assert_eq!(System.name(), "SYSTEM");
    }

    #[test]
    fn digit_ids_become_integers() {
        assert_eq!(normalise_id(&json!("102")).unwrap(), json!(102));
        assert_eq!(normalise_id(&json!("١٢")).unwrap(), json!(12));
        assert_eq!(normalise_id(&json!(" 12")).unwrap(), json!(" 12"));
        assert_eq!(normalise_id(&json!("")).unwrap(), json!(""));
        assert_eq!(normalise_id(&json!(5)).unwrap(), json!(5));
        let huge = "9".repeat(30);
        assert_eq!(
            normalise_id(&json!(huge)).unwrap_err().exception,
            "Unsupported"
        );
        let absurd = "9".repeat(4301);
        assert!(normalise_id(&json!(absurd)).unwrap_err().is_validation());
    }

    #[test]
    fn a_message_needs_an_id() {
        let error = VehicleMsgItemInfo::parse(&json!({"title": "x"}), &Value::Null);
        assert!(error.unwrap_err().is_validation());
    }

    #[test]
    fn a_dict_with_messages_is_not_unwrapped() {
        let value = json!({"messages": [], "list": [{"pushId": 1}]});
        let response = VehicleMsgListResponse::parse(&value, &Value::Null).unwrap();
        assert!(response.messages.is_empty());
    }

    #[test]
    fn a_dict_without_a_list_key_is_left_alone() {
        let response =
            VehicleMsgListResponse::parse(&json!({"list": "x", "rows": 5}), &Value::Null).unwrap();
        assert!(response.messages.is_empty());
    }
}
