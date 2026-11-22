//! The host surface end to end: a client driven by method name against the
//! fake server and the test broker, as a binding drives it.

#![cfg(feature = "testing")]

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use openbyd_host::{
    ASYNC_METHODS, Client, EVENTS, Host, HostError, SYNC_METHODS, runtime, testing,
};
use serde_json::{Value, json};

const VIN: &str = "TESTVIN0000000001";
const USER: &str = "user@example.com";
const PASSWORD: &str = "secret";
const PIN: &str = "123456";

const BROKER_LOOKUP: &str = "/app/emqAuth/getEmqBrokerIp";
const VEHICLES: &str = "/app/account/getAllListByUserId";
const LATEST_CONFIG: &str = "/vehicle/vehicleswitch/getLatestConfig";
const VERIFY: &str = "/vehicle/vehicleswitch/verifyControlPassword";
const HVAC: &str = "/control/getStatusNow";
const LOGIN: &str = "/app/account/login";
const REMOTE: &str = "/control/remoteControl";
const REALTIME: &str = "/vehicleInfo/vehicle/vehicleRealTimeRequest";

type Answerer = Box<dyn Fn(&str, &Value) -> Option<Value> + Send + Sync>;

/// A host that records every event and answers the client's questions.
#[derive(Default)]
struct Recorder {
    client: OnceLock<Weak<Client>>,
    events: Mutex<Vec<(String, Value)>>,
    /// The arguments of `resolve` (without `request`) for a question, or `None` to stay silent.
    answer: OnceLock<Answerer>,
}

impl Recorder {
    fn named(&self, name: &str) -> Vec<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(event, _)| event == name)
            .map(|(_, payload)| payload.clone())
            .collect()
    }
}

impl Host for Recorder {
    fn event(&self, name: &str, payload: &str) {
        let payload: Value = serde_json::from_str(payload).expect("an event's payload is JSON");
        assert!(payload.is_object(), "{name}: {payload}");
        self.events
            .lock()
            .unwrap()
            .push((name.to_owned(), payload.clone()));
        let Some(request) = payload.get("request").and_then(Value::as_u64) else {
            return;
        };
        let answer = self.answer.get().and_then(|answer| answer(name, &payload));
        let client = self.client.get().and_then(Weak::upgrade);
        if let (Some(mut answer), Some(client)) = (answer, client) {
            answer["request"] = json!(request);
            // A binding answers from its own thread; so does this host.
            std::thread::spawn(move || {
                client.call_sync("resolve", answer).expect("resolve");
            });
        }
    }
}

fn testing(name: &str, args: Value) -> Value {
    testing::call(name, args).unwrap_or_else(|error| panic!("{name}: {error}"))
}

/// A fake server and a broker wired together, and clients pointed at both.
struct World {
    server: Value,
    broker: Value,
    url: String,
}

impl World {
    fn new() -> Self {
        let started = testing(
            "server.start",
            json!({"accounts": [{"username": USER, "password": PASSWORD}]}),
        );
        let broker = testing("broker.start", Value::Null);
        let world = Self {
            url: started["url"].as_str().unwrap().to_owned(),
            server: started["server"].clone(),
            broker: broker["broker"].clone(),
        };
        let functions: Vec<Value> = [
            "1001", "1005", "1006", "1012", "1020", "1021", "1026", "1027", "1030", "1033",
        ]
        .iter()
        .map(|no| json!({ "functionNo": no }))
        .collect();
        world.reply(
            BROKER_LOOKUP,
            json!({"data": {"emqBorker": broker["address"]}}),
        );
        world.reply(
            VEHICLES,
            json!({"data": [{"vin": VIN, "energyType": "0", "modelName": "SEAL"}]}),
        );
        world.reply(
            LATEST_CONFIG,
            json!({"data": {VIN: {"cfFixedList": functions}}}),
        );
        world.reply(VERIFY, json!({"data": {"ok": true}}));
        world.reply(HVAC, json!({"data": {"acSwitch": 1, "tempInCar": 21}}));
        world
    }

    fn reply(&self, endpoint: &str, reply: Value) {
        testing(
            "server.reply",
            json!({"server": self.server, "endpoint": endpoint, "reply": reply}),
        );
    }

    fn requests(&self, endpoint: &str) -> Vec<Value> {
        testing(
            "server.requests",
            json!({"server": self.server, "endpoint": endpoint}),
        )
        .as_array()
        .unwrap()
        .clone()
    }

    fn config(&self, extra: Value) -> Value {
        let mut config = json!({
            "username": USER,
            "password": PASSWORD,
            "base_url": self.url,
            "mqtt_enabled": false,
            "mqtt_timeout": 5.0,
            "control_pin": PIN,
        });
        for (key, value) in extra.as_object().unwrap() {
            config[key] = value.clone();
        }
        config
    }

    /// A started client with these extra config fields and options.
    fn client(&self, config: Value, mut options: Value) -> (Arc<Client>, Arc<Recorder>) {
        options["test_tls"] = json!(true);
        let recorder = Arc::new(Recorder::default());
        let host: Arc<dyn Host> = recorder.clone();
        let client = Client::new(self.config(config), options, host).expect("a client");
        recorder.client.set(Arc::downgrade(&client)).ok();
        client.call_sync("start", Value::Null).unwrap();
        (client, recorder)
    }

