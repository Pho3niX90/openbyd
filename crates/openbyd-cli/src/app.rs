//! Running a command: configuration, connecting, printing, exit codes.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;

use openbyd_client::client::{
    BydClient, ClientOptions, MqttEventCallback, ProductionEnv, ReloginPolicy,
};
use openbyd_client::http::{HttpEnv, HttpPool};
use openbyd_client::mqtt::MqttRuntimeOptions;
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::error::{Error, ErrorKind};
use serde_json::{Map, Value, json};

use crate::cli::{Cli, Command, Global};
use crate::output::{Format, render_json, render_table};
use crate::scrub::{self, global as scrubber};
use crate::session_file::{self, LoadError, SavedSession};
use crate::{commands, sections};

/// The client type the tool uses.
pub type Client = BydClient<ProductionEnv>;

/// Where a connected client is parked so that it can be closed after an interrupt.
pub type Holder = Mutex<Option<Client>>;

/// Why a command did not succeed.
#[derive(Debug)]
pub enum Failure {
    /// The command line or the environment is wrong (exit 2).
    Usage(String),
    /// The API or the network failed (exit 1), or authentication did (exit 3).
    Api(Error),
    /// Ctrl-C (exit 130).
    Interrupted,
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Failure::Api(error)
    }
}

impl Failure {
    /// The process exit code.
    pub fn exit_code(&self) -> u8 {
        match self {
            Failure::Usage(_) => 2,
            Failure::Api(error) if error.is_a(ErrorKind::BydAuthenticationError) => 3,
            Failure::Api(_) => 1,
            Failure::Interrupted => 130,
        }
    }
}

/// Prints results on stdout in the chosen format, through the scrubber.
pub struct Printer {
    format: Format,
    pretty: bool,
}

impl Printer {
    /// A printer for `format`; JSON is pretty when stdout is a terminal.
    pub fn new(format: Format) -> Self {
        Self {
            format,
            pretty: std::io::stdout().is_terminal(),
        }
    }

    /// Prints one result.
    pub fn emit(&self, value: &Value) {
        let clean = scrubber().clean_value(value);
        let text = match self.format {
            Format::Json => render_json(&clean, self.pretty),
            Format::Table => render_table(&clean),
        };
        self.line(&text);
    }

    /// Prints one line of JSON, whatever the format (for `watch`).
    pub fn emit_json_line(&self, value: &Value) {
        let clean = scrubber().clean_value(value);
        self.line(&render_json(&clean, false));
    }

    fn line(&self, text: &str) {
        let text = scrubber().scrub_text(text);
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }

    /// Prints a failure: the JSON error object on stdout (in JSON format) and
    /// one line on stderr.
    pub fn failure(&self, failure: &Failure) {
        match failure {
            Failure::Usage(message) => {
                eprintln!("openbyd: error: {}", scrubber().scrub_text(message));
            }
            Failure::Api(error) => {
                if self.format == Format::Json {
                    self.emit(&sections::error_value(error));
                }
                let message = format!("error: {error}");
                eprintln!("openbyd: {}", scrubber().scrub_text(&message));
            }
            Failure::Interrupted => eprintln!("openbyd: interrupted"),
        }
    }
}

/// How a command wants its client set up.
#[derive(Default)]
pub struct Setup {
    /// Use MQTT whatever the flags say.
    pub force_mqtt: bool,
    /// Gets every MQTT event that names a VIN.
    pub on_event: Option<MqttEventCallback>,
    /// How many times a command may be sent.
    pub command_retries: Option<i64>,
}

/// A diagnostic line on stderr, scrubbed.
pub fn note(message: &str) {
    eprintln!("openbyd: {}", scrubber().scrub_text(message));
}

/// Reads a secret from the terminal without echoing it.
fn prompt_secret(prompt: &str) -> Result<String, std::io::Error> {
    rpassword::prompt_password(prompt)
}

/// Whether there is a person to ask.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// The secret in environment variable `name`, or one typed at a prompt.
pub fn secret_from_env_or_prompt(name: &str, prompt: &str) -> Result<String, Failure> {
    if let Some(value) = std::env::var(name).ok().filter(|value| !value.is_empty()) {
        return Ok(value);
    }
    if !interactive() {
        return Err(Failure::Usage(format!(
            "no {prompt} available: set {name} (it is never read from a command-line flag)"
        )));
    }
    match prompt_secret(&format!("{prompt}: ")) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(Failure::Usage(format!("no {prompt} entered"))),
    }
}

fn session_error(error: LoadError) -> Failure {
    match error {
        LoadError::Unusable(message) => {
            Failure::Usage(format!("cannot use session file {message}"))
        }
        LoadError::Expired => Failure::Api(Error::api(
            ErrorKind::BydSessionExpiredError,
            "the saved session has expired; log in again with `openbyd login --save-session FILE`",
            "session_file_expired",
            "",
        )),
    }
}

/// The MQTT settings: the platform's roots, except in the test build.
fn mqtt_options() -> MqttRuntimeOptions {
    #[allow(unused_mut)]
    let mut options = MqttRuntimeOptions::default();
    #[cfg(feature = "test-support")]
    if std::env::var_os("OPENBYD_TEST_MQTT_TRUST_ANY").is_some() {
        options.tls = Some(crate::test_support::trust_any_config());
        options.reconnect_min_delay = std::time::Duration::from_millis(20);
        options.reconnect_max_delay = std::time::Duration::from_millis(200);
    }
    options
}

