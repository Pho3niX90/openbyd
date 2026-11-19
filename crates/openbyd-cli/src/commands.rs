//! The commands.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use openbyd_client::client::{ChargePollSettings, MqttEventCallback};
use openbyd_protocol::constants::minutes_to_time_span;
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::hashing::md5_hex;
use openbyd_protocol::models::control::{ClimateStartArgs, ClimateStartParams};
use serde_json::{Value, json};

use crate::app::{
    Client, Failure, Holder, Printer, Setup, connect, hide_tokens, note, secret_from_env_or_prompt,
};
use crate::cli::{
    CommandArgs, CommandName, DumpArgs, GetArgs, Global, LoginArgs, Section, WatchArgs,
};
use crate::scrub::global as scrubber;
use crate::sections::{self, Fetched};
use crate::session_file::{self, SavedSession};

/// The `Failure` for something that went wrong on this machine.
fn local(message: String) -> Failure {
    Failure::Api(Error::new(ErrorKind::BydError, message))
}

/// `openbyd version`.
pub fn version(printer: &Printer) -> Result<(), Failure> {
    printer.emit(&json!({
        "version": env!("CARGO_PKG_VERSION"),
        "pybyd_commit": env!("OPENBYD_UPSTREAM_COMMIT"),
    }));
    Ok(())
}

/// `openbyd login`.
pub async fn login(
    global: &Global,
    args: &LoginArgs,
    printer: &Printer,
    holder: &Holder,
) -> Result<(), Failure> {
    if global.session.is_some() {
        return Err(Failure::Usage(
            "login logs in with the password; --session is for the other commands".to_owned(),
        ));
    }
    let client = connect(global, Setup::default(), holder).await?;
    let vehicles = client.get_vehicles().await?;
    let tokens = client
        .export_session()
        .ok_or_else(|| local("the login left no session".to_owned()))?;
    if let Some(path) = &args.save_session {
        let saved = SavedSession {
            username: client.config().username.clone(),
            tokens: tokens.clone(),
            saved_at: session_file::now_seconds(),
        };
        session_file::save(path, &saved)
            .map_err(|error| local(format!("cannot write {}: {error}", path.display())))?;
        note(&format!(
            "session saved to {} (keep it private: it lets anyone act as you until it expires)",
            path.display()
        ));
    }
    printer.emit(&json!({
        "user_id": tokens.user_id,
        "vehicle_count": vehicles.len(),
        "session_saved": args.save_session.is_some(),
    }));
    Ok(())
}

/// `openbyd vehicles`.
pub async fn vehicles(global: &Global, printer: &Printer, holder: &Holder) -> Result<(), Failure> {
    let client = connect(global, Setup::default(), holder).await?;
    let list: Vec<Value> = client
        .get_vehicles()
        .await?
        .iter()
        .map(sections::vehicle_summary)
        .collect();
    printer.emit(&Value::Array(list));
    Ok(())
}

/// `openbyd dump`.
pub async fn dump(
    global: &Global,
    args: &DumpArgs,
    printer: &Printer,
    holder: &Holder,
) -> Result<(), Failure> {
    if let Some(vin) = &args.vin {
        scrubber().add_identity(vin, false);
    }
    let client = connect(global, Setup::default(), holder).await?;
    let listed = client.get_vehicles().await;
    let summaries: Vec<Value> = listed
        .as_ref()
        .map(|list| list.iter().map(sections::vehicle_summary).collect())
        .unwrap_or_default();
    scrubber().learn(&Value::Array(summaries.clone()));
    let targets: Vec<(String, Value)> = match (&args.vin, listed) {
        (Some(vin), listed) => {
            let found = summaries
                .iter()
                .find(|summary| summary["vin"] == json!(vin));
            let head = match (found, listed) {
                (Some(summary), _) => summary.clone(),
                (None, Err(error)) if sections::is_fatal(&error) => return Err(error.into()),
                (None, Err(error)) => sections::error_value(&error),
                (None, Ok(_)) => Value::Null,
            };
            vec![(vin.clone(), head)]
        }
        (None, Err(error)) => return Err(error.into()),
        (None, Ok(_)) => summaries
            .iter()
            .map(|summary| {
                (
                    summary["vin"].as_str().unwrap_or_default().to_owned(),
                    summary.clone(),
                )
            })
            .collect(),
    };
    let mut documents = Vec::new();
    for (vin, head) in targets {
        let mut document = serde_json::Map::new();
        document.insert("vin".to_owned(), json!(vin));
        document.insert("vehicle".to_owned(), head);
        for section in Section::ALL.iter().filter(|s| !args.skip.contains(s)) {
            let value = match sections::fetch(&client, *section, &vin).await {
                Ok(fetched) => fetched.into_value(args.raw),
                Err(error) if sections::is_fatal(&error) => return Err(error.into()),
                Err(error) => sections::error_value(&error),
            };
            document.insert(section.name(), value);
        }
        documents.push(Value::Object(document));
    }
    printer.emit(&json!({"vehicles": documents}));
    Ok(())
}

