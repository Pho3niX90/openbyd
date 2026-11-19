//! Broker address, credentials and payload decoding for MQTT, without the network (`pybyd/_mqtt.py`).

use std::fmt;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::aes::{aes_decrypt_utf8, is_python_space};
use crate::coerce::{self, ValidationError};
use crate::config::{BydConfig, config_argument};
use crate::error::{Error, ErrorKind};
use crate::hashing::md5_hex;
use crate::models::base::{BydModel, Fields, model_call};
use crate::models::vehicle::py_truthy;
use crate::parity::{Args, Outcome, ParityError};
use crate::pyjson::{loads, py_int_from_str, py_str};
use crate::session::{Session, session_argument};

/// The endpoint that names the MQTT broker for the logged-in account.
pub const EMQ_BROKER_ENDPOINT: &str = "/app/emqAuth/getEmqBrokerIp";

/// Broker and credentials for connecting to BYD's MQTT push channel
/// (pyBYD's `MqttBootstrap`).
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct MqttBootstrap {
    /// The account's user ID.
    pub user_id: String,
    /// The broker's host name or address.
    pub broker_host: String,
    /// The broker's TLS port.
    pub broker_port: i64,
    /// The topic the account's events arrive on: `oversea/res/<user_id>`.
    pub topic: String,
    /// The MQTT client identifier.
    pub client_id: String,
    /// The MQTT user name (the user ID).
    pub username: String,
    /// The MQTT password, derived from the session and a timestamp. A secret.
    pub password: String,
}

impl fmt::Debug for MqttBootstrap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttBootstrap")
            .field("user_id", &self.user_id)
            .field("broker_host", &self.broker_host)
            .field("broker_port", &self.broker_port)
            .field("topic", &self.topic)
            .field("client_id", &self.client_id)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl BydModel for MqttBootstrap {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let mut fields = Fields::plain(raw)?;
        let model = Self {
            user_id: fields.required("user_id", coerce::string),
            broker_host: fields.required("broker_host", coerce::string),
            broker_port: fields.required("broker_port", coerce::int),
            topic: fields.required("topic", coerce::string),
            client_id: fields.required("client_id", coerce::string),
            username: fields.required("username", coerce::string),
            password: fields.required("password", coerce::string),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// A decrypted MQTT message (pyBYD's `MqttEvent`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MqttEvent {
    /// The message's `event` field as text (`vehicleInfo`, `remoteControl`,
    /// ...), or `""` when it has none.
    pub event: String,
    /// The vehicle the message is about, when it names one.
    pub vin: Option<String>,
    /// The topic it arrived on.
    pub topic: String,
    /// The whole decrypted message.
    pub payload: Map<String, Value>,
}

impl BydModel for MqttEvent {
    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        let mut fields = Fields::plain(raw)?;
        let model = Self {
            event: fields.required("event", coerce::string),
            vin: fields.required("vin", |value| coerce::optional(value, coerce::string)),
            topic: fields.required("topic", coerce::string),
            payload: fields.required("payload", coerce::any_dict),
        };
        fields.finish()?;
        Ok(model)
    }
}

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

/// The broker address in a decoded broker lookup reply, as pyBYD's
/// `_fetch_emq_broker` takes it: `emqBorker` (sic) when it is truthy, else
/// `emqBroker`, with surrounding whitespace removed.
///
/// Fails with [`ErrorKind::BydError`] when the reply is not an object or
/// names no broker.
pub fn broker_from_reply(decoded: &Value) -> Result<String, Error> {
    let Value::Object(reply) = decoded else {
        return Err(Error::new(
            ErrorKind::BydError,
            "Broker lookup response inner payload is not an object",
        ));
    };
    // Python's `decoded.get("emqBorker") or decoded.get("emqBroker")`.
    let broker = reply
        .get("emqBorker")
        .filter(|value| py_truthy(value))
        .or_else(|| reply.get("emqBroker"));
    match broker {
        Some(Value::String(text)) if !text.trim_matches(is_python_space).is_empty() => {
            Ok(text.trim_matches(is_python_space).to_owned())
        }
        _ => Err(Error::new(
            ErrorKind::BydError,
            "Broker lookup response missing emqBorker/emqBroker",
        )),
    }
}

