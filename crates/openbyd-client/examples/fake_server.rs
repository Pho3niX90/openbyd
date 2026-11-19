//! The fake BYD server as a process of its own, for the load benchmark (`bench/load.py`).
//!
//! ```text
//! cargo run --release -p openbyd-client --features testing --example fake_server -- \
//!     --accounts 200 --delay-ms 20
//! ```
//!
//! It accepts logins for `user<i>@example.com` / `secret`, `i` from 0, and
//! answers the endpoints of a poll cycle with payloads from `parity/corpus/`,
//! each after `--delay-ms` (an artificial network and server latency). The
//! first line it prints is `URL http://127.0.0.1:<port>`, the second `READY`.
//! It then reads commands from standard input, one per line:
//!
//! * `stats`: prints one JSON line with the number of requests received, by
//!   endpoint and by API code, and the TCP connections accepted;
//! * `reset`: forgets the recorded requests and prints `OK`;
//! * `quit` (or the end of input): exits.
//!
//! Running in its own process keeps the server's CPU and memory out of what
//! the benchmark measures in the client.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use openbyd_client::testing::{Account, FakeBydServer, Reply};
use serde_json::{Value, json};

const PASSWORD: &str = "secret";

struct Options {
    accounts: usize,
    delay: Duration,
    corpus: PathBuf,
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        accounts: 50,
        delay: Duration::from_millis(20),
        corpus: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parity/corpus"),
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match flag.as_str() {
            "--accounts" => {
                options.accounts = value("--accounts")?.parse().map_err(|e| format!("{e}"))?;
            }
            "--delay-ms" => {
                let ms: u64 = value("--delay-ms")?.parse().map_err(|e| format!("{e}"))?;
                options.delay = Duration::from_millis(ms);
            }
            "--corpus" => options.corpus = PathBuf::from(value("--corpus")?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(options)
}

/// The `payload` of the corpus entry of `file` whose note contains `note`.
fn corpus(dir: &Path, file: &str, note: &str) -> Value {
    let path = dir.join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let entries: Vec<Value> = serde_json::from_str(&text).expect("corpus is JSON");
    entries
        .into_iter()
        .find(|entry| entry["note"].as_str().is_some_and(|n| n.contains(note)))
        .unwrap_or_else(|| panic!("no entry with note {note:?} in {file}"))["payload"]
        .clone()
}

fn main() {
    let options = parse_args().unwrap_or_else(|message| {
        eprintln!("{message}");
        std::process::exit(2);
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let accounts =
        (0..options.accounts).map(|i| Account::new(&format!("user{i}@example.com"), PASSWORD));
    let server = runtime
        .block_on(FakeBydServer::start(accounts))
        .expect("server");

    let dir = &options.corpus;
    let delay = options.delay;
    let data = |endpoint: &str, payload: Value| {
        server.reply(endpoint, Reply::Data(payload).delayed(delay));
    };
    server.on("/app/account/login", move |_| Reply::Default.delayed(delay));
    data(
        "/app/account/getAllListByUserId",
        json!([corpus(dir, "Vehicle.json", "vehicleFunLearnInfo")]),
    );
    // A realtime or GPS read is a trigger that only names the request, then one poll with the data.
    data(
        "/vehicleInfo/vehicle/vehicleRealTimeRequest",
        json!({"requestSerial": "SERIAL-RT"}),
    );
    data(
        "/vehicleInfo/vehicle/vehicleRealTimeResult",
        corpus(dir, "VehicleRealtimeData.json", "every field present"),
    );
    data(
        "/control/getGpsInfo",
        json!({"requestSerial": "SERIAL-GPS"}),
    );
    data(
        "/control/getGpsInfoResult",
        corpus(dir, "GpsInfo.json", "valid fix"),
    );
    data(
        "/control/smartCharge/homePage",
        corpus(dir, "ChargingStatus.json", "charge power"),
    );
    data(
        "/control/getStatusNow",
        corpus(dir, "HvacStatus.json", "running climate"),
    );
    data(
        "/vehicleInfo/vehicle/getEnergyConsumption",
        corpus(dir, "EnergyConsumption.json", "energy_type 0"),
    );

    let mut out = std::io::stdout().lock();
    writeln!(out, "URL {}", server.url()).expect("stdout");
    writeln!(out, "READY").expect("stdout");
    out.flush().expect("stdout");

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        match line.trim() {
            "stats" => {
                let requests = server.requests();
                let mut by_endpoint = BTreeMap::<String, u64>::new();
                let mut by_code = BTreeMap::<String, u64>::new();
                for request in &requests {
                    *by_endpoint.entry(request.endpoint.clone()).or_default() += 1;
                    let code = request.code.clone().unwrap_or_else(|| "none".to_owned());
                    *by_code.entry(code).or_default() += 1;
                }
                let stats = json!({
                    "requests": requests.len(),
                    "by_endpoint": by_endpoint,
                    "by_code": by_code,
                    "connections": server.connections(),
                });
                writeln!(out, "{stats}").expect("stdout");
            }
            "reset" => {
                server.clear_requests();
                writeln!(out, "OK").expect("stdout");
            }
            "quit" => break,
            _ => writeln!(out, "ERR unknown command").expect("stdout"),
        }
        out.flush().expect("stdout");
    }
    drop(out);
    runtime.block_on(server.shutdown());
}
