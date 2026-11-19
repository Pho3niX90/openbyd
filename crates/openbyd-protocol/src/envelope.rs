//! Login and token request envelopes, and reply decoding (`pybyd/_api/login.py`, `_envelope.py`, `_common.py`, `_transport.py`).
//!
//! A request travels in three layers:
//!
//! 1. the *inner* payload, AES-encrypted into the outer `encryData`;
//! 2. the *outer* payload, signed (`sign`) and checksummed (`checkcode`);
//! 3. the HTTP body `{"request": "F..."}`, the outer payload's JSON in a
//!    Bangcle envelope.
//!
//! The reply comes back the same way. Everything here is pure: the client
//! supplies the clock and random values and does the HTTP.

use serde_json::{Map, Value, json};

use crate::aes::{aes_decrypt_utf8, aes_encrypt_hex, is_python_space};
use crate::api::common::random_hex;
use crate::config::BydConfig;
use crate::error::{Error, ErrorKind, raise_for_code};
use crate::hashing::{compute_checkcode, md5_hex, pwd_login_key, sha1_mixed};
use crate::models::token::AuthToken;
use crate::parity::{Args, Outcome, ParityError};
use crate::pyjson::{DumpOptions, dumps, dumps_compact, loads, py_str};
use crate::session::Session;
use crate::signing::build_sign_string;

/// The login endpoint.
pub const LOGIN_ENDPOINT: &str = "/app/account/login";

/// The HTTP headers pyBYD sends with every request (`SecureTransport.post_secure`).
/// The URL is the config's `base_url` followed by the endpoint.
pub const REQUEST_HEADERS: [(&str, &str); 3] = [
    ("accept-encoding", "identity"),
    ("content-type", "application/json; charset=UTF-8"),
    ("user-agent", crate::constants::USER_AGENT),
];

fn device_outer_fields(config: &BydConfig, outer: &mut Map<String, Value>) {
    let device = &config.device;
    for (key, value) in [
        ("ostype", &device.ostype),
        ("imei", &device.imei),
        ("mac", &device.mac),
        ("model", &device.model),
        ("sdk", &device.sdk),
        ("mod", &device.r#mod),
    ] {
        outer.insert(key.into(), json!(value));
    }
}

fn insert_all(map: &mut Map<String, Value>, pairs: &[(&str, &str)]) {
    for (key, value) in pairs {
        map.insert((*key).into(), json!(value));
    }
}

/// The outer payload of the login request (`pwdLogin`), as pyBYD's
/// `build_login_request` builds it.
///
/// `now_ms` is the request time and `service_time_ms` the `serviceTime`
/// field (pyBYD reads the clock a second time for it); both are
/// milliseconds since the epoch. `random` is 16 fresh random bytes.
pub fn build_login_request(
    config: &BydConfig,
    now_ms: i64,
    random: &[u8; 16],
    service_time_ms: i64,
) -> Map<String, Value> {
    let device = &config.device;
    let req_timestamp = now_ms.to_string();
    let device_name = format!("{}{}", device.mobile_brand, device.mobile_model);
    let mut inner = Map::new();
    insert_all(
        &mut inner,
        &[
            ("agreeStatus", "0"),
            ("agreementType", "[1,2]"),
            ("appInnerVersion", &config.app_inner_version),
            ("appVersion", &config.app_version),
            ("deviceName", &device_name),
            ("deviceType", &device.device_type),
            ("imeiMD5", &device.imei_md5),
            ("isAuto", &config.is_auto),
            ("mobileBrand", &device.mobile_brand),
            ("mobileModel", &device.mobile_model),
            ("networkType", &device.network_type),
            ("osType", &device.os_type),
            ("osVersion", &device.os_version),
            ("random", &random_hex(random)),
            ("softType", &config.soft_type),
            ("timeStamp", &req_timestamp),
            ("timeZone", &config.time_zone),
        ],
    );

    let encry_data = aes_encrypt_hex(
        &dumps_compact(&Value::Object(inner.clone())),
        &pwd_login_key(&config.password),
    )
    .expect("the login key is always 16 bytes of hex");

    let mut sign_fields = inner;
    insert_all(
        &mut sign_fields,
        &[
            ("appName", &config.app_name),
            ("countryCode", &config.country_code),
            ("functionType", "pwdLogin"),
            ("identifier", &config.username),
            ("identifierType", &config.identifier_type),
            ("language", &config.language),
            ("reqTimestamp", &req_timestamp),
        ],
    );
    let sign = sha1_mixed(&build_sign_string(&sign_fields, &md5_hex(&config.password)));

    let mut outer = Map::new();
    insert_all(
        &mut outer,
        &[
            ("appName", &config.app_name),
            ("countryCode", &config.country_code),
            ("encryData", &encry_data),
            ("functionType", "pwdLogin"),
            ("identifier", &config.username),
            ("identifierType", &config.identifier_type),
            ("imeiMD5", &device.imei_md5),
            ("isAuto", &config.is_auto),
            ("language", &config.language),
            ("reqTimestamp", &req_timestamp),
            ("sign", &sign),
            ("signKey", &config.password),
        ],
    );
    device_outer_fields(config, &mut outer);
    outer.insert("serviceTime".into(), json!(service_time_ms.to_string()));
    let checkcode = compute_checkcode(&Value::Object(outer.clone()));
    outer.insert("checkcode".into(), json!(checkcode));
    outer
}

/// Python's truth value of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|float| float != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn not_a_dict(what: &str) -> Error {
    Error::new(
        ErrorKind::AttributeError,
        format!("{what} has no attribute 'get'"),
    )
}

