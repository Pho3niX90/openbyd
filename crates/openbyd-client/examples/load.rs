//! One process holding many accounts: the openbyd side of the load benchmark (`bench/load.py`).
//!
//! ```text
//! cargo run --release -p openbyd-client --example load -- \
//!     --url http://127.0.0.1:PORT --accounts 200 --cycles 5 --workers 0
//! ```
//!
//! Every account is a [`BydClient`] on one shared [`HttpPool`], driven by a
//! task on a multi-threaded tokio runtime. An account logs in, lists its
//! vehicles, then runs `--cycles` poll cycles: realtime, GPS, charging
//! status, HVAC status and energy consumption, with `--think-ms` between
//! cycles. MQTT is off.
//!
//! The benchmark driver measures this process from outside. It speaks a
//! small protocol on standard input and output: the process prints
//! `READY` once the runtime is up, waits for a line `go`, runs, prints
//! `RESULT <json>` and waits for `quit`, so the driver can read the CPU
//! time and the peak memory after the work and before the exit.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use openbyd_client::client::{BydClient, PollSettings};
use openbyd_client::http::HttpPool;
use openbyd_protocol::config::BydConfig;
use serde_json::json;

const VIN: &str = "TESTVIN0000000001";
const PASSWORD: &str = "secret";

struct Options {
    url: String,
    accounts: usize,
    cycles: usize,
    think: Duration,
    poll_interval: f64,
    /// Tokio worker threads; 0 is tokio's default (one per core).
    workers: usize,
    ramp: Duration,
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        url: String::new(),
        accounts: 50,
        cycles: 5,
        think: Duration::from_millis(100),
        poll_interval: 0.05,
        workers: 0,
        ramp: Duration::ZERO,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        let number = |text: String| text.parse::<u64>().map_err(|e| format!("{e}"));
        match flag.as_str() {
            "--url" => options.url = value("--url")?,
            "--accounts" => options.accounts = number(value("--accounts")?)? as usize,
            "--cycles" => options.cycles = number(value("--cycles")?)? as usize,
            "--think-ms" => options.think = Duration::from_millis(number(value("--think-ms")?)?),
            "--poll-interval" => {
                options.poll_interval = value("--poll-interval")?
                    .parse()
                    .map_err(|e| format!("{e}"))?;
            }
            "--workers" => options.workers = number(value("--workers")?)? as usize,
            "--ramp-ms" => options.ramp = Duration::from_millis(number(value("--ramp-ms")?)?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if options.url.is_empty() {
        return Err("--url is required".to_owned());
    }
    Ok(options)
}

#[derive(Default)]
struct Tally {
    ok: AtomicU64,
    errors: AtomicU64,
    first_errors: std::sync::Mutex<BTreeMap<String, u64>>,
}

impl Tally {
    fn record<T>(&self, result: Result<T, openbyd_protocol::error::Error>) {
        match result {
            Ok(_) => {
                self.ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                let mut seen = self
                    .first_errors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if seen.len() < 5 || seen.contains_key(&error.to_string()) {
                    *seen.entry(error.to_string()).or_default() += 1;
                }
            }
        }
    }
}

async fn account(index: usize, options: Arc<Options>, pool: HttpPool, tally: Arc<Tally>) {
    if !options.ramp.is_zero() && options.accounts > 0 {
        let step = options.ramp / u32::try_from(options.accounts).unwrap_or(u32::MAX);
        tokio::time::sleep(step * u32::try_from(index).unwrap_or(u32::MAX)).await;
    }
    let mut config = BydConfig::new(&format!("user{index}@example.com"), PASSWORD);
    config.base_url = options.url.clone();
    config.mqtt_enabled = false;
    let client = BydClient::connect(config, &pool);
    let settings = PollSettings {
        poll_attempts: 10,
        poll_interval: options.poll_interval,
        mqtt_timeout: Some(0.0),
        signal_retries: 2,
    };
    tally.record(client.login().await);
    tally.record(client.get_vehicles().await);
    for cycle in 0..options.cycles {
        if cycle > 0 {
            tokio::time::sleep(options.think).await;
        }
        tally.record(client.get_vehicle_realtime(VIN, settings).await);
        tally.record(client.get_gps_info(VIN, settings).await);
        tally.record(client.get_charging_status(VIN).await);
        tally.record(client.get_hvac_status(VIN).await);
        tally.record(client.get_energy_consumption(VIN).await);
    }
    client.async_close();
}

fn line() -> String {
    let mut text = String::new();
    std::io::stdin().lock().read_line(&mut text).expect("stdin");
    text.trim().to_owned()
}

fn main() {
    let options = parse_args().unwrap_or_else(|message| {
        eprintln!("{message}");
        std::process::exit(2);
    });
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if options.workers > 0 {
        builder.worker_threads(options.workers);
    }
    let runtime = builder.build().expect("runtime");
    let workers = if options.workers > 0 {
        options.workers
    } else {
        std::thread::available_parallelism().map_or(1, usize::from)
    };
    // The pool must be made inside the runtime that will use it.
    let pool = runtime.block_on(async { HttpPool::new() });
    println!("READY");
    std::io::stdout().flush().expect("stdout");
    if line() != "go" {
        return;
    }

    let options = Arc::new(options);
    let tally = Arc::new(Tally::default());
    let started = Instant::now();
    runtime.block_on(async {
        let tasks: Vec<_> = (0..options.accounts)
            .map(|index| {
                tokio::spawn(account(
                    index,
                    Arc::clone(&options),
                    pool.clone(),
                    Arc::clone(&tally),
                ))
            })
            .collect();
        for task in tasks {
            let _ = task.await;
        }
    });
    let result = json!({
        "implementation": "openbyd",
        "workers": workers,
        "accounts": options.accounts,
        "cycles": options.cycles,
        "calls_ok": tally.ok.load(Ordering::Relaxed),
        "calls_failed": tally.errors.load(Ordering::Relaxed),
        "errors": *tally.first_errors.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        "elapsed_s": started.elapsed().as_secs_f64(),
    });
    println!("RESULT {result}");
    std::io::stdout().flush().expect("stdout");
    let _ = line();
}
