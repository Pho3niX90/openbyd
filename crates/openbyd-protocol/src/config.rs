//! Client configuration and device profile (`pybyd/config.py`).
//!
//! pyBYD's `BydConfig` and `DeviceProfile` are pydantic models. Building
//! them from loose values ([`BydConfig::from_kwargs`]) follows pydantic's
//! lax rules for the field types used here: strings are stripped, numbers
//! may be given as strings, `"yes"`/`"off"` are booleans, and unknown or
//! missing fields are a [`ErrorKind::ValidationError`].

use std::fmt;

use serde_json::{Map, Value, json};

use crate::error::{Error, ErrorKind};
use crate::hashing::md5_hex;
use crate::parity::{Args, Outcome, ParityError};
use crate::pyjson::{float_repr, int_literal, py_float_from_str, py_int_from_str, py_str};

/// The `imei_md5` placeholder that tells the config to derive it from the username.
pub const ZERO_IMEI_MD5: &str = "00000000000000000000000000000000";

/// The login request's `appName` that pyBYD sends at the pinned commit:
/// `pyBYD+` and the package version without its dev suffix.
pub const DEFAULT_APP_NAME: &str = "pyBYD+0.0.91";

/// Device identity fields sent with every request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceProfile {
    /// Outer `ostype`.
    pub ostype: String,
    /// Outer `imei`.
    pub imei: String,
    /// Outer `mac`.
    pub mac: String,
    /// Outer `model`.
    pub model: String,
    /// Outer `sdk`.
    pub sdk: String,
    /// Outer `mod` (the manufacturer).
    pub r#mod: String,
    /// `imeiMD5`; the zero placeholder is replaced by MD5 of the username.
    pub imei_md5: String,
    /// Login `mobileBrand`.
    pub mobile_brand: String,
    /// Login `mobileModel`.
    pub mobile_model: String,
    /// Inner `deviceType`.
    pub device_type: String,
    /// Inner `networkType`.
    pub network_type: String,
    /// Login `osType`.
    pub os_type: String,
    /// Login `osVersion`.
    pub os_version: String,
}

const DEVICE_FIELDS: [&str; 13] = [
    "ostype",
    "imei",
    "mac",
    "model",
    "sdk",
    "mod",
    "imei_md5",
    "mobile_brand",
    "mobile_model",
    "device_type",
    "network_type",
    "os_type",
    "os_version",
];

impl Default for DeviceProfile {
    fn default() -> Self {
        Self {
            ostype: "and".into(),
            imei: "BANGCLE01234".into(),
            mac: "00:00:00:00:00:00".into(),
            model: "POCO F1".into(),
            sdk: "35".into(),
            r#mod: "Xiaomi".into(),
            imei_md5: ZERO_IMEI_MD5.into(),
            mobile_brand: "XIAOMI".into(),
            mobile_model: "POCO F1".into(),
            device_type: "0".into(),
            network_type: "wifi".into(),
            os_type: "15".into(),
            os_version: "35".into(),
        }
    }
}

impl DeviceProfile {
    fn field_mut(&mut self, name: &str) -> Option<&mut String> {
        Some(match name {
            "ostype" => &mut self.ostype,
            "imei" => &mut self.imei,
            "mac" => &mut self.mac,
            "model" => &mut self.model,
            "sdk" => &mut self.sdk,
            "mod" => &mut self.r#mod,
            "imei_md5" => &mut self.imei_md5,
            "mobile_brand" => &mut self.mobile_brand,
            "mobile_model" => &mut self.mobile_model,
            "device_type" => &mut self.device_type,
            "network_type" => &mut self.network_type,
            "os_type" => &mut self.os_type,
            "os_version" => &mut self.os_version,
            _ => return None,
        })
    }

    fn field(&self, name: &str) -> &str {
        match name {
            "ostype" => &self.ostype,
            "imei" => &self.imei,
            "mac" => &self.mac,
            "model" => &self.model,
            "sdk" => &self.sdk,
            "mod" => &self.r#mod,
            "imei_md5" => &self.imei_md5,
            "mobile_brand" => &self.mobile_brand,
            "mobile_model" => &self.mobile_model,
            "device_type" => &self.device_type,
            "network_type" => &self.network_type,
            "os_type" => &self.os_type,
            "os_version" => &self.os_version,
            _ => "",
        }
    }