    fn push(&self, client: &Client, body: Value) {
        let session = client.call_sync("export_session", Value::Null).unwrap();
        let reached = testing(
            "broker.push",
            json!({"broker": self.broker, "session": session, "body": body}),
        );
        assert_eq!(reached, 1, "the push reached the client");
    }

    fn until_subscribed(&self, client: &Client) {
        let session = client.call_sync("export_session", Value::Null).unwrap();
        let topic = format!("oversea/res/{}", session["user_id"].as_str().unwrap());
        until("a subscription", || {
            testing(
                "broker.subscribers",
                json!({"broker": self.broker, "topic": topic}),
            )
            .as_u64()
                > Some(0)
        });
    }
}

impl Drop for World {
    fn drop(&mut self) {
        testing("server.stop", json!({"server": self.server}));
        testing("broker.stop", json!({"broker": self.broker}));
    }
}

fn until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Runs an async method as a binding does: on the shared runtime, waited for here.
fn call(client: &Arc<Client>, method: &str, args: Value) -> Result<Value, HostError> {
    let task = runtime::spawn(Arc::clone(client).call(method.to_owned(), args));
    runtime::block_on(task).expect("the call did not panic")
}

fn ok(client: &Arc<Client>, method: &str, args: Value) -> Value {
    call(client, method, args).unwrap_or_else(|error| panic!("{method}: {error}"))
}

fn vin() -> Value {
    json!({ "vin": VIN })
}

#[test]
fn a_client_is_closed_until_started_and_after_close() {
    let world = World::new();
    let recorder = Arc::new(Recorder::default());
    let client = Client::new(world.config(json!({})), Value::Null, recorder).unwrap();
    let error = call(&client, "get_hvac_status", vin()).unwrap_err();
    assert_eq!(error.class, "BydError");
    assert!(error.message.contains("not initialized"), "{error}");

    client.call_sync("start", Value::Null).unwrap();
    assert_eq!(ok(&client, "get_hvac_status", vin())["ac_switch"], 1);
    client.call_sync("close", Value::Null).unwrap();
    assert_eq!(
        call(&client, "login", Value::Null).unwrap_err().class,
        "BydError"
    );
}

#[test]
fn reads_return_pybyds_dumps() {
    let world = World::new();
    let (client, _) = world.client(json!({"country_code": "DE", "language": "de"}), json!({}));
    assert_eq!(
        client.call_sync("export_session", Value::Null).unwrap(),
        Value::Null
    );
    ok(&client, "login", Value::Null);

    let vehicles = ok(&client, "get_vehicles", Value::Null);
    assert_eq!(vehicles[0]["vin"], VIN);
    assert_eq!(vehicles[0]["energy_type"], 0);
    assert!(vehicles[0]["raw"].is_object());
    let login = &world.requests(LOGIN)[0];
    assert_eq!(login["outer"]["countryCode"], "DE");
    assert_eq!(login["outer"]["language"], "de");

    let capabilities = ok(&client, "get_vehicle_capabilities", vin());
    assert_eq!(capabilities["lock"], true);
    let configs = ok(&client, "get_latest_configs", json!({"vins": [VIN]}));
    assert!(configs[VIN].is_object());
    assert!(ok(&client, "get_latest_config", vin()).is_object());

    world.reply(
        "/control/smartCharge/homePage",
        json!({"data": {"elecPercent": 80, "chargingState": 0}}),
    );
    let homepage = ok(&client, "get_charging_homepage", vin());
    assert_eq!(homepage["status"]["soc"].as_f64(), Some(80.0));
    assert!(homepage["schedule"].is_object());

    let session = ok(&client, "ensure_session", Value::Null);
    assert!(session["sign_token"].is_string());
    assert!(session["ttl"].as_f64().unwrap() > 0.0);
    let metadata = client.call_sync("vehicle_metadata", vin()).unwrap();
    assert_eq!(metadata["model_name"], "SEAL");
    assert_eq!(
        client.call_sync("commands_enabled", Value::Null).unwrap(),
        false
    );
    assert_eq!(
        client.call_sync("mqtt_is_running", Value::Null).unwrap(),
        false
    );
}

#[test]
fn failures_carry_pybyds_class_and_attributes() {
    let world = World::new();
    let (client, _) = world.client(json!({}), json!({}));
    world.reply("/control/smartCharge/homePage", json!({"code": "1001"}));
    let error = call(&client, "get_charging_status", vin()).unwrap_err();
    assert_eq!(error.class, "BydEndpointNotSupportedError");
    assert_eq!(error.bases, ["BydApiError", "BydError"]);
    assert_eq!(error.code.as_deref(), Some("1001"));
    assert_eq!(
        error.endpoint.as_deref(),
        Some("/control/smartCharge/homePage")
    );

    world.reply(HVAC, json!({"status": 503, "body": "down"}));
    let error = call(&client, "get_hvac_status", vin()).unwrap_err();
    assert_eq!(error.class, "BydTransportError");
    assert_eq!(error.status_code, Some(503));

    let error = call(&client, "get_hvac_status", json!({})).unwrap_err();
    assert_eq!(
        (error.class.as_str(), error.message.as_str()),
        ("TypeError", "missing argument: vin")
    );
    let error = call(
        &client,
        "start_climate",
        json!({"vin": VIN, "params": {"temperature": "hot"}}),
    )
    .unwrap_err();
    assert_eq!(error.class, "ValidationError");
    assert_eq!(error.loc, Some(vec!["temperature".to_owned()]));
    let error = call(&client, "get_notifications", json!({"category": 9})).unwrap_err();
    assert_eq!(error.class, "ValueError");
    let error = client
        .call_sync(
            "seed_vehicle_metadata",
            json!({"vin": VIN, "energy_type": "x"}),
        )
        .unwrap_err();
    assert_eq!(error.class, "TypeError");
}

