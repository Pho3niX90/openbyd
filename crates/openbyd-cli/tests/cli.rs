//! The `openbyd` binary as a child process against the fake BYD server.

#![cfg(feature = "test-support")]

use std::path::PathBuf;
use std::process::{Command, Stdio};

use openbyd_client::testing::broker::TestBroker;
use openbyd_client::testing::{Account, FakeBydServer, Reply};
use openbyd_protocol::hashing::md5_hex;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_openbyd");
const VIN: &str = "TESTVIN0000000001";
const USER: &str = "user@example.com";
const PASSWORD: &str = "correct-horse-battery";
const PIN: &str = "135790";
const LOGIN: &str = "/app/account/login";
const VERIFY_PIN: &str = "/vehicle/vehicleswitch/verifyControlPassword";
const REMOTE_CONTROL: &str = "/control/remoteControl";
const REMOTE_CONTROL_RESULT: &str = "/control/remoteControlResult";
const CHARGE_TOGGLE: &str = "/control/smartCharge/changeChargeStatue";
const BROKER_LOOKUP: &str = "/app/emqAuth/getEmqBrokerIp";

/// What the fake car supports: every command the tests send.
const FUNCTION_NOS: [&str; 11] = [
    "1005", "1006", "1007", "1008", "1026", "1020", "1021", "1012", "1001", "10300001", "1015",
];

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {}", self.stdout))
    }
}

/// A command for the binary with every `BYD_*` variable cleared (the others,
/// such as the coverage tool's, are kept) and the account of the fake server.
fn base_command(url: &str) -> Command {
    let mut command = Command::new(BIN);
    for (name, _) in std::env::vars() {
        if name.starts_with("BYD_") || name.starts_with("OPENBYD_") {
            command.env_remove(name);
        }
    }
    command
        .env("BYD_USERNAME", USER)
        .env("BYD_PASSWORD", PASSWORD)
        .env("BYD_BASE_URL", url)
        .stdin(Stdio::null());
    command
}

async fn run(url: &str, args: &[&str], extra_env: &[(&str, &str)]) -> Out {
    let mut command = base_command(url);
    command.args(args);
    for (name, value) in extra_env {
        command.env(name, value);
    }
    tokio::task::spawn_blocking(move || {
        let output = command.output().expect("the binary runs");
        Out {
            code: output.status.code().expect("exit code"),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    })
    .await
    .expect("joined")
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cli-tests");
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir.join(name)
}

async fn world() -> FakeBydServer {
    let server = FakeBydServer::start([Account::new(USER, PASSWORD)])
        .await
        .expect("server");
    server.reply(
        "/app/account/getAllListByUserId",
        Reply::Data(json!([{
            "vin": VIN, "energyType": "0", "modelName": "SEAL", "autoAlias": "My Seal",
            "autoPlate": "AB12CDE", "ownerEmail": "owner@example.org"
        }])),
    );
    let realtime =
        json!({"requestSerial": "R1", "elecPercent": 80, "onlineState": 1, "time": 1700000000});
    server.reply(
        "/vehicleInfo/vehicle/vehicleRealTimeRequest",
        Reply::Data(realtime.clone()),
    );
    server.reply(
        "/vehicleInfo/vehicle/vehicleRealTimeResult",
        Reply::Data(realtime),
    );
    server.reply(
        "/control/getGpsInfo",
        Reply::Data(json!({
            "latitude": 52.12345, "longitude": 4.56789,
            "ownerEmail": "owner@example.org", "autoPlate": "AB12CDE", "note": "parked at 52.12345"
        })),
    );
    server.reply(
        "/control/getStatusNow",
        Reply::Data(json!({"acSwitch": 1, "tempInCar": 21})),
    );
    server.reply(
        "/control/smartCharge/homePage",
        Reply::Data(json!({"soc": 50})),
    );
    server.reply(
        "/vehicleInfo/vehicle/getEnergyConsumption",
        Reply::Data(json!({"totalEnergy": "15.5"})),
    );
    server.reply(
        "/control/smartCharge/getAcCurrentLimit",
        Reply::Data(json!({"supportList": [16, 32]})),
    );
    server.reply(
        "/vehicle/vehicleswitch/getPushSwitchState",
        Reply::Data(json!({"pushSwitchList": []})),
    );
    server.reply(
        "/control/getBookingList",
        Reply::Data(json!({"bookingList": []})),
    );
    server.reply(
        "/app/messageCenter/getUnReadMessageList",
        Reply::Data(json!([
            {"messageId": "7", "title": "Charging done", "content": "see AB12CDE"}
        ])),
    );
    let functions: Vec<Value> = FUNCTION_NOS
        .iter()
        .map(|no| json!({"functionNo": no}))
        .collect();
    server.reply(
        "/vehicle/vehicleswitch/getLatestConfig",
        Reply::Data(json!({VIN: {"cfFixedList": functions}})),
    );
    server.reply(VERIFY_PIN, Reply::Data(json!({"ok": true})));
    server
}

fn tokens_of(path: &PathBuf) -> (String, String, String) {
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("session file"))
        .expect("session file is JSON");
    let field = |name: &str| saved[name].as_str().expect(name).to_owned();
    (field("user_id"), field("sign_token"), field("encry_token"))
}

