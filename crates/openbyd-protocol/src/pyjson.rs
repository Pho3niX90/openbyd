//! JSON text exactly as Python's `json.dumps` writes it, and `json.loads` as far as serde_json can hold the result.
//!
//! BYD signs and checksums JSON *text*, so every byte matters: key order,
//! separators, which characters are escaped and how floats are written.
//! pyBYD writes that text with `json.dumps`; [`dumps`] writes the same.
//!
//! [`loads`] reads JSON the way `json.loads` does where serde_json can hold
//! the value. It differs from Python for `NaN`/`Infinity` tokens, numbers
//! out of `f64` range, integers outside `i64`/`u64` (Python keeps them
//! exact up to its 4,300-digit limit, serde_json turns them into floats),
//! lone surrogate escapes and nesting deeper than 127 levels.

use std::fmt::Write as _;

use serde_json::{Number, Value, json};

use crate::error::{Error, ErrorKind};
use crate::parity::{Args, Outcome, ParityError};

/// The separators `json.dumps` puts between items and between a key and its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Separators {
    /// `separators=(",", ":")`: no spaces. What pyBYD signs and sends.
    Compact,
    /// Python's default for `indent=None`: `", "` and `": "`.
    Default,
}

/// The `json.dumps` arguments that change the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DumpOptions {
    /// Escape every character outside printable ASCII as `\uXXXX`.
    pub ensure_ascii: bool,
    /// Item and key separators.
    pub separators: Separators,
    /// Write object keys sorted by code point instead of in insertion order.
    pub sort_keys: bool,
}

impl DumpOptions {
    /// `json.dumps(value, separators=(",", ":"))`: compact, ASCII only.
    pub const COMPACT_ASCII: Self = Self {
        ensure_ascii: true,
        separators: Separators::Compact,
        sort_keys: false,
    };
    /// `json.dumps(value, separators=(",", ":"), ensure_ascii=False)`.
    pub const COMPACT_UNICODE: Self = Self {
        ensure_ascii: false,
        separators: Separators::Compact,
        sort_keys: false,
    };
    /// `json.dumps(value)`: Python's defaults.
    pub const DEFAULT: Self = Self {
        ensure_ascii: true,
        separators: Separators::Default,
        sort_keys: false,
    };
}

impl Default for DumpOptions {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Serialises `value` to the text `json.dumps` gives with `options`.
pub fn dumps(value: &Value, options: DumpOptions) -> String {
    let mut out = String::new();
    write_value(&mut out, value, options);
    out
}

/// `json.dumps(value, separators=(",", ":"))`, the form pyBYD encrypts and sends.
pub fn dumps_compact(value: &Value) -> String {
    dumps(value, DumpOptions::COMPACT_ASCII)
}

fn write_value(out: &mut String, value: &Value, options: DumpOptions) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => write_number(out, number),
        Value::String(text) => write_string(out, text, options.ensure_ascii),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(item_separator(options));
                }
                write_value(out, item, options);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            if options.sort_keys {
                let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
                // Rust orders strings by UTF-8 bytes, which is code point order, as Python does.
                sorted.sort_by(|a, b| a.0.cmp(b.0));
                write_entries(out, sorted, options);
            } else {
                write_entries(out, map, options);
            }
            out.push('}');
        }
    }
}

fn write_entries<'a>(
    out: &mut String,
    entries: impl IntoIterator<Item = (&'a String, &'a Value)>,
    options: DumpOptions,
) {
    for (index, (key, item)) in entries.into_iter().enumerate() {
        if index > 0 {
            out.push_str(item_separator(options));
        }
        write_string(out, key, options.ensure_ascii);
        out.push_str(match options.separators {
            Separators::Compact => ":",
            Separators::Default => ": ",
        });
        write_value(out, item, options);
    }
}

fn item_separator(options: DumpOptions) -> &'static str {
    match options.separators {
        Separators::Compact => ",",
        Separators::Default => ", ",
    }
}

fn write_number(out: &mut String, number: &Number) {
    if number.is_i64() || number.is_u64() {
        // serde_json writes integers exactly as Python does.
        let _ = write!(out, "{number}");
    } else {
        push_float_repr(out, number.as_f64().unwrap_or(f64::NAN));
    }
}

/// A float written the way Python's `repr(float)` writes it, which is also
/// what `json.dumps` writes: shortest round-trip digits, `1.0` rather than
/// `1`, scientific notation from `1e+16` up and from `1e-05` down. Infinities
/// and NaN are written as `json.dumps` writes them.
pub fn float_repr(value: f64) -> String {
    let mut out = String::new();
    push_float_repr(&mut out, value);
    out
}

/// [`float_repr`], appended to `out`.
fn push_float_repr(out: &mut String, value: f64) {
    if value.is_nan() {
        out.push_str("NaN");
        return;
    }
    if value.is_infinite() {
        out.push_str(if value > 0.0 { "Infinity" } else { "-Infinity" });
        return;
    }
    // `{:e}` gives the shortest digits that round-trip, as Python's repr does.
    let mut buffer = [0u8; 40];
    let scientific = shortest_scientific(value, &mut buffer);
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("LowerExp output has an exponent");
    let exponent: i32 = exponent.parse().expect("LowerExp exponent is an integer");
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    // The significant digits are the mantissa without its point: `d` or `d.ddd`.
    let (first, rest) = mantissa.split_at(1);
    let rest = rest.strip_prefix('.').unwrap_or(rest);
    let count = 1 + rest.len();
    // `digits[from..to]` of the significant digits, without building them.
    let push_digits = |out: &mut String, from: usize, to: usize| {
        if from == 0 && to > 0 {
            out.push_str(first);
        }
        out.push_str(&rest[from.max(1) - 1..to.max(1) - 1]);
    };
    // Position of the decimal point relative to the digits, as in Python's
    // `float_repr_style == 'short'`: value = 0.DIGITS * 10**decimal_point.
    let decimal_point = exponent + 1;
    out.push_str(sign);
    if !(-4 < decimal_point && decimal_point <= 16) {
        out.push_str(first);
        if count > 1 {
            out.push('.');
            out.push_str(rest);
        }
        let _ = write!(
            out,
            "e{}{:02}",
            if exponent < 0 { '-' } else { '+' },
            exponent.unsigned_abs()
        );
    } else if decimal_point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n(
            '0',
            decimal_point.unsigned_abs() as usize,
        ));
        push_digits(out, 0, count);
    } else {
        let point = decimal_point as usize;
        if point >= count {
            push_digits(out, 0, count);
            out.extend(std::iter::repeat_n('0', point - count));
            out.push_str(".0");
        } else {
            push_digits(out, 0, point);
            out.push('.');
            push_digits(out, point, count);
        }
    }
}

/// Formats into a fixed buffer; `{:e}` of an `f64` needs at most 25 bytes.
fn format_into<'b>(buffer: &'b mut [u8; 40], arguments: std::fmt::Arguments<'_>) -> &'b str {
    let mut cursor = std::io::Cursor::new(&mut buffer[..]);
    std::io::Write::write_fmt(&mut cursor, arguments).expect("a float fits 40 bytes");
    let length = cursor.position() as usize;
    std::str::from_utf8(&buffer[..length]).expect("formatting writes UTF-8")
}

