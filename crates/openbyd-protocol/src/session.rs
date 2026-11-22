//! Session tokens and the keys derived from them (`pybyd/session.py`).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

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
///
/// The guest state is updated through `&self` (it sits behind a mutex), so
/// one session can be shared by flows running at the same time, as pyBYD
/// shares one `Session` object between its coroutines.
pub struct Session {
    user_id: String,
    sign_token: String,
    encry_token: String,
    refresh_token: String,
    created_at: f64,
    ttl: f64,
    content_key: String,
    sign_key: String,
    guest_vins: Mutex<HashMap<String, bool>>,
    user_type_settled: Mutex<HashSet<(String, String)>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // The guarded maps stay consistent even if a holder panicked.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Clone for Session {
    fn clone(&self) -> Self {
        Self {
            user_id: self.user_id.clone(),
            sign_token: self.sign_token.clone(),
            encry_token: self.encry_token.clone(),
            refresh_token: self.refresh_token.clone(),
            created_at: self.created_at,
            ttl: self.ttl,
            content_key: self.content_key.clone(),
            sign_key: self.sign_key.clone(),
            guest_vins: Mutex::new(self.guest_vins()),
            user_type_settled: Mutex::new(self.user_type_settled()),
        }
    }
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.user_id == other.user_id
            && self.sign_token == other.sign_token
            && self.encry_token == other.encry_token
            && self.refresh_token == other.refresh_token
            && self.created_at == other.created_at
            && self.ttl == other.ttl
            && self.guest_vins() == other.guest_vins()
            && self.user_type_settled() == other.user_type_settled()
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("user_id", &self.user_id)
            .field("sign_token", &"<redacted>")
            .field("encry_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("created_at", &self.created_at)
            .field("ttl", &self.ttl)
            .field("guest_vins", &self.guest_vins())
            .field("user_type_settled", &self.user_type_settled())
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
            refresh_token: String::new(),
            created_at,
            ttl,
            guest_vins: Mutex::default(),
            user_type_settled: Mutex::default(),
        }
    }

    /// This session with the token `/app/token/refresh` takes (pyBYD's
    /// `Session.refresh_token`), stripped as pydantic strips it.
    pub fn with_refresh_token(mut self, refresh_token: &str) -> Self {
        self.refresh_token = refresh_token.trim().to_owned();
        self
    }

    /// A new session with refreshed tokens that keeps what this one knows
    /// about guest access (pyBYD's `Session.with_refreshed_tokens`). It is
    /// created at monotonic time `now`, with this session's TTL, and keeps
    /// this session's refresh token when `refresh_token` is `None`.
    pub fn with_refreshed_tokens(
        &self,
        sign_token: &str,
        encry_token: &str,
        refresh_token: Option<&str>,
        now: f64,
    ) -> Self {
        let refreshed = Self::new(&self.user_id, sign_token, encry_token, now, self.ttl)
            .with_refresh_token(refresh_token.unwrap_or(&self.refresh_token));
        lock(&refreshed.guest_vins).extend(self.guest_vins());
        lock(&refreshed.user_type_settled).extend(self.user_type_settled());
        refreshed
    }

    /// Builds a session from loose values, as `Session(**kwargs)` does.
    /// `now` stands in for `time.monotonic()` when `created_at` is left out.
    pub fn from_kwargs(kwargs: &Map<String, Value>, now: f64) -> Result<Self, Error> {
        let mut user_id = None;
        let mut sign_token = None;
        let mut encry_token = None;
        let mut refresh_token = String::new();
        let mut created_at = now;
        let mut ttl = DEFAULT_SESSION_TTL;
        for (key, value) in kwargs {
            match key.as_str() {
                "user_id" => user_id = Some(lax_str(value, key)?),
                "sign_token" => sign_token = Some(lax_str(value, key)?),
                "encry_token" => encry_token = Some(lax_str(value, key)?),
                "refresh_token" => refresh_token = lax_str(value, key)?,
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
        Ok(
            Self::new(&user_id, &sign_token, &encry_token, created_at, ttl)
                .with_refresh_token(&refresh_token),
        )
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

    /// The token that buys new tokens at `/app/token/refresh`; empty when
    /// the login gave none. A secret.
    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
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

    /// A copy of the VIN to guest map: whether this account reaches each car
    /// as a guest (a car shared through "Family and friends"). Filled from
    /// the vehicle list and corrected by the cloud's answers (pyBYD's
    /// `session.guest_vins`).
    pub fn guest_vins(&self) -> HashMap<String, bool> {
        lock(&self.guest_vins).clone()
    }

    /// Whether this account is believed to reach `vin` as a guest: pyBYD's
    /// `session.guest_vins.get(vin, False)`.
    pub fn is_guest(&self, vin: &str) -> bool {
        lock(&self.guest_vins).get(vin).copied().unwrap_or(false)
    }

    /// Records whether this account reaches `vin` as a guest: pyBYD's
    /// `session.guest_vins[vin] = guest`.
    pub fn set_guest(&self, vin: &str, guest: bool) {
        lock(&self.guest_vins).insert(vin.to_owned(), guest);
    }

    /// A copy of the (VIN, endpoint) pairs whose refusal the other
    /// `userType` did not fix, so they are not retried the other way again
    /// (pyBYD's `session.user_type_settled`).
    pub fn user_type_settled(&self) -> HashSet<(String, String)> {
        lock(&self.user_type_settled).clone()
    }

    /// Whether the (VIN, endpoint) pair is settled:
    /// `(vin, endpoint) in session.user_type_settled`.
    pub fn is_user_type_settled(&self, vin: &str, endpoint: &str) -> bool {
        lock(&self.user_type_settled).contains(&(vin.to_owned(), endpoint.to_owned()))
    }

    /// Marks the (VIN, endpoint) pair settled:
    /// `session.user_type_settled.add((vin, endpoint))`.
    pub fn settle_user_type(&self, vin: &str, endpoint: &str) {
        lock(&self.user_type_settled).insert((vin.to_owned(), endpoint.to_owned()));
    }

    /// The session as pyBYD's `model_dump(mode="json")` gives it, with
    /// non-finite floats written as strings.
    pub fn to_dump(&self) -> Value {
        json!({
            "user_id": self.user_id,
            "sign_token": self.sign_token,
            "encry_token": self.encry_token,
            "refresh_token": self.refresh_token,
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
        "session.with_refreshed_tokens" => (|| {
            let session = session_argument(&args)?;
            for (vin, guest) in object_argument(&args, "guest_vins")? {
                session.set_guest(vin, guest.as_bool().unwrap_or(false));
            }
            for pair in args
                .value("user_type_settled")
                .as_array()
                .into_iter()
                .flatten()
            {
                if let (Some(vin), Some(endpoint)) = (pair[0].as_str(), pair[1].as_str()) {
                    session.settle_user_type(vin, endpoint);
                }
            }
            let token = |name: &str| -> Result<Option<String>, ParityError> {
                match args.value(name) {
                    Value::Null => Ok(None),
                    value => Ok(Some(lax_str(value, name)?)),
                }
            };
            let sign = token("sign_token")?.unwrap_or_default();
            let encry = token("encry_token")?.unwrap_or_default();
            let refresh = token("refresh_token")?;
            let refreshed =
                session.with_refreshed_tokens(&sign, &encry, refresh.as_deref(), now()?);
            let mut settled: Vec<_> = refreshed.user_type_settled().into_iter().collect();
            settled.sort();
            let guests: std::collections::BTreeMap<_, _> =
                refreshed.guest_vins().into_iter().collect();
            Ok(json!({
                "dump": refreshed.to_dump(),
                "guest_vins": guests,
                "user_type_settled": settled,
            }))
        })(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_expiry_and_guest_state() {
        let session = Session::new(" 42 ", " sign ", "encry", 100.0, 10.0);
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
        assert!(!session.is_guest("TESTVIN0000000001"));
        session.set_guest("TESTVIN0000000001", true);
        assert!(session.is_guest("TESTVIN0000000001"));
        assert!(!session.is_user_type_settled("TESTVIN0000000001", "/x"));
        session.settle_user_type("TESTVIN0000000001", "/x");
        assert!(session.is_user_type_settled("TESTVIN0000000001", "/x"));
        assert!(session.guest_vins()["TESTVIN0000000001"]);
        assert_eq!(session.user_type_settled().len(), 1);
        let copy = session.clone();
        assert_eq!(copy, session);
        copy.set_guest("TESTVIN0000000001", false);
        assert_ne!(copy, session);
        let shown = format!("{session:?}");
        assert!(!shown.contains("sign\"") && !shown.contains("encry\""));
    }

    #[test]
    fn refreshed_tokens_keep_the_guest_state() {
        let session = Session::new("1", "s", "e", 0.0, 60.0).with_refresh_token(" r ");
        assert_eq!(session.refresh_token(), "r");
        session.set_guest("TESTVIN0000000001", true);
        session.settle_user_type("TESTVIN0000000001", "/x");
        let refreshed = session.with_refreshed_tokens("s2", "e2", None, 5.0);
        assert_eq!(refreshed.sign_token(), "s2");
        assert_eq!(refreshed.content_key(), md5_hex("e2"));
        assert_eq!(refreshed.refresh_token(), "r");
        assert_eq!(refreshed.created_at(), 5.0);
        assert_eq!(refreshed.ttl(), 60.0);
        assert!(refreshed.is_guest("TESTVIN0000000001"));
        assert!(refreshed.is_user_type_settled("TESTVIN0000000001", "/x"));
        let renewed = session.with_refreshed_tokens("s3", "e3", Some("r3"), 6.0);
        assert_eq!(renewed.refresh_token(), "r3");
        assert_eq!(renewed.to_dump()["refresh_token"], "r3");
    }

    #[test]
    fn shared_between_threads() {
        fn assert_sync<T: Send + Sync>() {}
        assert_sync::<Session>();
        let session = Session::new("1", "s", "e", 0.0, 1.0);
        std::thread::scope(|scope| {
            for index in 0..4 {
                let session = &session;
                scope.spawn(move || session.set_guest(&format!("V{index}"), true));
            }
        });
        assert_eq!(session.guest_vins().len(), 4);
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
