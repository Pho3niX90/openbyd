//! The vehicle list and guest grant endpoints (`pybyd/_api/vehicle.py`),
//! and the helpers the read flows of this crate share.
//!
//! - `/app/account/getAllListByUserId`
//! - `/vehicle/empower/query/empowerDetailByVin`

use std::future::Future;

use openbyd_protocol::coerce::{IntEnum, ValidationError};
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::vehicle::{EmpowerDetail, EnergyType, Vehicle};
use openbyd_protocol::parity::{Outcome, ParityError};
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_token_json,
};
use crate::env::Env;
use crate::scripted::{Scenario, describe};

const ENDPOINT: &str = "/app/account/getAllListByUserId";
/// Where a shared guest's grant is read.
pub const EMPOWER_DETAIL_ENDPOINT: &str = "/vehicle/empower/query/empowerDetailByVin";

/// Exceptions a pyBYD model validator lets escape pydantic. They have no
/// [`ErrorKind`]; they travel as a `ValueError` whose message starts with
/// the exception's name, and the parity traces report that name.
const ESCAPING_EXCEPTIONS: [&str; 2] = ["OverflowError", "OSError"];

/// The error a model validation failure becomes: pydantic's `ValidationError`,
/// or (for the exceptions pydantic does not wrap) a `ValueError` carrying the
/// exception's name at the start of its message.
pub(crate) fn model_error(error: ValidationError) -> Error {
    let kind = if error.is_validation() {
        ErrorKind::ValidationError
    } else {
        ErrorKind::ValueError
    };
    Error::new(kind, error.to_string())
}

/// `Model.model_validate(raw)` with no context, as a flow error.
pub(crate) fn parse_model<M: BydModel>(raw: &Value) -> Result<M, Error> {
    M::parse(raw, &Value::Null).map_err(model_error)
}

/// [`parse_model`] of a reply not needed afterwards ([`BydModel::parse_owned`]).
pub(crate) fn parse_owned_model<M: BydModel>(raw: Value) -> Result<M, Error> {
    M::parse_owned(raw, &Value::Null).map_err(model_error)
}

/// Python's truth value of a JSON value.
pub(crate) fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(entries) => !entries.is_empty(),
    }
}

/// The class name a trace reports for `error`.
fn trace_class(error: &Error) -> String {
    ESCAPING_EXCEPTIONS
        .iter()
        .find(|name| error.kind() == ErrorKind::ValueError && error.message().starts_with(*name))
        .map_or_else(|| error.class_name().to_owned(), |name| (*name).to_owned())
}

const FAILED: &str = "\u{0}error";

/// Runs `flow` in the scenario like [`Scenario::run`], but reports the
/// escaping exceptions of [`model_error`] under their own class name.
pub(crate) fn run_trace<T>(
    scenario: &Scenario,
    flow: impl Future<Output = Result<T, Error>>,
    to_json: impl FnOnce(T) -> Value,
) -> Outcome {
    let trace = scenario.run(
        async { Ok::<_, Error>(flow.await) },
        |outcome| match outcome {
            Ok(value) => to_json(value),
            Err(error) => {
                let mut failure = describe(&error);
                failure["class"] = json!(trace_class(&error));
                json!({ FAILED: failure })
            }
        },
    )?;
    let Value::Object(entries) = trace else {
        return Ok(trace);
    };
    // Move a failure from `result` to `error`, keeping its place in the trace.
    Ok(Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| match (key.as_str(), value) {
                ("result", Value::Object(mut marked)) if marked.contains_key(FAILED) => (
                    "error".to_owned(),
                    marked.remove(FAILED).unwrap_or_default(),
                ),
                (_, value) => (key, value),
            })
            .collect(),
    ))
}

/// The energy type a parity call names by its integer value.
pub(crate) fn energy_type_argument(
    scenario: &Scenario,
    key: &str,
    default: EnergyType,
) -> Result<EnergyType, ParityError> {
    match scenario.args().value(key) {
        Value::Null => Ok(default),
        value => value
            .as_i64()
            .and_then(|number| {
                EnergyType::MEMBERS
                    .iter()
                    .copied()
                    .find(|member| member.value() == number)
            })
            .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be an energy type"))),
    }
}

/// Fetches all vehicles associated with the authenticated user. Anything
/// but a list in the reply is no vehicles; an item that is not a valid
/// vehicle is a validation error.
pub async fn fetch_vehicle_list<E: Env>(cx: Context<'_, E>) -> Result<Vec<Vehicle>, Error> {
    let inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        None,
        None,
    );
    let decoded = post_token_json(
        cx,
        ENDPOINT,
        &inner,
        None,
        None,
        ErrorCodes::default(),
        None,
    )
    .await?;
    let items: &[Value] = decoded.as_array().map_or(&[], Vec::as_slice);
    items.iter().map(parse_model).collect()
}

