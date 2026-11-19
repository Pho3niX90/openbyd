//! The production client (`BydClient` on `ProductionEnv`) against the fake
//! BYD server and the TLS test broker, over real sockets.

#![cfg(feature = "testing")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openbyd_client::client::{
    BydClient, ClientOptions, PollSettings, ProductionEnv, ReloginPolicy, RequestOutcome,
    RequestRecord, SessionTokens,
};
use openbyd_client::http::{HttpEnv, HttpPool};
use openbyd_client::testing::broker::{TestBroker, runtime_options};
use openbyd_client::testing::{Account, FakeBydServer, Reply};
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::error::ErrorKind;
use openbyd_protocol::models::control::CommandLifecycleStatus;
use serde_json::{Value, json};

const VIN: &str = "TESTVIN0000000001";
const USER: &str = "user@example.com";
const PASSWORD: &str = "secret";
const WAIT: Duration = Duration::from_secs(10);

const BROKER_LOOKUP: &str = "/app/emqAuth/getEmqBrokerIp";
const VEHICLES: &str = "/app/account/getAllListByUserId";
const REALTIME_TRIGGER: &str = "/vehicleInfo/vehicle/vehicleRealTimeRequest";
const REALTIME_RESULT: &str = "/vehicleInfo/vehicle/vehicleRealTimeResult";
const GPS_TRIGGER: &str = "/control/getGpsInfo";
const HVAC: &str = "/control/getStatusNow";
const LATEST_CONFIG: &str = "/vehicle/vehicleswitch/getLatestConfig";
const VERIFY_PIN: &str = "/vehicle/vehicleswitch/verifyControlPassword";
const REMOTE_CONTROL: &str = "/control/remoteControl";
const REMOTE_CONTROL_RESULT: &str = "/control/remoteControlResult";
const LOGIN: &str = "/app/account/login";

struct World {
    server: FakeBydServer,
    broker: Arc<TestBroker>,
    pool: HttpPool,
}

impl World {
    async fn start(accounts: impl IntoIterator<Item = Account>) -> Self {
        let server = FakeBydServer::start(accounts).await.expect("server");
        let broker = TestBroker::start().await;
        server.reply(
            BROKER_LOOKUP,
            Reply::Data(json!({"emqBorker": broker.address()})),
        );
        server.reply(
            VEHICLES,
            Reply::Data(json!([{"vin": VIN, "energyType": "0", "modelName": "SEAL"}])),
        );
        server.reply(HVAC, Reply::Data(json!({"acSwitch": 1, "tempInCar": 21})));
        Self {
            server,
            broker,
            pool: HttpPool::new(),
        }
    }

    fn config(&self, username: &str, mqtt: bool) -> BydConfig {
        let mut config = BydConfig::new(username, PASSWORD);
        config.base_url = self.server.url();
        config.mqtt_enabled = mqtt;
        config.mqtt_timeout = 5.0;
        config
    }

    fn client(&self, config: BydConfig, options: ClientOptions) -> BydClient<ProductionEnv> {
        let env = ProductionEnv::with_mqtt_options(
            HttpEnv::for_config(&self.pool, &config),
            runtime_options(),
        );
        BydClient::new(config, env, options)
    }

    fn logins(&self) -> usize {
        self.server.requests_to(LOGIN).len()
    }