/// The fewest significant digits that read back as `value` and, among
/// those, the ones closest to it, as Python's `repr` picks them. Rust's
/// `{:e}` finds the fewest digits but not always the closest last digit
/// (`153818817446116.12` comes out as `...13`), so the digits are redone
/// with exact rounding at that length. With 15 digits or fewer there is
/// nothing to redo: two such decimals are further apart than two doubles,
/// so only one of that length reads back as `value`, and it is the closest.
fn shortest_scientific(value: f64, buffer: &mut [u8; 40]) -> &str {
    let length = format_into(buffer, format_args!("{value:e}")).len();
    let digits = buffer[..length]
        .iter()
        .take_while(|&&byte| byte != b'e')
        .filter(|byte| byte.is_ascii_digit())
        .count();
    if digits <= 15 {
        return std::str::from_utf8(&buffer[..length]).expect("formatting writes UTF-8");
    }
    let mut closest = [0u8; 40];
    let closest = format_into(&mut closest, format_args!("{value:.*e}", digits - 1));
    if closest.parse::<f64>() != Ok(value) {
        return std::str::from_utf8(&buffer[..length]).expect("formatting writes UTF-8");
    }
    buffer[..closest.len()].copy_from_slice(closest.as_bytes());
    std::str::from_utf8(&buffer[..closest.len()]).expect("copied from a str")
}

fn write_string(out: &mut String, text: &str, ensure_ascii: bool) {
    out.push('"');
    // Copy runs that need no escaping in one go; escape from the first byte that does.
    let plain =
        |byte: u8| byte >= 0x20 && byte != b'"' && byte != b'\\' && !(ensure_ascii && byte >= 0x7f);
    let run = text
        .bytes()
        .position(|byte| !plain(byte))
        .unwrap_or(text.len());
    out.push_str(&text[..run]);
    for ch in text[run..].chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 || (ensure_ascii && (ch as u32) >= 0x7f) => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

/// Parses JSON text as `json.loads` does. See the module docs for the inputs
/// where serde_json cannot hold what Python would return.
///
/// Fails with [`ErrorKind::JsonDecodeError`].
pub fn loads(text: &str) -> Result<Value, Error> {
    if text.starts_with('\u{feff}') {
        return Err(Error::new(
            ErrorKind::JsonDecodeError,
            "Unexpected UTF-8 BOM (decode using utf-8-sig)",
        ));
    }
    serde_json::from_str(&negative_zero_as_int(text))
        .map_err(|error| Error::new(ErrorKind::JsonDecodeError, error.to_string()))
}

/// The deepest container nesting (`[` / `{`) [`loads`] reads: `serde_json`
/// fails at 128 levels.
const MAX_NESTING: usize = 127;

/// `sys.get_int_max_str_digits()`: Python's `json.loads` refuses integer
/// tokens longer than this (sign excluded) with `ValueError`.
const INT_MAX_STR_DIGITS: usize = 4300;

/// Whether a byte ends a non-string JSON token.
fn is_json_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' | b'[' | b']' | b'{' | b'}' | b'"'
    )
}

/// Whether `json.loads` reads `text` into a value [`loads`] cannot give: one
/// holding `NaN` or an infinity (the tokens, or a number beyond `f64`), an
/// integer outside `i64`/`u64` (within Python's 4,300-digit limit), a lone
/// surrogate, or containers nested deeper than [`MAX_NESTING`] levels.
///
/// For a caller that would send the decoded value on, and so must refuse
/// such text instead of sending something else. Text nested deeper than
/// [`MAX_NESTING`] containers answers `true` without checking the rest of
/// the syntax, so it refuses some deep text Python would also reject; that
/// is the safe side, because a plan decoded differently would be saved back
/// differently.
pub fn beyond_serde(text: &str) -> bool {
    // The text with every such token replaced by one serde_json holds. If
    // that parses, Python parses the original.
    let bytes = text.as_bytes();
    let mut plain = String::with_capacity(text.len());
    let mut copied = 0;
    let mut index = 0;
    let mut depth: usize = 0;
    let mut replace = |plain: &mut String, from: usize, to: usize, with: &str| {
        plain.push_str(&text[copied..from]);
        plain.push_str(with);
        copied = to;
    };
    let mut found = false;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index += 1;
                while index < bytes.len() && bytes[index] != b'"' {
                    if bytes[index] != b'\\' {
                        index += 1;
                    } else if let Some(unit) = hex_escape(bytes, index) {
                        let paired = (0xD800..0xDC00).contains(&unit)
                            && hex_escape(bytes, index + 6)
                                .is_some_and(|low| (0xDC00..0xE000).contains(&low));
                        if paired {
                            index += 12;
                        } else {
                            if (0xD800..0xE000).contains(&unit) {
                                found = true;
                                replace(&mut plain, index, index + 6, "\\u0041");
                            }
                            index += 6;
                        }
                    } else {
                        index += 2;
                    }
                }
                index += 1;
            }
            b'[' | b'{' => {
                depth += 1;
                if depth > MAX_NESTING {
                    return true;
                }
                index += 1;
            }
            b']' | b'}' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' => index += 1,
            _ => {
                let end = bytes[index..]
                    .iter()
                    .position(|&byte| is_json_delimiter(byte))
                    .map_or(bytes.len(), |length| index + length);
                let token = &text[index..end];
                if matches!(token, "NaN" | "Infinity" | "-Infinity") || number_beyond_serde(token) {
                    found = true;
                    replace(&mut plain, index, end, "0");
                }
                index = end;
            }
        }
    }
    if !found {
        return false;
    }
    plain.push_str(&text[copied..]);
    serde_json::from_str::<serde::de::IgnoredAny>(&plain).is_ok()
}

/// The code unit of the `\uXXXX` escape starting at `at`, if one is there.
fn hex_escape(bytes: &[u8], at: usize) -> Option<u32> {
    let escape = bytes.get(at..at + 6)?;
    if &escape[..2] != b"\\u" {
        return None;
    }
    let digits = std::str::from_utf8(&escape[2..]).ok()?;
    if !digits.bytes().all(|digit| digit.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// Whether a JSON number token is one Python holds and serde_json does not:
/// an integer outside `i64`/`u64` (within Python's digit limit), or a float
/// that overflows to an infinity.
fn number_beyond_serde(token: &str) -> bool {
    let digits = token.strip_prefix('-').unwrap_or(token);
    let (whole, rest) = digits.split_at(
        digits
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(digits.len()),
    );
    // JSON's integer part: `0`, or digits that do not start with `0`.
    if whole.is_empty() || (whole.len() > 1 && whole.starts_with('0')) {
        return false;
    }
    if rest.is_empty() {
        return whole.len() <= INT_MAX_STR_DIGITS
            && token.parse::<i64>().is_err()
            && token.parse::<u64>().is_err();
    }
    let (fraction, exponent) = match rest.find(['e', 'E']) {
        Some(at) => (&rest[..at], Some(&rest[at + 1..])),
        None => (rest, None),
    };
    let all_digits = |part: &str| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit());
    let fraction_ok = fraction.is_empty() || fraction.strip_prefix('.').is_some_and(all_digits);
    let exponent_ok = exponent
        .is_none_or(|exponent| all_digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)));
    fraction_ok && exponent_ok && token.parse::<f64>().is_ok_and(f64::is_infinite)
}