/// Python's `int(time.time())` when the clock reads `now_ms`, given that
/// `time.time()` is `now_ms` plus half a millisecond (see the parity rules).
pub fn unix_seconds(now_ms: i64) -> i64 {
    ((now_ms as f64 + 0.5) / 1000.0).trunc() as i64
}

/// The connection details for broker address `broker` at Unix time
/// `now_seconds`: the part of pyBYD's `fetch_mqtt_bootstrap` after the
/// broker lookup.
pub fn build_mqtt_bootstrap(
    config: &BydConfig,
    session: &Session,
    broker: &str,
    now_seconds: i64,
) -> Result<MqttBootstrap, Error> {
    let (broker_host, broker_port) = parse_broker(broker)?;
    let broker_port = i64::try_from(broker_port)
        .map_err(|_| Error::new(ErrorKind::ValueError, "broker port out of range"))?;
    let client_id = build_client_id(config);
    Ok(MqttBootstrap {
        user_id: session.user_id().to_owned(),
        broker_host,
        broker_port,
        topic: format!("oversea/res/{}", session.user_id()),
        password: build_mqtt_password(session, &client_id, now_seconds),
        client_id,
        username: session.user_id().to_owned(),
    })
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
    let is_kept = |byte: &u8| byte.is_ascii() && !is_python_space(char::from(*byte));
    let filtered: String;
    let text = match std::str::from_utf8(payload) {
        // The usual payload: hex digits only, nothing to replace or drop.
        Ok(text) if payload.iter().all(is_kept) => text,
        _ => {
            filtered = payload
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
            &filtered
        }
    };
    let plain = aes_decrypt_utf8(text, decrypt_key_hex)?;
    match loads(&plain)? {
        Value::Object(parsed) => Ok((parsed, plain)),
        _ => Err(Error::new(
            ErrorKind::BydError,
            "MQTT payload decrypted to non-object JSON",
        )),
    }
}

/// What became of one MQTT message (the branches of pyBYD's `on_message`).
#[derive(Debug, Clone, PartialEq)]
pub enum MessageOutcome {
    /// It decrypted: deliver the event.
    Event(MqttEvent),
    /// It did not decrypt, and no message has decrypted with the current
    /// key yet: the key is probably stale, so the owner should log in again.
    DecryptFailed,
    /// It did not decrypt, but an earlier message did: it is meant for
    /// another session on the same account. Ignored.
    NotForSession,
    /// It decrypted to something that is not a JSON object, or could not be
    /// read at all. Logged and ignored.
    Invalid(Error),
}

/// Decrypts the messages of one MQTT connection and tracks whether the
/// current key is confirmed (pyBYD's `on_message` and `_key_confirmed`).
///
/// Once a message has decrypted with the current key, a later decrypt
/// failure is a message for another session on the account (another login
/// polling), not a stale key, so it is ignored instead of asking for a new
/// login (hass-byd-vehicle#195). A new key has to be confirmed again.
#[derive(Clone)]
pub struct MessageDecoder {
    decrypt_key_hex: String,
    key_confirmed: bool,
}

impl fmt::Debug for MessageDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageDecoder")
            .field("decrypt_key_hex", &"<redacted>")
            .field("key_confirmed", &self.key_confirmed)
            .finish()
    }
}

impl MessageDecoder {
    /// A decoder for messages sealed with `decrypt_key_hex` (the session's content key).
    pub fn new(decrypt_key_hex: &str) -> Self {
        Self {
            decrypt_key_hex: decrypt_key_hex.to_owned(),
            key_confirmed: false,
        }
    }

    /// Switches to a new key, which then has to be confirmed again.
    pub fn update_decrypt_key(&mut self, decrypt_key_hex: &str) {
        decrypt_key_hex.clone_into(&mut self.decrypt_key_hex);
        self.key_confirmed = false;
    }