#[test]
fn every_listed_method_is_dispatched() {
    let world = World::new();
    let recorder = Arc::new(Recorder::default());
    let client = Client::new(world.config(json!({})), Value::Null, recorder).unwrap();
    for method in ASYNC_METHODS {
        if let Err(error) = call(&client, method, Value::Null) {
            assert_ne!(error.class, "AttributeError", "{method}: {error}");
        }
    }
    for method in SYNC_METHODS {
        if let Err(error) = client.call_sync(method, Value::Null) {
            assert_ne!(error.class, "AttributeError", "{method}: {error}");
        }
    }
    for (method, sync) in [
        ("nope", false),
        ("car.nope", false),
        ("car.lock.explode", false),
        ("nope", true),
    ] {
        let error = if sync {
            client.call_sync(method, Value::Null).unwrap_err()
        } else {
            call(&client, method, json!({"vin": VIN, "car": 1})).unwrap_err()
        };
        assert_eq!(error.class, "AttributeError", "{method}");
    }
    // A method of the other kind is unknown to each entry point.
    assert_eq!(
        client.call_sync("login", Value::Null).unwrap_err().class,
        "AttributeError"
    );
    assert_eq!(
        call(&client, "close", vin()).unwrap_err().class,
        "AttributeError"
    );
}

#[test]
fn options_are_checked() {
    let world = World::new();
    let make = |config: Value, options: Value| {
        let host: Arc<dyn Host> = Arc::new(Recorder::default());
        Client::new(world.config(config), options, host).map(drop)
    };
    for (options, class) in [
        (json!({"command_retries": 0}), "ValueError"),
        (json!({"command_ack_ttl_seconds": "long"}), "TypeError"),
        (json!({"gate_policy": "lenient"}), "ValueError"),
        (json!({"pin_resolver": "magic"}), "ValueError"),
        (json!({"relogin": "sometimes"}), "ValueError"),
        (json!({"events": ["weather"]}), "ValueError"),
        (json!({"events": "all"}), "TypeError"),
        (json!([1]), "TypeError"),
    ] {
        assert_eq!(
            make(json!({}), options.clone()).unwrap_err().class,
            class,
            "{options}"
        );
    }
    assert_eq!(
        make(json!({"device": {"nope": "1"}}), json!({}))
            .unwrap_err()
            .class,
        "ValidationError"
    );
    let host: Arc<dyn Host> = Arc::new(Recorder::default());
    assert_eq!(
        Client::new(json!("config"), Value::Null, host)
            .map(drop)
            .unwrap_err()
            .class,
        "TypeError"
    );
    let every: Vec<&str> = EVENTS.to_vec();
    make(
        json!({}),
        json!({"events": every, "gate_policy": "strict", "relogin": "password"}),
    )
    .unwrap();
}

fn lock_with(
    world: &World,
    functions: Option<Value>,
    options: Value,
    answer: Option<Answerer>,
) -> (Result<Value, HostError>, Arc<Recorder>) {
    if let Some(functions) = functions {
        world.reply(LATEST_CONFIG, json!({"data": {VIN: functions}}));
    }
    world.reply(
        REMOTE,
        json!({"data": {"controlState": 1, "requestSerial": "S1"}}),
    );
    let (client, recorder) = world.client(json!({}), options);
    if let Some(answer) = answer {
        recorder.answer.set(answer).ok();
    }
    ok(&client, "verify_command_access", vin());
    assert_eq!(
        client.call_sync("commands_enabled", Value::Null).unwrap(),
        true
    );
    (call(&client, "lock", vin()), recorder)
}

#[test]
fn no_gate_policy_sends_what_a_guest_grant_refuses() {
    // The grant allows basic control ("21") and only the lock functionNo:
    // pyBYD refuses closing the windows (function_no_missing), and neither
    // "allow_missing_data" nor the host is asked to let it through.
    let world = World::new();
    world.reply(
        VEHICLES,
        json!({"data": [{
            "vin": VIN, "energyType": "0", "empowerType": -1, "empowerId": 7,
            "rangeDetailList": [{"code": "2", "childList": [{"code": "21"}, {"code": "1005"}]}],
        }]}),
    );
    world.reply(
        LATEST_CONFIG,
        json!({"data": {VIN: {"cfFixedList": [{"functionNo": "1026"}, {"functionNo": "1005"}]}}}),
    );
    world.reply(
        REMOTE,
        json!({"data": {"controlState": 1, "requestSerial": "S1"}}),
    );
    for policy in ["allow_missing_data", "host"] {
        let (client, recorder) = world.client(json!({}), json!({"gate_policy": policy}));
        let allow: Answerer = Box::new(|_, _| Some(json!({"value": true})));
        recorder.answer.set(allow).ok();
        ok(&client, "verify_command_access", vin());
        ok(&client, "get_vehicles", json!({}));
        let refused = call(&client, "close_windows", vin()).unwrap_err();
        assert_eq!(refused.class, "BydEndpointNotSupportedError");
        assert_eq!(refused.code.as_deref(), Some("command_gate_blocked"));
        assert!(recorder.named("gate_policy").is_empty(), "{policy}");
    }
    assert!(world.requests(REMOTE).is_empty());
}

