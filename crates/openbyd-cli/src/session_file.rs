//! The session file written by `login --save-session` and read by `--session`.
//!
//! It holds the session tokens, which are secrets, so it is created readable
//! by its owner only wherever the platform allows (mode 0600 on Unix; on
//! Windows it keeps the folder's access rules). Its contents are never
//! printed.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use openbyd_client::client::SessionTokens;
use serde_json::{Map, Value, json};

const FORMAT: i64 = 1;

/// A saved session and the account it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedSession {
    /// The login name. The device identity sent with every request is
    /// derived from it, so a restored session needs the same one.
    pub username: String,
    /// The tokens, with the lifetime they had left when saved.
    pub tokens: SessionTokens,
    /// Unix seconds when the file was written.
    pub saved_at: u64,
}

/// Why a session file could not be used.
#[derive(Debug, PartialEq)]
pub enum LoadError {
    /// The file could not be read or is not a session file.
    Unusable(String),
    /// The session ran out while the file lay on disk.
    Expired,
}

/// The current time in Unix seconds.
pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Writes `session` to `path`, replacing the file, owner-only where possible.
pub fn save(path: &Path, session: &SavedSession) -> std::io::Result<()> {
    let guest: Map<String, Value> = session
        .tokens
        .guest_vins
        .iter()
        .map(|(vin, guest)| (vin.clone(), Value::Bool(*guest)))
        .collect();
    let document = json!({
        "format": FORMAT,
        "username": session.username,
        "user_id": session.tokens.user_id,
        "sign_token": session.tokens.sign_token,
        "encry_token": session.tokens.encry_token,
        "ttl_remaining": session.tokens.ttl_remaining,
        "saved_at": session.saved_at,
        "guest_vins": guest,
    });
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // An existing file keeps its old mode when opened; tighten it.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(document.to_string().as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()
}

/// Reads a session file and works out how much lifetime is left at `now`.
pub fn load(path: &Path, now: u64) -> Result<SavedSession, LoadError> {
    let unusable = |why: &str| LoadError::Unusable(format!("{}: {why}", path.display()));
    let text = std::fs::read_to_string(path).map_err(|error| unusable(&error.to_string()))?;
    let document: Value =
        serde_json::from_str(&text).map_err(|_| unusable("not a session file (not JSON)"))?;
    if document["format"] != json!(FORMAT) {
        return Err(unusable("not a session file (unknown format)"));
    }
    let text_field = |name: &str| {
        document[name]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| unusable(&format!("not a session file (no {name})")))
    };
    let saved_at = document["saved_at"].as_u64().unwrap_or(now);
    let ttl_remaining = match &document["ttl_remaining"] {
        Value::Null => None,
        value => {
            let saved = value
                .as_f64()
                .ok_or_else(|| unusable("not a session file (bad ttl_remaining)"))?;
            let left = saved - now.saturating_sub(saved_at) as f64;
            if left <= 0.0 {
                return Err(LoadError::Expired);
            }
            Some(left)
        }
    };
    let guest_vins: HashMap<String, bool> = document["guest_vins"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(vin, guest)| Some((vin.clone(), guest.as_bool()?)))
                .collect()
        })
        .unwrap_or_default();
    Ok(SavedSession {
        username: text_field("username")?,
        tokens: SessionTokens {
            user_id: text_field("user_id")?,
            sign_token: text_field("sign_token")?,
            encry_token: text_field("encry_token")?,
            ttl_remaining,
            guest_vins,
        },
        saved_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ttl: Option<f64>) -> SavedSession {
        SavedSession {
            username: "user@example.com".into(),
            tokens: SessionTokens {
                user_id: "u1".into(),
                sign_token: "sign-token-value".into(),
                encry_token: "encry-token-value".into(),
                ttl_remaining: ttl,
                guest_vins: HashMap::from([("TESTVIN0000000001".to_owned(), true)]),
            },
            saved_at: 1_000,
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tmp"))
            .join("session-file-unit");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn a_session_round_trips_and_ages() {
        let path = scratch("round-trip.json");
        save(&path, &sample(Some(600.0))).unwrap();
        // Saving again replaces the file.
        save(&path, &sample(Some(600.0))).unwrap();
        let loaded = load(&path, 1_100).unwrap();
        assert_eq!(loaded.tokens.ttl_remaining, Some(500.0));
        assert_eq!(loaded.tokens.sign_token, "sign-token-value");
        assert_eq!(loaded.username, "user@example.com");
        assert_eq!(loaded.tokens.guest_vins.len(), 1);
        assert_eq!(load(&path, 1_600), Err(LoadError::Expired));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_session_without_expiry_never_runs_out() {
        let path = scratch("no-expiry.json");
        save(&path, &sample(None)).unwrap();
        assert_eq!(load(&path, u64::MAX).unwrap().tokens.ttl_remaining, None);
    }

    #[test]
    fn bad_files_are_refused() {
        let missing = scratch("missing.json");
        let _ = std::fs::remove_file(&missing);
        assert!(matches!(load(&missing, 0), Err(LoadError::Unusable(_))));
        for (name, text) in [
            ("junk.json", "not json"),
            ("format.json", r#"{"format": 2}"#),
            ("fields.json", r#"{"format": 1, "username": "a"}"#),
            (
                "ttl.json",
                r#"{"format":1,"username":"a","user_id":"b","sign_token":"c","encry_token":"d","ttl_remaining":"x"}"#,
            ),
        ] {
            let path = scratch(name);
            std::fs::write(&path, text).unwrap();
            assert!(
                matches!(load(&path, 0), Err(LoadError::Unusable(_))),
                "{name}"
            );
        }
        // No guest map and no `saved_at` still loads.
        let path = scratch("minimal.json");
        std::fs::write(
            &path,
            r#"{"format":1,"username":"a","user_id":"b","sign_token":"c","encry_token":"d","ttl_remaining":null}"#,
        )
        .unwrap();
        let loaded = load(&path, 5).unwrap();
        assert!(loaded.tokens.guest_vins.is_empty());
        assert_eq!(loaded.saved_at, 5);
        assert!(now_seconds() > 1_000_000_000);
    }
}