    /// Forgets that the key was confirmed, as pyBYD's `start` does.
    pub fn reset(&mut self) {
        self.key_confirmed = false;
    }

    /// Whether a message has decrypted with the current key.
    pub fn key_confirmed(&self) -> bool {
        self.key_confirmed
    }

    /// Decodes one message that arrived on `topic` (raw bytes, UTF-8 when well formed).
    pub fn handle(&mut self, topic: &[u8], payload: &[u8]) -> MessageOutcome {
        let parsed = match decode_mqtt_payload(payload, &self.decrypt_key_hex) {
            Ok((parsed, _plain)) => parsed,
            Err(error) if error.is_a(ErrorKind::BydCryptoError) => {
                return if self.key_confirmed {
                    MessageOutcome::NotForSession
                } else {
                    MessageOutcome::DecryptFailed
                };
            }
            Err(error) => return MessageOutcome::Invalid(error),
        };
        self.key_confirmed = true;
        // paho decodes the topic strictly, and pyBYD reads it only after
        // confirming the key.
        let Ok(topic) = std::str::from_utf8(topic) else {
            return MessageOutcome::Invalid(Error::new(
                ErrorKind::UnicodeDecodeError,
                "MQTT topic is not UTF-8",
            ));
        };
        MessageOutcome::Event(event_from_payload(topic, parsed))
    }
}

/// The event for a decrypted message: `event` is Python's
/// `str(payload.get("event") or "")` and `vin` the payload's `vin` when it
/// is a non-empty string.
pub fn event_from_payload(topic: &str, payload: Map<String, Value>) -> MqttEvent {
    let event = match payload.get("event") {
        Some(value) if py_truthy(value) => py_str(value),
        _ => String::new(),
    };
    let vin = match payload.get("vin") {
        Some(Value::String(vin)) if !vin.is_empty() => Some(vin.clone()),
        _ => None,
    };
    MqttEvent {
        event,
        vin,
        topic: topic.to_owned(),
        payload,
    }
}