/// serde_json reads the integer `-0` as the float `-0.0`; Python reads it as
/// the integer `0`. Rewrites each `-0` number token (outside strings) to `0`.
fn negative_zero_as_int(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("-0") {
        return text.into();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        if in_string {
            match (escaped, byte) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
        } else if byte == b'"' {
            in_string = true;
        } else if byte == b'-'
            && (index == 0
                || matches!(
                    bytes[index - 1],
                    b'[' | b',' | b':' | b' ' | b'\t' | b'\n' | b'\r'
                ))
            && bytes.get(index + 1) == Some(&b'0')
            && !matches!(bytes.get(index + 2), Some(b'0'..=b'9' | b'.' | b'e' | b'E'))
        {
            out.push_str(&text[start..index]);
            start = index + 1;
        }
    }
    out.push_str(&text[start..]);
    out.into()
}

/// Python's `str(value)` for a JSON value: `None`, `True`, `1.0`, `['a']`.
///
/// Strings inside lists and objects are written with Python's `repr`, which
/// leaves printable non-ASCII text as it is and escapes the rest, using
/// Python 3.12's idea of printable (Unicode 15).
pub fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => py_repr(other),
    }
}

/// Python's `repr(value)` for a JSON value.
pub fn py_repr(value: &Value) -> String {
    let mut out = String::new();
    write_repr(&mut out, value);
    out
}

fn write_repr(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("None"),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::Number(number) => {
            if number.is_f64() {
                let float = number.as_f64().unwrap_or(f64::NAN);
                if float.is_nan() {
                    out.push_str("nan");
                } else if float.is_infinite() {
                    out.push_str(if float > 0.0 { "inf" } else { "-inf" });
                } else {
                    push_float_repr(out, float);
                }
            } else {
                write_number(out, number);
            }
        }
        Value::String(text) => write_str_repr(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_repr(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_str_repr(out, key);
                out.push_str(": ");
                write_repr(out, item);
            }
            out.push('}');
        }
    }
}

fn write_str_repr(out: &mut String, text: &str) {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            ch if !is_printable(ch) => {
                let code = ch as u32;
                let _ = if code <= 0xff {
                    write!(out, "\\x{code:02x}")
                } else if code <= 0xffff {
                    write!(out, "\\u{code:04x}")
                } else {
                    write!(out, "\\U{code:08x}")
                };
            }
            ch => out.push(ch),
        }
    }
    out.push(quote);
}