    /// Builds a profile from loose values as pydantic would: every field is
    /// a string (stripped), unknown fields are rejected.
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> Result<Self, Error> {
        let mut profile = Self::default();
        for (key, value) in kwargs {
            let slot = profile.field_mut(key).ok_or_else(|| {
                validation_error(format!("device.{key}: extra fields not permitted"))
            })?;
            *slot = lax_str(value, key)?;
        }
        Ok(profile)
    }

    /// The profile as pyBYD's `model_dump(mode="json")` gives it.
    pub fn to_dump(&self) -> Value {
        Value::Object(
            DEVICE_FIELDS
                .iter()
                .map(|name| ((*name).to_owned(), json!(self.field(name))))
                .collect(),
        )
    }
}

/// Client configuration.
///
/// Build one with [`BydConfig::new`] and adjust the public fields, or from
/// loose values with [`BydConfig::from_kwargs`] / [`BydConfig::from_env`].
/// After changing `username` or `device.imei_md5` by hand, call
/// [`BydConfig::derive_imei_md5`].
#[derive(Clone, PartialEq)]
pub struct BydConfig {
    /// BYD account email or phone number.
    pub username: String,
    /// BYD account password.
    pub password: String,
    /// API base URL.
    pub base_url: String,
    /// ISO country code, for example `"NL"`.
    pub country_code: String,
    /// Language code, for example `"en"`.
    pub language: String,
    /// IANA time zone.
    pub time_zone: String,
    /// App version sent to the API.
    pub app_version: String,
    /// Internal app version number.
    pub app_inner_version: String,
    /// Software type identifier.
    pub soft_type: String,
    /// T-Box version.
    pub tbox_version: String,
    /// Auto-login flag.
    pub is_auto: String,
    /// Login identifier type: `"0"` is email.
    pub identifier_type: String,
    /// The 6-digit remote control PIN, if set.
    pub control_pin: Option<String>,
    /// Session lifetime in seconds; 0 disables expiry.
    pub session_ttl: f64,
    /// Whether to use MQTT for realtime updates.
    pub mqtt_enabled: bool,
    /// MQTT keepalive in seconds.
    pub mqtt_keepalive: i64,
    /// Seconds to wait for an MQTT reply before polling over HTTP.
    pub mqtt_timeout: f64,
    /// Device identity fields.
    pub device: DeviceProfile,
    /// The login request's `appName`. Not a pyBYD config field: pyBYD
    /// derives it from its package version. Defaults to [`DEFAULT_APP_NAME`].
    pub app_name: String,
}

const CONFIG_FIELDS: [&str; 18] = [
    "username",
    "password",
    "base_url",
    "country_code",
    "language",
    "time_zone",
    "app_version",
    "app_inner_version",
    "soft_type",
    "tbox_version",
    "is_auto",
    "identifier_type",
    "control_pin",
    "session_ttl",
    "mqtt_enabled",
    "mqtt_keepalive",
    "mqtt_timeout",
    "device",
];

impl fmt::Debug for BydConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BydConfig")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("country_code", &self.country_code)
            .field("language", &self.language)
            .field("time_zone", &self.time_zone)
            .field("app_version", &self.app_version)
            .field("app_inner_version", &self.app_inner_version)
            .field("soft_type", &self.soft_type)
            .field("tbox_version", &self.tbox_version)
            .field("is_auto", &self.is_auto)
            .field("identifier_type", &self.identifier_type)
            .field(
                "control_pin",
                &self.control_pin.as_ref().map(|_| "<redacted>"),
            )
            .field("session_ttl", &self.session_ttl)
            .field("mqtt_enabled", &self.mqtt_enabled)
            .field("mqtt_keepalive", &self.mqtt_keepalive)
            .field("mqtt_timeout", &self.mqtt_timeout)
            .field("device", &self.device)
            .field("app_name", &self.app_name)
            .finish()
    }
}

impl BydConfig {
    /// A config with pyBYD's defaults for everything but the credentials.
    /// Surrounding whitespace is stripped, as pydantic does.
    pub fn new(username: &str, password: &str) -> Self {
        let mut config = Self {
            username: username.trim().to_owned(),
            password: password.trim().to_owned(),
            base_url: "https://dilinkappoversea-eu.byd.auto".into(),
            country_code: "NL".into(),
            language: "en".into(),
            time_zone: "Europe/Amsterdam".into(),
            app_version: "3.6.0".into(),
            app_inner_version: "363".into(),
            soft_type: "0".into(),
            tbox_version: "3".into(),
            is_auto: "1".into(),
            identifier_type: "0".into(),
            control_pin: None,
            session_ttl: 12.0 * 3600.0,
            mqtt_enabled: true,
            mqtt_keepalive: 120,
            mqtt_timeout: 10.0,
            device: DeviceProfile::default(),
            app_name: DEFAULT_APP_NAME.into(),
        };
        config.derive_imei_md5();
        config
    }

