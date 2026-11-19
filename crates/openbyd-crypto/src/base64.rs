//! Base64 with the same leniency as Python's `base64.b64decode`.
//!
//! pyBYD decodes envelopes with Python's default, non-validating decoder.
//! It skips characters outside the alphabet and stops at the first complete
//! padding, so this decoder does the same and accepts exactly what pyBYD
//! accepts.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const PAD: u8 = b'=';

pub(crate) fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let sextets = [
            bytes[0] >> 2,
            (bytes[0] & 0x03) << 4 | bytes[1] >> 4,
            (bytes[1] & 0x0F) << 2 | bytes[2] >> 6,
            bytes[2] & 0x3F,
        ];
        for (i, sextet) in sextets.iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[usize::from(*sextet)]));
            } else {
                out.push(char::from(PAD));
            }
        }
    }
    out
}

fn sextet(ch: u8) -> Option<u8> {
    match ch {
        b'A'..=b'Z' => Some(ch - b'A'),
        b'a'..=b'z' => Some(ch - b'a' + 26),
        b'0'..=b'9' => Some(ch - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub(crate) fn decode_lenient(text: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let mut position = 0u8;
    let mut carry = 0u8;
    let mut pads = 0u8;

    for &ch in text {
        if ch == PAD {
            if position >= 2 {
                pads += 1;
                if position + pads >= 4 {
                    return Ok(out);
                }
            }
            continue;
        }
        let Some(value) = sextet(ch) else { continue };
        pads = 0;
        match position {
            0 => carry = value,
            1 => {
                out.push(carry << 2 | value >> 4);
                carry = value & 0x0F;
            }
            2 => {
                out.push(carry << 4 | value >> 2);
                carry = value & 0x03;
            }
            _ => out.push(carry << 6 | value),
        }
        position = (position + 1) & 3;
    }

    match position {
        0 => Ok(out),
        1 => Err("number of data characters cannot be 1 more than a multiple of 4"),
        _ => Err("incorrect padding"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_rfc_4648_examples() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in cases {
            assert_eq!(encode(plain.as_bytes()), encoded);
            assert_eq!(
                decode_lenient(encoded.as_bytes()).unwrap(),
                plain.as_bytes()
            );
        }
    }

    #[test]
    fn skips_characters_outside_the_alphabet() {
        assert_eq!(decode_lenient(b"Zm9v!Ym*Fy").unwrap(), b"foobar");
    }

    #[test]
    fn stops_at_complete_padding() {
        assert_eq!(decode_lenient(b"Zg==Zm9v").unwrap(), b"f");
    }

    #[test]
    fn rejects_incomplete_groups() {
        assert!(decode_lenient(b"Z").is_err());
        assert!(decode_lenient(b"Zg").is_err());
        assert!(decode_lenient(b"Zg=").is_err());
    }
}
