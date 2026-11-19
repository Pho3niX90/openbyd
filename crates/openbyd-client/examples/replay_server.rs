//! The replay server as a process of its own: the fake BYD server and the
//! TLS test broker answering a scenario file (`bench/replay/SCENARIO.md`), for
//! running a real backend worker against realistic traffic.
//!
//! ```text
//! replay_server --scenario <path.jsonl> [--corpus <dir>] [--speed <f64, default 1>]
//!     [--start-at <seconds, default 0>] [--epoch scenario|now (default scenario)]
//!     [--delay-ms <u64, default 0>] [--port <u16, default 0>] [--mqtt-port <u16, default 0>]
//!     [--ca-out <file>] [--autostart] [--workers <n, default 2>]
//! ```
//!
//! It prints, each on its own line: `URL http://127.0.0.1:<port>`,
//! `MQTT 127.0.0.1:<port>`, `CA <path>` (only with `--ca-out`: the broker's
//! certificate as PEM, written before `READY`, for a client in another
//! process to trust), then `READY`. Commands on standard input, one per
//! line, each answered with one line:
//!
//! * `start`: starts the clock (unless `--autostart` did at `READY`) and
//!   prints `STARTED {"wall_unix_ms", "start_at", "speed", "epoch_unix_ms"}`;
//!   a second `start` prints the same line;
//! * `clock`: `{"t", "started", "speed", "wall_unix_ms", "finished"}`;
//! * `stats`: `{"requests", "by_endpoint", "by_code", "connections",
//!   "logins", "mqtt": {...}, "replay": {...}}`;
//! * `reset`: zeroes the counters (not the clock) and prints `OK`;
//! * `quit`: prints `OK`, then exits; the end of input exits without a word;
//! * anything else: `ERR <reason>`.
//!
//! A bad argument or scenario prints the reason on standard error and exits
//! with status 2.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use openbyd_client::testing::broker::certificate_pem;
use openbyd_client::testing::replay::{
    Epoch, ReplayOptions, ReplayServer, Scenario, default_corpus,
};

struct Options {
    scenario: PathBuf,
    replay: ReplayOptions,
    ca_out: Option<PathBuf>,
    autostart: bool,
    workers: usize,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut scenario = None;
    let mut replay = ReplayOptions {
        corpus: Some(default_corpus()),
        ..ReplayOptions::default()
    };
    let (mut ca_out, mut autostart, mut workers) = (None, false, 2);
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        fn number<T: std::str::FromStr>(flag: &str, text: String) -> Result<T, String> {
            text.parse()
                .map_err(|_| format!("{flag}: {text:?} is not a valid value"))
        }
        match flag.as_str() {
            "--scenario" => scenario = Some(PathBuf::from(value()?)),
            "--corpus" => replay.corpus = Some(PathBuf::from(value()?)),
            "--speed" => {
                let speed: f64 = number(&flag, value()?)?;
                if !(speed.is_finite() && speed > 0.0) {
                    return Err("--speed must be a positive number".into());
                }
                replay.speed = speed;
            }
            "--start-at" => {
                let start_at: f64 = number(&flag, value()?)?;
                if !(start_at.is_finite() && start_at >= 0.0) {
                    return Err("--start-at must be a non-negative number".into());
                }
                replay.start_at = start_at;
            }
            "--epoch" => {
                let text = value()?;
                replay.epoch = Epoch::parse(&text)
                    .ok_or_else(|| format!("--epoch must be scenario or now, not {text:?}"))?;
            }
            "--delay-ms" => replay.delay = Duration::from_millis(number(&flag, value()?)?),
            "--port" => replay.port = number(&flag, value()?)?,
            "--mqtt-port" => replay.mqtt_port = number(&flag, value()?)?,
            "--ca-out" => ca_out = Some(PathBuf::from(value()?)),
            "--autostart" => autostart = true,
            "--workers" => {
                workers = number(&flag, value()?)?;
                if workers == 0 {
                    return Err("--workers must be at least 1".into());
                }
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let scenario = scenario.ok_or("--scenario is required")?;
    Ok(Options {
        scenario,
        replay,
        ca_out,
        autostart,
        workers,
    })
}

fn fail(message: &str) -> ! {
    eprintln!("replay_server: {message}");
    std::process::exit(2);
}

fn say(out: &mut impl Write, line: &str) {
    writeln!(out, "{line}").expect("stdout");
    out.flush().expect("stdout");
}

fn main() {
    let options = parse_args(std::env::args().skip(1)).unwrap_or_else(|message| fail(&message));
    let scenario =
        Scenario::load(&options.scenario).unwrap_or_else(|error| fail(&error.to_string()));
    if options.replay.start_at > f64::from(scenario.header.duration_s) {
        fail("--start-at is after the end of the scenario");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(options.workers)
        .enable_all()
        .build()
        .expect("runtime");
    let replay = runtime
        .block_on(ReplayServer::start(scenario, options.replay))
        .unwrap_or_else(|error| fail(&format!("cannot start: {error}")));

    let mut out = std::io::stdout().lock();
    say(&mut out, &format!("URL {}", replay.url()));
    say(&mut out, &format!("MQTT {}", replay.broker_address()));
    if let Some(path) = &options.ca_out {
        std::fs::write(path, certificate_pem())
            .unwrap_or_else(|error| fail(&format!("cannot write {}: {error}", path.display())));
        say(&mut out, &format!("CA {}", path.display()));
    }
    if options.autostart {
        replay.start_clock();
    }
    say(&mut out, "READY");

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let answer = match line.trim() {
            "start" => format!("STARTED {}", replay.start_clock().to_json()),
            "clock" => replay.clock_json().to_string(),
            "stats" => replay.stats().to_json().to_string(),
            "reset" => {
                replay.reset_stats();
                "OK".to_owned()
            }
            "quit" => {
                say(&mut out, "OK");
                break;
            }
            "" => "ERR empty command".to_owned(),
            other => format!("ERR unknown command {other:?}"),
        };
        say(&mut out, &answer);
    }
    drop(out);
    runtime.block_on(replay.shutdown());
}