#[test]
fn the_gate_policy_is_the_built_in_one_or_the_hosts() {
    let world = World::new();
    let missing = json!({"cfFixedList": []});
    let (refused, _) = lock_with(&world, Some(missing.clone()), json!({}), None);
    let refused = refused.unwrap_err();
    assert_eq!(refused.class, "BydEndpointNotSupportedError");
    assert_eq!(refused.code.as_deref(), Some("command_gate_blocked"));
    assert!(world.requests(REMOTE).is_empty());

    let (sent, _) = lock_with(
        &world,
        Some(missing.clone()),
        json!({"gate_policy": "allow_missing_data"}),
        None,
    );
    assert_eq!(sent.unwrap()["success"], true);

    let allow: Answerer = Box::new(|name, payload| {
        assert_eq!(name, "gate_policy");
        Some(json!({"value": payload["verdict"]["reason"] == "function_no_missing"}))
    });
    let (sent, recorder) = lock_with(
        &world,
        Some(missing.clone()),
        json!({"gate_policy": "host"}),
        Some(allow),
    );
    assert_eq!(sent.unwrap()["success"], true);
    let asked = &recorder.named("gate_policy")[0];
    assert_eq!(asked["vin"], VIN);
    assert_eq!(asked["verdict"]["supported"], false);

    // A host that fails the question, or answers nonsense, falls back to the verdict.
    for answer in [
        json!({"error": {"class": "BydError", "message": "policy bug"}}),
        json!({"value": "yes"}),
    ] {
        let failing: Answerer = Box::new(move |_, _| Some(answer.clone()));
        let (refused, _) = lock_with(
            &world,
            Some(missing.clone()),
            json!({"gate_policy": "host"}),
            Some(failing),
        );
        assert_eq!(refused.unwrap_err().class, "BydEndpointNotSupportedError");
    }
}

#[test]
fn the_pin_resolver_can_be_the_hosts() {
    let world = World::new();
    let digest = "0123456789abcdef0123456789abcdef";
    let resolver: Answerer = Box::new(move |name, payload| {
        assert_eq!(name, "pin_resolver");
        assert_eq!(payload["control_pin"], PIN);
        assert_eq!(payload["command_pwd"], Value::Null);
        Some(json!({ "value": digest }))
    });
    let (sent, _) = lock_with(
        &world,
        None,
        json!({"pin_resolver": "host"}),
        Some(resolver),
    );
    sent.unwrap();
    assert_eq!(world.requests(REMOTE)[0]["inner"]["commandPwd"], digest);

    // An answer that is no string falls back to pyBYD's resolver: MD5, upper case.
    testing("server.clear_requests", json!({"server": world.server}));
    let nonsense: Answerer = Box::new(|_, _| Some(json!({"value": 1234})));
    let (sent, _) = lock_with(
        &world,
        None,
        json!({"pin_resolver": "host"}),
        Some(nonsense),
    );
    sent.unwrap();
    assert_eq!(
        world.requests(REMOTE)[0]["inner"]["commandPwd"],
        "E10ADC3949BA59ABBE56E057F20F883E"
    );
}

#[test]
fn the_request_event_reports_every_request() {
    let world = World::new();
    let (client, recorder) = world.client(json!({}), json!({"events": ["request"]}));
    ok(&client, "login", Value::Null);
    ok(&client, "get_hvac_status", vin());
    world.reply("/control/smartCharge/homePage", json!({"code": "1001"}));
    call(&client, "get_charging_status", vin()).unwrap_err();
    let records = recorder.named("request");
    let endpoints: Vec<&str> = records
        .iter()
        .map(|payload| payload["record"]["endpoint"].as_str().unwrap())
        .collect();
    assert_eq!(endpoints, [LOGIN, HVAC, "/control/smartCharge/homePage"]);
    assert_eq!(records[2]["record"]["code"], "1001");
    assert_eq!(records[0]["record"]["outcome"], "reply");
    assert!(records[0]["record"]["duration"].as_f64().unwrap() >= 0.0);
}

