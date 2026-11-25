//! Vehicle service history endpoint (`pybyd/_api/maintenance.py`).
//!
//! Calls `/maintenance?s=App.AppMaintenance.getInfoFromOthers`.
//! `App.AppMaintenance.getRecInfo` is intentionally left out: it returns empty
//! fields on every server tested and hangs ~60 s before a 504 on MEA.

use openbyd_protocol::coerce::py_strip;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::{BydModel, SENTINELS};
use openbyd_protocol::models::maintenance::{MaintenanceHistory, MaintenanceRecord};
use openbyd_protocol::parity::Outcome;
use openbyd_protocol::pyjson::py_str;
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_token_json,
};
use crate::api::control::truthy;
use crate::api::vehicle::{parse_owned_model, run_trace};
use crate::env::Env;
use crate::scripted::Scenario;

/// Where past workshop visits are read.
pub const MAINTENANCE_HISTORY_ENDPOINT: &str =
    "/maintenance?s=App.AppMaintenance.getInfoFromOthers";

/// Owner email and workshop contact/location, kept out of `raw` so it does not
/// end up in logs or diagnostics dumps.
const PERSONAL_KEYS: [&str; 5] = [
    "customer_email",
    "shop_address",
    "shop_phone",
    "latitude",
    "longitude",
];

const NOT_SUPPORTED: ErrorCodes<'static> = ErrorCodes {
    not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
    extra_code_map: &[],
};

/// Python's `value == 200` for a JSON value (`200` and `200.0`, not `"200"` or `true`).
fn is_ret_200(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            number.as_i64() == Some(200) || (number.is_f64() && number.as_f64() == Some(200.0))
        }
        _ => false,
    }
}

/// Extracts `data` from the `{"ret": 200, "data": ..., "msg": ...}` envelope.
fn unwrap_maintenance_data(decoded: &Value, endpoint: &str) -> Result<Map<String, Value>, Error> {
    let Value::Object(map) = decoded else {
        return Ok(Map::new());
    };
    if let Some(ret) = map.get("ret")
        && !is_ret_200(ret)
    {
        let message = map
            .get("msg")
            .filter(|value| truthy(value))
            .map(py_str)
            .unwrap_or_else(|| format!("Maintenance call failed ({endpoint})"));
        return Err(Error::api(
            ErrorKind::BydApiError,
            message,
            py_str(ret),
            endpoint,
        ));
    }
    match map.get("data") {
        Some(Value::Object(data)) => Ok(data.clone()),
        _ => Ok(map.clone()),
    }
}

fn scrub(item: &Map<String, Value>) -> Map<String, Value> {
    item.iter()
        .filter(|(key, _)| !PERSONAL_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// Fetches past workshop visits for `vin`.
pub async fn fetch_maintenance_history<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
) -> Result<MaintenanceHistory, Error> {
    let inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        None,
    );
    let decoded = post_token_json(
        cx,
        MAINTENANCE_HISTORY_ENDPOINT,
        &inner,
        None,
        Some(vin),
        NOT_SUPPORTED,
        None,
    )
    .await?;
    let data = unwrap_maintenance_data(&decoded, MAINTENANCE_HISTORY_ENDPOINT)?;
    let items: Vec<Map<String, Value>> = match data.get("list") {
        Some(Value::Array(raw_list)) => raw_list
            .iter()
            .filter_map(Value::as_object)
            .map(scrub)
            .collect(),
        _ => Vec::new(),
    };
    let mut records = Vec::with_capacity(items.len());
    for item in &items {
        let mut merged = item.clone();
        merged.insert("vin".into(), json!(vin));
        merged.insert("raw".into(), Value::Object(item.clone()));
        records.push(parse_owned_model::<MaintenanceRecord>(Value::Object(
            merged,
        ))?);
    }
    let mut raw = data;
    raw.insert(
        "list".into(),
        Value::Array(items.into_iter().map(Value::Object).collect()),
    );
    let cleaned_vin = if SENTINELS.contains(&py_strip(vin)) {
        String::new()
    } else {
        vin.to_owned()
    };
    Ok(MaintenanceHistory {
        raw,
        vin: cleaned_vin,
        records,
    })
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    if name != "api.maintenance.fetch_maintenance_history" {
        return None;
    }
    Some(Scenario::from_args(args).and_then(|scenario| {
        let vin = scenario.args().str("vin")?;
        run_trace(
            &scenario,
            fetch_maintenance_history(scenario.context(), vin),
            |history| history.dump(),
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwrap_and_scrub_follow_pybyd() {
        assert_eq!(
            unwrap_maintenance_data(&json!([]), MAINTENANCE_HISTORY_ENDPOINT).unwrap(),
            Map::new()
        );
        assert_eq!(
            unwrap_maintenance_data(
                &json!({"ret": 200.0, "data": {"list": []}}),
                MAINTENANCE_HISTORY_ENDPOINT
            )
            .unwrap(),
            json!({"list": []}).as_object().unwrap().clone()
        );
        assert_eq!(
            unwrap_maintenance_data(
                &json!({"ret": 200, "data": null}),
                MAINTENANCE_HISTORY_ENDPOINT
            )
            .unwrap(),
            json!({"ret": 200, "data": null})
                .as_object()
                .unwrap()
                .clone()
        );
        let error = unwrap_maintenance_data(
            &json!({"ret": 500, "data": null, "msg": "Internal error"}),
            MAINTENANCE_HISTORY_ENDPOINT,
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BydApiError);
        assert_eq!(error.code(), "500");
        assert_eq!(error.endpoint(), MAINTENANCE_HISTORY_ENDPOINT);
        assert_eq!(error.message(), "Internal error");

        let fallback = unwrap_maintenance_data(
            &json!({"ret": "200", "msg": ""}),
            MAINTENANCE_HISTORY_ENDPOINT,
        )
        .unwrap_err();
        assert_eq!(fallback.code(), "200");
        assert!(fallback.message().contains(MAINTENANCE_HISTORY_ENDPOINT));

        let scrubbed = scrub(
            json!({
                "ro_no": "RO-1",
                "customer_email": "owner@example.com",
                "shop_address": "123 St",
                "shop_phone": "555",
                "latitude": 52.0,
                "longitude": 4.0,
                "shop_name": "BYD Test"
            })
            .as_object()
            .unwrap(),
        );
        assert_eq!(
            Value::Object(scrubbed),
            json!({"ro_no": "RO-1", "shop_name": "BYD Test"})
        );
        assert!(parity_call("api.maintenance.other", &json!({})).is_none());
    }
}
