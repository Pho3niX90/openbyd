//! AES-CBC for the inner payload (`pybyd/_crypto/aes.py`).
//!
//! The inner request and reply payloads are AES-CBC with a zero IV and
//! PKCS#7 padding, written as uppercase hex. pyBYD's function names say
//! AES-128, but a 24- or 32-byte key selects AES-192 or AES-256.

use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Aes192, Aes256};
use serde_json::{Value, json};

use crate::error::{Error, ErrorKind};
use crate::parity::{Args, Outcome, ParityError};

const BLOCK: usize = 16;

/// Encrypts `plaintext` (as UTF-8) with the hex key `key_hex`, zero IV and
/// PKCS#7 padding, and returns the ciphertext as uppercase hex.
///
/// The key is hex for 16, 24 or 32 bytes; surrounding whitespace and a
/// `0x` prefix are accepted. Fails with [`ErrorKind::BydCryptoError`].
pub fn aes_encrypt_hex(plaintext: &str, key_hex: &str) -> Result<String, Error> {
    let cipher = Cipher::new(&parse_key(key_hex)?);
    let mut data = plaintext.as_bytes().to_vec();
    let fill = BLOCK - data.len() % BLOCK;
    data.resize(data.len() + fill, fill as u8);
    let mut previous = [0u8; BLOCK];
    for chunk in data.as_chunks_mut::<BLOCK>().0 {
        for (byte, prior) in chunk.iter_mut().zip(previous) {
            *byte ^= prior;
        }
        cipher.encrypt(chunk);
        previous.copy_from_slice(chunk);
    }
    Ok(data.iter().map(|byte| format!("{byte:02X}")).collect())
}

/// Decrypts uppercase or lowercase hex `cipher_hex` with the hex key
/// `key_hex` (zero IV, PKCS#7) and returns the plaintext as UTF-8.
///
/// Fails with [`ErrorKind::BydCryptoError`] for a bad key, ciphertext that
/// is not hex or not whole blocks, bad padding, or plaintext that is not UTF-8.
pub fn aes_decrypt_utf8(cipher_hex: &str, key_hex: &str) -> Result<String, Error> {
    let cipher = Cipher::new(&parse_key(key_hex)?);
    let mut data = parse_hex_bytes(cipher_hex, "AES ciphertext")?;
    if !data.len().is_multiple_of(BLOCK) {
        return Err(crypto_error(
            "AES decryption failed: The length of the provided data is not a multiple of the block length.",
        ));
    }
    let mut previous = [0u8; BLOCK];
    for chunk in data.as_chunks_mut::<BLOCK>().0 {
        let mut sealed = [0u8; BLOCK];
        sealed.copy_from_slice(chunk);
        cipher.decrypt(chunk);
        for (byte, prior) in chunk.iter_mut().zip(previous) {
            *byte ^= prior;
        }
        previous = sealed;
    }
    let fill = usize::from(*data.last().expect("ciphertext is never empty"));
    if fill == 0
        || fill > BLOCK
        || data[data.len() - fill..]
            .iter()
            .any(|&b| b as usize != fill)
    {
        return Err(crypto_error(
            "AES decryption failed: Invalid padding bytes.",
        ));
    }
    data.truncate(data.len() - fill);
    String::from_utf8(data).map_err(|error| crypto_error(format!("AES decryption failed: {error}")))
}

enum Cipher {
    Aes128(Box<Aes128>),
    Aes192(Box<Aes192>),
    Aes256(Box<Aes256>),
}

impl Cipher {
    fn new(key: &[u8]) -> Self {
        match key.len() {
            16 => Cipher::Aes128(Box::new(Aes128::new_from_slice(key).expect("16-byte key"))),
            24 => Cipher::Aes192(Box::new(Aes192::new_from_slice(key).expect("24-byte key"))),
            _ => Cipher::Aes256(Box::new(Aes256::new_from_slice(key).expect("32-byte key"))),
        }
    }

    fn encrypt(&self, chunk: &mut [u8]) {
        let block: &mut aes::Block = chunk.try_into().expect("a 16-byte block");
        match self {
            Cipher::Aes128(cipher) => cipher.encrypt_block(block),
            Cipher::Aes192(cipher) => cipher.encrypt_block(block),
            Cipher::Aes256(cipher) => cipher.encrypt_block(block),
        }
    }

    fn decrypt(&self, chunk: &mut [u8]) {
        let block: &mut aes::Block = chunk.try_into().expect("a 16-byte block");
        match self {
            Cipher::Aes128(cipher) => cipher.decrypt_block(block),
            Cipher::Aes192(cipher) => cipher.decrypt_block(block),
            Cipher::Aes256(cipher) => cipher.decrypt_block(block),
        }
    }
}