/// Builds the configuration from the environment and the flags.
fn build_config(global: &Global, saved: Option<&SavedSession>) -> Result<BydConfig, Failure> {
    let mut overrides = Map::new();
    let flags = [
        ("base_url", &global.base_url),
        ("country_code", &global.country_code),
        ("language", &global.language),
        ("time_zone", &global.time_zone),
    ];
    for (key, value) in flags {
        if let Some(value) = value {
            overrides.insert(key.to_owned(), json!(value));
        }
    }
    let env = |name: &str| std::env::var(name).ok();
    let username = global
        .username
        .clone()
        .or_else(|| env("BYD_USERNAME").filter(|name| !name.is_empty()))
        .or_else(|| saved.map(|saved| saved.username.clone()))
        .ok_or_else(|| {
            Failure::Usage("no username: set BYD_USERNAME or pass --username".to_owned())
        })?;
    scrubber().add_identity(&username, true);
    let password = match saved {
        // A restored session never sends the password.
        Some(_) => String::new(),
        None => secret_from_env_or_prompt("BYD_PASSWORD", "password")?,
    };
    scrubber().add_secret(&password);
    overrides.insert("username".to_owned(), json!(username));
    overrides.insert("password".to_owned(), json!(password));
    BydConfig::from_env(env, &overrides)
        .map_err(|error| Failure::Usage(format!("invalid configuration: {}", error.message())))
}

/// Builds the client and logs in, or restores the saved session.
pub async fn connect(global: &Global, setup: Setup, holder: &Holder) -> Result<Client, Failure> {
    let saved = match &global.session {
        Some(path) => {
            Some(session_file::load(path, session_file::now_seconds()).map_err(session_error)?)
        }
        None => None,
    };
    let mut config = build_config(global, saved.as_ref())?;
    config.mqtt_enabled = global.mqtt || setup.force_mqtt;
    if let Some(pin) = &config.control_pin {
        scrubber().add_secret(pin);
    }
    let mut options = ClientOptions::default();
    if saved.is_some() {
        options.relogin = ReloginPolicy::Never;
    }
    options.on_mqtt_event = setup.on_event;
    if let Some(retries) = setup.command_retries {
        options.command_retries = retries;
    }
    let pool = HttpPool::new();
    let env = ProductionEnv::with_mqtt_options(HttpEnv::for_config(&pool, &config), mqtt_options());
    let client = BydClient::new(config, env, options);
    *holder
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(client.clone());
    match saved {
        Some(saved) => {
            client.import_session(saved.tokens).await;
        }
        None => client.login().await?,
    }
    hide_tokens(&client);
    Ok(client)
}

/// Registers the session tokens as secrets.
pub fn hide_tokens(client: &Client) {
    if let Some(tokens) = client.export_session() {
        scrubber().add_secret(&tokens.sign_token);
        scrubber().add_secret(&tokens.encry_token);
        scrubber().add_secret(&tokens.refresh_token);
    }
}

/// Runs the command line to completion.
pub async fn run(cli: &Cli, printer: &Printer, holder: &Holder) -> Result<(), Failure> {
    match &cli.command {
        Command::Version => commands::version(printer),
        Command::Login(args) => commands::login(&cli.global, args, printer, holder).await,
        Command::Vehicles => commands::vehicles(&cli.global, printer, holder).await,
        Command::Dump(args) => commands::dump(&cli.global, args, printer, holder).await,
        Command::Get(args) => commands::get(&cli.global, args, printer, holder).await,
        Command::Watch(args) => commands::watch(&cli.global, args, printer, holder).await,
        Command::Send(args) => commands::send(&cli.global, args, printer, holder).await,
    }
}

/// Starts the tool: parses nothing, only wires the scrubber for `global`.
pub fn prepare(global: &Global) {
    scrubber().set_redact(global.redact);
    crate::logging::init(global.verbose, scrub::global());
}

/// Closes the client left in `holder`, if any.
pub fn close(holder: &Holder) {
    let client = holder
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(client) = client {
        client.async_close();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use openbyd_client::client::SessionTokens;

    use super::*;

    #[tokio::test]
    async fn every_session_token_is_hidden() {
        let config = BydConfig::new("user@example.com", "secret");
        let pool = HttpPool::new();
        let env =
            ProductionEnv::with_mqtt_options(HttpEnv::for_config(&pool, &config), mqtt_options());
        let client = BydClient::new(config, env, ClientOptions::default());
        client
            .import_session(SessionTokens {
                user_id: "1".into(),
                sign_token: "SIGN-hidden-token".into(),
                encry_token: "ENCRY-hidden-token".into(),
                refresh_token: "REFRESH-hidden-token".into(),
                ttl_remaining: None,
                guest_vins: HashMap::new(),
            })
            .await;
        hide_tokens(&client);
        let shown =
            scrubber().scrub_text("SIGN-hidden-token ENCRY-hidden-token REFRESH-hidden-token");
        assert!(!shown.contains("hidden-token"), "{shown}");
    }
}