/// `mqtt.runtime_messages`: feeds a decoder the `steps` (`{"topic_hex",
/// "payload_hex"}` messages and `{"update_key"}` key changes) and returns
/// what became of each message and whether the key ends up confirmed.
fn runtime_messages_call(args: &Args<'_>) -> Outcome {
    let mut decoder = MessageDecoder::new(args.str("decrypt_key_hex")?);
    let steps = args
        .value("steps")
        .as_array()
        .ok_or_else(|| ParityError::bad_arguments("steps must be a list"))?;
    let mut outcomes = Vec::new();
    for step in steps {
        let step = Args(step);
        if let Some(key) = step.value("update_key").as_str() {
            decoder.update_decrypt_key(key);
            continue;
        }
        let topic = hex_argument(step.str("topic_hex")?)?;
        let payload = hex_argument(step.str("payload_hex")?)?;
        outcomes.push(match decoder.handle(&topic, &payload) {
            MessageOutcome::Event(event) => json!({ "event": event.dump() }),
            MessageOutcome::DecryptFailed => json!("decrypt_failed"),
            MessageOutcome::NotForSession => json!("not_for_session"),
            MessageOutcome::Invalid(_) => json!("invalid"),
        });
    }
    Ok(json!({"outcomes": outcomes, "key_confirmed": decoder.key_confirmed()}))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    match name {
        "mqtt.MqttBootstrap" => return Some(model_call::<MqttBootstrap>(args)),
        "mqtt.MqttEvent" => return Some(model_call::<MqttEvent>(args)),
        _ => {}
    }
    let args = Args(args);
    Some(match name {
        "mqtt.runtime_messages" => runtime_messages_call(&args),
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

    #[test]
    fn broker_lookup_replies() {
        let broker = |reply: Value| broker_from_reply(&reply);
        assert_eq!(broker(json!({"emqBorker": " h:1 "})).unwrap(), "h:1");
        assert_eq!(
            broker(json!({"emqBorker": "", "emqBroker": "b"})).unwrap(),
            "b"
        );
        assert_eq!(
            broker(json!({"emqBorker": "a", "emqBroker": "b"})).unwrap(),
            "a"
        );
        for missing in [
            json!({}),
            json!({"emqBorker": 0}),
            json!({"emqBorker": 5, "emqBroker": "b"}),
            json!({"emqBroker": "  "}),
            json!([]),
        ] {
            assert_eq!(broker(missing).unwrap_err().kind(), ErrorKind::BydError);
        }
    }

    #[test]
    fn bootstraps() {
        assert_eq!(unix_seconds(1_700_000_000_999), 1_700_000_000);
        assert_eq!(unix_seconds(0), 0);
        assert_eq!(unix_seconds(-1000), 0);
        assert_eq!(unix_seconds(-1001), -1);
        let config = BydConfig::new("user@example.com", "p");
        let session = Session::new("7", "s", "e", 0.0, 1.0);
        let bootstrap = build_mqtt_bootstrap(&config, &session, "ssl://h:1883/x", 5).unwrap();
        assert_eq!(bootstrap.broker_host, "h");
        assert_eq!(bootstrap.broker_port, 1883);
        assert_eq!(bootstrap.topic, "oversea/res/7");
        assert_eq!(bootstrap.username, "7");
        assert!(!format!("{bootstrap:?}").contains(&bootstrap.password));
        let huge = build_mqtt_bootstrap(&config, &session, "h:18446744073709551615", 5);
        assert_eq!(huge.unwrap_err().kind(), ErrorKind::ValueError);
        let parsed = MqttBootstrap::parse(&bootstrap.dump(), &Value::Null).unwrap();
        assert_eq!(parsed, bootstrap);
        assert!(MqttEvent::parse(&json!({"event": "e"}), &Value::Null).is_err());
    }

    #[test]
    fn decoder_tracks_the_key() {
        let key = "000102030405060708090A0B0C0D0E0F";
        let other = "0F0E0D0C0B0A09080706050403020100";
        let sealed = |body: &str, key: &str| crate::aes::aes_encrypt_hex(body, key).unwrap();
        let mut decoder = MessageDecoder::new(key);
        assert!(!format!("{decoder:?}").contains(key));
        let foreign = sealed("{}", other);
        assert_eq!(
            decoder.handle(b"t", foreign.as_bytes()),
            MessageOutcome::DecryptFailed
        );
        let MessageOutcome::Event(event) =
            decoder.handle(b"t", sealed(r#"{"event": "", "vin": "V"}"#, key).as_bytes())
        else {
            panic!("an event");
        };
        assert_eq!(
            (event.event.as_str(), event.vin.as_deref()),
            ("", Some("V"))
        );
        assert!(decoder.key_confirmed());
        assert_eq!(
            decoder.handle(b"t", foreign.as_bytes()),
            MessageOutcome::NotForSession
        );
        assert!(matches!(
            decoder.handle(&[0xff], sealed("{}", key).as_bytes()),
            MessageOutcome::Invalid(_)
        ));
        decoder.reset();
        assert!(!decoder.key_confirmed());
        assert!(matches!(
            decoder.handle(b"t", sealed("[]", key).as_bytes()),
            MessageOutcome::Invalid(_)
        ));
        decoder.update_decrypt_key(other);
        assert!(matches!(
            decoder.handle(b"t", foreign.as_bytes()),
            MessageOutcome::Event(_)
        ));
    }

    #[test]
    fn runtime_message_arguments_are_checked() {
        let call = |args: Value| parity_call("mqtt.runtime_messages", &args).unwrap();
        assert!(call(json!({"decrypt_key_hex": "00", "steps": 1})).is_err());
        assert!(call(json!({"decrypt_key_hex": "00", "steps": [{"topic_hex": "zz"}]})).is_err());
        assert!(call(json!({"steps": []})).is_err());
        assert!(parity_call("mqtt.nothing", &json!({})).is_none());
    }
}