/// `openbyd get`.
pub async fn get(
    global: &Global,
    args: &GetArgs,
    printer: &Printer,
    holder: &Holder,
) -> Result<(), Failure> {
    scrubber().add_identity(&args.vin, false);
    let client = connect(global, Setup::default(), holder).await?;
    let fetched: Fetched = sections::fetch(&client, args.section, &args.vin).await?;
    printer.emit(&fetched.into_value(args.raw));
    Ok(())
}

/// The current time as `2026-01-31T12:34:56Z`.
fn iso_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    iso_time(seconds)
}

/// Unix seconds as an ISO 8601 UTC time.
fn iso_time(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// `openbyd watch`.
pub async fn watch(
    global: &Global,
    args: &WatchArgs,
    printer: &Printer,
    holder: &Holder,
) -> Result<(), Failure> {
    let limit = match args.seconds {
        None => None,
        Some(seconds) => Some(
            Duration::try_from_secs_f64(seconds)
                .ok()
                .filter(|limit| !limit.is_zero())
                .ok_or_else(|| Failure::Usage("--seconds must be a positive number".to_owned()))?,
        ),
    };
    if let Some(vin) = &args.vin {
        scrubber().add_identity(vin, false);
    }
    let (sender, mut events) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let wanted = args.vin.clone();
    let on_event: MqttEventCallback = Arc::new(move |event, vin, data| {
        if wanted.as_deref().is_none_or(|wanted| wanted == vin) {
            let _ = sender.send(json!({
                "time": iso_now(),
                "event": event,
                "vin": vin,
                "data": data,
            }));
        }
    });
    let setup = Setup {
        force_mqtt: true,
        on_event: Some(on_event),
        ..Setup::default()
    };
    let client = connect(global, setup, holder).await?;
    if !client.mqtt_is_running() {
        return Err(local(
            "the MQTT channel did not start; run with -vv to see why".to_owned(),
        ));
    }
    note("watching for events (Ctrl-C to stop)");
    let timer = async {
        match limit {
            Some(limit) => tokio::time::sleep(limit).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(timer);
    loop {
        tokio::select! {
            Some(event) = events.recv() => printer.emit_json_line(&event),
            () = &mut timer => break,
        }
    }
    Ok(())
}

/// What `command` is about to do, in words.
fn describe(args: &CommandArgs, climate: Option<(f64, i64)>) -> String {
    let extra = match climate {
        Some((temperature, minutes)) => format!(" at {temperature} C for {minutes} minutes"),
        None => String::new(),
    };
    format!(
        "{}{extra} to the car with VIN {}",
        args.name.name(),
        args.vin
    )
}

/// The climate settings of `start-climate`: temperature, minutes and the parameters.
fn climate_params(args: &CommandArgs) -> Result<(f64, i64, ClimateStartParams), Failure> {
    let temperature = args.temperature.unwrap_or(21.0);
    let minutes = args.minutes.unwrap_or(20);
    let time_span = minutes_to_time_span(minutes)
        .map_err(|error| Failure::Usage(format!("--minutes: {}", error.message())))?;
    let params = ClimateStartParams::new(ClimateStartArgs {
        temperature: Some(temperature),
        time_span: Some(time_span),
        ..ClimateStartArgs::default()
    })
    .map_err(|error| Failure::Usage(format!("--temperature: {error}")))?;
    Ok((temperature, minutes, params))
}

/// `openbyd command`.
pub async fn send(
    global: &Global,
    args: &CommandArgs,
    printer: &Printer,
    holder: &Holder,
) -> Result<(), Failure> {
    let climate = if args.name == CommandName::StartClimate {
        Some(climate_params(args)?)
    } else if args.temperature.is_some() || args.minutes.is_some() {
        return Err(Failure::Usage(
            "--temperature and --minutes only apply to start-climate".to_owned(),
        ));
    } else {
        None
    };
    scrubber().add_identity(&args.vin, false);
    let what = describe(args, climate.as_ref().map(|(t, m, _)| (*t, *m)));
    note(&format!("about to send {what}"));
    if !args.yes_send_to_car {
        return Err(Failure::Usage(
            "refusing to send: this moves a real car. Nothing was sent. Add --yes-send-to-car to send it"
                .to_owned(),
        ));
    }
    if args.name == CommandName::StopCharging {
        // The client refuses this without sending anything, so there is no
        // reason to log in or ask for the PIN.
        return Err(Failure::Api(Error::api(
            ErrorKind::BydEndpointNotSupportedError,
            "stop-charging is not supported: the BYD cloud accepts it but the car keeps \
             charging. Stop a charge with the key fob or the door-handle double-unlock",
            "cloud_stop_unsupported",
            "/control/smartCharge/changeChargeStatue",
        )));
    }
    let needs_pin = args.name != CommandName::StartCharging;
    let pin = if !needs_pin {
        None
    } else if let Some(pin) = args.pin.clone().filter(|pin| !pin.is_empty()) {
        Some(pin)
    } else {
        Some(secret_from_env_or_prompt("BYD_CONTROL_PIN", "control PIN")?)
    };
    if let Some(pin) = &pin {
        scrubber().add_secret(pin);
        scrubber().add_secret(&md5_hex(pin));
        scrubber().add_secret(&md5_hex(pin).to_lowercase());
    }
    let setup = Setup {
        command_retries: Some(i64::from(args.retries)),
        ..Setup::default()
    };
    let client = connect(global, setup, holder).await?;
    hide_tokens(&client);
    let (result, _raw) =
        run_command(&client, args, pin.as_deref(), climate.map(|(_, _, p)| p)).await?;
    printer.emit(&json!({
        "command": args.name.name(),
        "vin": args.vin,
        "sent": true,
        "result": result,
    }));
    Ok(())
}

/// Sends the command and returns the result without its `raw` payload, and the payload.
async fn run_command(
    client: &Client,
    args: &CommandArgs,
    pin: Option<&str>,
    climate: Option<ClimateStartParams>,
) -> Result<(Value, Value), Failure> {
    let vin = args.vin.as_str();
    if let Some(pin) = pin {
        client.verify_command_access(vin, Some(pin)).await?;
    }
    let result = match args.name {
        CommandName::Lock => sections::split_value(&client.lock(vin, pin).await?),
        CommandName::Unlock => sections::split_value(&client.unlock(vin, pin).await?),
        CommandName::FlashLights => sections::split_value(&client.flash_lights(vin, pin).await?),
        CommandName::FindCar => sections::split_value(&client.find_car(vin, pin).await?),
        CommandName::StartClimate => {
            let params = climate.expect("start-climate has its parameters");
            sections::split_value(&client.start_climate(vin, &params, pin).await?)
        }
        CommandName::StopClimate => sections::split_value(&client.stop_climate(vin, pin).await?),
        CommandName::CloseWindows => sections::split_value(&client.close_windows(vin, pin).await?),
        CommandName::OpenTrunk => sections::split_value(&client.open_trunk(vin, pin).await?),
        CommandName::CloseTrunk => sections::split_value(&client.close_trunk(vin, pin).await?),
        CommandName::StartCharging => sections::split_value(
            &client
                .start_charging(vin, ChargePollSettings::default())
                .await?,
        ),
        CommandName::StopCharging => unreachable!("stop-charging is refused before connecting"),
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_iso_8601() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_time(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso_time(1_783_773_296), "2026-07-11T12:34:56Z");
        assert_eq!(iso_time(4_102_444_799), "2099-12-31T23:59:59Z");
        assert_eq!(iso_now().len(), 20);
    }
}