async fn saved_session(server: &FakeBydServer, name: &str) -> PathBuf {
    let path = scratch(name);
    let _ = std::fs::remove_file(&path);
    let out = run(
        &server.url(),
        &["login", "--save-session", path.to_str().unwrap()],
        &[],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    server.clear_requests();
    path
}

fn logins(server: &FakeBydServer) -> usize {
    server.requests_to(LOGIN).len()
}

fn assert_no_leaks(out: &Out, leaks: &[&str]) {
    for leak in leaks {
        assert!(
            !out.stdout.contains(leak),
            "stdout leaks {leak}: {}",
            out.stdout
        );
        assert!(
            !out.stderr.contains(leak),
            "stderr leaks {leak}: {}",
            out.stderr
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_names_the_pybyd_commit() {
    let out = run("http://127.0.0.1:1", &["version"], &[]).await;
    assert_eq!(out.code, 0);
    let commit = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../parity/UPSTREAM_COMMIT"
    ))
    .unwrap();
    assert_eq!(out.json()["pybyd_commit"], json!(commit.trim()));
    assert_eq!(out.json()["version"], json!(env!("CARGO_PKG_VERSION")));
    let table = run("http://127.0.0.1:1", &["version", "--format", "table"], &[]).await;
    assert!(table.stdout.contains("pybyd_commit"), "{}", table.stdout);
    let flag = run("http://127.0.0.1:1", &["--version"], &[]).await;
    assert!(flag.stdout.starts_with("openbyd "));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_reports_the_user_and_the_vehicle_count() {
    let server = world().await;
    let out = run(&server.url(), &["login"], &[]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let report = out.json();
    assert_eq!(report["user_id"], json!(server.user_id(USER).unwrap()));
    assert_eq!(report["vehicle_count"], json!(1));
    assert_eq!(report["session_saved"], json!(false));
    assert_eq!(logins(&server), 1);
    // The default output is compact when it is not a terminal: one line.
    assert_eq!(out.stdout.trim_end().lines().count(), 1);
    let table = run(&server.url(), &["login", "--format", "table"], &[]).await;
    assert!(table.stdout.starts_with("FIELD"), "{}", table.stdout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vehicles_lists_vin_model_alias_and_energy_type() {
    let server = world().await;
    let out = run(&server.url(), &["vehicles"], &[]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let list = out.json();
    assert_eq!(list[0]["vin"], json!(VIN));
    assert_eq!(list[0]["model_name"], json!("SEAL"));
    assert_eq!(list[0]["auto_alias"], json!("My Seal"));
    assert_eq!(list[0]["energy_type"], json!("EV"));
    let table = run(&server.url(), &["vehicles", "--format", "table"], &[]).await;
    let lines: Vec<&str> = table.stdout.lines().collect();
    assert!(lines[0].starts_with("VIN"), "{}", table.stdout);
    assert!(lines[1].contains("SEAL") && lines[1].contains("My Seal"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_fetches_each_section() {
    let server = world().await;
    let expected = [
        ("realtime", "elec_percent", json!(80.0)),
        ("gps", "latitude", json!(52.12345)),
        ("charging", "soc", json!(50)),
        ("hvac", "ac_switch", json!(1)),
        ("energy", "timestamp", Value::Null),
        ("charging-schedule", "vin", json!(VIN)),
        ("latest-config", "widget_config_id", json!("")),
        ("capabilities", "lock", json!(true)),
        ("push-state", "vin", json!(VIN)),
        ("bookings", "vin", json!(VIN)),
        ("ac-current-limit", "vin", json!(VIN)),
    ];
    for (section, field, value) in expected {
        let out = run(&server.url(), &["get", section, "--vin", VIN], &[]).await;
        assert_eq!(out.code, 0, "{section}: {}", out.stderr);
        assert_eq!(out.json()[field], value, "{section}");
        assert!(out.json().get("raw").is_none(), "{section}");
    }
    let raw = run(&server.url(), &["get", "gps", "--vin", VIN, "--raw"], &[]).await;
    assert_eq!(raw.json()["parsed"]["longitude"], json!(4.56789));
    assert_eq!(raw.json()["raw"]["longitude"], json!(4.56789));
    let table = run(
        &server.url(),
        &["get", "hvac", "--vin", VIN, "--format", "table"],
        &[],
    )
    .await;
    assert!(table.stdout.contains("ac_switch"), "{}", table.stdout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_get_prints_the_error_and_exits_1() {
    let server = world().await;
    server.reply("/control/getStatusNow", Reply::code("9999"));
    let out = run(&server.url(), &["get", "hvac", "--vin", VIN], &[]).await;
    assert_eq!(out.code, 1);
    assert_eq!(out.json()["error"]["class"], json!("BydApiError"));
    assert_eq!(out.json()["error"]["code"], json!("9999"));
    assert!(out.stderr.contains("BydApiError"), "{}", out.stderr);
    let table = run(
        &server.url(),
        &["get", "hvac", "--vin", VIN, "--format", "table"],
        &[],
    )
    .await;
    assert_eq!((table.code, table.stdout.as_str()), (1, ""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dump_has_a_key_per_section() {
    let server = world().await;
    let out = run(&server.url(), &["dump"], &[]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let vehicle = &out.json()["vehicles"][0];
    assert_eq!(vehicle["vin"], json!(VIN));
    assert_eq!(vehicle["vehicle"]["model_name"], json!("SEAL"));
    for section in [
        "realtime",
        "gps",
        "charging",
        "hvac",
        "energy",
        "charging-schedule",
        "latest-config",
        "capabilities",
        "push-state",
        "bookings",
        "notifications",
        "ac-current-limit",
    ] {
        assert!(vehicle.get(section).is_some(), "{section} is missing");
        assert!(vehicle[section].get("error").is_none(), "{section}");
        assert!(vehicle[section].get("raw").is_none(), "{section}");
    }
    assert_eq!(vehicle["gps"]["latitude"], json!(52.12345));

    let skipped = run(
        &server.url(),
        &[
            "dump", "--vin", VIN, "--skip", "gps,hvac", "--skip", "energy", "--raw",
        ],
        &[],
    )
    .await;
    assert_eq!(skipped.code, 0, "{}", skipped.stderr);
    let vehicle = &skipped.json()["vehicles"][0];
    for gone in ["gps", "hvac", "energy"] {
        assert!(vehicle.get(gone).is_none(), "{gone}");
    }
    assert_eq!(vehicle["realtime"]["raw"]["elecPercent"], json!(80));
    assert_eq!(vehicle["realtime"]["parsed"]["elec_percent"], json!(80.0));

    let table = run(&server.url(), &["dump", "--format", "table"], &[]).await;
    assert!(
        table.stdout.contains("vehicles[0].gps.latitude"),
        "{}",
        table.stdout
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dump_goes_on_when_a_section_fails() {
    let server = world().await;
    server.reply("/control/getStatusNow", Reply::code("9999"));
    let out = run(&server.url(), &["dump"], &[]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let vehicle = &out.json()["vehicles"][0];
    let error = &vehicle["hvac"]["error"];
    assert_eq!(error["class"], json!("BydApiError"));
    assert_eq!(error["code"], json!("9999"));
    assert!(error["message"].as_str().unwrap().contains("9999"));
    assert_eq!(vehicle["gps"]["latitude"], json!(52.12345));
    assert_eq!(
        vehicle["notifications"]["VEHICLE"][0]["title"],
        json!("Charging done")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dump_stops_when_the_session_is_rejected() {
    let server = world().await;
    let session = saved_session(&server, "dump-expired.json").await;
    server.expire_sessions(USER);
    let out = run(
        &server.url(),
        &["dump", "--vin", VIN, "--session", session.to_str().unwrap()],
        &[],
    )
    .await;
    assert_eq!(out.code, 3, "{}", out.stderr);
    assert_eq!(
        out.json()["error"]["class"],
        json!("BydSessionExpiredError")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_and_personal_data_stay_out_of_the_output() {
    let server = world().await;
    let session = saved_session(&server, "leaks.json").await;
    let (_, sign, encry) = tokens_of(&session);
    let file = session.to_str().unwrap();

    // A plain dump shows the car's data but never a secret.
    let plain = run(
        &server.url(),
        &["dump", "--raw", "-vv", "--session", file],
        &[],
    )
    .await;
    assert_eq!(plain.code, 0, "{}", plain.stderr);
    assert!(plain.stdout.contains(VIN) && plain.stdout.contains("4.56789"));
    assert_no_leaks(&plain, &[&sign, &encry, PASSWORD]);

    // A password login with debug logs does not print the password or the tokens.
    let login = run(&server.url(), &["dump", "--raw", "-vv"], &[]).await;
    assert_eq!(login.code, 0, "{}", login.stderr);
    assert_no_leaks(&login, &[PASSWORD, &md5_hex(PASSWORD), &sign, &encry]);

    // --redact hides the VIN, coordinates, e-mail addresses and plates too.
    let leaks = [
        VIN,
        "52.12345",
        "4.56789",
        "owner@example.org",
        "AB12CDE",
        USER,
        PASSWORD,
        &sign,
        &encry,
    ];
    let redacted = run(
        &server.url(),
        &["dump", "--raw", "--redact", "-vv", "--session", file],
        &[],
    )
    .await;
    assert_eq!(redacted.code, 0, "{}", redacted.stderr);
    assert_no_leaks(&redacted, &leaks);
    assert!(redacted.stdout.contains("<vin-1>") && redacted.stdout.contains("<coordinate>"));
    assert!(redacted.stdout.contains("<email>") && redacted.stdout.contains("<plate>"));
    let again = run(&server.url(), &["dump", "--raw", "--redact", "-vv"], &[]).await;
    assert_no_leaks(&again, &leaks);

    // Single sections, errors and lists are redacted as well.
    let got = run(
        &server.url(),
        &[
            "get", "gps", "--vin", VIN, "--redact", "--raw", "--format", "table",
        ],
        &[],
    )
    .await;
    assert_no_leaks(&got, &leaks);
    server.reply("/control/getStatusNow", Reply::code("9999"));
    let failed = run(
        &server.url(),
        &["get", "hvac", "--vin", VIN, "--redact"],
        &[],
    )
    .await;
    assert_eq!(failed.code, 1);
    assert_no_leaks(&failed, &leaks);
    let listed = run(&server.url(), &["vehicles", "--redact"], &[]).await;
    assert_no_leaks(&listed, &leaks);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saved_session_is_reused_without_logging_in() {
    let server = world().await;
    let path = saved_session(&server, "reuse.json").await;
    let file = path.to_str().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("sign_token") && text.contains(USER));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // The session file replaces the password and even the username.
    let out = run(
        &server.url(),
        &["vehicles", "--session", file],
        &[("BYD_PASSWORD", ""), ("BYD_USERNAME", "")],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json()[0]["vin"], json!(VIN));
    let out = run(
        &server.url(),
        &["get", "hvac", "--vin", VIN, "--session", file],
        &[("BYD_PASSWORD", "")],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(logins(&server), 0, "no login request after the first");

    // login itself never takes a session; a missing file is a usage error.
    let both = run(&server.url(), &["login", "--session", file], &[]).await;
    assert_eq!(both.code, 2);
    let missing = scratch("does-not-exist.json");
    let _ = std::fs::remove_file(&missing);
    let out = run(
        &server.url(),
        &["vehicles", "--session", missing.to_str().unwrap()],
        &[],
    )
    .await;
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("session file"), "{}", out.stderr);

    // A session that ran out is an authentication failure, with no login attempt.
    let old = scratch("old.json");
    let mut saved: Value = serde_json::from_str(&text).unwrap();
    saved["saved_at"] = json!(1_000);
    std::fs::write(&old, saved.to_string()).unwrap();
    let out = run(
        &server.url(),
        &["vehicles", "--session", old.to_str().unwrap()],
        &[],
    )
    .await;
    assert_eq!(out.code, 3, "{}", out.stderr);
    assert_eq!(out.json()["error"]["code"], json!("session_file_expired"));
    assert_eq!(logins(&server), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_reports_a_session_file_it_cannot_write() {
    let server = world().await;
    let nowhere = scratch("no-such-dir").join("inside").join("s.json");
    let out = run(
        &server.url(),
        &["login", "--save-session", nowhere.to_str().unwrap()],
        &[],
    )
    .await;
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("cannot write"), "{}", out.stderr);
}

fn command_args<'a>(name: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["command", name, "--vin", VIN];
    args.extend_from_slice(extra);
    args
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_refuses_without_the_yes_flag() {
    let server = world().await;
    let out = run(&server.url(), &command_args("unlock", &["--pin", PIN]), &[]).await;
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("--yes-send-to-car"), "{}", out.stderr);
    assert!(out.stderr.contains("Nothing was sent"), "{}", out.stderr);
    assert!(
        out.stderr.contains("about to send unlock"),
        "{}",
        out.stderr
    );
    assert_eq!(out.stdout, "");
    assert!(server.requests().is_empty(), "nothing reached the server");
    assert_no_leaks(&out, &[PIN, PASSWORD]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_sends_exactly_one_request() {
    let server = world().await;
    server.reply(
        REMOTE_CONTROL,
        Reply::Data(json!({"requestSerial": "C1", "controlState": 1})),
    );
    let out = run(
        &server.url(),
        &command_args("lock", &["--yes-send-to-car", "-vv"]),
        &[("BYD_CONTROL_PIN", PIN)],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let report = out.json();
    assert_eq!(report["command"], json!("lock"));
    assert_eq!(report["vin"], json!(VIN));
    assert_eq!(report["sent"], json!(true));
    assert_eq!(report["result"]["success"], json!(true));
    assert!(out.stderr.contains("about to send lock"), "{}", out.stderr);
    let sent = server.requests_to(REMOTE_CONTROL);
    assert_eq!(sent.len(), 1);
    assert!(server.requests_to(REMOTE_CONTROL_RESULT).is_empty());
    assert_eq!(
        sent[0].inner_field("commandPwd"),
        Some(&json!(md5_hex(PIN)))
    );
    assert_eq!(sent[0].inner_field("commandType"), Some(&json!("LOCKDOOR")));
    assert_no_leaks(
        &out,
        &[PIN, &md5_hex(PIN), &md5_hex(PIN).to_lowercase(), PASSWORD],
    );

    // --pin gives the same; the table format works.
    let out = run(
        &server.url(),
        &command_args(
            "lock",
            &["--yes-send-to-car", "--pin", PIN, "--format", "table"],
        ),
        &[],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("result.success"), "{}", out.stdout);
    assert_no_leaks(&out, &[PIN, &md5_hex(PIN)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_command_has_a_happy_path() {
    let server = world().await;
    server.reply(
        REMOTE_CONTROL,
        Reply::Data(json!({"requestSerial": "C1", "controlState": 1})),
    );
    server.reply(
        CHARGE_TOGGLE,
        Reply::Data(json!({"res": 2, "requestSerial": "S1"})),
    );
    let expected = [
        ("lock", "LOCKDOOR"),
        ("unlock", "OPENDOOR"),
        ("flash-lights", "FLASHLIGHTNOWHISTLE"),
        ("find-car", "FINDCAR"),
        ("stop-climate", "CLOSEAIR"),
        ("close-windows", "CLOSEWINDOW"),
        ("open-trunk", "OPENTRUNK"),
        ("close-trunk", "CLOSETRUNK"),
    ];
    for (name, wire) in expected {
        server.clear_requests();
        let out = run(
            &server.url(),
            &command_args(name, &["--yes-send-to-car"]),
            &[("BYD_CONTROL_PIN", PIN)],
        )
        .await;
        assert_eq!(out.code, 0, "{name}: {} {}", out.stdout, out.stderr);
        let sent = server.requests_to(REMOTE_CONTROL);
        assert_eq!(sent.len(), 1, "{name}");
        assert_eq!(
            sent[0].inner_field("commandType"),
            Some(&json!(wire)),
            "{name}"
        );
    }

    server.clear_requests();
    let out = run(
        &server.url(),
        &command_args(
            "start-climate",
            &[
                "--yes-send-to-car",
                "--temperature",
                "22",
                "--minutes",
                "15",
            ],
        ),
        &[("BYD_CONTROL_PIN", PIN)],
    )
    .await;
    assert_eq!(out.code, 0, "{} {}", out.stdout, out.stderr);
    assert!(out.stderr.contains("22 C for 15 minutes"), "{}", out.stderr);
    let sent = server.requests_to(REMOTE_CONTROL);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].inner_field("commandType"), Some(&json!("OPENAIR")));
    let params = sent[0]
        .inner_field("controlParamsMap")
        .and_then(Value::as_str)
        .unwrap();
    assert!(params.contains("\"timeSpan\":2"), "{params}");

    // The defaults are 21 C for 20 minutes.
    let out = run(
        &server.url(),
        &command_args("start-climate", &["--yes-send-to-car"]),
        &[("BYD_CONTROL_PIN", PIN)],
    )
    .await;
    assert_eq!(out.code, 0, "{} {}", out.stdout, out.stderr);
    assert!(out.stderr.contains("21 C for 20 minutes"), "{}", out.stderr);

    // Charging needs no PIN.
    server.clear_requests();
    let out = run(
        &server.url(),
        &command_args("start-charging", &["--yes-send-to-car"]),
        &[],
    )
    .await;
    assert_eq!(out.code, 0, "{} {}", out.stdout, out.stderr);
    assert_eq!(out.json()["result"]["res"], json!(2));
    assert_eq!(server.requests_to(CHARGE_TOGGLE).len(), 1);
    assert!(server.requests_to(VERIFY_PIN).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_charging_never_reaches_the_car() {
    let server = world().await;
    let out = run(
        &server.url(),
        &command_args("stop-charging", &["--yes-send-to-car"]),
        &[],
    )
    .await;
    assert_eq!(out.code, 1);
    assert_eq!(out.json()["error"]["code"], json!("cloud_stop_unsupported"));
    assert_eq!(
        out.json()["error"]["class"],
        json!("BydEndpointNotSupportedError")
    );
    assert!(server.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_command_is_sent_once_unless_retries_say_otherwise() {
    let server = world().await;
    // The cloud takes the request but the car refuses it.
    server.reply(
        REMOTE_CONTROL,
        Reply::Data(json!({"requestSerial": "C1", "controlState": 2})),
    );
    let once = run(
        &server.url(),
        &command_args("lock", &["--yes-send-to-car"]),
        &[("BYD_CONTROL_PIN", PIN)],
    )
    .await;
    assert_eq!(once.code, 1, "{} {}", once.stdout, once.stderr);
    assert_eq!(
        once.json()["error"]["class"],
        json!("BydRemoteControlError")
    );
    assert_eq!(server.requests_to(REMOTE_CONTROL).len(), 1);

    server.clear_requests();
    let twice = run(
        &server.url(),
        &command_args("lock", &["--yes-send-to-car", "--retries", "2"]),
        &[("BYD_CONTROL_PIN", PIN)],
    )
    .await;
    assert_eq!(twice.code, 1, "{} {}", twice.stdout, twice.stderr);
    assert_eq!(server.requests_to(REMOTE_CONTROL).len(), 2);

    for bad in ["0", "11", "x"] {
        let out = run(
            &server.url(),
            &command_args("lock", &["--yes-send-to-car", "--retries", bad]),
            &[("BYD_CONTROL_PIN", PIN)],
        )
        .await;
        assert_eq!(out.code, 2, "--retries {bad}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn command_arguments_and_the_pin_are_checked_before_anything_is_sent() {
    let server = world().await;
    let yes = "--yes-send-to-car";
    let cases: [(Vec<&str>, &str); 5] = [
        (
            command_args("start-climate", &[yes, "--minutes", "12"]),
            "--minutes",
        ),
        (
            command_args("start-climate", &[yes, "--temperature", "50"]),
            "--temperature",
        ),
        (
            command_args("lock", &[yes, "--minutes", "10"]),
            "only apply to start-climate",
        ),
        (
            command_args("lock", &[yes, "--temperature", "20"]),
            "only apply to start-climate",
        ),
        (command_args("lock", &[yes]), "BYD_CONTROL_PIN"),
    ];
    for (args, message) in cases {
        let out = run(&server.url(), &args, &[]).await;
        assert_eq!(out.code, 2, "{args:?}: {}", out.stderr);
        assert!(out.stderr.contains(message), "{args:?}: {}", out.stderr);
        assert!(server.requests().is_empty(), "{args:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_pin_is_an_error_and_nothing_is_sent() {
    let server = world().await;
    server.reply(VERIFY_PIN, Reply::code("5011"));
    let out = run(
        &server.url(),
        &command_args("lock", &["--yes-send-to-car"]),
        &[("BYD_CONTROL_PIN", PIN)],
    )
    .await;
    assert_eq!(out.code, 1, "{} {}", out.stdout, out.stderr);
    assert!(server.requests_to(REMOTE_CONTROL).is_empty());
    assert_no_leaks(&out, &[PIN, &md5_hex(PIN)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_password_is_exit_code_3() {
    let server = world().await;
    let out = run(
        &server.url(),
        &["vehicles"],
        &[("BYD_PASSWORD", "not-the-password")],
    )
    .await;
    assert_eq!(out.code, 3, "{}", out.stderr);
    assert_eq!(
        out.json()["error"]["class"],
        json!("BydAuthenticationError")
    );
    assert_no_leaks(&out, &["not-the-password"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_server_is_exit_code_1() {
    let out = run("http://127.0.0.1:1", &["vehicles"], &[]).await;
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert_eq!(out.json()["error"]["class"], json!("BydTransportError"));
    let dump = run("http://127.0.0.1:1", &["dump", "--vin", VIN], &[]).await;
    assert_eq!(dump.code, 1, "{}", dump.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_errors_are_exit_code_2() {
    let server = world().await;
    let url = server.url();
    let cases: [&[&str]; 9] = [
        &[],
        &["get", "realtime"],
        &["get", "nonsense", "--vin", VIN],
        &["command", "lock"],
        &["command", "explode", "--vin", VIN],
        &["dump", "--skip", "nonsense"],
        &["login", "--password", "x"],
        &["--password=x", "vehicles"],
        &["watch", "--seconds", "0"],
    ];
    for args in cases {
        let out = run(&url, args, &[]).await;
        assert_eq!(out.code, 2, "{args:?}: {}", out.stderr);
        assert_eq!(out.stdout, "", "{args:?}");
    }
    assert!(server.requests().is_empty());

    // No username, or no password and nobody to ask.
    let out = run(&url, &["vehicles"], &[("BYD_USERNAME", "")]).await;
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("BYD_USERNAME"), "{}", out.stderr);
    let out = run(&url, &["vehicles"], &[("BYD_PASSWORD", "")]).await;
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("BYD_PASSWORD"), "{}", out.stderr);
    let out = run(&url, &["vehicles"], &[("BYD_SESSION_TTL", "soon")]).await;
    assert_eq!(out.code, 2);
    assert!(
        out.stderr.contains("invalid configuration"),
        "{}",
        out.stderr
    );
    assert!(server.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_help_says_how_secrets_are_handled() {
    let out = run("http://127.0.0.1:1", &["--help"], &[]).await;
    assert_eq!(out.code, 0);
    for needle in [
        "BYD_PASSWORD",
        "no --password flag",
        "EXIT CODES",
        "authentication failed",
        "--redact",
    ] {
        assert!(
            out.stdout.contains(needle),
            "help lacks {needle}: {}",
            out.stdout
        );
    }
    assert!(
        !out.stdout
            .lines()
            .any(|line| line.trim_start().starts_with("--password"))
    );
    let login = run("http://127.0.0.1:1", &["login", "--help"], &[]).await;
    assert!(
        login.stdout.contains("ends an existing session"),
        "{}",
        login.stdout
    );
    let command = run("http://127.0.0.1:1", &["command", "--help"], &[]).await;
    assert!(command.stdout.contains("--yes-send-to-car"));
    assert!(command.stdout.contains("MOVES A REAL CAR"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_prints_each_event_as_a_line_of_json() {
    let server = world().await;
    let broker = TestBroker::start().await;
    server.reply(
        BROKER_LOOKUP,
        Reply::Data(json!({"emqBorker": broker.address()})),
    );
    let session = saved_session(&server, "watch.json").await;
    let (user_id, _, encry) = tokens_of(&session);
    let key = md5_hex(&encry);
    let topic = format!("oversea/res/{user_id}");

    let mut command = base_command(&server.url());
    let child = command
        .args(["watch", "--session", session.to_str().unwrap()])
        .args(["--vin", VIN, "--seconds", "4"])
        .env("OPENBYD_TEST_MQTT_TRUST_ANY", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    broker.until_subscribed(&topic).await;
    let other = "TESTVIN0000000002";
    for (vin, percent) in [(other, 11), (VIN, 55)] {
        let event = json!({"event": "vehicleInfo", "vin": vin, "data": {
            "uuid": "R7",
            "respondData": {"elecPercent": percent, "onlineState": 1, "time": 1700000000},
        }});
        assert_eq!(broker.publish_sealed(&topic, &event, &key), 1);
    }
    let output = tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "only the watched car's event: {stdout}");
    let event: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(event["event"], json!("vehicleInfo"));
    assert_eq!(event["vin"], json!(VIN));
    assert_eq!(event["data"]["elecPercent"], json!(55));
    assert!(event["time"].as_str().unwrap().ends_with('Z'));
    assert!(stderr.contains("watching"), "{stderr}");
    assert!(!stdout.contains(&encry) && !stderr.contains(&encry));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_fails_when_mqtt_cannot_start() {
    let server = world().await;
    let session = saved_session(&server, "watch-nobroker.json").await;
    let out = run(
        &server.url(),
        &[
            "watch",
            "--session",
            session.to_str().unwrap(),
            "--seconds",
            "1",
        ],
        &[],
    )
    .await;
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("MQTT"), "{}", out.stderr);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notifications_list_the_messages_of_each_category() {
    let server = world().await;
    let out = run(
        &server.url(),
        &["get", "notifications", "--vin", VIN, "--raw"],
        &[],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let parsed = &out.json()["parsed"];
    assert_eq!(parsed["VEHICLE"][0]["title"], json!("Charging done"));
    assert_eq!(parsed["VEHICLE"][0]["message_id"], json!(7));
    assert_eq!(out.json()["raw"]["SYSTEM"][0]["messageId"], json!("7"));
    assert_eq!(parsed["SYSTEM"][0]["title"], json!("Charging done"));
    assert!(parsed["VEHICLE"][0].get("raw").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flags_override_the_environment() {
    let server = world().await;
    let out = run(
        "http://127.0.0.1:1",
        &[
            "vehicles",
            "--base-url",
            &server.url(),
            "--username",
            USER,
            "--country-code",
            "NL",
            "--language",
            "en",
            "--time-zone",
            "Europe/Amsterdam",
        ],
        &[("BYD_USERNAME", "someone.else@example.com")],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(logins(&server), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dump_names_a_vehicle_the_list_does_not_have() {
    let server = world().await;
    let unknown = "TESTVIN0000000009";
    let out = run(
        &server.url(),
        &["dump", "--vin", unknown, "--skip", "realtime,gps"],
        &[],
    )
    .await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let vehicle = &out.json()["vehicles"][0];
    assert_eq!(vehicle["vin"], json!(unknown));
    assert_eq!(vehicle["vehicle"], Value::Null);

    // A vehicle list that fails ends a whole-account dump but not a dump of one VIN.
    server.reply("/app/account/getAllListByUserId", Reply::code("9999"));
    let all = run(&server.url(), &["dump"], &[]).await;
    assert_eq!(all.code, 1);
    assert_eq!(all.json()["error"]["code"], json!("9999"));
    let one = run(
        &server.url(),
        &["dump", "--vin", VIN, "--skip", "realtime,gps"],
        &[],
    )
    .await;
    assert_eq!(one.code, 0, "{}", one.stderr);
    assert_eq!(
        one.json()["vehicles"][0]["vehicle"]["error"]["code"],
        json!("9999")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capabilities_report_why_the_latest_config_failed() {
    let server = world().await;
    server.reply(
        "/vehicle/vehicleswitch/getLatestConfig",
        Reply::code("9999"),
    );
    let out = run(&server.url(), &["get", "capabilities", "--vin", VIN], &[]).await;
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert_eq!(out.json()["error"]["code"], json!("9999"));
}
