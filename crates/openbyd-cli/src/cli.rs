//! The command line: arguments, options and the help text.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::output::Format;

const ABOUT: &str = "Check a BYD account, see what the API returns for a car, or debug a problem";

const LONG_ABOUT: &str = "\
openbyd talks to the BYD overseas app API, the same service the BYD phone app \
uses. It is an unofficial tool, a Rust port of the pyBYD library.

It reads its settings from the same BYD_* environment variables as pyBYD \
(BYD_USERNAME, BYD_PASSWORD, BYD_BASE_URL, BYD_COUNTRY_CODE, BYD_LANGUAGE, \
BYD_TIME_ZONE, BYD_CONTROL_PIN, ...); the flags below override them.";

const AFTER_HELP: &str = "\
PASSWORD AND PIN:
  The password is read from BYD_PASSWORD, or typed at a prompt that does not \
echo. There is deliberately no --password flag: a flag would show in the \
process list and in your shell history. The control PIN comes from --pin, \
BYD_CONTROL_PIN or a prompt; the environment or the prompt is safer than --pin.

OUTPUT:
  JSON on stdout (pretty on a terminal, compact when piped), or a table with \
--format table. Diagnostics and logs go to stderr; -v and -vv add detail. \
Tokens, passwords and PINs are never printed. Use --redact to mask VINs, \
coordinates, e-mail addresses and plates before pasting output into a bug report.

EXIT CODES:
  0    success
  1    an API or transport error (a JSON error object is printed on stdout)
  2    a usage error (bad or missing arguments, no password or PIN available)
  3    authentication failed (wrong password, or the saved session has expired)
  130  interrupted with Ctrl-C (`watch` stops cleanly with 0)";

/// The whole command line.
#[derive(Debug, Parser)]
#[command(
    name = "openbyd",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    after_help = AFTER_HELP,
    arg_required_else_help = true,
)]
pub struct Cli {
    /// Options every command accepts.
    #[command(flatten)]
    pub global: Global,
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Options every command accepts.
#[derive(Debug, Args)]
pub struct Global {
    /// BYD account e-mail or phone number [env: BYD_USERNAME].
    #[arg(long, global = true, value_name = "NAME")]
    pub username: Option<String>,
    /// API base URL [env: BYD_BASE_URL].
    #[arg(long, global = true, value_name = "URL")]
    pub base_url: Option<String>,
    /// ISO country code, for example NL [env: BYD_COUNTRY_CODE].
    #[arg(long, global = true, value_name = "CODE")]
    pub country_code: Option<String>,
    /// Language code, for example en [env: BYD_LANGUAGE].
    #[arg(long, global = true, value_name = "CODE")]
    pub language: Option<String>,
    /// IANA time zone, for example Europe/Amsterdam [env: BYD_TIME_ZONE].
    #[arg(long, global = true, value_name = "ZONE")]
    pub time_zone: Option<String>,
    /// Reuse the session saved by `login --save-session` instead of logging in.
    ///
    /// The password is then not needed and never sent. When the session has
    /// expired the command fails with exit code 3.
    #[arg(long, global = true, value_name = "FILE")]
    pub session: Option<PathBuf>,
    /// Also listen for MQTT pushes while reading (faster answers, needs a broker connection).
    ///
    /// `watch` always uses MQTT; other commands poll over HTTP unless this is given.
    #[arg(long, global = true)]
    pub mqtt: bool,
    /// Output format.
    #[arg(long, global = true, value_enum, default_value = "json")]
    pub format: Format,
    /// Mask VINs, coordinates, e-mail addresses and plates in the output.
    #[arg(long, global = true)]
    pub redact: bool,
    /// More log detail on stderr (-v info, -vv debug).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Log in and report success (user id, number of vehicles).
    ///
    /// With --save-session the session tokens are written to a file (readable by
    /// you only, where the platform allows) so later commands can reuse them with
    /// --session instead of logging in again.
    ///
    /// WARNING: on some BYD clusters a new login ends an existing session of the
    /// same account, including the one in the BYD phone app. Log in as seldom as
    /// you can; save the session and reuse it.
    Login(LoginArgs),
    /// List the account's vehicles (VIN, model name, alias, energy type).
    Vehicles,
    /// Call every read-only endpoint for each vehicle and print one JSON document.
    ///
    /// The document has a key per section holding the parsed model, or an
    /// {"error": {"class", "code", "message"}} object for a section that failed,
    /// so one unsupported endpoint does not stop the dump.
    Dump(DumpArgs),
    /// Fetch one section for one vehicle.
    Get(GetArgs),
    /// Print each MQTT event as a line of JSON until interrupted or the time is up.
    ///
    /// Events are always printed as JSON lines, whatever --format says.
    Watch(WatchArgs),
    /// Send a remote command to a car. THIS MOVES A REAL CAR.
    ///
    /// It refuses unless --yes-send-to-car is given, prints what it is about to
    /// send, and needs the control PIN (--pin, BYD_CONTROL_PIN, or a prompt that
    /// does not echo). It makes a single attempt unless --retries N says
    /// otherwise: BYD can act on a command even when it reports a failure, so an
    /// automatic second send could lock, unlock or open twice.
    ///
    /// stop-charging is listed for completeness: the BYD cloud accepts it but the
    /// car keeps charging, so it always fails without sending anything.
    #[command(name = "command")]
    Send(CommandArgs),
    /// Print the tool's version and the pyBYD commit it matches.
    Version,
}

/// Arguments of `login`.
#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Write the session tokens to this file (owner-readable only where possible).
    #[arg(long, value_name = "FILE")]
    pub save_session: Option<PathBuf>,
}