/// Checks the login reply's outer payload and decrypts the token from it,
/// as pyBYD's `parse_login_response`.
///
/// Fails with [`ErrorKind::BydAuthenticationError`] for a non-zero code or
/// a reply without token fields, [`ErrorKind::BydCryptoError`] when
/// `respondData` does not decrypt with the password's key, and
/// [`ErrorKind::JsonDecodeError`] when it decrypts to something that is not
/// JSON. A reply that is not an object, or a `token` that is neither empty
/// nor an object, fails with [`ErrorKind::AttributeError`] as in Python.
pub fn parse_login_response(outer_response: &Value, password: &str) -> Result<AuthToken, Error> {
    let response = outer_response
        .as_object()
        .ok_or_else(|| not_a_dict("login response"))?;
    let code = response.get("code").unwrap_or(&Value::Null);
    if py_str(code) != "0" {
        let message = response.get("message").map(py_str).unwrap_or_default();
        return Err(Error::api(
            ErrorKind::BydAuthenticationError,
            format!("Login failed: code={} message={message}", py_str(code)),
            response.get("code").map(py_str).unwrap_or_default(),
            LOGIN_ENDPOINT,
        ));
    }
    let auth_error = |message: &str| {
        Error::api(
            ErrorKind::BydAuthenticationError,
            message,
            "",
            LOGIN_ENDPOINT,
        )
    };
    let respond_data = response.get("respondData").unwrap_or(&Value::Null);
    if !truthy(respond_data) {
        return Err(auth_error("Login response missing respondData"));
    }
    let Value::String(respond_data) = respond_data else {
        return Err(Error::new(
            ErrorKind::BydCryptoError,
            "AES decryption failed: respondData is not a string",
        ));
    };
    let inner = loads(&aes_decrypt_utf8(respond_data, &pwd_login_key(password))?)?;
    let token = inner
        .get("token")
        .filter(|_| inner.is_object())
        .unwrap_or(&Value::Null);
    if !truthy(token) {
        return Err(auth_error("Login response missing token fields"));
    }
    let token = token.as_object().ok_or_else(|| not_a_dict("token"))?;
    let field = |name: &str| token.get(name).filter(|value| truthy(value)).map(py_str);
    match (field("userId"), field("signToken"), field("encryToken")) {
        (Some(user_id), Some(sign_token), Some(encry_token)) => Ok(AuthToken {
            user_id,
            sign_token,
            encry_token,
            raw: token.clone(),
        }),
        _ => Err(auth_error("Login response missing token fields")),
    }
}