fn crypto_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::BydCryptoError, message)
}

fn parse_key(key_hex: &str) -> Result<Vec<u8>, Error> {
    let key = parse_hex_bytes(key_hex, "AES key")?;
    if ![16, 24, 32].contains(&key.len()) {
        return Err(crypto_error(format!(
            "AES key must be 16, 24, 32 bytes (got {})",
            key.len()
        )));
    }
    Ok(key)
}

/// True for the characters Python's `str.strip()` removes.
pub(crate) fn is_python_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

/// pyBYD's `_parse_hex_bytes`: strip, drop one `0x`, require an even number
/// of characters, then `bytes.fromhex`, which skips ASCII whitespace
/// before each pair of digits.
fn parse_hex_bytes(value: &str, name: &str) -> Result<Vec<u8>, Error> {
    let mut text = value.trim_matches(is_python_space);
    if let Some(rest) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        text = rest;
    }
    if text.is_empty() {
        return Err(crypto_error(format!("{name} is empty")));
    }
    let length = text.chars().count();
    if !length.is_multiple_of(2) {
        return Err(crypto_error(format!(
            "{name} hex length must be even (got {length})"
        )));
    }
    let not_hex = || crypto_error(format!("{name} must be hex-encoded"));
    let mut bytes = Vec::with_capacity(length / 2);
    let mut chars = text.chars().peekable();
    loop {
        while chars
            .next_if(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'))
            .is_some()
        {}
        let Some(high) = chars.next() else { break };
        let high = high.to_digit(16).ok_or_else(not_hex)?;
        let low = chars
            .next()
            .and_then(|ch| ch.to_digit(16))
            .ok_or_else(not_hex)?;
        bytes.push((high * 16 + low) as u8);
    }
    Ok(bytes)
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "aes.aes_encrypt_hex" => (|| {
            let text = aes_encrypt_hex(args.str("plaintext")?, args.str("key_hex")?)?;
            Ok::<_, ParityError>(json!(text))
        })(),
        "aes.aes_decrypt_utf8" => (|| {
            let text = aes_decrypt_utf8(args.str("cipher_hex")?, args.str("key_hex")?)?;
            Ok::<_, ParityError>(json!(text))
        })(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "000102030405060708090A0B0C0D0E0F";

    #[test]
    fn round_trips_with_every_key_size() {
        for key in [
            KEY.to_owned(),
            format!("{KEY}0001020304050607"),
            format!("{KEY}{KEY}"),
        ] {
            for text in ["", "a", "exactly sixteen!", "Zoë 🚗 比亚迪"] {
                let sealed = aes_encrypt_hex(text, &key).unwrap();
                assert_eq!(sealed, sealed.to_uppercase());
                assert_eq!(aes_decrypt_utf8(&sealed, &key).unwrap(), text);
            }
        }
    }

    #[test]
    fn known_answer() {
        // AES-128 zero key, zero IV, one full padding block.
        assert_eq!(
            aes_encrypt_hex("", "00000000000000000000000000000000").unwrap(),
            "0143DB63EE66B0CDFF9F69917680151E"
        );
    }

    #[test]
    fn rejects_bad_input() {
        let kind = |result: Result<String, Error>| result.unwrap_err().kind();
        assert_eq!(kind(aes_encrypt_hex("x", "")), ErrorKind::BydCryptoError);
        assert_eq!(kind(aes_encrypt_hex("x", "0x")), ErrorKind::BydCryptoError);
        assert_eq!(kind(aes_encrypt_hex("x", "abc")), ErrorKind::BydCryptoError);
        assert_eq!(kind(aes_encrypt_hex("x", "zz")), ErrorKind::BydCryptoError);
        assert_eq!(
            kind(aes_encrypt_hex("x", "a b ")),
            ErrorKind::BydCryptoError
        );
        assert_eq!(kind(aes_encrypt_hex("x", "00")), ErrorKind::BydCryptoError);
        assert_eq!(kind(aes_decrypt_utf8("00", KEY)), ErrorKind::BydCryptoError);
        let sealed = aes_encrypt_hex("x", KEY).unwrap();
        assert_eq!(
            aes_decrypt_utf8(&format!(" 0x{sealed}\n"), &format!("0X{KEY}")).unwrap(),
            "x"
        );
        let spaced: String = KEY
            .as_bytes()
            .chunks(2)
            .map(|p| format!("{}  ", std::str::from_utf8(p).unwrap()))
            .collect();
        assert_eq!(aes_decrypt_utf8(&sealed, spaced.trim_end()).unwrap(), "x");
        assert!(aes_decrypt_utf8(&"00".repeat(16), KEY).is_err());
    }
}
