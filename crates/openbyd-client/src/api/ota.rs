//! OTA firmware version and night auto-upgrade endpoints (`pybyd/_api/ota.py`).
//!
//! - `/control/otaUpgrade/getOtaVersion`: the current and offered version
//!   and the auto-upgrade settings.
//! - `/control/otaUpgrade/syncAutoUpgrade`: turns the night auto-upgrade on
//!   or off and sets its time.

use openbyd_protocol::aes::is_python_space;
use openbyd_protocol::coerce::py_decimal;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::control::CommandAck;
use openbyd_protocol::models::ota::OtaUpgradeInfo;
use openbyd_protocol::parity::{Args, Outcome, ParityError};
use serde_json::{Map, Value, json};

use crate::api::Context;
use crate::api::common::{
    ENDPOINT_NOT_SUPPORTED_CODES, ErrorCodes, build_inner_base, post_token_json,
};
use crate::api::vehicle::{parse_owned_model, run_trace};
use crate::env::Env;
use crate::scripted::Scenario;

/// Where the OTA version is read.
pub const GET_OTA_VERSION_ENDPOINT: &str = "/control/otaUpgrade/getOtaVersion";
/// Where the night auto-upgrade is set.
pub const SYNC_AUTO_UPGRADE_ENDPOINT: &str = "/control/otaUpgrade/syncAutoUpgrade";

const NOT_SUPPORTED: ErrorCodes<'static> = ErrorCodes {
    not_supported_codes: Some(&ENDPOINT_NOT_SUPPORTED_CODES),
    extra_code_map: &[],
};

/// One or more Unicode decimal digits, as Python's `\d` matches them, and
/// their value; `None` for anything else.
fn decimal(text: &str) -> Option<u32> {
    text.chars().try_fold(0u32, |total, ch| {
        let digit = py_decimal(ch)?;
        total.checked_mul(10)?.checked_add(digit)
    })
}

/// Checks a night-upgrade time and writes it `H:mm` (pyBYD's
/// `normalize_ota_upgrade_time`): `"02:00"` is `"2:00"`. Anything but one
/// or two digits, a colon and two digits, an hour above 23 or a minute
/// above 59 is a `ValueError`.
pub fn normalize_ota_upgrade_time(value: &str) -> Result<String, Error> {
    let invalid = || {
        Error::new(
            ErrorKind::ValueError,
            format!("Invalid auto_upgrade_time {value:?}; expected H:mm (e.g. '2:00')"),
        )
    };
    // `^(\d{1,2}):(\d{2})$` on `value.strip()`.
    let stripped = value.trim_matches(is_python_space);
    let (hour_text, minute_text) = stripped.split_once(':').ok_or_else(invalid)?;
    let hour_digits = hour_text.chars().count();
    if !(1..=2).contains(&hour_digits) || minute_text.chars().count() != 2 {
        return Err(invalid());
    }
    let (hour, minute) = match (decimal(hour_text), decimal(minute_text)) {
        (Some(hour), Some(minute)) => (hour, minute),
        _ => return Err(invalid()),
    };
    if hour > 23 || minute > 59 {
        return Err(Error::new(
            ErrorKind::ValueError,
            format!("Invalid auto_upgrade_time {value:?}; hour must be 0..23 and minute 0..59"),
        ));
    }
    Ok(format!("{hour}:{minute:02}"))
}

/// `{"vin": vin, **raw, "raw": raw}`, validated as `M`.
fn with_vin_and_raw<M: BydModel>(vin: &str, decoded: Value) -> Result<M, Error> {
    let raw = match decoded {
        Value::Object(raw) => raw,
        _ => Map::new(),
    };
    let mut merged = Map::new();
    merged.insert("vin".into(), json!(vin));
    merged.extend(raw.clone());
    merged.insert("raw".into(), Value::Object(raw));
    parse_owned_model(Value::Object(merged))
}