#[test]
fn sessions_are_exported_imported_and_renewed_by_the_host() {
    let world = World::new();
    let (first, _) = world.client(json!({}), json!({}));
    ok(&first, "login", Value::Null);
    let saved = first.call_sync("export_session", Value::Null).unwrap();
    assert!(saved["ttl_remaining"].as_f64().unwrap() > 0.0);

    let (restored, _) = world.client(json!({"password": ""}), json!({"relogin": "never"}));
    ok(&restored, "import_session", json!({ "session": saved }));
    assert_eq!(ok(&restored, "get_hvac_status", vin())["ac_switch"], 1);
    assert_eq!(
        world.requests(LOGIN).len(),
        1,
        "the restored client never logged in"
    );
    let bad = call(
        &restored,
        "import_session",
        json!({"session": {"user_id": ""}}),
    )
    .unwrap_err();
    assert_eq!(bad.class, "ValueError");

    testing(
        "server.expire_sessions",
        json!({"server": world.server, "username": USER}),
    );
    let expired = call(&restored, "get_hvac_status", vin()).unwrap_err();
    assert_eq!(expired.class, "BydSessionExpiredError");
    // BYD's own expiry code survives a disabled re-login.
    assert_eq!(
        expired.code.as_deref(),
        Some(openbyd_client::testing::codes::SESSION_EXPIRED)
    );
    restored
        .call_sync("invalidate_session", Value::Null)
        .unwrap();

    // The host hands over a new session when asked.
    let (helper, _) = world.client(json!({}), json!({}));
    ok(&helper, "login", Value::Null);
    let fresh = helper.call_sync("export_session", Value::Null).unwrap();
    let (asking, recorder) = world.client(json!({}), json!({"relogin": "host"}));
    let handed = fresh.clone();
    recorder
        .answer
        .set(Box::new(move |name, _| {
            (name == "relogin").then(|| json!({ "value": handed }))
        }))
        .ok();
    assert_eq!(ok(&asking, "get_hvac_status", vin())["ac_switch"], 1);
    assert_eq!(recorder.named("relogin").len(), 1);

    // Its refusal is the call's error; an answer that is no session is a BydError.
    for (answer, class, text) in [
        (
            json!({"error": {"class": "BydAuthenticationError", "message": "no", "code": "1002", "endpoint": "x"}}),
            "BydAuthenticationError",
            "no",
        ),
        (
            json!({"value": 42}),
            "BydError",
            "relogin returned no session",
        ),
        (
            json!({"error": {"class": "KeyError", "message": "bug"}}),
            "BydError",
            "KeyError",
        ),
    ] {
        let (refused, recorder) = world.client(json!({}), json!({"relogin": "host"}));
        recorder
            .answer
            .set(Box::new(move |_, _| Some(answer.clone())))
            .ok();
        let error = call(&refused, "get_hvac_status", vin()).unwrap_err();
        assert_eq!(error.class, class);
        assert!(error.message.contains(text), "{error}");
    }
    assert_eq!(
        first.call_sync("resolve", json!({"request": 999})).unwrap(),
        false
    );
}

fn realtime(lock: i64, time: i64, percent: i64) -> Value {
    let mut payload = json!({"onlineState": 1, "time": time, "elecPercent": percent});
    for door in [
        "leftFrontDoorLock",
        "rightFrontDoorLock",
        "leftRearDoorLock",
        "rightRearDoorLock",
    ] {
        payload[door] = json!(lock);
    }
    payload
}

fn vehicle_info(data: Value) -> Value {
    json!({"event": "vehicleInfo", "vin": VIN, "data": {"respondData": data}})
}

#[test]
fn pushes_arrive_as_events() {
    let world = World::new();
    let (client, recorder) = world.client(
        json!({"mqtt_enabled": true}),
        json!({"events": ["vehicle_info", "mqtt_event", "mqtt_connect", "command_ack", "command_lifecycle"]}),
    );
    ok(&client, "login", Value::Null);
    assert_eq!(
        client.call_sync("mqtt_is_running", Value::Null).unwrap(),
        true
    );
    world.until_subscribed(&client);
    until("mqtt_connect", || {
        !recorder.named("mqtt_connect").is_empty()
    });
    world.push(
        &client,
        vehicle_info(json!({"elecPercent": 64, "time": 1_700_000_000})),
    );
    world.push(
        &client,
        json!({"event": "smartCharge", "vin": VIN, "data": {"respondData": {"elecPercent": 64}}}),
    );
    until("two events", || recorder.named("mqtt_event").len() == 2);
    until("the vehicle info", || {
        recorder.named("vehicle_info").len() == 1
    });
    let info = &recorder.named("vehicle_info")[0];
    assert_eq!(
        (info["vin"].as_str(), info["data"]["elec_percent"].as_f64()),
        (Some(VIN), Some(64.0))
    );
    let events = recorder.named("mqtt_event");
    assert_eq!(events[0]["event"], "vehicleInfo");
    assert_eq!(events[0]["data"]["elecPercent"], 64);
    assert_eq!(events[1]["event"], "smartCharge");

    // A command acknowledged by push.
    world.reply(REMOTE, json!({"data": {"requestSerial": "C1"}}));
    ok(&client, "verify_command_access", vin());
    let locking = runtime::spawn(Arc::clone(&client).call("lock".into(), vin()));
    until("the command", || world.requests(REMOTE).len() == 1);
    std::thread::sleep(Duration::from_millis(50));
    world.push(
        &client,
        json!({"event": "remoteControl", "vin": VIN, "data": {"uuid": "C1", "respondData": {"res": 2}}}),
    );
    let result = runtime::block_on(locking).unwrap().unwrap();
    assert_eq!(result["request_serial"], "C1");
    until("the ack", || recorder.named("command_ack").len() == 1);
    assert_eq!(
        recorder.named("command_ack")[0]["ack"]["request_serial"],
        "C1"
    );
    let statuses: Vec<Value> = recorder
        .named("command_lifecycle")
        .iter()
        .map(|payload| payload["event"]["status"].clone())
        .collect();
    assert_eq!(statuses, ["registered", "matched"]);
    let diagnostics = client
        .call_sync("get_command_ack_diagnostics", Value::Null)
        .unwrap();
    assert_eq!(diagnostics["matched"], 1);
    ok(&client, "start_mqtt", Value::Null);
}