    /// Replaces the zero `imei_md5` placeholder with MD5 of the username, as
    /// pyBYD's model validator does.
    pub fn derive_imei_md5(&mut self) {
        if self.device.imei_md5 == ZERO_IMEI_MD5 {
            self.device.imei_md5 = md5_hex(&self.username);
        }
    }

    /// Builds a config from loose values, as `BydConfig(**kwargs)` does.
    pub fn from_kwargs(kwargs: &Map<String, Value>) -> Result<Self, Error> {
        for required in ["username", "password"] {
            if !kwargs.contains_key(required) {
                return Err(validation_error(format!("{required}: field required")));
            }
        }
        let mut config = Self::new("", "");
        config.device.imei_md5 = ZERO_IMEI_MD5.into();
        for (key, value) in kwargs {
            match key.as_str() {
                "username" => config.username = lax_str(value, key)?,
                "password" => config.password = lax_str(value, key)?,
                "base_url" => config.base_url = lax_str(value, key)?,
                "country_code" => config.country_code = lax_str(value, key)?,
                "language" => config.language = lax_str(value, key)?,
                "time_zone" => config.time_zone = lax_str(value, key)?,
                "app_version" => config.app_version = lax_str(value, key)?,
                "app_inner_version" => config.app_inner_version = lax_str(value, key)?,
                "soft_type" => config.soft_type = lax_str(value, key)?,
                "tbox_version" => config.tbox_version = lax_str(value, key)?,
                "is_auto" => config.is_auto = lax_str(value, key)?,
                "identifier_type" => config.identifier_type = lax_str(value, key)?,
                "control_pin" => {
                    config.control_pin = match value {
                        Value::Null => None,
                        value => Some(lax_str(value, key)?),
                    }
                }
                "session_ttl" => config.session_ttl = lax_float(value, key)?,
                "mqtt_enabled" => config.mqtt_enabled = lax_bool(value, key)?,
                "mqtt_keepalive" => config.mqtt_keepalive = lax_int(value, key)?,
                "mqtt_timeout" => config.mqtt_timeout = lax_float(value, key)?,
                "device" => {
                    config.device = match value {
                        Value::Object(fields) => DeviceProfile::from_kwargs(fields)?,
                        _ => return Err(validation_error("device: input should be an object")),
                    }
                }
                other => {
                    return Err(validation_error(format!(
                        "{other}: extra fields not permitted"
                    )));
                }
            }
        }
        config.derive_imei_md5();
        Ok(config)
    }

