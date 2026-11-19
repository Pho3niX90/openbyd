//! Message center (account inbox) endpoints (`pybyd/_api/message_center.py`).
//!
//! - `/app/messageCenter/getUnReadMessageList`: unread messages by group.
//!
//! The request is account-scoped: it sends `messageGroup` without a VIN or
//! paging fields.

use openbyd_protocol::coerce::IntEnum;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::message_center::{NotificationCategory, VehicleMsgListResponse};
use openbyd_protocol::parity::Outcome;
use serde_json::{Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_token_json,
};
use crate::api::vehicle::{parse_owned_model, run_trace};
use crate::env::Env;
use crate::scripted::Scenario;

const UNREAD_ENDPOINT: &str = "/app/messageCenter/getUnReadMessageList";

/// Fetches the unread inbox messages of one category (pyBYD's default is
/// `NotificationCategory::Vehicle`) for the logged-in account.
pub async fn fetch_notifications<E: Env>(
    cx: Context<'_, E>,
    category: NotificationCategory,
) -> Result<VehicleMsgListResponse, Error> {
    let mut inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        None,
        None,
    );
    inner.insert("messageGroup".into(), json!(category.value().to_string()));
    let codes = ErrorCodes {
        not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
        ..ErrorCodes::default()
    };
    let decoded = post_token_json(cx, UNREAD_ENDPOINT, &inner, None, None, codes, None).await?;
    parse_owned_model(decoded)
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.message_center.fetch_notifications" => {
            Scenario::from_args(args).and_then(|scenario| {
                // `NotificationCategory(category)` fails before anything is sent
                // for a value that is no member; an argument left out is the default.
                let requested = scenario.args().value("category");
                let category = if scenario.args().0.get("category").is_none() {
                    Some(NotificationCategory::Vehicle)
                } else {
                    NotificationCategory::from_json(requested)
                };
                let to_json = |response: VehicleMsgListResponse| response.dump();
                match category {
                    Some(category) => run_trace(
                        &scenario,
                        fetch_notifications(scenario.context(), category),
                        to_json,
                    ),
                    None => run_trace(
                        &scenario,
                        async {
                            Err::<VehicleMsgListResponse, _>(Error::new(
                                ErrorKind::ValueError,
                                "not a valid NotificationCategory",
                            ))
                        },
                        to_json,
                    ),
                }
            })
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_names_are_not_answered() {
        assert!(parity_call("api.message_center.nothing", &json!({})).is_none());
    }

    #[test]
    fn a_bad_scenario_is_reported() {
        let error = parity_call("api.message_center.fetch_notifications", &json!({}))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, "BadArguments");
    }
}