#[test]
fn a_car_merges_projects_and_reports_its_state() {
    let world = World::new();
    world.reply(REALTIME, json!({"data": realtime(1, 1_700_000_000, 80)}));
    world.reply(
        "/control/getGpsInfo",
        json!({"data": {"latitude": 52.0, "longitude": 4.0}}),
    );
    world.reply(
        "/control/smartCharge/homePage",
        json!({"data": {"elecPercent": 80, "chargingState": 0}}),
    );
    world.reply(
        "/vehicleInfo/vehicle/getEnergyConsumption",
        json!({"data": {"totalEnergy": "15.2kW·h/100km"}}),
    );
    world.reply(
        "/control/smartCharge/getAcCurrentLimit",
        json!({"data": {"acCurrentLimit": 16}}),
    );
    let (client, recorder) = world.client(json!({}), json!({}));
    ok(&client, "verify_command_access", vin());

    let car = ok(
        &client,
        "get_car",
        json!({"vin": VIN, "state_events": true}),
    );
    assert_eq!(car["vin"], VIN);
    let again = ok(&client, "get_car", vin());
    assert_eq!(again["car"], car["car"], "the kept car has the same number");
    let me = json!({"car": car["car"]});

    let features = client.call_sync("car.features", me.clone()).unwrap();
    assert_eq!(features["lock"]["available"], true);
    assert_eq!(features["hvac"]["temp_range"], json!([15.0, 31.0]));
    assert!(features["seat"]["rear_left"]["heat"].is_boolean());
    let names: Vec<&String> = features.as_object().unwrap().keys().collect();
    assert_eq!(
        names,
        [
            "lock", "hvac", "seat", "steering", "battery", "finder", "windows", "trunk"
        ]
    );
    assert_eq!(
        client.call_sync("car.capabilities", me.clone()).unwrap()["lock"],
        true
    );

    assert_eq!(
        ok(&client, "car.update_realtime", me.clone())["elec_percent"],
        80.0
    );
    for section in ["hvac", "gps", "charging", "energy"] {
        ok(&client, &format!("car.update_{section}"), me.clone());
    }
    assert!(ok(&client, "car.get_ac_current_limit", me.clone()).is_object());
    let state = client.call_sync("car.state", me.clone()).unwrap();
    for section in [
        "vehicle",
        "realtime",
        "hvac",
        "gps",
        "charging",
        "energy",
        "charging_schedule",
    ] {
        assert!(state[section].is_object(), "{section}");
    }
    until("state change events", || {
        recorder.named("state_changed").len() >= 5
    });
    assert_eq!(recorder.named("state_changed")[0]["vin"], VIN);
    assert!(recorder.named("state_changed")[0]["state"]["vehicle"].is_object());

    // A command projects its outcome onto the state.
    world.reply(
        REMOTE,
        json!({"data": {"controlState": 1, "requestSerial": "S1"}}),
    );
    ok(&client, "car.lock.lock", me.clone());
    let doors = |client: &Client| {
        client.call_sync("car.state", me.clone()).unwrap()["realtime"]["left_front_door_lock"]
            .clone()
    };
    assert_eq!(doors(&client), 2, "locked, projected");

    // Pushes handed to the car directly, and their validation.
    let mut push = me.clone();
    push["data"] = json!({"elecPercent": 81, "chargingState": 1});
    client
        .call_sync("car.handle_mqtt_charging", push.clone())
        .unwrap();
    until("the charging push", || {
        client.call_sync("car.state", me.clone()).unwrap()["charging"]["soc"].as_f64() == Some(81.0)
    });
    push["data"] = realtime(2, 1_700_000_400, 79);
    client
        .call_sync("car.handle_mqtt_realtime", push.clone())
        .unwrap();
    until("the realtime push", || {
        client.call_sync("car.state", me.clone()).unwrap()["realtime"]["elec_percent"].as_f64()
            == Some(79.0)
    });
    push["data"] = json!({"time": 1_700_000_300});
    client
        .call_sync("car.handle_mqtt_energy", push.clone())
        .unwrap();
    push["data"] = json!({"elecPercent": "lots"});
    let invalid = client
        .call_sync("car.handle_mqtt_charging", push)
        .unwrap_err();
    assert_eq!(invalid.class, "ValidationError");

    assert_eq!(
        call(&client, "car.update_hvac", json!({"car": 99}))
            .unwrap_err()
            .class,
        "ValueError"
    );
    assert_eq!(
        call(&client, "car.update_hvac", json!({"car": 0}))
            .unwrap_err()
            .class,
        "ValueError"
    );

    // After the client closed, the car's calls fail as a closed client's do.
    client.call_sync("close", Value::Null).unwrap();
    assert_eq!(
        call(&client, "car.update_hvac", me.clone())
            .unwrap_err()
            .class,
        "BydError"
    );
    client.call_sync("car.close", me).unwrap();
}

