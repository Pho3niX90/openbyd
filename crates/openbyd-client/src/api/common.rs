//! Posting token requests, with the guest `userType` retry (`pybyd/_api/_common.py`).
//!
//! The pure helpers of the same pyBYD file (`build_inner_base`,
//! `_raise_for_code`, `decode_respond_data` and the code sets) live in
//! [`openbyd_protocol::api::common`] and are re-exported here.

use openbyd_protocol::envelope::{build_token_outer_envelope, handle_token_response};
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::parity::{Outcome, ParityError};
use serde_json::{Map, Value, json};

pub use openbyd_protocol::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, GUEST_NOT_DECLARED_CODE, NOT_A_GUEST_CODE, SERVICE_BUSY_CODES,
    USER_TYPE_GUEST, USER_TYPE_OWNER, VEHICLE_UNREACHABLE_CODES, build_inner_base,
    decode_respond_data, raise_for_code, random_hex,
};

use crate::api::Context;
use crate::env::Env;
use crate::scripted::{Scenario, borrow_codes, code_map, code_set};

/// How an endpoint maps API codes to errors: pyBYD's `not_supported_codes`
/// and `extra_code_map` arguments. See [`raise_for_code`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ErrorCodes<'a> {
    /// Codes meaning the endpoint is not supported (`BydEndpointNotSupportedError`).
    pub not_supported_codes: Option<&'a [&'a str]>,
    /// Further code sets and the error each maps to, checked in order.
    pub extra_code_map: &'a [(&'a [&'a str], ErrorKind)],
}

/// Posts a token-enveloped request and returns the decrypted `respondData`
/// (an empty object when there is none), as pyBYD's `post_token_json`.
///
/// `now_ms` defaults to the environment's clock plus the server clock
/// offset; with an offset, the inner `timeStamp` is that time too. `user_type` is sent and
/// signed when given. A non-zero `code` becomes the error
/// [`raise_for_code`] picks; a reply that is not an object is an
/// `AttributeError`, as in pyBYD.
pub async fn post_token_json<E: Env>(
    cx: Context<'_, E>,
    endpoint: &str,
    inner: &Map<String, Value>,
    now_ms: Option<i64>,
    vin: Option<&str>,
    codes: ErrorCodes<'_>,
    user_type: Option<&str>,
) -> Result<Value, Error> {
    // Requests are stamped with BYD's clock once its `Date` header showed a drift.
    let offset_ms = cx.env.server_time_offset_ms();
    let now_ms = now_ms.unwrap_or_else(|| cx.env.now_ms() + offset_ms);
    let stamped;
    let inner = if offset_ms != 0 && inner.contains_key("timeStamp") {
        let mut copy = inner.clone();
        copy.insert("timeStamp".into(), json!(now_ms.to_string()));
        stamped = copy;
        &stamped
    } else {
        inner
    };
    // pyBYD's envelope reads the clock again for `serviceTime`.
    let service_time_ms = cx.env.now_ms();
    let (outer, content_key) = build_token_outer_envelope(
        cx.config,
        cx.session,
        inner,
        now_ms,
        user_type,
        service_time_ms,
    );
    let response = cx.env.post_secure(endpoint, &outer).await?;
    handle_token_response(
        endpoint,
        &response,
        &content_key,
        vin,
        codes.not_supported_codes,
        codes.extra_code_map,
    )
}

/// Whether an error is one `post_vehicle_json` may retry: a `BydApiError`
/// other than `BydSessionExpiredError` (pyBYD's `except` order).
fn retryable(error: &Error) -> bool {
    error.is_a(ErrorKind::BydApiError) && !error.is_a(ErrorKind::BydSessionExpiredError)
}

