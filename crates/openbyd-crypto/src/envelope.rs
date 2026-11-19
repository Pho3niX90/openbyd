use crate::cbc::{BLOCK_SIZE, decrypt_cbc, encrypt_cbc};
use crate::{Error, Tables, base64};

const ZERO_IV: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
const PREFIX: char = 'F';

fn pad(data: &[u8]) -> Vec<u8> {
    let fill = BLOCK_SIZE - data.len() % BLOCK_SIZE;
    let mut padded = Vec::with_capacity(data.len() + fill);
    padded.extend_from_slice(data);
    padded.resize(data.len() + fill, fill as u8);
    padded
}

/// Removes PKCS#7 padding. Data whose padding is not valid is returned as
/// it is, which is what pyBYD does.
fn unpad(mut data: Vec<u8>) -> Vec<u8> {
    let Some(&last) = data.last() else {
        return data;
    };
    let fill = usize::from(last);
    if fill == 0 || fill > BLOCK_SIZE || fill > data.len() {
        return data;
    }
    if data[data.len() - fill..].iter().all(|&byte| byte == last) {
        data.truncate(data.len() - fill);
    }
    data
}

/// True for the characters Python's `str.strip()` removes.
fn is_python_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

/// Wraps `plaintext` in an envelope, using the tables shipped with this crate.
pub fn encode_envelope(plaintext: &[u8]) -> String {
    encode_envelope_with(Tables::embedded(), plaintext)
}

/// Wraps `plaintext` in an envelope: `F` followed by base64 of the ciphertext.
pub fn encode_envelope_with(tables: &Tables<'_>, plaintext: &[u8]) -> String {
    let sealed = encrypt_cbc(tables, &pad(plaintext), &ZERO_IV)
        .expect("padded data is a whole number of blocks");
    let mut envelope = String::with_capacity(1 + sealed.len().div_ceil(3) * 4);
    envelope.push(PREFIX);
    envelope.push_str(&base64::encode(&sealed));
    envelope
}

/// Opens an envelope, using the tables shipped with this crate.
pub fn decode_envelope(envelope: &str) -> Result<Vec<u8>, Error> {
    decode_envelope_with(Tables::embedded(), envelope)
}

/// Opens an envelope and returns the plaintext bytes.
///
/// Spaces, tabs and line breaks anywhere in the envelope are ignored, and
/// the URL-safe base64 alphabet and missing `=` padding are accepted.
pub fn decode_envelope_with(tables: &Tables<'_>, envelope: &str) -> Result<Vec<u8>, Error> {
    let cleaned: String = envelope
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '\t' | '\n' | '\r'))
        .collect();
    let cleaned = cleaned.trim_matches(is_python_space);

    if cleaned.is_empty() {
        return Err(Error::EmptyInput);
    }
    let Some(body) = cleaned.strip_prefix(PREFIX) else {
        return Err(Error::MissingPrefix);
    };
    if !body.is_ascii() {
        return Err(Error::InvalidBase64("only ASCII characters are allowed"));
    }

    let mut text: Vec<u8> = body
        .bytes()
        .map(|byte| match byte {
            b'-' => b'+',
            b'_' => b'/',
            other => other,
        })
        .collect();
    while !text.len().is_multiple_of(4) {
        text.push(b'=');
    }

    let sealed = base64::decode_lenient(&text).map_err(Error::InvalidBase64)?;
    if sealed.is_empty() {
        return Err(Error::EmptyCiphertext);
    }
    Ok(unpad(decrypt_cbc(tables, &sealed, &ZERO_IV)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_length_around_a_block() {
        for len in 0..=65 {
            let plain: Vec<u8> = (0..len).map(|i| (i * 7 + len) as u8 | 0x20).collect();
            let envelope = encode_envelope(&plain);
            assert!(envelope.starts_with(PREFIX));
            assert_eq!(decode_envelope(&envelope).unwrap(), plain, "length {len}");
        }
    }

    #[test]
    fn padding_always_adds_at_least_one_byte() {
        assert_eq!(pad(&[]).len(), 16);
        assert_eq!(pad(&[1; 15]).len(), 16);
        assert_eq!(pad(&[1; 16]).len(), 32);
    }

    #[test]
    fn invalid_padding_is_left_alone() {
        assert_eq!(unpad(vec![1, 2, 3, 0]), vec![1, 2, 3, 0]);
        assert_eq!(unpad(vec![1, 2, 3, 17]), vec![1, 2, 3, 17]);
        assert_eq!(unpad(vec![1, 2, 3, 2]), vec![1, 2, 3, 2]);
        assert_eq!(unpad(vec![9, 2, 2]), vec![9]);
        assert_eq!(unpad(vec![]), Vec::<u8>::new());
    }

    #[test]
    fn reports_what_is_wrong_with_an_envelope() {
        assert_eq!(decode_envelope(""), Err(Error::EmptyInput));
        assert_eq!(decode_envelope(" \n\t "), Err(Error::EmptyInput));
        assert_eq!(decode_envelope("Zm9v"), Err(Error::MissingPrefix));
        assert_eq!(decode_envelope("F"), Err(Error::EmptyCiphertext));
        assert!(matches!(
            decode_envelope("FZ"),
            Err(Error::InvalidBase64(_))
        ));
        assert!(matches!(
            decode_envelope("Fé"),
            Err(Error::InvalidBase64(_))
        ));
        assert_eq!(decode_envelope("FZm9v"), Err(Error::BadDataLength(3)));
    }
}