#[test]
fn every_car_command_reaches_the_client() {
    let world = World::new();
    world.reply(
        REMOTE,
        json!({"data": {"controlState": 1, "requestSerial": "C1"}}),
    );
    world.reply(
        "/control/remoteControlBatteryHeat",
        json!({"data": {"controlState": 1, "requestSerial": "C2"}}),
    );
    world.reply(
        "/control/smartCharge/changeChargeStatue",
        json!({"data": {"res": 2, "requestSerial": "S"}}),
    );
    world.reply(
        "/control/smartCharge/saveOrUpdate",
        json!({"data": {"res": 2, "requestSerial": "S"}}),
    );
    world.reply(
        "/control/smartCharge/homePage",
        json!({"data": {"elecPercent": 80}}),
    );
    let functions: Vec<Value> = [
        "1001", "1005", "1006", "1007", "1008", "1009", "1012", "1020", "1021", "1026", "10300002",
        "10030010", "10030002", "10030001", "10030004",
    ]
    .iter()
    .map(|no| json!({ "functionNo": no }))
    .collect();
    world.reply(
        LATEST_CONFIG,
        json!({"data": {VIN: {"cfFixedList": functions}}}),
    );
    let (client, _) = world.client(json!({}), json!({}));
    ok(&client, "verify_command_access", vin());
    let vehicle =
        json!({"vin": VIN, "energyType": "0", "vehicleFunLearnInfo": {"openWindowLearnInfo": 1}});
    let car = ok(&client, "get_car", json!({"vin": VIN, "vehicle": vehicle}))["car"].clone();
    let with = |extra: Value| {
        let mut args = extra;
        args["car"] = car.clone();
        args
    };
    for (method, args) in [
        ("car.lock.lock", json!({})),
        ("car.lock.unlock", json!({})),
        ("car.hvac.start", json!({"temperature": 22, "duration": 10})),
        ("car.hvac.stop", json!({})),
        (
            "car.hvac.schedule",
            json!({"params": {"booking_id": 1, "booking_time": 1_700_000_000}}),
        ),
        ("car.seat.heat", json!({"position": "driver", "level": 2})),
        (
            "car.seat.ventilation",
            json!({"position": "copilot", "level": 1}),
        ),
        ("car.steering.heat", json!({"on": true})),
        ("car.battery.heat", json!({"on": true})),
        ("car.finder.find", json!({})),
        ("car.finder.flash_lights", json!({})),
        ("car.windows.close", json!({})),
        ("car.windows.open", json!({})),
        ("car.trunk.open", json!({})),
        ("car.trunk.close", json!({})),
        ("car.start_charging", json!({})),
        (
            "car.save_charging_schedule",
            json!({"start_charge_time": "22:00", "end_charge_time": "06:00", "charge_way": "e"}),
        ),
    ] {
        ok(&client, method, with(args));
    }
    let stop = call(&client, "car.stop_charging", with(json!({}))).unwrap_err();
    assert_eq!(stop.class, "BydEndpointNotSupportedError");
    for (method, args, class) in [
        (
            "car.seat.heat",
            json!({"position": "boot", "level": 1}),
            "ValueError",
        ),
        (
            "car.seat.heat",
            json!({"position": "driver", "level": 7}),
            "ValueError",
        ),
        ("car.steering.heat", json!({}), "TypeError"),
        (
            "car.hvac.start",
            json!({"temperature": "warm"}),
            "TypeError",
        ),
    ] {
        assert_eq!(
            call(&client, method, with(args)).unwrap_err().class,
            class,
            "{method}"
        );
    }
    assert!(world.requests(REMOTE).len() >= 12);

    // The client's own commands, with their parameter models.
    for (method, args) in [
        ("unlock", json!({})),
        ("stop_climate", json!({})),
        ("flash_lights", json!({})),
        ("close_windows", json!({})),
        ("find_car", json!({})),
        ("open_trunk", json!({})),
        ("close_trunk", json!({})),
        (
            "start_climate",
            json!({"params": {"temperature": 21.0, "timeSpan": 2}}),
        ),
        (
            "schedule_climate",
            json!({"params": {"booking_id": 1, "booking_time": 1_700_000_000}}),
        ),
        ("set_seat_climate", json!({"params": {"main_heat": 2}})),
        ("set_battery_heat", json!({"params": {"on": true}})),
        ("toggle_smart_charging", json!({"enable": true})),
        ("start_charging", json!({"poll_attempts": 1})),
    ] {
        let mut args = args;
        args["vin"] = json!(VIN);
        if let Err(error) = call(&client, method, args) {
            // The fake server has no reply for a few endpoints and this car lacks a
            // few features; that the method is dispatched is the point.
            assert!(
                [
                    "BydApiError",
                    "BydRemoteControlError",
                    "BydEndpointNotSupportedError"
                ]
                .contains(&error.class.as_str()),
                "{method}: {error}"
            );
        }
    }
}