/// The signed outer payload of a request made with a session, as pyBYD's
/// `build_token_outer_envelope`. Returns the payload and the content key
/// that decrypts the reply's `respondData`.
///
/// `user_type` is sent *and signed* when given (`"1"` for a guest
/// account): the cloud answers 1007 to an unsigned one.
pub fn build_token_outer_envelope(
    config: &BydConfig,
    session: &Session,
    inner: &Map<String, Value>,
    now_ms: i64,
    user_type: Option<&str>,
    service_time_ms: i64,
) -> (Map<String, Value>, String) {
    let req_timestamp = now_ms.to_string();
    let content_key = session.content_key().to_owned();
    let encry_data = aes_encrypt_hex(&dumps_compact(&Value::Object(inner.clone())), &content_key)
        .expect("the content key is always 16 bytes of hex");

    let mut sign_fields = inner.clone();
    insert_all(
        &mut sign_fields,
        &[
            ("countryCode", &config.country_code),
            ("identifier", session.user_id()),
            ("imeiMD5", &config.device.imei_md5),
            ("language", &config.language),
            ("reqTimestamp", &req_timestamp),
        ],
    );
    if let Some(user_type) = user_type {
        sign_fields.insert("userType".into(), json!(user_type));
    }
    let sign = sha1_mixed(&build_sign_string(&sign_fields, session.sign_key()));

    let mut outer = Map::new();
    insert_all(
        &mut outer,
        &[
            ("countryCode", &config.country_code),
            ("encryData", &encry_data),
            ("identifier", session.user_id()),
            ("imeiMD5", &config.device.imei_md5),
            ("language", &config.language),
            ("reqTimestamp", &req_timestamp),
            ("sign", &sign),
        ],
    );
    device_outer_fields(config, &mut outer);
    outer.insert("serviceTime".into(), json!(service_time_ms.to_string()));
    if let Some(user_type) = user_type {
        outer.insert("userType".into(), json!(user_type));
    }
    let checkcode = compute_checkcode(&Value::Object(outer.clone()));
    outer.insert("checkcode".into(), json!(checkcode));
    (outer, content_key)
}

/// Decrypts and parses a reply's `respondData`, as pyBYD's
/// `decode_respond_data`. A missing or empty `respondData`, or one that
/// decrypts to blank text, gives an empty object.
///
/// Fails with [`ErrorKind::BydCryptoError`] when it does not decrypt and
/// with a [`ErrorKind::BydApiError`] of code `invalid_json` when it is not JSON.
pub fn decode_respond_data(
    endpoint: &str,
    response: &Value,
    content_key: &str,
) -> Result<Value, Error> {
    let response = response.as_object().ok_or_else(|| not_a_dict("response"))?;
    let Some(Value::String(respond_data)) = response.get("respondData") else {
        return Ok(json!({}));
    };
    if respond_data.is_empty() {
        return Ok(json!({}));
    }
    let plaintext = aes_decrypt_utf8(respond_data, content_key)?;
    if plaintext.trim_matches(is_python_space).is_empty() {
        return Ok(json!({}));
    }
    loads(&plaintext).map_err(|_| {
        let head: String = plaintext.chars().take(128).collect();
        Error::api(
            ErrorKind::BydApiError,
            format!("{endpoint} respondData is not JSON: {head}"),
            "invalid_json",
            endpoint,
        )
    })
}

