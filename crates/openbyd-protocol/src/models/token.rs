//! The token a login returns (`pybyd/models/token.py`).

use std::fmt;

use serde_json::{Map, Value, json};

use crate::config::validation_error;
use crate::error::Error;
use crate::parity::{Args, Outcome};
use crate::session::Session;

/// The token returned by a successful login.
#[derive(Clone, PartialEq)]
pub struct AuthToken {
    /// The authenticated user's ID.
    pub user_id: String,
    /// The token request signatures are derived from. A secret.
    pub sign_token: String,
    /// The token the content key is derived from. A secret.
    pub encry_token: String,
    /// The token `/app/token/refresh` takes; empty when the login gave none. A secret.
    pub refresh_token: String,
    /// The whole decoded token object, for fields not listed here.
    pub raw: Map<String, Value>,
}

impl fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthToken")
            .field("user_id", &self.user_id)
            .field("sign_token", &"<redacted>")
            .field("encry_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("raw", &format_args!("<{} fields>", self.raw.len()))
            .finish()
    }
}

impl AuthToken {
    /// Validates loose values as pydantic's `AuthToken.model_validate` does:
    /// the three tokens must be strings (kept as given), `refresh_token` a
    /// string when given, `raw` an object, and other keys are ignored.
    pub fn from_raw(raw: &Value) -> Result<Self, Error> {
        let fields = raw
            .as_object()
            .ok_or_else(|| validation_error("AuthToken: input should be an object"))?;
        let text = |name: &str| match fields.get(name) {
            Some(Value::String(text)) => Ok(text.clone()),
            Some(_) => Err(validation_error(format!(
                "{name}: input should be a valid string"
            ))),
            None => Err(validation_error(format!("{name}: field required"))),
        };
        let user_id = text("user_id");
        let sign_token = text("sign_token");
        let encry_token = text("encry_token");
        let refresh_token = match fields.get("refresh_token") {
            None => Ok(String::new()),
            Some(_) => text("refresh_token"),
        };
        let raw = match fields.get("raw") {
            Some(Value::Object(raw)) => Ok(raw.clone()),
            Some(_) => Err(validation_error("raw: input should be a valid dictionary")),
            None => Err(validation_error("raw: field required")),
        };
        Ok(Self {
            user_id: user_id?,
            sign_token: sign_token?,
            encry_token: encry_token?,
            refresh_token: refresh_token?,
            raw: raw?,
        })
    }

    /// The token as pyBYD's `model_dump(mode="json")` gives it.
    pub fn to_dump(&self) -> Value {
        json!({
            "user_id": self.user_id,
            "sign_token": self.sign_token,
            "encry_token": self.encry_token,
            "refresh_token": self.refresh_token,
            "raw": self.raw,
        })
    }

    /// A session for this token, created at monotonic time `created_at` and
    /// living `ttl` seconds.
    pub fn to_session(&self, created_at: f64, ttl: f64) -> Session {
        Session::new(
            &self.user_id,
            &self.sign_token,
            &self.encry_token,
            created_at,
            ttl,
        )
        .with_refresh_token(&self.refresh_token)
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "models.token.AuthToken" => AuthToken::from_raw(args.value("raw"))
            .map(|token| json!({"dump": token.to_dump(), "properties": {}}))
            .map_err(Into::into),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_hides_secrets() {
        let raw = json!({"user_id": "1", "sign_token": "S3CRET", "encry_token": "E", "raw": {"x": 1}, "extra": 2});
        let token = AuthToken::from_raw(&raw).unwrap();
        assert!(!format!("{token:?}").contains("S3CRET"));
        assert_eq!(token.to_session(0.0, 1.0).sign_token(), "S3CRET");
        assert_eq!(token.refresh_token, "");
        let raw = json!({"user_id": "1", "sign_token": "S", "encry_token": "E", "refresh_token": "R", "raw": {}});
        assert_eq!(
            AuthToken::from_raw(&raw)
                .unwrap()
                .to_session(0.0, 1.0)
                .refresh_token(),
            "R"
        );
        let raw = json!({"user_id": "1", "sign_token": "S", "encry_token": "E", "refresh_token": 5, "raw": {}});
        assert!(AuthToken::from_raw(&raw).is_err());
        assert!(AuthToken::from_raw(&json!([])).is_err());
        assert!(
            AuthToken::from_raw(
                &json!({"user_id": 1, "sign_token": "", "encry_token": "", "raw": {}})
            )
            .is_err()
        );
        assert!(
            AuthToken::from_raw(
                &json!({"user_id": "1", "sign_token": "", "encry_token": "", "raw": []})
            )
            .is_err()
        );
        assert!(
            AuthToken::from_raw(&json!({"user_id": "1", "sign_token": "", "encry_token": ""}))
                .is_err()
        );
        assert!(
            AuthToken::from_raw(&json!({"sign_token": "", "encry_token": "", "raw": {}})).is_err()
        );
    }
}