    /// Builds a config from `BYD_*` environment variables, as
    /// `BydConfig.from_env(**overrides)` does. `env` looks a variable up;
    /// pass `|name| std::env::var(name).ok()` for the real environment.
    ///
    /// Fails with [`ErrorKind::ValueError`] when `BYD_SESSION_TTL`,
    /// `BYD_MQTT_KEEPALIVE` or `BYD_MQTT_TIMEOUT` is not a number, and with
    /// [`ErrorKind::ValidationError`] when the result is not a valid config.
    pub fn from_env(
        env: impl Fn(&str) -> Option<String>,
        overrides: &Map<String, Value>,
    ) -> Result<Self, Error> {
        const DEVICE_ENV: [(&str, &str); 13] = [
            ("BYD_OSTYPE", "ostype"),
            ("BYD_IMEI", "imei"),
            ("BYD_MAC", "mac"),
            ("BYD_MODEL", "model"),
            ("BYD_SDK", "sdk"),
            ("BYD_MOD", "mod"),
            ("BYD_IMEI_MD5", "imei_md5"),
            ("BYD_MOBILE_BRAND", "mobile_brand"),
            ("BYD_MOBILE_MODEL", "mobile_model"),
            ("BYD_DEVICE_TYPE", "device_type"),
            ("BYD_NETWORK_TYPE", "network_type"),
            ("BYD_OS_TYPE", "os_type"),
            ("BYD_OS_VERSION", "os_version"),
        ];
        const CONFIG_ENV: [(&str, &str); 13] = [
            ("BYD_USERNAME", "username"),
            ("BYD_PASSWORD", "password"),
            ("BYD_BASE_URL", "base_url"),
            ("BYD_COUNTRY_CODE", "country_code"),
            ("BYD_LANGUAGE", "language"),
            ("BYD_TIME_ZONE", "time_zone"),
            ("BYD_APP_VERSION", "app_version"),
            ("BYD_APP_INNER_VERSION", "app_inner_version"),
            ("BYD_SOFT_TYPE", "soft_type"),
            ("BYD_TBOX_VERSION", "tbox_version"),
            ("BYD_IS_AUTO", "is_auto"),
            ("BYD_IDENTIFIER_TYPE", "identifier_type"),
            ("BYD_CONTROL_PIN", "control_pin"),
        ];

        let mut device_kwargs = Map::new();
        for (variable, field) in DEVICE_ENV {
            if let Some(value) = env(variable) {
                device_kwargs.insert(field.into(), Value::String(value));
            }
        }
        let mut overrides = overrides.clone();
        // pyBYD ignores a `device` override that is neither a dict nor a profile.
        if let Some(Value::Object(fields)) = overrides.shift_remove("device") {
            for (key, value) in fields {
                device_kwargs.insert(key, Value::String(py_str(&value)));
            }
        }
        let device = DeviceProfile::from_kwargs(&device_kwargs)?;

        let mut kwargs = Map::new();
        kwargs.insert("device".into(), device.to_dump());
        for (variable, field) in CONFIG_ENV {
            if let Some(value) = env(variable) {
                kwargs.insert(field.into(), Value::String(value));
            }
        }
        let not_a_number =
            |name: &str| Error::new(ErrorKind::ValueError, format!("{name} is not a number"));
        if let Some(text) = env("BYD_SESSION_TTL")
            && !overrides.contains_key("session_ttl")
        {
            let ttl = py_float_from_str(&text).ok_or_else(|| not_a_number("BYD_SESSION_TTL"))?;
            kwargs.insert("session_ttl".into(), float_argument(ttl));
        }
        if !overrides.contains_key("mqtt_enabled") {
            let enabled = env_bool(env("BYD_MQTT_ENABLED").as_deref(), true);
            kwargs.insert("mqtt_enabled".into(), Value::Bool(enabled));
        }
        if let Some(text) = env("BYD_MQTT_KEEPALIVE")
            && !overrides.contains_key("mqtt_keepalive")
        {
            let keepalive =
                py_int_from_str(&text).ok_or_else(|| not_a_number("BYD_MQTT_KEEPALIVE"))?;
            kwargs.insert(
                "mqtt_keepalive".into(),
                Value::String(keepalive.to_string()),
            );
        }
        // `or`: an empty BYD_MQTT_TIMEOUT falls through to the older name.
        let timeout = env("BYD_MQTT_TIMEOUT")
            .filter(|text| !text.is_empty())
            .or_else(|| env("BYD_MQTT_COMMAND_TIMEOUT"));
        if let Some(text) = timeout
            && !overrides.contains_key("mqtt_timeout")
        {
            let seconds =
                py_float_from_str(&text).ok_or_else(|| not_a_number("BYD_MQTT_TIMEOUT"))?;
            kwargs.insert("mqtt_timeout".into(), float_argument(seconds));
        }
        for (key, value) in overrides {
            kwargs.insert(key, value);
        }
        Self::from_kwargs(&kwargs)
    }

    /// The config as pyBYD's `model_dump(mode="json")` gives it. Infinite
    /// and NaN floats, which JSON cannot hold, are written as `"inf"`,
    /// `"-inf"` and `"nan"`. `app_name` is not a pyBYD field and is left out.
    pub fn to_dump(&self) -> Value {
        let mut dump = Map::new();
        for name in CONFIG_FIELDS {
            let value = match name {
                "username" => json!(self.username),
                "password" => json!(self.password),
                "base_url" => json!(self.base_url),
                "country_code" => json!(self.country_code),
                "language" => json!(self.language),
                "time_zone" => json!(self.time_zone),
                "app_version" => json!(self.app_version),
                "app_inner_version" => json!(self.app_inner_version),
                "soft_type" => json!(self.soft_type),
                "tbox_version" => json!(self.tbox_version),
                "is_auto" => json!(self.is_auto),
                "identifier_type" => json!(self.identifier_type),
                "control_pin" => json!(self.control_pin),
                "session_ttl" => float_dump(self.session_ttl),
                "mqtt_enabled" => json!(self.mqtt_enabled),
                "mqtt_keepalive" => json!(self.mqtt_keepalive),
                "mqtt_timeout" => float_dump(self.mqtt_timeout),
                _ => self.device.to_dump(),
            };
            dump.insert(name.into(), value);
        }
        Value::Object(dump)
    }
}

