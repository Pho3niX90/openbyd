//! Broker address, credentials and payload decoding for MQTT, without the network (`pybyd/_mqtt.py`).

use serde_json::{Map, Value, json};

use crate::aes::{aes_decrypt_utf8, is_python_space};
use crate::config::{BydConfig, config_argument};
use crate::error::{Error, ErrorKind};
use crate::hashing::md5_hex;
use crate::parity::{Args, Outcome, ParityError};
use crate::pyjson::{loads, py_int_from_str};
use crate::session::{Session, session_argument};

/// The broker port when the address names none.
pub const DEFAULT_BROKER_PORT: u64 = 8883;

/// Splits the broker address the API returns (`emqBorker`) into host and
/// port, as pyBYD's `_parse_broker`: a scheme and a path are dropped, and
/// the port defaults to 8883.
///
/// Fails with [`ErrorKind::ValueError`] for an empty address, and, as in
/// pyBYD, for a port made of digits `int()` cannot read (superscripts,
/// circled digits) or, unlike pyBYD, one too large for 64 bits.
pub fn parse_broker(raw_broker: &str) -> Result<(String, u64), Error> {
    let mut value = raw_broker.trim_matches(is_python_space);
    if value.is_empty() {
        return Err(Error::new(ErrorKind::ValueError, "Broker value is empty"));
    }
    if let Some((_, rest)) = value.split_once("://") {
        value = rest;
    }
    if let Some((first, _)) = value.split_once('/') {
        value = first;
    }
    if let Some((host, port)) = value.rsplit_once(':')
        && !host.is_empty()
        && !port.is_empty()
        && port.chars().all(is_digit)
    {
        let port = py_int_from_str(port)
            .and_then(|port| u64::try_from(port).ok())
            .ok_or_else(|| Error::new(ErrorKind::ValueError, format!("invalid port {port:?}")))?;
        return Ok((host.to_owned(), port));
    }
    Ok((value.to_owned(), DEFAULT_BROKER_PORT))
}

/// Characters Python's `str.isdigit` accepts that are not decimal digits
/// (Unicode 15): superscripts, subscripts, circled and other digit forms.
const OTHER_DIGITS: [(u32, u32); 20] = [
    (0xb2, 0xb3),
    (0xb9, 0xb9),
    (0x1369, 0x1371),
    (0x19da, 0x19da),
    (0x2070, 0x2070),
    (0x2074, 0x2079),
    (0x2080, 0x2089),
    (0x2460, 0x2468),
    (0x2474, 0x247c),
    (0x2488, 0x2490),
    (0x24ea, 0x24ea),
    (0x24f5, 0x24fd),
    (0x24ff, 0x24ff),
    (0x2776, 0x277e),
    (0x2780, 0x2788),
    (0x278a, 0x2792),
    (0x10a40, 0x10a43),
    (0x10e60, 0x10e68),
    (0x11052, 0x1105a),
    (0x1f100, 0x1f10a),
];

/// Python's `str.isdigit` for one character.
fn is_digit(ch: char) -> bool {
    let code = ch as u32;
    py_int_from_str(ch.encode_utf8(&mut [0; 4])).is_some()
        || OTHER_DIGITS
            .iter()
            .any(|&(low, high)| (low..=high).contains(&code))
}

/// The MQTT client id: `oversea_` and the device's `imei_md5`, or MD5 of
/// the IMEI when `imei_md5` is all zeros.
pub fn build_client_id(config: &BydConfig) -> String {
    let imei_md5 = config
        .device
        .imei_md5
        .trim_matches(is_python_space)
        .to_uppercase();
    if !imei_md5.is_empty() && imei_md5.chars().any(|ch| ch != '0') {
        format!("oversea_{imei_md5}")
    } else {
        format!("oversea_{}", md5_hex(&config.device.imei))
    }
}

/// The MQTT password for `client_id` at Unix time `ts_seconds`: the
/// timestamp followed by MD5 of sign token, client id, user id and timestamp.
pub fn build_mqtt_password(session: &Session, client_id: &str, ts_seconds: i64) -> String {
    let base = format!(
        "{}{client_id}{}{ts_seconds}",
        session.sign_token(),
        session.user_id()
    );
    format!("{ts_seconds}{}", md5_hex(&base))
}