/// Checks a token request's decoded reply and returns its decrypted data:
/// the pure half of pyBYD's `post_token_json`. A non-zero `code` becomes
/// the error [`raise_for_code`] picks.
pub fn handle_token_response(
    endpoint: &str,
    response: &Value,
    content_key: &str,
    vin: Option<&str>,
    not_supported_codes: Option<&[&str]>,
    extra_code_map: &[(&[&str], ErrorKind)],
) -> Result<Value, Error> {
    let fields = response.as_object().ok_or_else(|| not_a_dict("response"))?;
    let code = fields.get("code").map(py_str).unwrap_or_default();
    if code != "0" {
        let message = fields.get("message").map(py_str).unwrap_or_default();
        return Err(raise_for_code(
            endpoint,
            &code,
            &message,
            vin,
            not_supported_codes,
            extra_code_map,
        ));
    }
    decode_respond_data(endpoint, response, content_key)
}

/// The HTTP body for an outer payload: `{"request": "F..."}` with the
/// payload's compact JSON in a Bangcle envelope.
pub fn encode_request_body(outer: &Map<String, Value>) -> String {
    let text = dumps_compact(&Value::Object(outer.clone()));
    let envelope = openbyd_crypto::encode_envelope(text.as_bytes());
    dumps(&json!({ "request": envelope }), DumpOptions::DEFAULT)
}

/// Turns an HTTP reply into the decoded outer payload, as the second half
/// of pyBYD's `SecureTransport.post_secure`.
///
/// Fails with [`ErrorKind::BydTransportError`] for a status other than 200,
/// a body that is not JSON or has no usable `response`, and decoded text
/// that is not JSON; with [`ErrorKind::BangcleError`] when the envelope does
/// not open; and with [`ErrorKind::UnicodeDecodeError`] when it opens to
/// bytes that are not UTF-8. A stray `F` before the decoded JSON is dropped.
pub fn decode_response_body(endpoint: &str, status: u16, text: &str) -> Result<Value, Error> {
    let head = |count: usize| text.chars().take(count).collect::<String>();
    if status != 200 {
        return Err(Error::transport(
            format!("HTTP {status} from {endpoint}: {}", head(200)),
            Some(status),
            endpoint,
        ));
    }
    let body = loads(text).map_err(|_| {
        Error::transport(
            format!("Invalid JSON from {endpoint}: {}", head(200)),
            None,
            endpoint,
        )
    })?;
    let Some(response) = body.as_object().and_then(|body| body.get("response")) else {
        return Err(Error::transport(
            format!("Missing 'response' field from {endpoint}"),
            None,
            endpoint,
        ));
    };
    let response = match response {
        Value::String(response) if !response.trim_matches(is_python_space).is_empty() => response,
        _ => {
            return Err(Error::transport(
                format!("Empty response payload from {endpoint}"),
                None,
                endpoint,
            ));
        }
    };
    let bytes = openbyd_crypto::decode_envelope(response)?;
    let decoded = String::from_utf8(bytes)
        .map_err(|error| Error::new(ErrorKind::UnicodeDecodeError, error.to_string()))?;
    let mut decoded = decoded.trim_matches(is_python_space);
    // Some replies carry a stray `F` before the JSON.
    if decoded.starts_with("F{") || decoded.starts_with("F[") {
        decoded = &decoded[1..];
    }
    loads(decoded).map_err(|_| {
        let head: String = decoded.chars().take(64).collect();
        Error::transport(
            format!("Bangcle response from {endpoint} is not JSON: {head}"),
            None,
            endpoint,
        )
    })
}

fn map_argument<'a>(args: &Args<'a>, key: &str) -> Result<&'a Map<String, Value>, ParityError> {
    crate::config::object_argument(args, key)
}

fn status_argument(args: &Args<'_>) -> Result<u16, ParityError> {
    u16::try_from(args.i64("status")?)
        .map_err(|_| ParityError::bad_arguments("status must fit in u16"))
}