/// Code point ranges Python's `str.isprintable` rejects (Unicode 15.0.0):
/// controls, format characters, separators other than the space, surrogates,
/// private use and unassigned code points. Generated from Python 3.12.
const NOT_PRINTABLE: [(u32, u32); 712] = [
    (0x0, 0x1f),
    (0x7f, 0xa0),
    (0xad, 0xad),
    (0x378, 0x379),
    (0x380, 0x383),
    (0x38b, 0x38b),
    (0x38d, 0x38d),
    (0x3a2, 0x3a2),
    (0x530, 0x530),
    (0x557, 0x558),
    (0x58b, 0x58c),
    (0x590, 0x590),
    (0x5c8, 0x5cf),
    (0x5eb, 0x5ee),
    (0x5f5, 0x605),
    (0x61c, 0x61c),
    (0x6dd, 0x6dd),
    (0x70e, 0x70f),
    (0x74b, 0x74c),
    (0x7b2, 0x7bf),
    (0x7fb, 0x7fc),
    (0x82e, 0x82f),
    (0x83f, 0x83f),
    (0x85c, 0x85d),
    (0x85f, 0x85f),
    (0x86b, 0x86f),
    (0x88f, 0x897),
    (0x8e2, 0x8e2),
    (0x984, 0x984),
    (0x98d, 0x98e),
    (0x991, 0x992),
    (0x9a9, 0x9a9),
    (0x9b1, 0x9b1),
    (0x9b3, 0x9b5),
    (0x9ba, 0x9bb),
    (0x9c5, 0x9c6),
    (0x9c9, 0x9ca),
    (0x9cf, 0x9d6),
    (0x9d8, 0x9db),
    (0x9de, 0x9de),
    (0x9e4, 0x9e5),
    (0x9ff, 0xa00),
    (0xa04, 0xa04),
    (0xa0b, 0xa0e),
    (0xa11, 0xa12),
    (0xa29, 0xa29),
    (0xa31, 0xa31),
    (0xa34, 0xa34),
    (0xa37, 0xa37),
    (0xa3a, 0xa3b),
    (0xa3d, 0xa3d),
    (0xa43, 0xa46),
    (0xa49, 0xa4a),
    (0xa4e, 0xa50),
    (0xa52, 0xa58),
    (0xa5d, 0xa5d),
    (0xa5f, 0xa65),
    (0xa77, 0xa80),
    (0xa84, 0xa84),
    (0xa8e, 0xa8e),
    (0xa92, 0xa92),
    (0xaa9, 0xaa9),
    (0xab1, 0xab1),
    (0xab4, 0xab4),
    (0xaba, 0xabb),
    (0xac6, 0xac6),
    (0xaca, 0xaca),
    (0xace, 0xacf),
    (0xad1, 0xadf),
    (0xae4, 0xae5),
    (0xaf2, 0xaf8),
    (0xb00, 0xb00),
    (0xb04, 0xb04),
    (0xb0d, 0xb0e),
    (0xb11, 0xb12),
    (0xb29, 0xb29),
    (0xb31, 0xb31),
    (0xb34, 0xb34),
    (0xb3a, 0xb3b),
    (0xb45, 0xb46),
    (0xb49, 0xb4a),
    (0xb4e, 0xb54),
    (0xb58, 0xb5b),
    (0xb5e, 0xb5e),
    (0xb64, 0xb65),
    (0xb78, 0xb81),
    (0xb84, 0xb84),
    (0xb8b, 0xb8d),
    (0xb91, 0xb91),
    (0xb96, 0xb98),
    (0xb9b, 0xb9b),
    (0xb9d, 0xb9d),
    (0xba0, 0xba2),
    (0xba5, 0xba7),
    (0xbab, 0xbad),
    (0xbba, 0xbbd),
    (0xbc3, 0xbc5),
    (0xbc9, 0xbc9),
    (0xbce, 0xbcf),
    (0xbd1, 0xbd6),
    (0xbd8, 0xbe5),
    (0xbfb, 0xbff),
    (0xc0d, 0xc0d),
    (0xc11, 0xc11),
    (0xc29, 0xc29),
    (0xc3a, 0xc3b),
    (0xc45, 0xc45),
    (0xc49, 0xc49),
    (0xc4e, 0xc54),
    (0xc57, 0xc57),
    (0xc5b, 0xc5c),
    (0xc5e, 0xc5f),
    (0xc64, 0xc65),
    (0xc70, 0xc76),
    (0xc8d, 0xc8d),
    (0xc91, 0xc91),
    (0xca9, 0xca9),
    (0xcb4, 0xcb4),
    (0xcba, 0xcbb),
    (0xcc5, 0xcc5),
    (0xcc9, 0xcc9),
    (0xcce, 0xcd4),
    (0xcd7, 0xcdc),
    (0xcdf, 0xcdf),
    (0xce4, 0xce5),
    (0xcf0, 0xcf0),
    (0xcf4, 0xcff),
    (0xd0d, 0xd0d),
    (0xd11, 0xd11),
    (0xd45, 0xd45),
    (0xd49, 0xd49),
    (0xd50, 0xd53),
    (0xd64, 0xd65),
    (0xd80, 0xd80),
    (0xd84, 0xd84),
    (0xd97, 0xd99),
    (0xdb2, 0xdb2),
    (0xdbc, 0xdbc),
    (0xdbe, 0xdbf),
    (0xdc7, 0xdc9),
    (0xdcb, 0xdce),
    (0xdd5, 0xdd5),
    (0xdd7, 0xdd7),
    (0xde0, 0xde5),
    (0xdf0, 0xdf1),
    (0xdf5, 0xe00),
    (0xe3b, 0xe3e),
    (0xe5c, 0xe80),
    (0xe83, 0xe83),
    (0xe85, 0xe85),
    (0xe8b, 0xe8b),
    (0xea4, 0xea4),
    (0xea6, 0xea6),
    (0xebe, 0xebf),
    (0xec5, 0xec5),
    (0xec7, 0xec7),
    (0xecf, 0xecf),
    (0xeda, 0xedb),
    (0xee0, 0xeff),
    (0xf48, 0xf48),
    (0xf6d, 0xf70),
    (0xf98, 0xf98),
    (0xfbd, 0xfbd),
    (0xfcd, 0xfcd),
    (0xfdb, 0xfff),
    (0x10c6, 0x10c6),
    (0x10c8, 0x10cc),
    (0x10ce, 0x10cf),
    (0x1249, 0x1249),
    (0x124e, 0x124f),
    (0x1257, 0x1257),
    (0x1259, 0x1259),
    (0x125e, 0x125f),
    (0x1289, 0x1289),
    (0x128e, 0x128f),
    (0x12b1, 0x12b1),
    (0x12b6, 0x12b7),
    (0x12bf, 0x12bf),
    (0x12c1, 0x12c1),
    (0x12c6, 0x12c7),
    (0x12d7, 0x12d7),
    (0x1311, 0x1311),
    (0x1316, 0x1317),
    (0x135b, 0x135c),
    (0x137d, 0x137f),
    (0x139a, 0x139f),
    (0x13f6, 0x13f7),
    (0x13fe, 0x13ff),
    (0x1680, 0x1680),
    (0x169d, 0x169f),
    (0x16f9, 0x16ff),
    (0x1716, 0x171e),
    (0x1737, 0x173f),
    (0x1754, 0x175f),
    (0x176d, 0x176d),
    (0x1771, 0x1771),
    (0x1774, 0x177f),
    (0x17de, 0x17df),
    (0x17ea, 0x17ef),
    (0x17fa, 0x17ff),
    (0x180e, 0x180e),
    (0x181a, 0x181f),
    (0x1879, 0x187f),
    (0x18ab, 0x18af),
    (0x18f6, 0x18ff),
    (0x191f, 0x191f),
    (0x192c, 0x192f),
    (0x193c, 0x193f),
    (0x1941, 0x1943),
    (0x196e, 0x196f),
    (0x1975, 0x197f),
    (0x19ac, 0x19af),
    (0x19ca, 0x19cf),
    (0x19db, 0x19dd),
    (0x1a1c, 0x1a1d),
    (0x1a5f, 0x1a5f),
    (0x1a7d, 0x1a7e),
    (0x1a8a, 0x1a8f),
    (0x1a9a, 0x1a9f),
    (0x1aae, 0x1aaf),
    (0x1acf, 0x1aff),
    (0x1b4d, 0x1b4f),
    (0x1b7f, 0x1b7f),
    (0x1bf4, 0x1bfb),
    (0x1c38, 0x1c3a),
    (0x1c4a, 0x1c4c),
    (0x1c89, 0x1c8f),
    (0x1cbb, 0x1cbc),
    (0x1cc8, 0x1ccf),
    (0x1cfb, 0x1cff),
    (0x1f16, 0x1f17),
    (0x1f1e, 0x1f1f),
    (0x1f46, 0x1f47),
    (0x1f4e, 0x1f4f),
    (0x1f58, 0x1f58),
    (0x1f5a, 0x1f5a),
    (0x1f5c, 0x1f5c),
    (0x1f5e, 0x1f5e),
    (0x1f7e, 0x1f7f),
    (0x1fb5, 0x1fb5),
    (0x1fc5, 0x1fc5),
    (0x1fd4, 0x1fd5),
    (0x1fdc, 0x1fdc),
    (0x1ff0, 0x1ff1),
    (0x1ff5, 0x1ff5),
    (0x1fff, 0x200f),
    (0x2028, 0x202f),
    (0x205f, 0x206f),
    (0x2072, 0x2073),
    (0x208f, 0x208f),
    (0x209d, 0x209f),
    (0x20c1, 0x20cf),
    (0x20f1, 0x20ff),
    (0x218c, 0x218f),
    (0x2427, 0x243f),
    (0x244b, 0x245f),
    (0x2b74, 0x2b75),
    (0x2b96, 0x2b96),
    (0x2cf4, 0x2cf8),
    (0x2d26, 0x2d26),
    (0x2d28, 0x2d2c),
    (0x2d2e, 0x2d2f),
    (0x2d68, 0x2d6e),
    (0x2d71, 0x2d7e),
    (0x2d97, 0x2d9f),
    (0x2da7, 0x2da7),
    (0x2daf, 0x2daf),
    (0x2db7, 0x2db7),
    (0x2dbf, 0x2dbf),
    (0x2dc7, 0x2dc7),
    (0x2dcf, 0x2dcf),
    (0x2dd7, 0x2dd7),
    (0x2ddf, 0x2ddf),
    (0x2e5e, 0x2e7f),
    (0x2e9a, 0x2e9a),
    (0x2ef4, 0x2eff),
    (0x2fd6, 0x2fef),
    (0x2ffc, 0x3000),
    (0x3040, 0x3040),
    (0x3097, 0x3098),
    (0x3100, 0x3104),
    (0x3130, 0x3130),
    (0x318f, 0x318f),
    (0x31e4, 0x31ef),
    (0x321f, 0x321f),
    (0xa48d, 0xa48f),
    (0xa4c7, 0xa4cf),
    (0xa62c, 0xa63f),
    (0xa6f8, 0xa6ff),
    (0xa7cb, 0xa7cf),
    (0xa7d2, 0xa7d2),
    (0xa7d4, 0xa7d4),
    (0xa7da, 0xa7f1),
    (0xa82d, 0xa82f),
    (0xa83a, 0xa83f),
    (0xa878, 0xa87f),
    (0xa8c6, 0xa8cd),
    (0xa8da, 0xa8df),
    (0xa954, 0xa95e),
    (0xa97d, 0xa97f),
    (0xa9ce, 0xa9ce),
    (0xa9da, 0xa9dd),
    (0xa9ff, 0xa9ff),
    (0xaa37, 0xaa3f),
    (0xaa4e, 0xaa4f),
    (0xaa5a, 0xaa5b),
    (0xaac3, 0xaada),
    (0xaaf7, 0xab00),
    (0xab07, 0xab08),
    (0xab0f, 0xab10),
    (0xab17, 0xab1f),
    (0xab27, 0xab27),
    (0xab2f, 0xab2f),
    (0xab6c, 0xab6f),
    (0xabee, 0xabef),
    (0xabfa, 0xabff),
    (0xd7a4, 0xd7af),
    (0xd7c7, 0xd7ca),
    (0xd7fc, 0xf8ff),
    (0xfa6e, 0xfa6f),
    (0xfada, 0xfaff),
    (0xfb07, 0xfb12),
    (0xfb18, 0xfb1c),
    (0xfb37, 0xfb37),
    (0xfb3d, 0xfb3d),
    (0xfb3f, 0xfb3f),
    (0xfb42, 0xfb42),
    (0xfb45, 0xfb45),
    (0xfbc3, 0xfbd2),
    (0xfd90, 0xfd91),
    (0xfdc8, 0xfdce),
    (0xfdd0, 0xfdef),
    (0xfe1a, 0xfe1f),
    (0xfe53, 0xfe53),
    (0xfe67, 0xfe67),
    (0xfe6c, 0xfe6f),
    (0xfe75, 0xfe75),
    (0xfefd, 0xff00),
    (0xffbf, 0xffc1),
    (0xffc8, 0xffc9),
    (0xffd0, 0xffd1),
    (0xffd8, 0xffd9),
    (0xffdd, 0xffdf),
    (0xffe7, 0xffe7),
    (0xffef, 0xfffb),
    (0xfffe, 0xffff),
    (0x1000c, 0x1000c),
    (0x10027, 0x10027),
    (0x1003b, 0x1003b),
    (0x1003e, 0x1003e),
    (0x1004e, 0x1004f),
    (0x1005e, 0x1007f),
    (0x100fb, 0x100ff),
    (0x10103, 0x10106),
    (0x10134, 0x10136),
    (0x1018f, 0x1018f),
    (0x1019d, 0x1019f),
    (0x101a1, 0x101cf),
    (0x101fe, 0x1027f),
    (0x1029d, 0x1029f),
    (0x102d1, 0x102df),
    (0x102fc, 0x102ff),
    (0x10324, 0x1032c),
    (0x1034b, 0x1034f),
    (0x1037b, 0x1037f),
    (0x1039e, 0x1039e),
    (0x103c4, 0x103c7),
    (0x103d6, 0x103ff),
    (0x1049e, 0x1049f),
    (0x104aa, 0x104af),
    (0x104d4, 0x104d7),
    (0x104fc, 0x104ff),
    (0x10528, 0x1052f),
    (0x10564, 0x1056e),
    (0x1057b, 0x1057b),
    (0x1058b, 0x1058b),
    (0x10593, 0x10593),
    (0x10596, 0x10596),
    (0x105a2, 0x105a2),
    (0x105b2, 0x105b2),
    (0x105ba, 0x105ba),
    (0x105bd, 0x105ff),
    (0x10737, 0x1073f),
    (0x10756, 0x1075f),
    (0x10768, 0x1077f),
    (0x10786, 0x10786),
    (0x107b1, 0x107b1),
    (0x107bb, 0x107ff),
    (0x10806, 0x10807),
    (0x10809, 0x10809),
    (0x10836, 0x10836),
    (0x10839, 0x1083b),
    (0x1083d, 0x1083e),
    (0x10856, 0x10856),
    (0x1089f, 0x108a6),
    (0x108b0, 0x108df),
    (0x108f3, 0x108f3),
    (0x108f6, 0x108fa),
    (0x1091c, 0x1091e),
    (0x1093a, 0x1093e),
    (0x10940, 0x1097f),
    (0x109b8, 0x109bb),
    (0x109d0, 0x109d1),
    (0x10a04, 0x10a04),
    (0x10a07, 0x10a0b),
    (0x10a14, 0x10a14),
    (0x10a18, 0x10a18),
    (0x10a36, 0x10a37),
    (0x10a3b, 0x10a3e),
    (0x10a49, 0x10a4f),
    (0x10a59, 0x10a5f),
    (0x10aa0, 0x10abf),
    (0x10ae7, 0x10aea),
    (0x10af7, 0x10aff),
    (0x10b36, 0x10b38),
    (0x10b56, 0x10b57),
    (0x10b73, 0x10b77),
    (0x10b92, 0x10b98),
    (0x10b9d, 0x10ba8),
    (0x10bb0, 0x10bff),
    (0x10c49, 0x10c7f),
    (0x10cb3, 0x10cbf),
    (0x10cf3, 0x10cf9),
    (0x10d28, 0x10d2f),
    (0x10d3a, 0x10e5f),
    (0x10e7f, 0x10e7f),
    (0x10eaa, 0x10eaa),
    (0x10eae, 0x10eaf),
    (0x10eb2, 0x10efc),
    (0x10f28, 0x10f2f),
    (0x10f5a, 0x10f6f),
    (0x10f8a, 0x10faf),
    (0x10fcc, 0x10fdf),
    (0x10ff7, 0x10fff),
    (0x1104e, 0x11051),
    (0x11076, 0x1107e),
    (0x110bd, 0x110bd),
    (0x110c3, 0x110cf),
    (0x110e9, 0x110ef),
    (0x110fa, 0x110ff),
    (0x11135, 0x11135),
    (0x11148, 0x1114f),
    (0x11177, 0x1117f),
    (0x111e0, 0x111e0),
    (0x111f5, 0x111ff),
    (0x11212, 0x11212),
    (0x11242, 0x1127f),
    (0x11287, 0x11287),
    (0x11289, 0x11289),
    (0x1128e, 0x1128e),
    (0x1129e, 0x1129e),
    (0x112aa, 0x112af),
    (0x112eb, 0x112ef),
    (0x112fa, 0x112ff),
    (0x11304, 0x11304),
    (0x1130d, 0x1130e),
    (0x11311, 0x11312),
    (0x11329, 0x11329),
    (0x11331, 0x11331),
    (0x11334, 0x11334),
    (0x1133a, 0x1133a),
    (0x11345, 0x11346),
    (0x11349, 0x1134a),
    (0x1134e, 0x1134f),
    (0x11351, 0x11356),
    (0x11358, 0x1135c),
    (0x11364, 0x11365),
    (0x1136d, 0x1136f),
    (0x11375, 0x113ff),
    (0x1145c, 0x1145c),
    (0x11462, 0x1147f),
    (0x114c8, 0x114cf),
    (0x114da, 0x1157f),
    (0x115b6, 0x115b7),
    (0x115de, 0x115ff),
    (0x11645, 0x1164f),
    (0x1165a, 0x1165f),
    (0x1166d, 0x1167f),
    (0x116ba, 0x116bf),
    (0x116ca, 0x116ff),
    (0x1171b, 0x1171c),
    (0x1172c, 0x1172f),
    (0x11747, 0x117ff),
    (0x1183c, 0x1189f),
    (0x118f3, 0x118fe),
    (0x11907, 0x11908),
    (0x1190a, 0x1190b),
    (0x11914, 0x11914),
    (0x11917, 0x11917),
    (0x11936, 0x11936),
    (0x11939, 0x1193a),
    (0x11947, 0x1194f),
    (0x1195a, 0x1199f),
    (0x119a8, 0x119a9),
    (0x119d8, 0x119d9),
    (0x119e5, 0x119ff),
    (0x11a48, 0x11a4f),
    (0x11aa3, 0x11aaf),
    (0x11af9, 0x11aff),
    (0x11b0a, 0x11bff),
    (0x11c09, 0x11c09),
    (0x11c37, 0x11c37),
    (0x11c46, 0x11c4f),
    (0x11c6d, 0x11c6f),
    (0x11c90, 0x11c91),
    (0x11ca8, 0x11ca8),
    (0x11cb7, 0x11cff),
    (0x11d07, 0x11d07),
    (0x11d0a, 0x11d0a),
    (0x11d37, 0x11d39),
    (0x11d3b, 0x11d3b),
    (0x11d3e, 0x11d3e),
    (0x11d48, 0x11d4f),
    (0x11d5a, 0x11d5f),
    (0x11d66, 0x11d66),
    (0x11d69, 0x11d69),
    (0x11d8f, 0x11d8f),
    (0x11d92, 0x11d92),
    (0x11d99, 0x11d9f),
    (0x11daa, 0x11edf),
    (0x11ef9, 0x11eff),
    (0x11f11, 0x11f11),
    (0x11f3b, 0x11f3d),
    (0x11f5a, 0x11faf),
    (0x11fb1, 0x11fbf),
    (0x11ff2, 0x11ffe),
    (0x1239a, 0x123ff),
    (0x1246f, 0x1246f),
    (0x12475, 0x1247f),
    (0x12544, 0x12f8f),
    (0x12ff3, 0x12fff),
    (0x13430, 0x1343f),
    (0x13456, 0x143ff),
    (0x14647, 0x167ff),
    (0x16a39, 0x16a3f),
    (0x16a5f, 0x16a5f),
    (0x16a6a, 0x16a6d),
    (0x16abf, 0x16abf),
    (0x16aca, 0x16acf),
    (0x16aee, 0x16aef),
    (0x16af6, 0x16aff),
    (0x16b46, 0x16b4f),
    (0x16b5a, 0x16b5a),
    (0x16b62, 0x16b62),
    (0x16b78, 0x16b7c),
    (0x16b90, 0x16e3f),
    (0x16e9b, 0x16eff),
    (0x16f4b, 0x16f4e),
    (0x16f88, 0x16f8e),
    (0x16fa0, 0x16fdf),
    (0x16fe5, 0x16fef),
    (0x16ff2, 0x16fff),
    (0x187f8, 0x187ff),
    (0x18cd6, 0x18cff),
    (0x18d09, 0x1afef),
    (0x1aff4, 0x1aff4),
    (0x1affc, 0x1affc),
    (0x1afff, 0x1afff),
    (0x1b123, 0x1b131),
    (0x1b133, 0x1b14f),
    (0x1b153, 0x1b154),
    (0x1b156, 0x1b163),
    (0x1b168, 0x1b16f),
    (0x1b2fc, 0x1bbff),
    (0x1bc6b, 0x1bc6f),
    (0x1bc7d, 0x1bc7f),
    (0x1bc89, 0x1bc8f),
    (0x1bc9a, 0x1bc9b),
    (0x1bca0, 0x1ceff),
    (0x1cf2e, 0x1cf2f),
    (0x1cf47, 0x1cf4f),
    (0x1cfc4, 0x1cfff),
    (0x1d0f6, 0x1d0ff),
    (0x1d127, 0x1d128),
    (0x1d173, 0x1d17a),
    (0x1d1eb, 0x1d1ff),
    (0x1d246, 0x1d2bf),
    (0x1d2d4, 0x1d2df),
    (0x1d2f4, 0x1d2ff),
    (0x1d357, 0x1d35f),
    (0x1d379, 0x1d3ff),
    (0x1d455, 0x1d455),
    (0x1d49d, 0x1d49d),
    (0x1d4a0, 0x1d4a1),
    (0x1d4a3, 0x1d4a4),
    (0x1d4a7, 0x1d4a8),
    (0x1d4ad, 0x1d4ad),
    (0x1d4ba, 0x1d4ba),
    (0x1d4bc, 0x1d4bc),
    (0x1d4c4, 0x1d4c4),
    (0x1d506, 0x1d506),
    (0x1d50b, 0x1d50c),
    (0x1d515, 0x1d515),
    (0x1d51d, 0x1d51d),
    (0x1d53a, 0x1d53a),
    (0x1d53f, 0x1d53f),
    (0x1d545, 0x1d545),
    (0x1d547, 0x1d549),
    (0x1d551, 0x1d551),
    (0x1d6a6, 0x1d6a7),
    (0x1d7cc, 0x1d7cd),
    (0x1da8c, 0x1da9a),
    (0x1daa0, 0x1daa0),
    (0x1dab0, 0x1deff),
    (0x1df1f, 0x1df24),
    (0x1df2b, 0x1dfff),
    (0x1e007, 0x1e007),
    (0x1e019, 0x1e01a),
    (0x1e022, 0x1e022),
    (0x1e025, 0x1e025),
    (0x1e02b, 0x1e02f),
    (0x1e06e, 0x1e08e),
    (0x1e090, 0x1e0ff),
    (0x1e12d, 0x1e12f),
    (0x1e13e, 0x1e13f),
    (0x1e14a, 0x1e14d),
    (0x1e150, 0x1e28f),
    (0x1e2af, 0x1e2bf),
    (0x1e2fa, 0x1e2fe),
    (0x1e300, 0x1e4cf),
    (0x1e4fa, 0x1e7df),
    (0x1e7e7, 0x1e7e7),
    (0x1e7ec, 0x1e7ec),
    (0x1e7ef, 0x1e7ef),
    (0x1e7ff, 0x1e7ff),
    (0x1e8c5, 0x1e8c6),
    (0x1e8d7, 0x1e8ff),
    (0x1e94c, 0x1e94f),
    (0x1e95a, 0x1e95d),
    (0x1e960, 0x1ec70),
    (0x1ecb5, 0x1ed00),
    (0x1ed3e, 0x1edff),
    (0x1ee04, 0x1ee04),
    (0x1ee20, 0x1ee20),
    (0x1ee23, 0x1ee23),
    (0x1ee25, 0x1ee26),
    (0x1ee28, 0x1ee28),
    (0x1ee33, 0x1ee33),
    (0x1ee38, 0x1ee38),
    (0x1ee3a, 0x1ee3a),
    (0x1ee3c, 0x1ee41),
    (0x1ee43, 0x1ee46),
    (0x1ee48, 0x1ee48),
    (0x1ee4a, 0x1ee4a),
    (0x1ee4c, 0x1ee4c),
    (0x1ee50, 0x1ee50),
    (0x1ee53, 0x1ee53),
    (0x1ee55, 0x1ee56),
    (0x1ee58, 0x1ee58),
    (0x1ee5a, 0x1ee5a),
    (0x1ee5c, 0x1ee5c),
    (0x1ee5e, 0x1ee5e),
    (0x1ee60, 0x1ee60),
    (0x1ee63, 0x1ee63),
    (0x1ee65, 0x1ee66),
    (0x1ee6b, 0x1ee6b),
    (0x1ee73, 0x1ee73),
    (0x1ee78, 0x1ee78),
    (0x1ee7d, 0x1ee7d),
    (0x1ee7f, 0x1ee7f),
    (0x1ee8a, 0x1ee8a),
    (0x1ee9c, 0x1eea0),
    (0x1eea4, 0x1eea4),
    (0x1eeaa, 0x1eeaa),
    (0x1eebc, 0x1eeef),
    (0x1eef2, 0x1efff),
    (0x1f02c, 0x1f02f),
    (0x1f094, 0x1f09f),
    (0x1f0af, 0x1f0b0),
    (0x1f0c0, 0x1f0c0),
    (0x1f0d0, 0x1f0d0),
    (0x1f0f6, 0x1f0ff),
    (0x1f1ae, 0x1f1e5),
    (0x1f203, 0x1f20f),
    (0x1f23c, 0x1f23f),
    (0x1f249, 0x1f24f),
    (0x1f252, 0x1f25f),
    (0x1f266, 0x1f2ff),
    (0x1f6d8, 0x1f6db),
    (0x1f6ed, 0x1f6ef),
    (0x1f6fd, 0x1f6ff),
    (0x1f777, 0x1f77a),
    (0x1f7da, 0x1f7df),
    (0x1f7ec, 0x1f7ef),
    (0x1f7f1, 0x1f7ff),
    (0x1f80c, 0x1f80f),
    (0x1f848, 0x1f84f),
    (0x1f85a, 0x1f85f),
    (0x1f888, 0x1f88f),
    (0x1f8ae, 0x1f8af),
    (0x1f8b2, 0x1f8ff),
    (0x1fa54, 0x1fa5f),
    (0x1fa6e, 0x1fa6f),
    (0x1fa7d, 0x1fa7f),
    (0x1fa89, 0x1fa8f),
    (0x1fabe, 0x1fabe),
    (0x1fac6, 0x1facd),
    (0x1fadc, 0x1fadf),
    (0x1fae9, 0x1faef),
    (0x1faf9, 0x1faff),
    (0x1fb93, 0x1fb93),
    (0x1fbcb, 0x1fbef),
    (0x1fbfa, 0x1ffff),
    (0x2a6e0, 0x2a6ff),
    (0x2b73a, 0x2b73f),
    (0x2b81e, 0x2b81f),
    (0x2cea2, 0x2ceaf),
    (0x2ebe1, 0x2f7ff),
    (0x2fa1e, 0x2ffff),
    (0x3134b, 0x3134f),
    (0x323b0, 0xe00ff),
    (0xe01f0, 0x10ffff),
];