/// Posts a request that reaches the car, declaring this account a guest
/// (`userType: "1"`) when the session believes it is one for `vin`, as
/// pyBYD's `post_vehicle_json`.
///
/// When the cloud answers with the code that means "wrong `userType`"
/// (1001 to an undeclared guest, 8003 to an owner claiming to be a guest),
/// the request is sent once more the other way, with a fresh `random` and
/// `timeStamp`, unless the (VIN, endpoint) pair is settled:
///
/// - the retry succeeds: the session's guest flag for `vin` is flipped and
///   its answer returned;
/// - the retry is refused the other way too: the pair is settled and the
///   *first* error returned;
/// - the retry fails with any other API code: the guest flag is flipped and
///   that error returned;
/// - an expired session, or a failure that is not an API error, is returned
///   as it is, with no change to the session.
pub async fn post_vehicle_json<E: Env>(
    cx: Context<'_, E>,
    endpoint: &str,
    inner: &Map<String, Value>,
    vin: &str,
    now_ms: Option<i64>,
    codes: ErrorCodes<'_>,
) -> Result<Value, Error> {
    let guest = cx.session.is_guest(vin);
    let declared = Some(if guest {
        USER_TYPE_GUEST
    } else {
        USER_TYPE_OWNER
    });
    let first = match post_token_json(cx, endpoint, inner, now_ms, Some(vin), codes, declared).await
    {
        Ok(decoded) => return Ok(decoded),
        Err(error) if !retryable(&error) => return Err(error),
        Err(error) => error,
    };
    let wrong_user_type = if guest {
        NOT_A_GUEST_CODE
    } else {
        GUEST_NOT_DECLARED_CODE
    };
    if first.code() != wrong_user_type || cx.session.is_user_type_settled(vin, endpoint) {
        return Err(first);
    }
    // A request's random and timeStamp are single-use.
    let retry_ms = cx.env.now_ms();
    let mut retry_inner = inner.clone();
    retry_inner.insert("random".into(), json!(random_hex(&cx.env.random_bytes())));
    retry_inner.insert("timeStamp".into(), json!(retry_ms.to_string()));
    let other = Some(if guest {
        USER_TYPE_OWNER
    } else {
        USER_TYPE_GUEST
    });
    match post_token_json(
        cx,
        endpoint,
        &retry_inner,
        Some(retry_ms),
        Some(vin),
        codes,
        other,
    )
    .await
    {
        Ok(decoded) => {
            cx.session.set_guest(vin, !guest);
            Ok(decoded)
        }
        Err(second) if !retryable(&second) => Err(second),
        Err(second) => {
            let refused_both_ways = if guest {
                GUEST_NOT_DECLARED_CODE
            } else {
                NOT_A_GUEST_CODE
            };
            if second.code() == refused_both_ways {
                // Not a userType problem after all: report what the cloud
                // said first, and do not ask twice again this session.
                cx.session.settle_user_type(vin, endpoint);
                return Err(first);
            }
            // Any other answer got past the userType check: the guess is
            // corrected and this is the real answer.
            cx.session.set_guest(vin, !guest);
            Err(second)
        }
    }
}

/// The parsed code arguments of a parity call, owned so [`ErrorCodes`] can borrow them.
struct CodeArguments {
    not_supported: Option<Vec<String>>,
    extra: Vec<(Vec<String>, ErrorKind)>,
}

impl CodeArguments {
    fn read(scenario: &Scenario) -> Result<Self, ParityError> {
        let args = scenario.args();
        Ok(Self {
            not_supported: code_set(args.value("not_supported_codes"))?,
            extra: code_map(args.value("extra_code_map"))?,
        })
    }
}

fn inner_argument(scenario: &Scenario) -> Result<&Map<String, Value>, ParityError> {
    scenario
        .args()
        .value("inner")
        .as_object()
        .ok_or_else(|| ParityError::bad_arguments("inner must be an object"))
}

fn opt_i64(scenario: &Scenario, key: &str) -> Result<Option<i64>, ParityError> {
    match scenario.args().value(key) {
        Value::Null => Ok(None),
        _ => scenario.args().i64(key).map(Some),
    }
}

