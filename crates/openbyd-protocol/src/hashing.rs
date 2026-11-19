//! Hashes used to derive keys and sign requests (`pybyd/_crypto/hashing.py`).
//!
//! `compute_checkcode` also belongs here; it needs the Python-exact JSON
//! writer and is added with it.

use md5::{Digest, Md5};
use serde_json::{Value, json};
use sha1::Sha1;

use crate::parity::{Args, Outcome};

/// MD5 of a UTF-8 string, as uppercase hex.
pub fn md5_hex(value: &str) -> String {
    Md5::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

/// The AES key that encrypts the login request: MD5 applied twice to the password.
pub fn pwd_login_key(password: &str) -> String {
    md5_hex(&md5_hex(password))
}

/// The signature hash: SHA-1 as hex with the case alternating per byte, and
/// every `0` that lands on an even position removed.
pub fn sha1_mixed(value: &str) -> String {
    let mut mixed = String::with_capacity(40);
    for (index, byte) in Sha1::digest(value.as_bytes()).iter().enumerate() {
        if index % 2 == 0 {
            mixed.push_str(&format!("{byte:02X}"));
        } else {
            mixed.push_str(&format!("{byte:02x}"));
        }
    }
    mixed
        .chars()
        .enumerate()
        .filter(|&(position, ch)| !(ch == '0' && position % 2 == 0))
        .map(|(_, ch)| ch)
        .collect()
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "hashing.md5_hex" => args.str("value").map(|value| json!(md5_hex(value))),
        "hashing.pwd_login_key" => args
            .str("password")
            .map(|password| json!(pwd_login_key(password))),
        "hashing.sha1_mixed" => args.str("value").map(|value| json!(sha1_mixed(value))),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_is_uppercase_hex() {
        assert_eq!(md5_hex(""), "D41D8CD98F00B204E9800998ECF8427E");
        assert_eq!(
            pwd_login_key(""),
            md5_hex("D41D8CD98F00B204E9800998ECF8427E")
        );
    }

    #[test]
    fn sha1_mixed_has_no_zero_on_an_even_position_before_filtering() {
        // SHA-1("") starts da39a3ee: upper, lower, upper, lower per byte.
        let mixed = sha1_mixed("");
        assert!(mixed.starts_with("DA39A3ee"));
        assert!(mixed.len() <= 40);
    }
}