/// Python's `str.isprintable` for one character.
fn is_printable(ch: char) -> bool {
    let code = ch as u32;
    NOT_PRINTABLE
        .binary_search_by(|&(low, high)| {
            if high < code {
                std::cmp::Ordering::Less
            } else if low > code {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_err()
}

/// Python's `float(text)` for a string: surrounding whitespace, a sign,
/// `inf`/`infinity`/`nan` in any case, `_` between digits, and decimal
/// digits of any script are accepted.
pub fn py_float_from_str(text: &str) -> Option<f64> {
    let text = python_number_text(text)?;
    let text = text.as_str();
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let lower = unsigned.to_ascii_lowercase();
    if matches!(lower.as_str(), "inf" | "infinity" | "nan") {
        let value = if lower == "nan" {
            f64::NAN
        } else {
            f64::INFINITY
        };
        return Some(if text.starts_with('-') { -value } else { value });
    }
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
        None => (unsigned, None),
    };
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (mantissa, None),
    };
    let digits_ok = |part: &str, may_be_empty: bool| {
        if part.is_empty() {
            return may_be_empty;
        }
        part.starts_with(|ch: char| ch.is_ascii_digit())
            && part.ends_with(|ch: char| ch.is_ascii_digit())
            && part.chars().all(|ch| ch.is_ascii_digit() || ch == '_')
            && !part.contains("__")
    };
    if !digits_ok(whole, true)
        || !fraction.is_none_or(|fraction| digits_ok(fraction, true))
        || whole.is_empty() && fraction.is_none_or(str::is_empty)
    {
        return None;
    }
    if let Some(exponent) = exponent {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if !digits_ok(digits, false) {
            return None;
        }
    }
    text.replace('_', "").parse().ok()
}

/// Python's `int(text)` for a string in base 10, within `i128`.
pub fn py_int_from_str(text: &str) -> Option<i128> {
    int_literal(&python_number_text(text)?)
}

/// The zero of every run of Unicode decimal digits (category Nd, Unicode 15).
const DECIMAL_ZEROS: [u32; 68] = [
    0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
    0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
    0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10,
    0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0, 0x11650,
    0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16a60, 0x16ac0,
    0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e950,
    0x1fbf0,
];

/// The value of a Unicode decimal digit.
fn decimal_value(ch: char) -> Option<u8> {
    let code = ch as u32;
    DECIMAL_ZEROS
        .iter()
        .find(|&&zero| (zero..zero + 10).contains(&code))
        .map(|zero| (code - zero) as u8)
}

/// What `float()` and `int()` parse: CPython maps non-ASCII digits to ASCII
/// and non-ASCII whitespace to spaces, rejects any other non-ASCII
/// character, then strips ASCII whitespace (not `\x1c`-`\x1f`).
pub(crate) fn python_number_text(text: &str) -> Option<String> {
    let mapped: String = text
        .chars()
        .map(|ch| {
            if (ch as u32) < 0x7f {
                Some(ch)
            } else if ch.is_whitespace() {
                Some(' ')
            } else {
                decimal_value(ch).map(|digit| char::from(b'0' + digit))
            }
        })
        .collect::<Option<_>>()?;
    Some(
        mapped
            .trim_matches(|ch: char| ch.is_ascii_whitespace() || ch == '\x0b')
            .to_owned(),
    )
}

/// A base-10 integer with an optional sign and `_` between digits, nothing around it.
pub(crate) fn int_literal(text: &str) -> Option<i128> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty()
        || !digits.starts_with(|ch: char| ch.is_ascii_digit())
        || !digits.ends_with(|ch: char| ch.is_ascii_digit())
        || !digits.chars().all(|ch| ch.is_ascii_digit() || ch == '_')
        || digits.contains("__")
    {
        return None;
    }
    text.replace('_', "").parse().ok()
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "pyjson.dumps" => {
            dump_options(&args).map(|options| json!(dumps(args.value("value"), options)))
        }
        "pyjson.loads" => args
            .str("text")
            .and_then(|text| loads(text).map_err(ParityError::from)),
        "pyjson.py_str" => Ok(json!(py_str(args.value("value")))),
        _ => return None,
    })
}