    /// Waits until the server has seen `count` requests to `endpoint`.
    async fn until_requests(&self, endpoint: &str, count: usize) {
        let deadline = Instant::now() + WAIT;
        while self.server.requests_to(endpoint).len() < count {
            assert!(Instant::now() < deadline, "no request to {endpoint}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Publishes a push for `client`'s account, sealed with its session key.
    fn push(&self, client: &BydClient<ProductionEnv>, body: &Value) {
        let session = client.session().expect("logged in");
        let topic = format!("oversea/res/{}", session.user_id());
        assert_eq!(
            self.broker
                .publish_sealed(&topic, body, session.content_key()),
            1
        );
    }

    async fn until_subscribed(&self, client: &BydClient<ProductionEnv>) {
        let session = client.session().expect("logged in");
        self.broker
            .until_subscribed(&format!("oversea/res/{}", session.user_id()))
            .await;
    }
}

fn quick() -> PollSettings {
    PollSettings {
        poll_attempts: 5,
        poll_interval: 0.05,
        mqtt_timeout: Some(0.0),
        signal_retries: 2,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_and_a_full_poll_cycle() {
    let world = World::start([Account::new(USER, PASSWORD)]).await;
    world.server.script(
        REALTIME_TRIGGER,
        [Reply::Data(json!({"requestSerial": "R1"}))],
    );
    world.server.reply(
        REALTIME_RESULT,
        Reply::Data(
            json!({"requestSerial": "R1", "elecPercent": 80, "onlineState": 1, "time": 1700000000}),
        ),
    );
    world.server.reply(
        GPS_TRIGGER,
        Reply::Data(json!({"latitude": 52.0, "longitude": 4.0})),
    );
    let client = world.client(world.config(USER, true), ClientOptions::default());

    client.login().await.expect("login");
    world.until_subscribed(&client).await;
    assert!(client.mqtt_is_running());

    let vehicles = client.get_vehicles().await.expect("vehicles");
    assert_eq!(vehicles[0].vin, VIN);
    let realtime = client
        .get_vehicle_realtime(VIN, quick())
        .await
        .expect("realtime");
    assert_eq!(realtime.elec_percent, Some(80.0));
    let gps = client.get_gps_info(VIN, quick()).await.expect("gps");
    assert_eq!(gps.latitude, Some(52.0));
    client.get_hvac_status(VIN).await.expect("hvac");
    assert_eq!(world.logins(), 1);
    assert_eq!(world.server.requests_to(REALTIME_RESULT).len(), 1);

    client.async_close();
    assert!(!client.mqtt_is_running());
    let error = client.get_hvac_status(VIN).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydError);
    client.async_start();
    client.get_hvac_status(VIN).await.expect("reopened");
    world.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn realtime_is_answered_by_a_push() {
    let world = World::start([Account::new(USER, PASSWORD)]).await;
    world.server.reply(
        REALTIME_TRIGGER,
        Reply::Data(json!({"requestSerial": "R7"})),
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut options = ClientOptions::default();
    let record = Arc::clone(&seen);
    options.on_vehicle_info = Some(Arc::new(move |vin, data| {
        record
            .lock()
            .unwrap()
            .push((vin.to_owned(), data.elec_percent));
    }));
    let client = world.client(world.config(USER, true), options);
    client.login().await.expect("login");
    world.until_subscribed(&client).await;
    client
        .seed_vehicle_metadata(VIN, openbyd_protocol::models::vehicle::EnergyType::Ev, None)
        .unwrap();

    let reader = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .get_vehicle_realtime(VIN, PollSettings::default())
                .await
        })
    };
    world.until_requests(REALTIME_TRIGGER, 1).await;
    // Give the waiter a moment to register after the trigger's reply.
    tokio::time::sleep(Duration::from_millis(50)).await;
    world.push(
        &client,
        &json!({"event": "vehicleInfo", "vin": VIN, "data": {
            "uuid": "R7", "respondData": {"elecPercent": 55, "onlineState": 1, "time": 1700000000},
        }}),
    );
    let realtime = tokio::time::timeout(WAIT, reader)
        .await
        .expect("in time")
        .expect("joined")
        .expect("realtime");
    assert_eq!(realtime.elec_percent, Some(55.0));
    assert!(world.server.requests_to(REALTIME_RESULT).is_empty());
    assert_eq!(seen.lock().unwrap().clone(), [(VIN.to_owned(), Some(55.0))]);
    assert_eq!(world.server.requests_to(VEHICLES).len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_is_acknowledged_over_mqtt() {
    let world = World::start([Account::new(USER, PASSWORD)]).await;
    world
        .server
        .reply(VERIFY_PIN, Reply::Data(json!({"ok": true})));
    world.server.reply(
        LATEST_CONFIG,
        Reply::Data(json!({VIN: {"cfFixedList": [{"functionNo": "1005"}]}})),
    );
    world
        .server
        .reply(REMOTE_CONTROL, Reply::Data(json!({"requestSerial": "C1"})));
    let lifecycle = Arc::new(Mutex::new(Vec::new()));
    let acks = Arc::new(AtomicUsize::new(0));
    let mut options = ClientOptions::default();
    let record = Arc::clone(&lifecycle);
    options.on_command_lifecycle = Some(Arc::new(move |event| {
        record.lock().unwrap().push(event.status);
    }));
    let counted = Arc::clone(&acks);
    options.on_command_ack = Some(Arc::new(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
    }));
    let client = world.client(world.config(USER, true), options);
    client.login().await.expect("login");
    world.until_subscribed(&client).await;

    let error = client.lock(VIN, Some("123456")).await.unwrap_err();
    assert_eq!(error.code(), "commands_disabled");
    client
        .verify_command_access(VIN, Some("123456"))
        .await
        .expect("PIN accepted");
    assert!(client.commands_enabled());

    let locker = {
        let client = client.clone();
        tokio::spawn(async move { client.lock(VIN, Some("123456")).await })
    };
    world.until_requests(REMOTE_CONTROL, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    world.push(
        &client,
        &json!({"event": "remoteControl", "vin": VIN, "data": {"uuid": "C1", "respondData": {"res": 2}}}),
    );
    let result = tokio::time::timeout(WAIT, locker)
        .await
        .expect("in time")
        .expect("joined")
        .expect("locked");
    assert!(result.success);
    assert!(world.server.requests_to(REMOTE_CONTROL_RESULT).is_empty());
    assert_eq!(
        lifecycle.lock().unwrap().clone(),
        [
            CommandLifecycleStatus::Registered,
            CommandLifecycleStatus::Matched
        ]
    );
    assert_eq!(acks.load(Ordering::SeqCst), 1);
    let diagnostics = client.get_command_ack_diagnostics();
    assert_eq!((diagnostics.pending, diagnostics.matched), (0, 1));
    let pin = world.server.requests_to(REMOTE_CONTROL)[0]
        .inner_field("commandPwd")
        .cloned();
    assert_eq!(
        pin,
        Some(json!(openbyd_protocol::hashing::md5_hex("123456")))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_session_is_renewed_transparently() {
    let world = World::start([Account::new(USER, PASSWORD)]).await;
    let client = world.client(world.config(USER, false), ClientOptions::default());
    client.login().await.expect("login");
    let first = client.session().unwrap();
    world.server.expire_sessions(USER);
    client.get_hvac_status(VIN).await.expect("renewed");
    assert_eq!(world.logins(), 2);
    assert_ne!(client.session().unwrap().sign_token(), first.sign_token());

    // An expired TTL logs in before the call.
    let mut config = world.config(USER, false);
    config.session_ttl = 0.05;
    let short = world.client(config, ClientOptions::default());
    short.login().await.expect("login");
    tokio::time::sleep(Duration::from_millis(80)).await;
    short.get_hvac_status(VIN).await.expect("renewed");
    assert_eq!(world.logins(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_client_never_sends_the_password() {
    let world = World::start([Account::new(USER, PASSWORD)]).await;
    let first = world.client(world.config(USER, false), ClientOptions::default());
    first.login().await.expect("login");
    first.get_vehicles().await.expect("vehicles");
    let tokens = first.export_session().expect("a session");
    assert!(tokens.ttl_remaining.is_some_and(|left| left > 43_000.0));
    assert_eq!(tokens.guest_vins, HashMap::from([(VIN.to_owned(), false)]));

    let mut options = ClientOptions::default();
    options.relogin = ReloginPolicy::Never;
    let mut config = world.config(USER, false);
    config.password = "not the password".to_owned();
    let restored = world.client(config, options);
    restored.import_session(tokens.clone()).await;
    restored.get_hvac_status(VIN).await.expect("restored");
    let exported = restored.export_session().unwrap();
    assert_eq!(exported.user_id, tokens.user_id);
    assert_eq!(exported.encry_token, tokens.encry_token);
    assert_eq!(exported.guest_vins, tokens.guest_vins);
    assert_eq!(world.logins(), 1);

    world.server.expire_sessions(USER);
    let error = restored.get_hvac_status(VIN).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydSessionExpiredError);
    assert_eq!(error.code(), "relogin_disabled");
    assert_eq!(world.logins(), 1);
    assert!(restored.export_session().is_none());

    // A handler may hand over a session refreshed elsewhere.
    let refreshed = world.client(world.config(USER, false), ClientOptions::default());
    refreshed.login().await.expect("login");
    let fresh = refreshed.export_session().unwrap();
    let handed = Arc::new(Mutex::new(Some(fresh)));
    let mut options = ClientOptions::default();
    options.relogin = ReloginPolicy::Handler(Arc::new(move || {
        let tokens = handed.lock().unwrap().take();
        Box::pin(async move {
            tokens.ok_or_else(|| {
                openbyd_protocol::error::Error::new(ErrorKind::BydError, "no tokens left")
            })
        })
    }));
    let handled = world.client(world.config(USER, false), options);
    handled.import_session(tokens).await;
    handled
        .get_hvac_status(VIN)
        .await
        .expect("handed a session");
    assert_eq!(world.logins(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_request_observer_sees_every_request() {
    let world = World::start([Account::new(USER, PASSWORD)]).await;
    world.server.reply(GPS_TRIGGER, Reply::code("1008"));
    let records: Arc<Mutex<Vec<RequestRecord>>> = Arc::default();
    let observed = Arc::clone(&records);
    let mut options = ClientOptions::default();
    options.request_observer = Some(Arc::new(move |record: &RequestRecord| {
        observed.lock().unwrap().push(record.clone());
    }));
    let client = world.client(world.config(USER, false), options);
    client.login().await.unwrap();
    client.get_hvac_status(VIN).await.unwrap();
    let error = client.get_gps_info(VIN, quick()).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydServiceBusyError);
    world.server.shutdown().await;
    let error = client.get_hvac_status(VIN).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydTransportError);

    let records = records.lock().unwrap().clone();
    let summary: Vec<(&str, RequestOutcome)> = records
        .iter()
        .map(|record| (record.endpoint.as_str(), record.outcome.clone()))
        .collect();
    let code = |code: &str| RequestOutcome::Reply {
        code: Some(code.to_owned()),
    };
    assert_eq!(
        summary,
        [
            (LOGIN, code("0")),
            (HVAC, code("0")),
            (GPS_TRIGGER, code("1008")),
            (HVAC, RequestOutcome::Transport { status: None }),
        ]
    );
    assert!(records.iter().all(|record| record.duration < WAIT));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_hundred_clients_poll_concurrently_on_one_pool() {
    const CLIENTS: usize = 200;
    let accounts =
        (0..CLIENTS).map(|index| Account::new(&format!("user{index}@example.com"), PASSWORD));
    let world = World::start(accounts).await;
    world.server.reply(
        REALTIME_TRIGGER,
        Reply::Data(json!({"requestSerial": "R1"})),
    );
    world.server.reply(
        REALTIME_RESULT,
        Reply::Data(
            json!({"requestSerial": "R1", "elecPercent": 42, "onlineState": 1, "time": 1700000000}),
        ),
    );
    let clients: Vec<_> = (0..CLIENTS)
        .map(|index| {
            world.client(
                world.config(&format!("user{index}@example.com"), true),
                ClientOptions::default(),
            )
        })
        .collect();
    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for client in &clients {
        let client = client.clone();
        tasks.spawn(async move {
            client.login().await?;
            client.get_vehicles().await?;
            let realtime = client.get_vehicle_realtime(VIN, quick()).await?;
            client.get_hvac_status(VIN).await?;
            Ok::<_, openbyd_protocol::error::Error>(realtime.elec_percent)
        });
    }
    let mut done = 0;
    while let Some(joined) = tasks.join_next().await {
        assert_eq!(joined.expect("task").expect("poll cycle"), Some(42.0));
        done += 1;
    }
    assert_eq!(done, CLIENTS);
    assert!(started.elapsed() < Duration::from_secs(60));
    assert_eq!(world.logins(), CLIENTS);
    let deadline = Instant::now() + WAIT;
    while clients.iter().any(|client| !client.mqtt_is_running()) || world.broker.open() < CLIENTS {
        assert!(
            Instant::now() < deadline,
            "MQTT did not start for every client"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for client in &clients {
        client.async_close();
    }
    let deadline = Instant::now() + WAIT;
    while world.broker.open() > 0 {
        assert!(
            Instant::now() < deadline,
            "{} MQTT connections left",
            world.broker.open()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connack_refusals_lead_to_a_relogin() {
    let world = World::start([Account::new(USER, PASSWORD)]).await;
    let client = world.client(world.config(USER, true), ClientOptions::default());
    client.login().await.expect("login");
    world.until_subscribed(&client).await;
    // The broker drops the connection and refuses the next three CONNECTs:
    // the third refusal makes the client log in again and restart MQTT.
    world.broker.refuse_next(3);
    world.broker.drop_all();
    let deadline = Instant::now() + WAIT;
    while world.logins() < 2 {
        assert!(Instant::now() < deadline, "no re-login after refusals");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let deadline = Instant::now() + WAIT;
    while !client.mqtt_is_running() {
        assert!(Instant::now() < deadline, "MQTT did not restart");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn session_tokens_compare_by_value() {
    let tokens = SessionTokens {
        user_id: "1".into(),
        sign_token: "s".into(),
        encry_token: "e".into(),
        ttl_remaining: None,
        guest_vins: HashMap::new(),
    };
    assert_eq!(tokens.clone(), tokens);
}

fn send<F: std::future::Future + Send>(_: F) {}

/// Every method's future can move between threads (only compiled, never run).
#[allow(dead_code)]
fn every_method_is_send(client: &BydClient<ProductionEnv>) {
    use openbyd_client::client::ChargePollSettings;
    use openbyd_protocol::models::control::{BatteryHeatParams, RemoteCommand};
    use openbyd_protocol::models::message_center::NotificationCategory;
    let vin = VIN;
    send(client.login());
    send(client.ensure_session());
    send(client.get_vehicles());
    send(client.get_latest_configs(&[]));
    send(client.get_latest_config(vin));
    send(client.get_vehicle_capabilities(vin, false));
    send(client.get_vehicle_realtime(vin, PollSettings::default()));
    send(client.get_gps_info(vin, PollSettings::default()));
    send(client.get_hvac_status(vin));
    send(client.get_charging_status(vin));
    send(client.get_charging_schedule(vin));
    send(client.get_charging_homepage(vin));
    send(client.get_ac_current_limit(vin, None));
    send(client.get_energy_consumption(vin));
    send(client.get_push_state(vin));
    send(client.set_push_state(vin, true, 701));
    send(client.get_notifications(NotificationCategory::Vehicle));
    send(client.get_all_notifications());
    send(client.get_booking_list(vin));
    send(client.rename_vehicle(vin, "x"));
    send(client.verify_command_access(vin, None));
    send(client.lock(vin, None));
    send(client.remote_control(vin, RemoteCommand::Lock, None, None, 1, 0.0));
    send(client.set_battery_heat(vin, &BatteryHeatParams::new(true), None));
    send(client.start_charging(vin, ChargePollSettings::default()));
    send(client.save_charging_schedule(
        vin,
        "22:00",
        "full",
        "e",
        true,
        ChargePollSettings::default(),
    ));
    send(client.toggle_smart_charging(vin, true));
    send(client.import_session(SessionTokens {
        user_id: String::new(),
        sign_token: String::new(),
        encry_token: String::new(),
        ttl_remaining: None,
        guest_vins: HashMap::new(),
    }));
    send(client.start_mqtt());
}

#[test]
fn clients_are_send_sync_and_clone() {
    fn check<T: Send + Sync + Clone + 'static>() {}
    check::<BydClient<ProductionEnv>>();
}
