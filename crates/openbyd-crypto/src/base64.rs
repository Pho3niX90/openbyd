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

const fn sextet(ch: u8) -> Option<u8> {
    match ch {
        b'A'..=b'Z' => Some(ch - b'A'),
        b'a'..=b'z' => Some(ch - b'a' + 26),
        b'0'..=b'9' => Some(ch - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Marks a byte outside the alphabet in [`SEXTETS`].
const NOT_A_SEXTET: u8 = 0xFF;

/// [`sextet`] of every byte, or [`NOT_A_SEXTET`].
const SEXTETS: [u8; 256] = {
    let mut table = [NOT_A_SEXTET; 256];
    let mut byte = 0;
    while byte < 256 {
        if let Some(value) = sextet(byte as u8) {
            table[byte] = value;
        }
        byte += 1;
    }
    table
};

/// Decodes the leading groups of four alphabet characters into `out` and
/// returns how many bytes of `text` they are. Stops at the first group with
/// anything else in it (padding, a skipped character), which is left to the
/// character-by-character decoder; between whole groups that decoder
/// carries no state, so it can take over there.
fn decode_whole_groups(text: &[u8], out: &mut Vec<u8>) -> usize {
    let mut consumed = 0;
    for group in text.as_chunks::<4>().0 {
        let [a, b, c, d] = group.map(|ch| SEXTETS[usize::from(ch)]);
        if (a | b | c | d) & 0x80 != 0 {
            break;
        }
        out.extend_from_slice(&[a << 2 | b >> 4, b << 4 | c >> 2, c << 6 | d]);
        consumed += 4;
    }
    consumed
}

pub(crate) fn decode_lenient(text: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let whole = decode_whole_groups(text, &mut out);
    let mut position = 0u8;
    let mut carry = 0u8;
    let mut pads = 0u8;

    for &ch in &text[whole..] {
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

    /// The decoder as it was before whole groups were decoded first.
    fn decode_one_by_one(text: &[u8]) -> Result<Vec<u8>, &'static str> {
        let mut out = Vec::new();
        let (mut position, mut carry, mut pads) = (0u8, 0u8, 0u8);
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

    #[test]
    fn whole_groups_stop_at_the_first_group_with_anything_else() {
        let consumed = |text: &[u8]| {
            let mut out = Vec::new();
            (decode_whole_groups(text, &mut out), out)
        };
        assert_eq!(consumed(b"Zm9vYmFy"), (8, b"foobar".to_vec()));
        assert_eq!(consumed(b"Zm9v!YmFy"), (4, b"foo".to_vec()));
        assert_eq!(consumed(b"Zm9vYg=="), (4, b"foo".to_vec()));
        assert_eq!(consumed(b"Zm9vYmF"), (4, b"foo".to_vec()));
        assert_eq!(consumed(b"=m9vYmFy"), (0, Vec::new()));
        assert_eq!(consumed(b"Zm9\xffYmFy"), (0, Vec::new()));
        assert_eq!(consumed(b""), (0, Vec::new()));
    }

    #[test]
    fn whole_groups_first_decodes_as_one_by_one() {
        // Mostly alphabet characters, with padding and skipped characters
        // (also bytes over 127) scattered at every offset within a group.
        let pool = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let odd = [b'=', b'=', b'!', b' ', b'-', b'_', 0x00, 0x80, 0xFF];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for case in 0..20_000 {
            let len = (next() % 40) as usize;
            let rate = [0, 1, 4, 16][case % 4];
            let text: Vec<u8> = (0..len)
                .map(|_| {
                    let roll = next();
                    if roll % 32 < rate {
                        odd[(roll >> 8) as usize % odd.len()]
                    } else {
                        pool[(roll >> 8) as usize % pool.len()]
                    }
                })
                .collect();
            assert_eq!(decode_lenient(&text), decode_one_by_one(&text), "{text:?}");
        }
        for byte in 0..=255u8 {
            assert_eq!(
                SEXTETS[usize::from(byte)] != NOT_A_SEXTET,
                sextet(byte).is_some()
            );
            assert!(sextet(byte).is_none_or(|value| value < 64));
        }
    }

    #[test]
    fn rejects_incomplete_groups() {
        assert!(decode_lenient(b"Z").is_err());
        assert!(decode_lenient(b"Zg").is_err());
        assert!(decode_lenient(b"Zg=").is_err());
    }
}
