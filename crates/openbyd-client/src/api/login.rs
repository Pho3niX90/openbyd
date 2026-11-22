//! The token refresh of `pybyd/_api/login.py` (`refresh_session_token`).
//!
//! The login request and reply are pure and live in
//! [`openbyd_protocol::envelope`]; the refresh posts through the session,
//! so it is a flow.
//!
//! - `/app/token/refresh`

use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::token::AuthToken;
use openbyd_protocol::parity::Outcome;
use openbyd_protocol::pyjson::py_str;
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{ErrorCodes, build_inner_base, post_token_json};
use crate::api::vehicle::{py_truthy, run_trace};
use crate::env::Env;
use crate::scripted::Scenario;

/// Where the session's tokens are refreshed.
pub const TOKEN_REFRESH_ENDPOINT: &str = "/app/token/refresh";

/// Buys new `signToken` and `encryToken` with the session's refresh token
/// (pyBYD's `refresh_session_token`).
///
/// The reply is the tokens, or `{"newTokens": {...}}`. Without both tokens
/// it fails with `BydAuthenticationError`. A reply without `userId` or
/// `refreshToken` keeps the session's.
pub async fn refresh_session_token<E: Env>(cx: Context<'_, E>) -> Result<AuthToken, Error> {
    let mut inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        None,
        None,
    );
    inner.insert("refreshToken".into(), json!(cx.session.refresh_token()));
    let decoded = post_token_json(
        cx,
        TOKEN_REFRESH_ENDPOINT,
        &inner,
        None,
        None,
        ErrorCodes::default(),
        None,
    )
    .await?;
    let payload = match decoded {
        Value::Object(payload) => payload,
        _ => Map::new(),
    };
    let token = match payload.get("newTokens") {
        Some(Value::Object(tokens)) => tokens.clone(),
        _ => payload,
    };
    let field = |name: &str| token.get(name).filter(|value| py_truthy(value)).map(py_str);
    let (Some(sign_token), Some(encry_token)) = (field("signToken"), field("encryToken")) else {
        return Err(Error::api(
            ErrorKind::BydAuthenticationError,
            "Token refresh response missing signToken/encryToken",
            "",
            TOKEN_REFRESH_ENDPOINT,
        ));
    };
    Ok(AuthToken {
        user_id: field("userId").unwrap_or_else(|| cx.session.user_id().to_owned()),
        sign_token,
        encry_token,
        refresh_token: field("refreshToken")
            .unwrap_or_else(|| cx.session.refresh_token().to_owned()),
        raw: token,
    })
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "envelope.refresh_session_token" => Scenario::from_args(args).and_then(|scenario| {
            run_trace(
                &scenario,
                refresh_session_token(scenario.context()),
                |token| token.to_dump(),
            )
        }),
        _ => return None,
    })
}
