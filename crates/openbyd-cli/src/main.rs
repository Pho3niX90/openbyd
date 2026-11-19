//! `openbyd`: a command-line tool for the BYD overseas app API.
//!
//! Log in, list vehicles, dump what every read-only endpoint returns, watch
//! MQTT events and send remote commands, without writing code. See
//! `openbyd --help`.

mod app;
mod cli;
mod commands;
mod logging;
mod output;
mod scrub;
mod sections;
mod session_file;
#[cfg(feature = "test-support")]
mod test_support;

use std::process::ExitCode;

use clap::Parser;

use app::{Failure, Holder, Printer};
use cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    app::prepare(&cli.global);
    let printer = Printer::new(cli.global.format);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("openbyd: cannot start the async runtime: {error}");
            return ExitCode::from(1);
        }
    };
    let holder = Holder::default();
    let outcome = runtime.block_on(async {
        let outcome = tokio::select! {
            result = app::run(&cli, &printer, &holder) => result,
            _ = tokio::signal::ctrl_c() => Err(Failure::Interrupted),
        };
        // Closing stops MQTT, which needs the runtime.
        app::close(&holder);
        outcome
    });
    // Ending `watch` with Ctrl-C is how it is meant to end.
    let outcome = match outcome {
        Err(Failure::Interrupted) if matches!(cli.command, Command::Watch(_)) => Ok(()),
        other => other,
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            printer.failure(&failure);
            ExitCode::from(failure.exit_code())
        }
    }
}
