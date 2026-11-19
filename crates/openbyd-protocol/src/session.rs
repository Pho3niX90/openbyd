//! Session tokens and the keys derived from them (`pybyd/session.py`).

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde_json::{Map, Value, json};

use crate::config::{float_dump, lax_float, lax_str, object_argument, validation_error};
use crate::error::Error;
use crate::hashing::md5_hex;
use crate::parity::{Args, Outcome, ParityError};

/// The default session lifetime: 12 hours, in seconds.
pub const DEFAULT_SESSION_TTL: f64 = 12.0 * 3600.0;

/// The state of a logged-in account: its tokens, the keys derived from
/// them, and what the client has learnt about guest access per vehicle.
///
/// Times are seconds on a monotonic clock the caller chooses; this crate
/// never reads a clock, so "now" is always an argument.
#[derive(Clone, PartialEq)]
pub struct Session {
    user_id: String,
    sign_token: String,
    encry_token: String,
    created_at: f64,
    ttl: f64,
    content_key: String,
    sign_key: String,
    guest_vins: HashMap<String, bool>,
    user_type_settled: HashSet<(String, String)>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("user_id", &self.user_id)
            .field("sign_token", &"<redacted>")
            .field("encry_token", &"<redacted>")
            .field("created_at", &self.created_at)
            .field("ttl", &self.ttl)
            .field("guest_vins", &self.guest_vins)
            .field("user_type_settled", &self.user_type_settled)
            .finish()
    }
}

impl Session {
    /// A session for the tokens a login returned. Surrounding whitespace is
    /// stripped from the tokens, as pydantic does.
    pub fn new(
        user_id: &str,
        sign_token: &str,
        encry_token: &str,
        created_at: f64,
        ttl: f64,
    ) -> Self {
        let sign_token = sign_token.trim().to_owned();
        let encry_token = encry_token.trim().to_owned();
        Self {
            user_id: user_id.trim().to_owned(),
            content_key: md5_hex(&encry_token),
            sign_key: md5_hex(&sign_token),
            sign_token,
            encry_token,
            created_at,
            ttl,
            guest_vins: HashMap::new(),
            user_type_settled: HashSet::new(),
        }
    }

    /// Builds a session from loose values, as `Session(**kwargs)` does.
    /// `now` stands in for `time.monotonic()` when `created_at` is left out.
    pub fn from_kwargs(kwargs: &Map<String, Value>, now: f64) -> Result<Self, Error> {
        let mut user_id = None;
        let mut sign_token = None;
        let mut encry_token = None;
        let mut created_at = now;
        let mut ttl = DEFAULT_SESSION_TTL;
        for (key, value) in kwargs {
            match key.as_str() {
                "user_id" => user_id = Some(lax_str(value, key)?),
                "sign_token" => sign_token = Some(lax_str(value, key)?),
                "encry_token" => encry_token = Some(lax_str(value, key)?),
                "created_at" => created_at = lax_float(value, key)?,
                "ttl" => ttl = lax_float(value, key)?,
                other => {
                    return Err(validation_error(format!(
                        "{other}: extra fields not permitted"
                    )));
                }
            }
        }
        let required = |value: Option<String>, name: &str| {
            value.ok_or_else(|| validation_error(format!("{name}: field required")))
        };
        let user_id = required(user_id, "user_id")?;
        let sign_token = required(sign_token, "sign_token")?;
        let encry_token = required(encry_token, "encry_token")?;
        Ok(Self::new(
            &user_id,
            &sign_token,
            &encry_token,
            created_at,
            ttl,
        ))
    }

    /// The authenticated user's ID.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// The token request signatures are derived from. A secret.
    pub fn sign_token(&self) -> &str {
        &self.sign_token
    }

    /// The token the content key is derived from. A secret.
    pub fn encry_token(&self) -> &str {
        &self.encry_token
    }

    /// When the session was created, in the caller's monotonic seconds.
    pub fn created_at(&self) -> f64 {
        self.created_at
    }

    /// Lifetime in seconds.
    pub fn ttl(&self) -> f64 {
        self.ttl
    }

    /// The AES key for inner payloads: MD5 of `encry_token`, uppercase hex.
    pub fn content_key(&self) -> &str {
        &self.content_key
    }

    /// The key that signs requests: MD5 of `sign_token`, uppercase hex.
    pub fn sign_key(&self) -> &str {
        &self.sign_key
    }