/// A read-only section of a vehicle's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Section {
    /// Realtime vehicle data.
    Realtime,
    /// GPS position.
    Gps,
    /// Charging status.
    Charging,
    /// Climate status.
    Hvac,
    /// Energy consumption.
    Energy,
    /// The smart charging schedule.
    ChargingSchedule,
    /// The latest config, with what the car supports.
    LatestConfig,
    /// The capabilities derived from the latest config.
    Capabilities,
    /// Push notification state.
    PushState,
    /// Scheduled-climate bookings.
    Bookings,
    /// The account's unread inbox messages.
    Notifications,
    /// Supported and active AC charging current limits.
    AcCurrentLimit,
}

impl Section {
    /// Every section, in the order `dump` fetches them.
    pub const ALL: [Section; 12] = [
        Section::Realtime,
        Section::Gps,
        Section::Charging,
        Section::Hvac,
        Section::Energy,
        Section::ChargingSchedule,
        Section::LatestConfig,
        Section::Capabilities,
        Section::PushState,
        Section::Bookings,
        Section::Notifications,
        Section::AcCurrentLimit,
    ];

    /// The name used on the command line and as the key in `dump`.
    pub fn name(self) -> String {
        self.to_possible_value()
            .expect("every section has a name")
            .get_name()
            .to_owned()
    }
}

/// Arguments of `dump`.
#[derive(Debug, Args)]
pub struct DumpArgs {
    /// Only this vehicle (default: every vehicle on the account).
    #[arg(long, value_name = "VIN")]
    pub vin: Option<String>,
    /// Leave these sections out (repeat the flag or separate with commas).
    #[arg(long, value_enum, value_delimiter = ',', value_name = "SECTION")]
    pub skip: Vec<Section>,
    /// Put the raw API payload next to each parsed model.
    ///
    /// A section then holds {"parsed": ..., "raw": ...}.
    #[arg(long)]
    pub raw: bool,
}

/// Arguments of `get`.
#[derive(Debug, Args)]
pub struct GetArgs {
    /// The section to fetch.
    #[arg(value_enum)]
    pub section: Section,
    /// The vehicle.
    #[arg(long, value_name = "VIN")]
    pub vin: String,
    /// Put the raw API payload next to the parsed model.
    #[arg(long)]
    pub raw: bool,
}

/// Arguments of `watch`.
#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Only events for this vehicle.
    #[arg(long, value_name = "VIN")]
    pub vin: Option<String>,
    /// Stop after this many seconds (default: until Ctrl-C).
    #[arg(long, value_name = "N")]
    pub seconds: Option<f64>,
}

/// A remote command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CommandName {
    /// Lock the doors.
    Lock,
    /// Unlock the doors.
    Unlock,
    /// Flash the lights.
    FlashLights,
    /// Sound the horn and flash the lights.
    FindCar,
    /// Start climate control (--temperature, --minutes).
    StartClimate,
    /// Stop climate control.
    StopClimate,
    /// Close all windows.
    CloseWindows,
    /// Release the trunk.
    OpenTrunk,
    /// Lower the trunk.
    CloseTrunk,
    /// Start charging now.
    StartCharging,
    /// Not supported by the BYD cloud; always fails without sending.
    StopCharging,
}

impl CommandName {
    /// The name used on the command line.
    pub fn name(self) -> String {
        self.to_possible_value()
            .expect("every command has a name")
            .get_name()
            .to_owned()
    }
}

/// Arguments of `command`.
#[derive(Debug, Args)]
pub struct CommandArgs {
    /// The command to send.
    #[arg(value_enum)]
    pub name: CommandName,
    /// The vehicle.
    #[arg(long, value_name = "VIN")]
    pub vin: String,
    /// Really send the command to the car. Without it nothing is sent.
    #[arg(long)]
    pub yes_send_to_car: bool,
    /// The control PIN [env: BYD_CONTROL_PIN]; visible in the process list, so prefer the environment or the prompt.
    #[arg(long, value_name = "PIN")]
    pub pin: Option<String>,
    /// How many times a refused or unconfirmed command may be sent in all.
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=10))]
    pub retries: u32,
    /// start-climate: target temperature in degrees C (15 to 31).
    #[arg(long, value_name = "C")]
    pub temperature: Option<f64>,
    /// start-climate: how long it runs: 10, 15, 20, 25 or 30 minutes.
    #[arg(long, value_name = "M")]
    pub minutes: Option<i64>,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn names_are_kebab_case() {
        assert_eq!(Section::AcCurrentLimit.name(), "ac-current-limit");
        assert_eq!(CommandName::FlashLights.name(), "flash-lights");
        assert_eq!(Section::ALL.len(), 12);
    }

    #[test]
    fn there_is_no_password_flag() {
        let command = Cli::command();
        assert!(
            command
                .get_arguments()
                .all(|arg| arg.get_id() != "password")
        );
        assert!(Cli::try_parse_from(["openbyd", "login", "--password", "x"]).is_err());
        assert!(Cli::try_parse_from(["openbyd", "--password=x", "vehicles"]).is_err());
    }
}