/// Fetches the OTA version, release notes and night auto-upgrade settings.
/// The request carries `language`: the cloud answers 1010 without it.
pub async fn fetch_ota_version<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
) -> Result<OtaUpgradeInfo, Error> {
    let mut inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        None,
    );
    inner.insert("language".into(), json!(cx.config.language));
    let decoded = post_token_json(
        cx,
        GET_OTA_VERSION_ENDPOINT,
        &inner,
        None,
        Some(vin),
        NOT_SUPPORTED,
        None,
    )
    .await?;
    with_vin_and_raw(vin, decoded)
}

/// Turns the night auto-upgrade on or off and sets its time (`H:mm`,
/// checked by [`normalize_ota_upgrade_time`] before anything is sent).
pub async fn sync_auto_upgrade<E: Env>(
    cx: Context<'_, E>,
    vin: &str,
    enabled: bool,
    auto_upgrade_time: &str,
    command_pwd: &str,
) -> Result<CommandAck, Error> {
    let normalized = normalize_ota_upgrade_time(auto_upgrade_time)?;
    let mut inner = build_inner_base(
        cx.config,
        cx.env.now_ms(),
        &cx.env.random_bytes(),
        Some(vin),
        None,
    );
    inner.insert(
        "autoUpgradeSwitch".into(),
        json!(if enabled { "1" } else { "0" }),
    );
    inner.insert("autoUpgradeTime".into(), json!(normalized));
    inner.insert("commandPwd".into(), json!(command_pwd));
    let decoded = post_token_json(
        cx,
        SYNC_AUTO_UPGRADE_ENDPOINT,
        &inner,
        None,
        Some(vin),
        NOT_SUPPORTED,
        None,
    )
    .await?;
    with_vin_and_raw(vin, decoded)
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "api.ota.normalize_ota_upgrade_time" => match Args(args).value("value") {
            Value::String(value) => normalize_ota_upgrade_time(value)
                .map(|time| json!(time))
                .map_err(ParityError::from),
            _ => Err(ParityError::new(
                "AttributeError",
                "object has no attribute 'strip'",
            )),
        },
        "api.ota.fetch_ota_version" => Scenario::from_args(args).and_then(|scenario| {
            let vin = scenario.args().str("vin")?;
            run_trace(
                &scenario,
                fetch_ota_version(scenario.context(), vin),
                |info| info.dump(),
            )
        }),
        "api.ota.sync_auto_upgrade" => Scenario::from_args(args).and_then(|scenario| {
            let args = scenario.args();
            let vin = args.str("vin")?;
            let enabled = args
                .value("enabled")
                .as_bool()
                .ok_or_else(|| ParityError::bad_arguments("enabled must be a boolean"))?;
            let time = match args.value("auto_upgrade_time") {
                Value::Null => "2:00",
                _ => args.str("auto_upgrade_time")?,
            };
            let pwd = args.str("command_pwd")?;
            run_trace(
                &scenario,
                sync_auto_upgrade(scenario.context(), vin, enabled, time, pwd),
                |ack| ack.dump(),
            )
        }),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_normalized_like_pybyd() {
        for (raw, expected) in [
            ("2:00", "2:00"),
            ("02:00", "2:00"),
            ("00:05", "0:05"),
            ("23:59", "23:59"),
            (" 3:07\n", "3:07"),
            ("\u{0663}:\u{0660}\u{0660}", "3:00"),
        ] {
            assert_eq!(normalize_ota_upgrade_time(raw).unwrap(), expected);
        }
        for bad in [
            "",
            "24:00",
            "12:60",
            "2am",
            "1230",
            "1:2",
            "123:00",
            ":00",
            "1:000",
            "\u{b2}:00",
        ] {
            let error = normalize_ota_upgrade_time(bad).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ValueError, "{bad}");
            assert!(error.message().contains("auto_upgrade_time"));
        }
        let call = |value: Value| {
            parity_call(
                "api.ota.normalize_ota_upgrade_time",
                &json!({"value": value}),
            )
            .unwrap()
        };
        assert_eq!(call(json!(5)).unwrap_err().kind, "AttributeError");
        assert!(parity_call("api.ota.nothing", &json!({})).is_none());
    }
}