fn token_call(scenario: &Scenario, vehicle: bool) -> Outcome {
    let parsed = CodeArguments::read(scenario)?;
    let not_supported = parsed.not_supported.as_deref().map(borrow_codes);
    let extra_codes: Vec<Vec<&str>> = parsed
        .extra
        .iter()
        .map(|(codes, _)| borrow_codes(codes))
        .collect();
    let extra: Vec<(&[&str], ErrorKind)> = extra_codes
        .iter()
        .zip(&parsed.extra)
        .map(|(codes, (_, kind))| (codes.as_slice(), *kind))
        .collect();
    let codes = ErrorCodes {
        not_supported_codes: not_supported.as_deref(),
        extra_code_map: &extra,
    };
    let args = scenario.args();
    let endpoint = args.str("endpoint")?;
    let inner = inner_argument(scenario)?;
    let now_ms = opt_i64(scenario, "now_ms")?;
    let cx = scenario.context();
    if vehicle {
        let vin = args.str("vin")?;
        scenario.run(
            post_vehicle_json(cx, endpoint, inner, vin, now_ms, codes),
            |value| value,
        )
    } else {
        let vin = args.opt_str("vin")?;
        let user_type = args.opt_str("user_type")?;
        scenario.run(
            post_token_json(cx, endpoint, inner, now_ms, vin, codes, user_type),
            |value| value,
        )
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let vehicle = match name {
        "api.common.post_token_json" => false,
        "api.common.post_vehicle_json" => true,
        _ => return None,
    };
    Some(Scenario::from_args(args).and_then(|scenario| token_call(&scenario, vehicle)))
}

#[cfg(test)]
mod tests {
    use openbyd_protocol::config::BydConfig;
    use openbyd_protocol::session::Session;

    use super::*;
    use crate::scripted::{Reply, ScriptedEnv, block_on};

    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn flows_share_a_session_across_threads() {
        let config = BydConfig::new("user@example.com", "p");
        let session = Session::new("1", "s", "e", 0.0, 1.0);
        let refused = || Reply::Outer(json!({"code": "1001", "message": ""}));
        let ok = || Reply::Outer(json!({"code": "0"}));
        std::thread::scope(|scope| {
            for vin in ["V1", "V2"] {
                let (config, session) = (&config, &session);
                scope.spawn(move || {
                    let env = ScriptedEnv::new(0, vin, vec![refused(), ok()]);
                    let cx = Context::new(&env, config, session);
                    let inner = Map::new();
                    let flow =
                        post_vehicle_json(cx, "/e", &inner, vin, None, ErrorCodes::default());
                    assert_send(&flow);
                    assert_eq!(block_on(flow).unwrap().unwrap(), json!({}));
                });
            }
        });
        assert!(session.is_guest("V1") && session.is_guest("V2"));
    }

    #[test]
    fn arguments_are_checked() {
        let base = json!({
            "config": {"username": "user@example.com", "password": "p"},
            "session": {"user_id": "1", "sign_token": "s", "encry_token": "e"},
            "now_ms": 5, "seed": "x", "replies": [],
            "args": {"endpoint": "/e", "inner": {}, "vin": "V", "now_ms": 7},
        });
        let kind = |name: &str, key: &str, value: Value| {
            let mut args = base.clone();
            args["args"][key] = value;
            parity_call(name, &args)
                .unwrap()
                .err()
                .map(|error| error.kind)
        };
        for name in ["api.common.post_token_json", "api.common.post_vehicle_json"] {
            assert_eq!(kind(name, "inner", json!(null)).unwrap(), "BadArguments");
            assert_eq!(kind(name, "now_ms", json!("x")).unwrap(), "BadArguments");
            assert_eq!(kind(name, "endpoint", json!(1)).unwrap(), "BadArguments");
            assert_eq!(
                kind(name, "not_supported_codes", json!(1)).unwrap(),
                "BadArguments"
            );
            assert_eq!(
                kind(name, "extra_code_map", json!([[[], "Nope"]])).unwrap(),
                "BadArguments"
            );
        }
        assert_eq!(
            kind("api.common.post_vehicle_json", "vin", json!(null)).unwrap(),
            "BadArguments"
        );
        assert_eq!(
            kind("api.common.post_token_json", "user_type", json!(1)).unwrap(),
            "BadArguments"
        );
        assert!(kind("api.common.post_token_json", "vin", json!(null)).is_none());
        assert!(parity_call("api.common.nothing", &base).is_none());
    }
}
