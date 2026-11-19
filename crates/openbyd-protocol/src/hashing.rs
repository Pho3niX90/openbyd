//! Hashes used to derive keys and sign requests (`pybyd/_crypto/hashing.py`).

use md5::{Digest, Md5};
use serde_json::{Value, json};
use sha1::Sha1;

use crate::parity::{Args, Outcome};

/// Bytes as hex, two digits per byte, upper or lower case.
pub(crate) fn hex(bytes: &[u8], upper: bool) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    push_hex(&mut text, bytes, upper);
    text
}

/// Appends `bytes` as hex to `text`.
fn push_hex(text: &mut String, bytes: &[u8], upper: bool) {
    let digits: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    for byte in bytes {
        text.push(char::from(digits[usize::from(byte >> 4)]));
        text.push(char::from(digits[usize::from(byte & 0x0f)]));
    }
}

/// MD5 of a UTF-8 string, as uppercase hex.
pub fn md5_hex(value: &str) -> String {
    hex(&Md5::digest(value.as_bytes()), true)
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
        push_hex(&mut mixed, std::slice::from_ref(byte), index % 2 == 0);
    }
    mixed
        .chars()
        .enumerate()
        .filter(|&(position, ch)| !(ch == '0' && position % 2 == 0))
        .map(|(_, ch)| ch)
        .collect()
}

/// The `checkcode` field of an outer payload: MD5 (lowercase hex) of the
/// payload as `json.dumps(payload, separators=(",", ":"), ensure_ascii=False)`,
/// with its four 8-character chunks reordered 4, 2, 3, 1.
pub fn compute_checkcode(payload: &Value) -> String {
    let text = crate::pyjson::dumps(payload, crate::pyjson::DumpOptions::COMPACT_UNICODE);
    let md5 = hex(&Md5::digest(text.as_bytes()), false);
    [&md5[24..32], &md5[8..16], &md5[16..24], &md5[0..8]].concat()
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "hashing.md5_hex" => args.str("value").map(|value| json!(md5_hex(value))),
        "hashing.pwd_login_key" => args
            .str("password")
            .map(|password| json!(pwd_login_key(password))),
        "hashing.sha1_mixed" => args.str("value").map(|value| json!(sha1_mixed(value))),
        "hashing.compute_checkcode" => Ok(json!(compute_checkcode(args.value("payload")))),
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

    #[test]
    fn checkcode_reorders_the_md5_chunks() {
        // MD5("{}") = 99914b932bd37a50b983c5e7c90ae93b
        assert_eq!(
            compute_checkcode(&json!({})),
            "c90ae93b2bd37a50b983c5e799914b93"
        );
    }
}