fn dump_options(args: &Args<'_>) -> Result<DumpOptions, ParityError> {
    let separators = match args.str("separators")? {
        "compact" => Separators::Compact,
        "default" => Separators::Default,
        other => {
            return Err(ParityError::bad_arguments(format!(
                "separators must be compact or default, not {other}"
            )));
        }
    };
    Ok(DumpOptions {
        ensure_ascii: args.bool("ensure_ascii")?,
        separators,
        sort_keys: args.bool("sort_keys")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `\uXXXX` escape, spelt out so the source holds no such escape itself.
    fn u(hex: &str) -> String {
        format!("{}u{hex}", '\\')
    }

    #[test]
    fn floats_follow_python_repr() {
        let cases = [
            (1.0, "1.0"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (0.1, "0.1"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1.5e16, "1.5e+16"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (1.2345e-7, "1.2345e-07"),
            (1e100, "1e+100"),
            (-2.5, "-2.5"),
            (123456.789, "123456.789"),
            (5e-324, "5e-324"),
            (153818817446116.12, "153818817446116.12"),
            (2048945272793487.2, "2048945272793487.2"),
            (f64::MAX, "1.7976931348623157e+308"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ];
        for (value, text) in cases {
            assert_eq!(float_repr(value), text, "{value:e}");
        }
    }

    #[test]
    fn strings_are_escaped_like_python() {
        let value = json!({"k": "a\"\\\n\r\t\u{8}\u{c}\u{1}\u{7f}é🚗/"});
        assert_eq!(
            dumps(&value, DumpOptions::COMPACT_ASCII),
            format!(
                r#"{{"k":"a\"\\\n\r\t\b\f{}{}{}{}{}/"}}"#,
                u("0001"),
                u("007f"),
                u("00e9"),
                u("d83d"),
                u("de97")
            )
        );
        assert_eq!(
            dumps(&value, DumpOptions::COMPACT_UNICODE),
            "{\"k\":\"a\\\"\\\\\\n\\r\\t\\b\\f\\u0001\u{7f}é🚗/\"}"
        );
    }

    #[test]
    fn separators_and_sorting() {
        let value = json!({"b": [1, 2.5, null, true, false], "a": {}});
        assert_eq!(
            dumps(&value, DumpOptions::DEFAULT),
            r#"{"b": [1, 2.5, null, true, false], "a": {}}"#
        );
        let sorted = DumpOptions {
            sort_keys: true,
            ..DumpOptions::COMPACT_ASCII
        };
        assert_eq!(
            dumps(&value, sorted),
            r#"{"a":{},"b":[1,2.5,null,true,false]}"#
        );
        assert_eq!(DumpOptions::default(), DumpOptions::DEFAULT);
        assert_eq!(dumps_compact(&json!(u64::MAX)), u64::MAX.to_string());
    }

    #[test]
    fn loads_rejects_a_bom() {
        assert_eq!(
            loads("\u{feff}{}").unwrap_err().kind(),
            ErrorKind::JsonDecodeError
        );
        assert_eq!(loads("[1]").unwrap(), json!([1]));
    }

    #[test]
    fn text_python_reads_and_serde_cannot_hold_is_recognised() {
        let int_4300 = format!("1{}", "0".repeat(4299));
        let neg_4300 = format!("-{int_4300}");
        let int_4301 = format!("1{}", "0".repeat(4300));
        let neg_4301 = format!("-{int_4301}");
        let float_4301 = format!("{int_4301}.0");
        // A surrogate pair written as two escapes is one character, which
        // both sides hold. (Built from pieces so that no tool reads the
        // escape before the compiler does.)
        let escaped_pair = ["\"", "\\", "ud83d", "\\", "ude00\""].concat();
        let deep_arr_ok = format!("{}1{}", "[".repeat(127), "]".repeat(127));
        let deep_arr_over = format!("{}1{}", "[".repeat(128), "]".repeat(128));
        let deep_obj_ok = format!("{}1{}", r#"{"a":"#.repeat(127), "}".repeat(127));
        let deep_obj_over = format!("{}1{}", r#"{"a":"#.repeat(128), "}".repeat(128));
        let deep_mixed_ok = format!(r#"{{"a":{}1{}}}"#, "[".repeat(126), "]".repeat(126));
        let deep_mixed_over = format!(r#"{{"a":{}1{}}}"#, "[".repeat(127), "]".repeat(127));

        for text in [
            r#"{"a":NaN}"#,
            "[Infinity]",
            "-Infinity",
            r#"{"a":5,"b":[1,-Infinity]}"#,
            "100000000000000000000",
            "[-9223372036854775809]",
            "18446744073709551616",
            "1e999",
            "[-1.5E+400]",
            r#""\ud800""#,
            r#"["\udc00 x"]"#,
            r#""\ud83dA""#,
            &int_4300,
            &neg_4300,
            &float_4301,
            &deep_arr_over,
            &deep_obj_over,
            &deep_mixed_over,
        ] {
            assert!(beyond_serde(text), "{text}");
        }
        for text in [
            "",
            "{}",
            r#"{"a":1,"b":-2.5e3,"c":"NaN","d":"Infinity 1e999"}"#,
            "18446744073709551615",
            "-9223372036854775808",
            "1e308",
            "1e-999",
            r#""😀 \\ud800 \n""#,
            &escaped_pair,
            &deep_arr_ok,
            &deep_obj_ok,
            &deep_mixed_ok,
            // Not JSON for Python either.
            "NaNx",
            "[NaN",
            "nan",
            "-NaN",
            "1NaN",
            "-1NaN",
            "1.NaN",
            "1eNaN",
            "1e+Infinity",
            "1e-Infinity",
            "1e-NaN",
            "NaN.5",
            "NaNe5",
            "Infinity.0",
            "Infinitye5",
            "-Infinity.5",
            "1Infinity",
            r#"{"a": -NaN}"#,
            "[NaN, -NaN]",
            &int_4301,
            &neg_4301,
            "0123456789012345678901234567890",
            "1.e999",
            "1e+",
            "--1",
            "1e999e9",
            r#""\ud800"#,
            r#""\u12"#,
        ] {
            assert!(!beyond_serde(text), "{text}");
        }
        assert!(loads(&deep_arr_ok).is_ok());
        assert!(loads(&deep_arr_over).is_err());
        assert!(loads(&deep_obj_ok).is_ok());
        assert!(loads(&deep_obj_over).is_err());
        assert!(loads(&deep_mixed_ok).is_ok());
        assert!(loads(&deep_mixed_over).is_err());
    }

    #[test]
    fn python_str_of_values() {
        assert_eq!(py_str(&json!(null)), "None");
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(false)), "False");
        assert_eq!(py_str(&json!(1.0)), "1.0");
        assert_eq!(py_str(&json!("x")), "x");
        assert_eq!(
            py_str(&json!([
                "a'",
                "b\"",
                "it's \"q\"",
                "\u{1}\u{a0}\u{2028}\u{e0001}\\\n\r\t",
                1
            ])),
            format!(
                r#"["a'", 'b"', 'it\'s "q"', '\x01\xa0{}\U000e0001\\\n\r\t', 1]"#,
                u("2028")
            )
        );
        assert_eq!(py_str(&json!({"k": {"n": null}})), "{'k': {'n': None}}");
        assert!(dump_options(&Args(&json!({"separators": "x"}))).is_err());
    }
}