/// A float as the parity dumps write it: a number, or `"inf"`, `"-inf"`, `"nan"`.
pub(crate) fn float_dump(value: f64) -> Value {
    if value.is_finite() {
        json!(value)
    } else if value.is_nan() {
        json!("nan")
    } else if value > 0.0 {
        json!("inf")
    } else {
        json!("-inf")
    }
}

/// A float passed on to [`lax_float`] without losing NaN or infinity.
fn float_argument(value: f64) -> Value {
    if value.is_finite() {
        json!(value)
    } else {
        Value::String(float_repr(value))
    }
}

/// pyBYD's `_env_bool`.
fn env_bool(value: Option<&str>, default: bool) -> bool {
    let Some(value) = value else { return default };
    match value
        .trim_matches(crate::aes::is_python_space)
        .to_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "y" | "on" => true,
        "0" | "false" | "no" | "n" | "off" => false,
        _ => default,
    }
}

pub(crate) fn validation_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::ValidationError, message)
}

/// pydantic's lax `str` with `str_strip_whitespace`: only strings, trimmed
/// of Unicode whitespace (pydantic trims with Rust's `str::trim`).
pub(crate) fn lax_str(value: &Value, field: &str) -> Result<String, Error> {
    match value {
        Value::String(text) => Ok(text.trim().to_owned()),
        _ => Err(validation_error(format!(
            "{field}: input should be a valid string"
        ))),
    }
}

/// pydantic's lax `float`: numbers, booleans and numeric strings.
pub(crate) fn lax_float(value: &Value, field: &str) -> Result<f64, Error> {
    let error = || validation_error(format!("{field}: input should be a valid number"));
    match value {
        Value::Bool(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) => number.as_f64().ok_or_else(error),
        Value::String(text) => {
            if let Ok(float) = text.trim().parse::<f64>() {
                return Ok(float);
            }
            if text.starts_with('_') || text.ends_with('_') || text.contains("__") {
                return Err(error());
            }
            let stripped = text.replace('_', "");
            if stripped.is_empty() {
                return Err(error());
            }
            stripped.parse().map_err(|_| error())
        }
        _ => Err(error()),
    }
}

/// pydantic's lax `int`: integers, whole floats, booleans and integer strings
/// (`"1_000"`, `" 12 "`, `"3.00"`), within `i64`.
pub(crate) fn lax_int(value: &Value, field: &str) -> Result<i64, Error> {
    let error = || validation_error(format!("{field}: input should be a valid integer"));
    match value {
        Value::Bool(flag) => Ok(i64::from(*flag)),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                return Ok(int);
            }
            let float = number.as_f64().ok_or_else(error)?;
            // pydantic rejects ±2**63 and beyond, though -2**63 fits in i64.
            if float.fract() != 0.0
                || float <= -9.223372036854776e18
                || float >= 9.223372036854776e18
            {
                return Err(error());
            }
            Ok(float as i64)
        }
        Value::String(text) => {
            let mut text = text.trim();
            if let Some((whole, fraction)) = text.split_once('.') {
                if fraction.is_empty() || !fraction.chars().all(|ch| ch == '0') {
                    return Err(error());
                }
                text = whole;
            }
            int_literal(text)
                .and_then(|int| i64::try_from(int).ok())
                .ok_or_else(error)
        }
        _ => Err(error()),
    }
}

/// pydantic's lax `bool`: booleans, 0/1, and the usual words in any case.
pub(crate) fn lax_bool(value: &Value, field: &str) -> Result<bool, Error> {
    let error = || validation_error(format!("{field}: input should be a valid boolean"));
    match value {
        Value::Bool(flag) => Ok(*flag),
        Value::Number(number) => match number.as_f64() {
            Some(0.0) => Ok(false),
            Some(1.0) => Ok(true),
            _ => Err(error()),
        },
        Value::String(text) => match text.to_lowercase().as_str() {
            "0" | "off" | "f" | "false" | "n" | "no" => Ok(false),
            "1" | "on" | "t" | "true" | "y" | "yes" => Ok(true),
            _ => Err(error()),
        },
        _ => Err(error()),
    }
}