    /// Whether the session has lived its TTL at monotonic time `now`.
    pub fn is_expired(&self, now: f64) -> bool {
        now - self.created_at >= self.ttl
    }

    /// Seconds since the session was created, at monotonic time `now`.
    pub fn age(&self, now: f64) -> f64 {
        now - self.created_at
    }

    /// VIN to whether this account reaches the car as a guest (a car shared
    /// through "Family and friends"). Filled from the vehicle list and
    /// corrected by the cloud's answers.
    pub fn guest_vins(&self) -> &HashMap<String, bool> {
        &self.guest_vins
    }

    /// Mutable access to [`Session::guest_vins`].
    pub fn guest_vins_mut(&mut self) -> &mut HashMap<String, bool> {
        &mut self.guest_vins
    }

    /// (VIN, endpoint) pairs whose refusal the other `userType` did not fix,
    /// so they are not retried the other way again.
    pub fn user_type_settled(&self) -> &HashSet<(String, String)> {
        &self.user_type_settled
    }

    /// Mutable access to [`Session::user_type_settled`].
    pub fn user_type_settled_mut(&mut self) -> &mut HashSet<(String, String)> {
        &mut self.user_type_settled
    }

    /// The session as pyBYD's `model_dump(mode="json")` gives it, with
    /// non-finite floats written as strings.
    pub fn to_dump(&self) -> Value {
        json!({
            "user_id": self.user_id,
            "sign_token": self.sign_token,
            "encry_token": self.encry_token,
            "created_at": float_dump(self.created_at),
            "ttl": float_dump(self.ttl),
        })
    }
}

/// The session a parity call describes: `session` holds `Session` keyword
/// arguments and `now` the monotonic time.
pub(crate) fn session_argument(args: &Args<'_>) -> Result<Session, ParityError> {
    let now = args.value("now").as_f64().unwrap_or(0.0);
    Ok(Session::from_kwargs(
        object_argument(args, "session")?,
        now,
    )?)
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    let now = || {
        args.value("now")
            .as_f64()
            .ok_or_else(|| ParityError::bad_arguments("now must be a number"))
    };
    Some(match name {
        "session.Session" => session_argument(&args).map(|session| session.to_dump()),
        "session.content_key" => {
            session_argument(&args).map(|session| json!(session.content_key()))
        }
        "session.sign_key" => session_argument(&args).map(|session| json!(session.sign_key())),
        "session.is_expired" => (|| Ok(json!(session_argument(&args)?.is_expired(now()?))))(),
        "session.age" => (|| Ok(float_dump(session_argument(&args)?.age(now()?))))(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_expiry_and_guest_state() {
        let mut session = Session::new(" 42 ", " sign ", "encry", 100.0, 10.0);
        assert_eq!(session.user_id(), "42");
        assert_eq!(session.sign_token(), "sign");
        assert_eq!(session.encry_token(), "encry");
        assert_eq!(session.sign_key(), md5_hex("sign"));
        assert_eq!(session.content_key(), md5_hex("encry"));
        assert_eq!(session.created_at(), 100.0);
        assert_eq!(session.ttl(), 10.0);
        assert!(!session.is_expired(109.9));
        assert!(session.is_expired(110.0));
        assert_eq!(session.age(105.0), 5.0);
        session
            .guest_vins_mut()
            .insert("TESTVIN0000000001".into(), true);
        session
            .user_type_settled_mut()
            .insert(("TESTVIN0000000001".into(), "/x".into()));
        assert!(session.guest_vins()["TESTVIN0000000001"]);
        assert_eq!(session.user_type_settled().len(), 1);
        let shown = format!("{session:?}");
        assert!(!shown.contains("sign\"") && !shown.contains("encry\""));
    }

    #[test]
    fn kwargs_are_validated() {
        let kwargs = json!({"user_id": "1", "sign_token": "s"});
        assert!(Session::from_kwargs(kwargs.as_object().unwrap(), 0.0).is_err());
        let kwargs = json!({"user_id": "1", "sign_token": "s", "encry_token": "e", "x": 1});
        assert!(Session::from_kwargs(kwargs.as_object().unwrap(), 0.0).is_err());
        let kwargs = json!({"user_id": "1", "sign_token": "s", "encry_token": "e"});
        assert_eq!(
            Session::from_kwargs(kwargs.as_object().unwrap(), 7.0)
                .unwrap()
                .created_at(),
            7.0
        );
        assert!(
            parity_call("session.age", &json!({"session": {}}))
                .unwrap()
                .is_err()
        );
    }
}