/// Decrypts an MQTT message body (hex text, whitespace ignored) with the
/// session's content key and parses it as a JSON object. Returns the object
/// and the decrypted text.
///
/// Fails with [`ErrorKind::BydCryptoError`] when it does not decrypt,
/// [`ErrorKind::JsonDecodeError`] when the text is not JSON, and
/// [`ErrorKind::BydError`] when the JSON is not an object.
pub fn decode_mqtt_payload(
    payload: &[u8],
    decrypt_key_hex: &str,
) -> Result<(Map<String, Value>, String), Error> {
    // `bytes.decode("ascii", errors="replace")`, then drop all whitespace.
    let text: String = payload
        .iter()
        .map(|&byte| {
            if byte.is_ascii() {
                char::from(byte)
            } else {
                '\u{fffd}'
            }
        })
        .filter(|&ch| !is_python_space(ch))
        .collect();
    let plain = aes_decrypt_utf8(&text, decrypt_key_hex)?;
    match loads(&plain)? {
        Value::Object(parsed) => Ok((parsed, plain)),
        _ => Err(Error::new(
            ErrorKind::BydError,
            "MQTT payload decrypted to non-object JSON",
        )),
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "mqtt.parse_broker" => (|| {
            let (host, port) = parse_broker(args.str("raw_broker")?)?;
            Ok(json!([host, port]))
        })(),
        "mqtt.build_client_id" => {
            config_argument(&args).map(|config| json!(build_client_id(&config)))
        }
        "mqtt.build_mqtt_password" => (|| {
            let session = session_argument(&args)?;
            let password =
                build_mqtt_password(&session, args.str("client_id")?, args.i64("ts_seconds")?);
            Ok(json!(password))
        })(),
        "mqtt.decode_mqtt_payload" => (|| {
            let payload = hex_argument(args.str("payload_hex")?)?;
            let (parsed, plain) = decode_mqtt_payload(&payload, args.str("decrypt_key_hex")?)?;
            Ok(json!([parsed, plain]))
        })(),
        _ => return None,
    })
}

fn hex_argument(text: &str) -> Result<Vec<u8>, ParityError> {
    (0..text.len())
        .step_by(2)
        .map(|at| {
            text.get(at..at + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| ParityError::bad_arguments("payload_hex must be hex"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_addresses() {
        let parse = |raw| parse_broker(raw).unwrap();
        assert_eq!(
            parse("ssl://emq.example.com:1883/x"),
            ("emq.example.com".into(), 1883)
        );
        assert_eq!(parse(" emq.example.com "), ("emq.example.com".into(), 8883));
        assert_eq!(parse(":1883"), (":1883".into(), 8883));
        assert_eq!(parse("h:"), ("h:".into(), 8883));
        assert_eq!(
            parse_broker("  ").unwrap_err().kind(),
            ErrorKind::ValueError
        );
        assert!(parse_broker("h:99999999999999999999999").is_err());
        assert!(hex_argument("zz").is_err());
    }

    #[test]
    fn credentials() {
        let mut config = BydConfig::new("user@example.com", "p");
        assert_eq!(
            build_client_id(&config),
            format!("oversea_{}", md5_hex("user@example.com"))
        );
        config.device.imei_md5 = "000".into();
        assert_eq!(
            build_client_id(&config),
            format!("oversea_{}", md5_hex("BANGCLE01234"))
        );
        let session = Session::new("7", "s", "e", 0.0, 1.0);
        assert_eq!(
            build_mqtt_password(&session, "c", 5),
            format!("5{}", md5_hex("sc75"))
        );
    }

    #[test]
    fn payloads() {
        let key = "000102030405060708090A0B0C0D0E0F";
        let sealed = crate::aes::aes_encrypt_hex("{\"event\":\"x\"}", key).unwrap();
        let spaced = format!(" {}\n{} ", &sealed[..10], &sealed[10..]);
        let (parsed, plain) = decode_mqtt_payload(spaced.as_bytes(), key).unwrap();
        assert_eq!(parsed["event"], "x");
        assert_eq!(plain, "{\"event\":\"x\"}");
        let list = crate::aes::aes_encrypt_hex("[]", key).unwrap();
        assert_eq!(
            decode_mqtt_payload(list.as_bytes(), key)
                .unwrap_err()
                .kind(),
            ErrorKind::BydError
        );
        assert_eq!(
            decode_mqtt_payload(&[0xff], key).unwrap_err().kind(),
            ErrorKind::BydCryptoError
        );
    }
}