pub(crate) fn object_argument<'a>(
    args: &Args<'a>,
    key: &str,
) -> Result<&'a Map<String, Value>, ParityError> {
    args.value(key)
        .as_object()
        .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be an object")))
}

/// The config a parity call describes in `config` (keyword arguments of `BydConfig`).
pub(crate) fn config_argument(args: &Args<'_>) -> Result<BydConfig, ParityError> {
    Ok(BydConfig::from_kwargs(object_argument(args, "config")?)?)
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "config.DeviceProfile" => {
            (|| Ok(DeviceProfile::from_kwargs(object_argument(&args, "kwargs")?)?.to_dump()))()
        }
        "config.BydConfig" => {
            (|| Ok(BydConfig::from_kwargs(object_argument(&args, "kwargs")?)?.to_dump()))()
        }
        "config.BydConfig.from_env" => (|| {
            let env = object_argument(&args, "env")?;
            let overrides = object_argument(&args, "overrides")?;
            let lookup = |name: &str| env.get(name).and_then(Value::as_str).map(str::to_owned);
            Ok(BydConfig::from_env(lookup, overrides)?.to_dump())
        })(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_derived_imei_md5() {
        let config = BydConfig::new(" user@example.com ", "secret");
        assert_eq!(config.username, "user@example.com");
        assert_eq!(config.device.imei_md5, md5_hex("user@example.com"));
        assert_eq!(config.app_name, "pyBYD+0.0.91");
        let shown = format!("{config:?}");
        assert!(!shown.contains("secret"));
        let mut with_pin = config.clone();
        with_pin.control_pin = Some("123456".into());
        assert!(!format!("{with_pin:?}").contains("123456"));
    }

    #[test]
    fn lax_values() {
        assert_eq!(lax_float(&json!("1_0.5"), "f").unwrap(), 10.5);
        assert!(lax_float(&json!("_1"), "f").is_err());
        assert!(lax_float(&json!("1__0"), "f").is_err());
        assert!(lax_float(&json!("_"), "f").is_err());
        assert!(lax_float(&json!(null), "f").is_err());
        assert_eq!(lax_int(&json!(" 1_0.00 "), "i").unwrap(), 10);
        assert_eq!(lax_int(&json!(true), "i").unwrap(), 1);
        assert_eq!(
            lax_int(&json!(1e18), "i").unwrap(),
            1_000_000_000_000_000_000
        );
        assert!(lax_int(&json!(9.3e18), "i").is_err());
        assert!(lax_int(&json!(1.5), "i").is_err());
        assert!(lax_int(&json!("1."), "i").is_err());
        assert!(lax_int(&json!("- 1"), "i").is_err());
        assert!(lax_int(&json!([]), "i").is_err());
        assert!(lax_int(&json!(u64::MAX), "i").is_err());
        assert!(lax_bool(&json!("On"), "b").unwrap());
        assert!(!lax_bool(&json!(0.0), "b").unwrap());
        assert!(lax_bool(&json!(2), "b").is_err());
        assert!(lax_bool(&json!(null), "b").is_err());
        assert!(lax_bool(&json!("x"), "b").is_err());
        assert!(env_bool(Some(" YES "), false));
        assert!(!env_bool(Some("off"), true));
        assert!(env_bool(Some("maybe"), true));
        assert_eq!(float_dump(f64::NEG_INFINITY), json!("-inf"));
        assert_eq!(float_dump(f64::NAN), json!("nan"));
        assert_eq!(float_argument(f64::INFINITY), json!("Infinity"));
    }

    #[test]
    fn kwargs_are_validated() {
        let missing = BydConfig::from_kwargs(json!({"username": "u"}).as_object().unwrap());
        assert_eq!(missing.unwrap_err().kind(), ErrorKind::ValidationError);
        let extra = BydConfig::from_kwargs(
            json!({"username": "u", "password": "p", "x": 1})
                .as_object()
                .unwrap(),
        );
        assert_eq!(extra.unwrap_err().kind(), ErrorKind::ValidationError);
        let device = BydConfig::from_kwargs(
            json!({"username": "u", "password": "p", "device": "x"})
                .as_object()
                .unwrap(),
        );
        assert_eq!(device.unwrap_err().kind(), ErrorKind::ValidationError);
        assert_eq!(DeviceProfile::default().field("nope"), "");
    }
}