fn login_request_call(args: &Args<'_>) -> Result<Map<String, Value>, ParityError> {
    let config = crate::config::config_argument(args)?;
    Ok(build_login_request(
        &config,
        args.i64("now_ms")?,
        &crate::api::common::random_argument(args)?,
        args.i64("service_time_ms")?,
    ))
}

fn token_envelope_call(args: &Args<'_>) -> Result<(Map<String, Value>, String), ParityError> {
    let config = crate::config::config_argument(args)?;
    let session = crate::session::session_argument(args)?;
    Ok(build_token_outer_envelope(
        &config,
        &session,
        map_argument(args, "inner")?,
        args.i64("now_ms")?,
        args.opt_str("user_type")?,
        args.i64("service_time_ms")?,
    ))
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "envelope.build_login_request" => login_request_call(&args).map(Value::Object),
        "envelope.parse_login_response" => (|| {
            let token = parse_login_response(args.value("outer_response"), args.str("password")?)?;
            Ok(token.to_dump())
        })(),
        "envelope.build_token_outer_envelope" => {
            token_envelope_call(&args).map(|(outer, key)| json!([outer, key]))
        }
        "envelope.request_headers" => Ok(Value::Object(
            REQUEST_HEADERS
                .iter()
                .map(|(name, value)| ((*name).to_owned(), json!(value)))
                .collect(),
        )),
        "envelope.encode_request_body" => {
            map_argument(&args, "outer").map(|outer| json!(encode_request_body(outer)))
        }
        "envelope.decode_response_body" => (|| {
            Ok(decode_response_body(
                args.str("endpoint")?,
                status_argument(&args)?,
                args.str("text")?,
            )?)
        })(),
        "envelope.golden_login" => (|| {
            let body = encode_request_body(&login_request_call(&args)?);
            let reply = decode_response_body(LOGIN_ENDPOINT, 200, args.str("reply_text")?)?;
            let config = crate::config::config_argument(&args)?;
            let token = parse_login_response(&reply, &config.password)?;
            Ok(json!({"body": body, "token": token.to_dump()}))
        })(),
        "envelope.golden_token" => (|| {
            let (outer, content_key) = token_envelope_call(&args)?;
            let body = encode_request_body(&outer);
            let endpoint = args.str("endpoint")?;
            let reply = decode_response_body(endpoint, 200, args.str("reply_text")?)?;
            let not_supported: Option<Vec<&str>> = match args.value("not_supported_codes") {
                Value::Null => None,
                codes => Some(
                    codes
                        .as_array()
                        .and_then(|codes| codes.iter().map(Value::as_str).collect())
                        .ok_or_else(|| ParityError::bad_arguments("not_supported_codes"))?,
                ),
            };
            let result = handle_token_response(
                endpoint,
                &reply,
                &content_key,
                args.opt_str("vin")?,
                not_supported.as_deref(),
                &[],
            )?;
            Ok(json!({"body": body, "result": result}))
        })(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(outer: &Value) -> String {
        let envelope = openbyd_crypto::encode_envelope(dumps_compact(outer).as_bytes());
        dumps(&json!({ "response": envelope }), DumpOptions::DEFAULT)
    }

    #[test]
    fn transport_errors() {
        let kind =
            |status, text: &str| decode_response_body("/e", status, text).unwrap_err().kind();
        assert_eq!(kind(500, "x"), ErrorKind::BydTransportError);
        assert_eq!(
            decode_response_body("/e", 503, "")
                .unwrap_err()
                .status_code(),
            Some(503)
        );
        assert_eq!(kind(200, "x"), ErrorKind::BydTransportError);
        assert_eq!(kind(200, "[]"), ErrorKind::BydTransportError);
        assert_eq!(
            kind(200, r#"{"response": " "}"#),
            ErrorKind::BydTransportError
        );
        assert_eq!(kind(200, r#"{"response": "G"}"#), ErrorKind::BangcleError);
        let not_utf8 = openbyd_crypto::encode_envelope(&[0xff]);
        assert_eq!(
            kind(200, &format!(r#"{{"response": "{not_utf8}"}}"#)),
            ErrorKind::UnicodeDecodeError
        );
        let not_json = openbyd_crypto::encode_envelope(b"nope");
        assert_eq!(
            kind(200, &format!(r#"{{"response": "{not_json}"}}"#)),
            ErrorKind::BydTransportError
        );
        let stray = openbyd_crypto::encode_envelope(b" F{\"a\":1} ");
        assert_eq!(
            decode_response_body("/e", 200, &format!(r#"{{"response": "{stray}"}}"#)).unwrap(),
            json!({"a": 1})
        );
    }

    #[test]
    fn login_round_trip() {
        let config = BydConfig::new("user@example.com", "pa$$ \"wörd\" 🚗");
        let outer = build_login_request(&config, 1_700_000_000_000, &[7; 16], 1_700_000_000_005);
        assert_eq!(outer["appName"], "pyBYD+0.0.86");
        assert!(encode_request_body(&outer).starts_with("{\"request\": \"F"));
        let token = json!({"userId": 5, "signToken": "s", "encryToken": "e"});
        let respond = aes_encrypt_hex(
            &dumps_compact(&json!({"token": token})),
            &pwd_login_key(&config.password),
        )
        .unwrap();
        let text = reply(&json!({"code": "0", "respondData": respond}));
        let decoded = decode_response_body(LOGIN_ENDPOINT, 200, &text).unwrap();
        let parsed = parse_login_response(&decoded, &config.password).unwrap();
        assert_eq!(parsed.user_id, "5");
        let fail = |value: Value| parse_login_response(&value, "p").unwrap_err().kind();
        assert_eq!(fail(json!([])), ErrorKind::AttributeError);
        assert_eq!(
            fail(json!({"code": 0.0})),
            ErrorKind::BydAuthenticationError
        );
        assert_eq!(fail(json!({"code": 0})), ErrorKind::BydAuthenticationError);
        assert_eq!(
            fail(json!({"code": "0", "respondData": 5})),
            ErrorKind::BydCryptoError
        );
    }

    #[test]
    fn token_round_trip() {
        let config = BydConfig::new("user@example.com", "p");
        let session = Session::new("42", "sign", "encry", 0.0, 1.0);
        let inner = crate::api::common::build_inner_base(
            &config,
            1,
            &[1; 16],
            Some("TESTVIN0000000001"),
            None,
        );
        let (outer, key) = build_token_outer_envelope(&config, &session, &inner, 1, Some("1"), 2);
        assert_eq!(outer["userType"], "1");
        assert_eq!(key, session.content_key());
        let data = aes_encrypt_hex("{\"ok\":true}", &key).unwrap();
        let ok = json!({"code": "0", "respondData": data});
        assert_eq!(
            handle_token_response("/e", &ok, &key, None, None, &[]).unwrap(),
            json!({"ok": true})
        );
        let busy = json!({"code": 1008, "message": "busy"});
        let error = handle_token_response("/e", &busy, &key, None, None, &[]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BydServiceBusyError);
        assert_eq!(error.code(), "1008");
        assert_eq!(
            handle_token_response("/e", &json!(1), &key, None, None, &[])
                .unwrap_err()
                .kind(),
            ErrorKind::AttributeError
        );
        let blank = aes_encrypt_hex("  ", &key).unwrap();
        assert_eq!(
            decode_respond_data("/e", &json!({"respondData": blank}), &key).unwrap(),
            json!({})
        );
        let bad = aes_encrypt_hex("{", &key).unwrap();
        let error = decode_respond_data("/e", &json!({"respondData": bad}), &key).unwrap_err();
        assert_eq!(error.code(), "invalid_json");
        assert_eq!(
            decode_respond_data("/e", &json!({"respondData": ""}), &key).unwrap(),
            json!({})
        );
    }
}