/// Fetches a shared guest's grant in detail. The reply's `data` object
/// holds the grant; its `empowerControlPwdStatus` sits beside it.
pub async fn fetch_empower_detail_by_vin<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    empower_id: i64,
) -> Result<EmpowerDetail, Error> {
    let mut inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        None,
    );
    inner.insert("empowerId".into(), json!(empower_id.to_string()));
    let codes = ErrorCodes {
        not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
        ..ErrorCodes::default()
    };
    let decoded = post_token_json(
        cx,
        EMPOWER_DETAIL_ENDPOINT,
        &inner,
        None,
        Some(vin),
        codes,
        None,
    )
    .await?;
    let payload = match decoded {
        Value::Object(payload) => payload,
        _ => Map::new(),
    };
    // {"vin": vin, **data, "empowerControlPwdStatus": bool(...), "raw": payload}
    let mut merged = Map::new();
    merged.insert("vin".into(), json!(vin));
    if let Some(Value::Object(data)) = payload.get("data") {
        merged.extend(data.clone());
    }
    let pwd_status = payload
        .get("empowerControlPwdStatus")
        .is_some_and(py_truthy);
    merged.insert("empowerControlPwdStatus".into(), json!(pwd_status));
    merged.insert("raw".into(), Value::Object(payload));
    parse_owned_model(Value::Object(merged))
}

/// The dump of each model, as a JSON list.
pub(crate) fn dump_all<'a, M: BydModel + 'a>(models: impl IntoIterator<Item = &'a M>) -> Value {
    Value::Array(models.into_iter().map(BydModel::dump).collect())
}

/// The dump of a map of a model by key, as a JSON object in order.
pub(crate) fn dump_map<'a, M: BydModel + 'a>(
    models: impl IntoIterator<Item = &'a (String, M)>,
) -> Value {
    Value::Object(
        models
            .into_iter()
            .map(|(key, model)| (key.clone(), model.dump()))
            .collect::<Map<_, _>>(),
    )
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.vehicle.fetch_vehicle_list" => Scenario::from_args(args).and_then(|scenario| {
            run_trace(
                &scenario,
                fetch_vehicle_list(scenario.context()),
                |vehicles| dump_all(&vehicles),
            )
        }),
        "api.vehicle.fetch_empower_detail_by_vin" => {
            Scenario::from_args(args).and_then(|scenario| {
                let args = scenario.args();
                let vin = args.str("vin")?;
                let empower_id = args.i64("empower_id")?;
                run_trace(
                    &scenario,
                    fetch_empower_detail_by_vin(scenario.context(), vin, empower_id),
                    |detail| detail.dump(),
                )
            })
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthiness_follows_python() {
        for falsy in [
            json!(null),
            json!(false),
            json!(0),
            json!(0.0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            assert!(!py_truthy(&falsy), "{falsy}");
        }
        for truthy in [
            json!(true),
            json!(1),
            json!(-0.5),
            json!("0"),
            json!([0]),
            json!({"a": 1}),
        ] {
            assert!(py_truthy(&truthy), "{truthy}");
        }
    }

    #[test]
    fn escaping_exceptions_keep_their_name() {
        let overflow = model_error(ValidationError::raised("OverflowError", "time_t"));
        assert_eq!(overflow.kind(), ErrorKind::ValueError);
        assert_eq!(trace_class(&overflow), "OverflowError");
        let invalid = model_error(ValidationError::new("int_parsing"));
        assert_eq!(invalid.kind(), ErrorKind::ValidationError);
        assert_eq!(trace_class(&invalid), "ValidationError");
        let other = Error::new(ErrorKind::ValueError, "bad");
        assert_eq!(trace_class(&other), "ValueError");
    }

    #[test]
    fn energy_types_are_read_by_value() {
        let scenario = |value: Value| {
            Scenario::from_args(&json!({
                "config": {"username": "user@example.com", "password": "p"},
                "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
                "now_ms": 5, "seed": "x", "replies": [], "args": {"energy_type": value},
            }))
            .unwrap()
        };
        let read =
            |value: Value| energy_type_argument(&scenario(value), "energy_type", EnergyType::Ev);
        assert_eq!(read(json!(null)).unwrap(), EnergyType::Ev);
        assert_eq!(read(json!(2)).unwrap(), EnergyType::Hybrid);
        assert_eq!(read(json!(-1)).unwrap(), EnergyType::Unknown);
        assert_eq!(read(json!(7)).unwrap_err().kind, "BadArguments");
        assert_eq!(read(json!("1")).unwrap_err().kind, "BadArguments");
    }

    #[test]
    fn unknown_names_are_not_answered() {
        assert!(parity_call("api.vehicle.nothing", &json!({})).is_none());
    }
}