#[test]
fn standby_plans_and_status_push() {
    const LIST: &str = "/control/standby-vehicles/list";
    const SAVE: &str = "/control/standby-vehicles/save";
    const SET_DEFAULT: &str = "/control/standby-vehicles/setDefault";
    const DELETE: &str = "/control/standby-vehicles/delete";
    const SEND: &str = "/control/standby-vehicles/send";
    const SEND_RESULT: &str = "/control/standby-vehicles/send-result";

    let world = World::new();
    let preset = json!({
        "id": "101",
        "planName": "Preset",
        "planType": 1,
        "defaultPlan": 1,
        "enable": 1,
        "executeType": "2",
        "startTime": "",
        "weeklyDays": [],
        "controlParams": [
            {"commandType": "OPENAIR", "commandId": "1", "controlParamsMap": "{\"temperature\":21,\"timeSpan\":2}"}
        ]
    });
    let created = json!({
        "id": "202",
        "planName": "Morning",
        "planType": 2,
        "defaultPlan": 0,
        "enable": 1,
        "executeType": "1",
        "startTime": "",
        "weeklyDays": [],
        "controlParams": [
            {"commandType": "OPENAIR", "commandId": "2", "controlParamsMap": "{\"temperature\":21,\"timeSpan\":2}"}
        ]
    });
    world.reply(SAVE, json!({"data": {}}));
    world.reply(SET_DEFAULT, json!({"data": {}}));
    world.reply(DELETE, json!({"data": {}}));
    world.reply(SEND, json!({"data": {"requestSerial": "RUN1"}}));
    world.reply(
        SEND_RESULT,
        json!({"data": {"requestSerial": "RUN1", "res": 2, "message": "ok"}}),
    );
    testing(
        "server.script",
        json!({
            "server": world.server,
            "endpoint": LIST,
            "replies": [
                {"data": [preset.clone()]},
                {"data": [preset.clone()]},
                {"data": [preset.clone()]},
                {"data": [preset.clone(), created.clone()]},
                {"data": [preset.clone(), created.clone()]},
                {"data": [preset.clone()]},
            ],
        }),
    );

    let (client, _) = world.client(json!({}), json!({}));
    let list = ok(&client, "get_standby_plans", vin());
    assert_eq!(list["plans"].as_array().unwrap().len(), 1);
    let plan = list["plans"][0].clone();

    let saved = ok(
        &client,
        "save_standby_plan",
        json!({
            "vin": VIN,
            "plan": plan,
            "schedule": {
                "repeat": "daily",
                "at": "2026-10-24T08:00:00+02:00",
                "zone": "Europe/Madrid",
            },
        }),
    );
    assert_eq!(saved["plans"][0]["id"], "101");
    let save_req = &world.requests(SAVE)[0]["inner"];
    assert_eq!(save_req["executeType"], "1");
    assert!(save_req["executeTime"].as_i64().unwrap() > 0);

    let bad_zone = call(
        &client,
        "save_standby_plan",
        json!({
            "vin": VIN,
            "plan": plan,
            "schedule": {
                "repeat": "daily",
                "at": "2026-10-24T08:00:00+02:00",
                "zone": "Mars/Olympus_Mons",
            },
        }),
    )
    .unwrap_err();
    assert_eq!(bad_zone.class, "ZoneInfoNotFoundError");
    assert_eq!(bad_zone.bases, ["KeyError", "LookupError"]);

    let new_plan = ok(
        &client,
        "create_standby_plan",
        json!({
            "vin": VIN,
            "name": "Morning",
            "commands": plan["commands"],
            "schedule": {"repeat": "weekly", "start_time": "07:30", "weekly_days": [0, 1, 2, 3, 4]},
        }),
    );
    assert_eq!(new_plan["id"], "202");

    let defaulted = ok(
        &client,
        "set_default_standby_plan",
        json!({"vin": VIN, "plan_id": "202"}),
    );
    assert_eq!(defaulted["plans"].as_array().unwrap().len(), 2);

    let after_delete = ok(
        &client,
        "delete_standby_plan",
        json!({"vin": VIN, "plan_id": "202"}),
    );
    assert_eq!(after_delete["plans"].as_array().unwrap().len(), 1);

    let run = ok(
        &client,
        "run_standby_plan",
        json!({"vin": VIN, "plan_id": "101", "poll_attempts": 2, "poll_interval": 0.01}),
    );
    assert_eq!(run["res"], 2);
    assert_eq!(run["vin"], VIN);

    // car.handle_mqtt_status_push patches existing realtime, HVAC and charging sections.
    let car = ok(&client, "get_car", vin())["car"].clone();
    ok(&client, "car.update_hvac", json!({"car": car}));
    client
        .call_sync(
            "car.handle_mqtt_realtime",
            json!({"car": car, "data": {"elecPercent": 80, "time": 1_700_000_000}}),
        )
        .unwrap();
    client
        .call_sync(
            "car.handle_mqtt_charging",
            json!({"car": car, "data": {"elecPercent": 80, "chargingState": 0}}),
        )
        .unwrap();
    until("base sections seeded", || {
        let state = client.call_sync("car.state", json!({"car": car})).unwrap();
        state["realtime"]["elec_percent"] == 80.0
            && state["hvac"]["temp_in_car"] == 21.0
            && state["charging"]["soc"] == 80
    });
    client
        .call_sync(
            "car.handle_mqtt_status_push",
            json!({
                "car": car,
                "data": {"elecPercent": 77, "leftFrontDoor": 2},
                "hvac": {"acSwitch": 1, "tempInCar": 22},
                "charging": {"elecPercent": 77, "chargingState": 1},
            }),
        )
        .unwrap();
    until("status push merged", || {
        let state = client.call_sync("car.state", json!({"car": car})).unwrap();
        state["realtime"]["elec_percent"] == 77.0
            && state["hvac"]["temp_in_car"] == 22.0
            && state["charging"]["soc"] == 77
    });

    // An invalid optional section fails without merging anything.
    let bad = client
        .call_sync(
            "car.handle_mqtt_status_push",
            json!({
                "car": car,
                "data": {"elecPercent": 55},
                "hvac": {"tempInCar": "not-a-number"},
            }),
        )
        .unwrap_err();
    assert_eq!(bad.class, "ValidationError");
    let state = client.call_sync("car.state", json!({"car": car})).unwrap();
    assert_eq!(state["realtime"]["elec_percent"], 77.0);
}
