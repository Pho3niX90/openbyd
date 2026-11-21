//! The replay server against openbyd's own client, over real sockets, on a
//! manual replay clock: what a client reads back at each moment, the errors
//! and session expiries at theirs, and pushes over MQTT.

#![cfg(feature = "testing")]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openbyd_client::client::{BydClient, ClientOptions, PollSettings, ProductionEnv};
use openbyd_client::http::{HttpEnv, HttpPool};
use openbyd_client::testing::broker::runtime_options;
use openbyd_client::testing::codes;
use openbyd_client::testing::replay::templates::position;
use openbyd_client::testing::replay::{
    CarState, Energy, Epoch, ReplayOptions, ReplayServer, Scenario,
};
use openbyd_protocol::coerce::IntEnum;
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::error::ErrorKind;
use openbyd_protocol::models::realtime::{OnlineState, VehicleRealtimeData};
use serde_json::{Map, Value, json};

const WAIT: Duration = Duration::from_secs(10);
const EV: &str = "TESTVIN0000000001";
const HYBRID: &str = "TESTVIN0000000002";
const OTHER: &str = "TESTVIN0000000003";
const START: &str = "2026-01-05T00:00:00Z";
const START_UNIX: i64 = 1_767_571_200;

/// A `state` line; `fields` overrides the keys of a parked, online car.
fn state(vin: &str, t: u32, fields: Value) -> String {
    let mut line = json!({
        "type": "state", "vin": vin, "t": t, "online": true, "soc": 71.0,
        "odometer_km": 20431.4, "speed_kmh": 0.0, "vehicle_on": false, "plugged": false,
        "charging": false, "connect_state": 0, "charge_power_kw": null,
        "minutes_to_full": null, "hvac_on": false, "fuel_percent": null, "gps": true,
    });
    for (key, value) in fields.as_object().expect("fields") {
        line[key] = value.clone();
    }
    line.to_string()
}

fn line(value: Value) -> String {
    value.to_string()
}

/// Two accounts: acct-0001 with an EV and a hybrid, acct-0002 with an EV.
fn scenario(timeline: &[String]) -> Scenario {
    let mut text = [
        line(json!({"type": "scenario", "version": 1, "source": "synthetic", "start": START,
            "duration_s": 10_000, "accounts": 2, "cars": 3, "seed": "test", "stats": {}})),
        line(json!({"type": "account", "account": "acct-0001", "cars": [EV, HYBRID]})),
        line(json!({"type": "account", "account": "acct-0002", "cars": [OTHER]})),
        line(json!({"type": "car", "vin": EV, "account": "acct-0001", "model": "seal", "energy": "ev"})),
        line(json!({"type": "car", "vin": HYBRID, "account": "acct-0001", "model": "seal-6-dm-i", "energy": "hybrid"})),
        line(json!({"type": "car", "vin": OTHER, "account": "acct-0002", "model": "unknown", "energy": "ev"})),
    ]
    .join("\n");
    for entry in timeline {
        text.push('\n');
        text.push_str(entry);
    }
    Scenario::parse(&text).expect("valid scenario")
}

async fn replay(scenario: Scenario) -> ReplayServer {
    let options = ReplayOptions {
        manual: true,
        record: true,
        ..ReplayOptions::default()
    };
    ReplayServer::start(scenario, options)
        .await
        .expect("replay")
}

fn quick() -> PollSettings {
    PollSettings {
        poll_attempts: 2,
        poll_interval: 0.0,
        mqtt_timeout: Some(0.0),
        signal_retries: 2,
    }
}

type Pushes = Arc<Mutex<Vec<(String, String, Map<String, Value>)>>>;

fn client(
    replay: &ReplayServer,
    pool: &HttpPool,
    account: &str,
    mqtt: Option<&Pushes>,
) -> BydClient<ProductionEnv> {
    let mut config = BydConfig::new(&format!("{account}@example.com"), "secret");
    config.base_url = replay.url();
    config.mqtt_enabled = mqtt.is_some();
    config.mqtt_timeout = 0.0;
    let mut options = ClientOptions::default();
    if let Some(pushes) = mqtt {
        let pushes = pushes.clone();
        options.on_mqtt_event = Some(Arc::new(
            move |event: &str, vin: &str, data: &Map<String, Value>| {
                pushes
                    .lock()
                    .unwrap()
                    .push((event.to_owned(), vin.to_owned(), data.clone()));
            },
        ));
    }
    let env =
        ProductionEnv::with_mqtt_options(HttpEnv::for_config(pool, &config), runtime_options());
    BydClient::new(config, env, options)
}

/// What the client reads of a car, compared with the state it should show.
async fn check_car(
    client: &BydClient<ProductionEnv>,
    vin: &str,
    index: usize,
    energy: Energy,
    want: &CarState,
    t: u32,
) {
    let at = format!("{vin} at t={t}");
    let realtime = client.get_vehicle_realtime(vin, quick()).await.expect(&at);
    check_realtime(&realtime, want, energy, &at);
    let stamp = START_UNIX + i64::from(t);
    if want.online {
        assert_eq!(realtime.raw["time"], json!(stamp), "{at}");
    }

    let charging = client.get_charging_status(vin).await.expect(&at);
    assert_eq!(
        charging.soc,
        want.soc.map(|soc| soc.round() as i64),
        "{at}: charging soc"
    );
    assert_eq!(charging.is_charging(), want.charging, "{at}: charging");
    assert_eq!(
        charging.connect_state, want.connect_state,
        "{at}: connect_state"
    );
    assert_eq!(charging.is_connected(), want.plugged, "{at}: plugged");
    assert_eq!(
        charging.time_to_full_minutes(),
        want.minutes_to_full,
        "{at}: minutes_to_full"
    );

    let hvac = client.get_hvac_status(vin).await.expect(&at);
    if want.online {
        assert_eq!(hvac.is_ac_on(), want.hvac_on == Some(true), "{at}: hvac");
    }

    let gps = client.get_gps_info(vin, quick()).await.expect(&at);
    let (lat, lon) = if want.gps {
        let (lat, lon, _) = position(index, want.odometer_km);
        (lat, lon)
    } else {
        (0.0, 0.0)
    };
    assert_eq!(
        (gps.latitude, gps.longitude),
        (Some(lat), Some(lon)),
        "{at}: gps"
    );

    let energy_use = client.get_energy_consumption(vin).await.expect(&at);
    let mileage = energy_use
        .cumulative_energy_consumption
        .as_ref()
        .and_then(|cumulative| cumulative.total_mileage);
    assert_eq!(
        mileage,
        want.odometer_km.filter(|km| *km > 0.0),
        "{at}: energy odometer"
    );
}

fn check_realtime(realtime: &VehicleRealtimeData, want: &CarState, energy: Energy, at: &str) {
    if !want.online {
        assert_eq!(realtime.online_state, OnlineState::Offline, "{at}: asleep");
        return;
    }
    assert_eq!(realtime.online_state, OnlineState::Online, "{at}: online");
    assert_eq!(realtime.elec_percent, want.soc, "{at}: soc");
    assert_eq!(realtime.total_mileage, want.odometer_km, "{at}: odometer");
    assert_eq!(
        realtime.raw["totalMileageV2"],
        json!(want.odometer_km),
        "{at}: odometer v2"
    );
    assert_eq!(realtime.speed, want.speed_kmh, "{at}: speed");
    assert_eq!(realtime.is_charging(), want.charging, "{at}: charging");
    assert_eq!(
        realtime.charge_power, want.charge_power_kw,
        "{at}: charge power"
    );
    let gear = realtime.power_gear.map(|gear| gear.value());
    let expected_gear = match want.vehicle_on {
        Some(true) => 3,
        Some(false) => 1,
        None => -1,
    };
    assert_eq!(gear, Some(expected_gear), "{at}: vehicle_on");
    let fuel = match energy {
        Energy::Hybrid => want.fuel_percent,
        Energy::Ev => None,
    };
    assert_eq!(realtime.oil_percent, fuel, "{at}: fuel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clients_read_each_cars_state_at_each_moment() {
    let scenario = scenario(&[
        state(EV, 100, json!({})),
        state(
            HYBRID,
            100,
            json!({"fuel_percent": 95.0, "soc": 54.0, "odometer_km": 168.0}),
        ),
        state(
            EV,
            200,
            json!({"speed_kmh": 62.0, "vehicle_on": true, "odometer_km": 20440.0, "hvac_on": true}),
        ),
        state(
            HYBRID,
            250,
            json!({"plugged": true, "charging": true, "connect_state": 1,
            "charge_power_kw": 6.3, "minutes_to_full": 95, "soc": 60.5, "fuel_percent": 0.0, "gps": false}),
        ),
        state(EV, 300, json!({"online": false})),
        state(
            EV,
            400,
            json!({"soc": null, "odometer_km": null, "speed_kmh": null, "vehicle_on": null,
            "connect_state": null, "hvac_on": null}),
        ),
    ]);
    let expected = scenario.clone();
    let replay = replay(scenario).await;
    let pool = HttpPool::new();
    let owner = client(&replay, &pool, "acct-0001", None);
    owner.login().await.expect("login");
    let vehicles = owner.get_vehicles().await.expect("vehicles");
    assert_eq!(vehicles.len(), 2);
    assert_eq!(vehicles[1].model_name, "SEAL 6 DM I");
    assert_eq!(vehicles[1].energy_type.value(), 2);
    for t in [0, 99, 100, 150, 199, 200, 250, 299, 300, 399, 400, 10_000] {
        replay.set_time(f64::from(t)).unwrap();
        for (index, vin, energy) in [(0, EV, Energy::Ev), (1, HYBRID, Energy::Hybrid)] {
            let want = *expected.lookup(index, t).state;
            check_car(&owner, vin, index, energy, &want, t).await;
        }
    }
    // Before its first state a car is asleep with a zero odometer.
    assert_eq!(expected.lookup(0, 99).state, &CarState::UNSEEN);
    // A car of another account is refused.
    let refused = owner.get_charging_status(OTHER).await.unwrap_err();
    assert_eq!(refused.code(), "1001");
    // Past the end the clock holds.
    replay.set_time(20_000.0).unwrap();
    assert_eq!(replay.clock().now(), 10_000.0);
    assert_eq!(replay.clock_json()["finished"], json!(true));
    replay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn errors_answer_at_their_moments() {
    let scenario = scenario(&[
        state(EV, 0, json!({})),
        state(HYBRID, 0, json!({"fuel_percent": 50.0})),
        line(json!({"type": "error", "vin": EV, "t": 100, "code": "1008"})),
        state(EV, 110, json!({"soc": 70.0})),
        line(json!({"type": "error", "vin": EV, "t": 200, "code": "6002"})),
        state(EV, 210, json!({"soc": 69.0})),
        line(json!({"type": "error", "vin": EV, "t": 300, "code": "1001"})),
        state(EV, 310, json!({"soc": 68.0})),
        line(json!({"type": "error", "vin": EV, "t": 400, "code": "5005"})),
        state(EV, 410, json!({"soc": 67.0})),
    ]);
    let replay = replay(scenario).await;
    let pool = HttpPool::new();
    let owner = client(&replay, &pool, "acct-0001", None);
    owner.login().await.expect("login");
    owner.get_vehicles().await.expect("vehicles");
    async fn realtime(
        owner: &BydClient<ProductionEnv>,
    ) -> Result<VehicleRealtimeData, openbyd_protocol::error::Error> {
        owner.get_vehicle_realtime(EV, quick()).await
    }

    // 1008: every read of the car, not the account's reads or other cars.
    replay.set_time(99.0).unwrap();
    assert_eq!(realtime(&owner).await.unwrap().elec_percent, Some(71.0));
    replay.set_time(100.0).unwrap();
    let busy = realtime(&owner).await.unwrap_err();
    assert_eq!(
        (busy.kind(), busy.code()),
        (ErrorKind::BydServiceBusyError, "1008")
    );
    let busy = owner.get_charging_status(EV).await.unwrap_err();
    assert_eq!(busy.code(), "1008");
    assert_eq!(owner.get_hvac_status(EV).await.unwrap_err().code(), "1008");
    assert_eq!(
        owner.get_gps_info(EV, quick()).await.unwrap_err().code(),
        "1008"
    );
    owner
        .get_vehicles()
        .await
        .expect("account reads still work");
    owner
        .get_vehicle_realtime(HYBRID, quick())
        .await
        .expect("other cars still work");
    replay.set_time(109.0).unwrap();
    assert!(realtime(&owner).await.is_err());
    replay.set_time(110.0).unwrap();
    assert_eq!(realtime(&owner).await.unwrap().elec_percent, Some(70.0));

    // 6002: the realtime read only; the others answer the previous state.
    replay.set_time(199.0).unwrap();
    realtime(&owner).await.unwrap();
    replay.set_time(200.0).unwrap();
    let unreachable = realtime(&owner).await.unwrap_err();
    assert_eq!(
        (unreachable.kind(), unreachable.code()),
        (ErrorKind::BydDataUnavailableError, "6002")
    );
    assert_eq!(owner.get_charging_status(EV).await.unwrap().soc, Some(70));
    replay.set_time(210.0).unwrap();
    assert_eq!(realtime(&owner).await.unwrap().elec_percent, Some(69.0));

    // 1001: not supported.
    replay.set_time(299.0).unwrap();
    realtime(&owner).await.unwrap();
    replay.set_time(300.0).unwrap();
    let unsupported = realtime(&owner).await.unwrap_err();
    assert_eq!(
        (unsupported.kind(), unsupported.code()),
        (ErrorKind::BydEndpointNotSupportedError, "1001")
    );
    replay.set_time(310.0).unwrap();
    assert_eq!(realtime(&owner).await.unwrap().elec_percent, Some(68.0));

    // Any other code answers the realtime read.
    replay.set_time(400.0).unwrap();
    let other = realtime(&owner).await.unwrap_err();
    assert_eq!(
        (other.kind(), other.code()),
        (ErrorKind::BydApiError, "5005")
    );
    replay.set_time(410.0).unwrap();
    assert_eq!(realtime(&owner).await.unwrap().elec_percent, Some(67.0));

    let stats = replay.stats();
    assert_eq!(stats.replay.errors_by_code["1008"], 5);
    assert_eq!(stats.replay.errors_by_code["6002"], 1);
    // pyBYD sends a 1001 a second time with the other userType once.
    assert_eq!(stats.replay.errors_by_code["1001"], 1);
    assert_eq!(stats.server.by_code["8003"], 1);
    assert_eq!(stats.replay.errors_by_code["5005"], 1);
    replay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_1005_expires_the_sessions_once() {
    let scenario = scenario(&[
        state(EV, 0, json!({"soc": 80.0})),
        state(OTHER, 0, json!({"soc": 30.0})),
        line(json!({"type": "error", "vin": EV, "t": 500, "code": "1005"})),
        state(EV, 600, json!({"soc": 79.0})),
    ]);
    let replay = replay(scenario).await;
    let pool = HttpPool::new();
    let owner = client(&replay, &pool, "acct-0001", None);
    let other = client(&replay, &pool, "acct-0002", None);
    owner.login().await.unwrap();
    other.login().await.unwrap();
    let logins = || replay.stats().server.logins;
    assert_eq!(logins(), 2);
    replay.set_time(499.0).unwrap();
    assert_eq!(owner.get_charging_status(EV).await.unwrap().soc, Some(80));
    assert_eq!(logins(), 2, "no expiry a second early");
    replay.set_time(500.0).unwrap();
    assert_eq!(replay.stats().replay.sessions_expired, 1);
    // The next request is refused as expired; the client logs in again and reads the same state.
    assert_eq!(owner.get_charging_status(EV).await.unwrap().soc, Some(80));
    assert_eq!(logins(), 3);
    assert_eq!(replay.stats().server.by_code[codes::SESSION_EXPIRED], 1);
    // Only once, and only that account.
    replay.set_time(550.0).unwrap();
    assert_eq!(owner.get_charging_status(EV).await.unwrap().soc, Some(80));
    assert_eq!(
        other.get_charging_status(OTHER).await.unwrap().soc,
        Some(30)
    );
    assert_eq!(logins(), 3);
    replay.set_time(600.0).unwrap();
    assert_eq!(
        owner
            .get_vehicle_realtime(EV, quick())
            .await
            .unwrap()
            .elec_percent,
        Some(79.0)
    );
    assert_eq!(replay.stats().replay.sessions_expired, 1);
    replay.shutdown().await;
}

async fn until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushes_arrive_at_their_moments() {
    let scenario = scenario(&[
        state(EV, 0, json!({"soc": 80.0})),
        line(
            json!({"type": "push", "vin": EV, "t": 100, "event": "vehicleInfo", "soc": 64.0, "speed_kmh": 30.0}),
        ),
        line(
            json!({"type": "push", "vin": EV, "t": 200, "event": "smartCharge", "plugged": true, "charging": true, "connect_state": 1}),
        ),
        line(json!({"type": "push", "vin": EV, "t": 300, "event": "remoteControl"})),
        line(json!({"type": "push", "vin": EV, "t": 400, "event": "energyConsumption"})),
        line(json!({"type": "push", "vin": EV, "t": 500, "event": "vehicleInfo", "soc": 63.0})),
        line(json!({"type": "push", "vin": EV, "t": 600, "event": "vehicleInfo", "soc": 62.0})),
        line(json!({"type": "push", "vin": OTHER, "t": 600, "event": "vehicleInfo", "soc": 10.0})),
    ]);
    let replay = replay(scenario).await;
    let pool = HttpPool::new();
    let pushes: Pushes = Arc::default();
    let owner = client(&replay, &pool, "acct-0001", Some(&pushes));
    owner.login().await.unwrap();
    let topic = format!("oversea/res/{}", owner.session().unwrap().user_id());
    replay.broker().until_subscribed(&topic).await;
    let count = || pushes.lock().unwrap().len();
    let stats = || replay.stats().replay;

    replay.set_time(99.0).unwrap();
    assert_eq!(stats().pushes_sent, 0, "not a second early");
    replay.set_time(100.0).unwrap();
    assert_eq!(stats().pushes_sent, 1);
    until("the vehicleInfo push", || count() == 1).await;
    {
        let seen = pushes.lock().unwrap();
        let (event, vin, data) = &seen[0];
        assert_eq!((event.as_str(), vin.as_str()), ("vehicleInfo", EV));
        assert_eq!(data["elecPercent"], json!(64.0));
        assert_eq!(data["speed"], json!(30.0));
        assert_eq!(
            data["totalMileage"],
            json!(20431.4),
            "the rest from the state"
        );
        assert_eq!(data["time"], json!(START_UNIX + 100));
    }
    replay.set_time(250.0).unwrap();
    until("the smartCharge push", || count() == 2).await;
    {
        let seen = pushes.lock().unwrap();
        let (event, _, data) = &seen[1];
        assert_eq!(event, "smartCharge");
        assert_eq!(
            (data["chargingState"].clone(), data["connectState"].clone()),
            (json!(1), json!(1))
        );
        assert_eq!(data["elecPercent"], json!(80));
    }
    replay.set_time(400.0).unwrap();
    until("two more pushes", || count() == 4).await;
    {
        let seen = pushes.lock().unwrap();
        assert_eq!(seen[2].0, "remoteControl");
        assert_eq!(seen[2].2["res"], json!(2));
        assert_eq!(seen[3].0, "energyConsumption");
    }

    // Disconnected, a push is missed; connected again, the next one arrives.
    owner.stop_mqtt();
    until("the disconnection", || {
        replay.broker().subscribers(&topic) == 0
    })
    .await;
    replay.set_time(500.0).unwrap();
    assert_eq!(stats().pushes_missed, 1);
    owner.start_mqtt().await;
    replay.broker().until_subscribed(&topic).await;
    replay.set_time(600.0).unwrap();
    until("the push after reconnecting", || count() == 5).await;
    assert_eq!(pushes.lock().unwrap()[4].2["elecPercent"], json!(62.0));
    // acct-0002 never logged in: no session, missed.
    let stats = replay.stats();
    assert_eq!(
        (stats.replay.pushes_sent, stats.replay.pushes_missed),
        (5, 2)
    );
    assert!(stats.broker.connects >= 2 && stats.broker.subscriptions >= 2);
    replay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timestamps_can_count_from_now() {
    let scenario = scenario(&[state(EV, 0, json!({}))]);
    let options = ReplayOptions {
        manual: true,
        epoch: Epoch::Now,
        start_at: 50.0,
        corpus: None,
        ..ReplayOptions::default()
    };
    let replay = ReplayServer::start(scenario, options).await.unwrap();
    let started = replay.start_clock();
    assert_eq!(started.started.start_at, 50.0);
    assert_eq!(started.epoch_unix_ms, started.started.wall_unix_ms - 50_000);
    assert_eq!(
        replay.start_clock(),
        started,
        "a second start answers the same"
    );
    assert_eq!(replay.template_note(EV), None, "built-in templates");
    let pool = HttpPool::new();
    let owner = client(&replay, &pool, "acct-0001", None);
    owner.login().await.unwrap();
    replay.set_time(60.0).unwrap();
    let realtime = owner.get_vehicle_realtime(EV, quick()).await.unwrap();
    assert_eq!(
        realtime.raw["time"],
        json!(started.epoch_unix_ms.div_euclid(1000) + 60)
    );
    assert_eq!(realtime.elec_percent, Some(71.0));
    // Recording was off: only counters.
    assert!(replay.server().requests().is_empty());
    assert!(replay.stats().server.requests >= 3);
    replay.reset_stats();
    assert_eq!(replay.stats().server.requests, 0);
    assert_eq!(replay.stats().to_json()["replay"]["cars"], json!(3));
    replay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wall_clock_fires_pushes_on_its_own() {
    let scenario = scenario(&[
        state(EV, 0, json!({})),
        line(json!({"type": "push", "vin": EV, "t": 20, "event": "vehicleInfo", "soc": 50.0})),
    ]);
    let options = ReplayOptions {
        speed: 200.0,
        ..ReplayOptions::default()
    };
    let replay = ReplayServer::start(scenario, options).await.unwrap();
    assert!(replay.set_time(5.0).is_err(), "a wall clock cannot be set");
    let pool = HttpPool::new();
    let pushes: Pushes = Arc::default();
    let owner = client(&replay, &pool, "acct-0001", Some(&pushes));
    owner.login().await.unwrap();
    let topic = format!("oversea/res/{}", owner.session().unwrap().user_id());
    replay.broker().until_subscribed(&topic).await;
    assert_eq!(replay.clock().now(), 0.0, "stands until started");
    replay.start_clock();
    // 20 scenario seconds at 200x: a tenth of a second.
    until("the scheduled push", || pushes.lock().unwrap().len() == 1).await;
    assert_eq!(pushes.lock().unwrap()[0].2["elecPercent"], json!(50.0));
    replay.shutdown().await;
}

fn demo_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/replay/samples/demo.jsonl")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_demo_scenario_replays() {
    let scenario = Scenario::load(&demo_path()).expect("bench/replay/samples/demo.jsonl loads");
    let expected = scenario.clone();
    let duration = scenario.header.duration_s;
    let replay = replay(scenario).await;
    let pool = HttpPool::new();
    let mut clients = Vec::new();
    for account in &expected.accounts {
        let client = client(&replay, &pool, &account.name, None);
        client.login().await.unwrap();
        clients.push(client);
    }
    let mut reads = 0;
    for step in 0..=12 {
        let t = duration / 12 * step;
        replay.set_time(f64::from(t)).unwrap();
        for (account, client) in expected.accounts.iter().zip(&clients) {
            for &index in &account.cars {
                let car = &expected.cars[index];
                let lookup = expected.lookup(index, t);
                let at = format!("{} at t={t}", car.vin);
                let result = client.get_vehicle_realtime(&car.vin, quick()).await;
                match lookup.error {
                    Some(code) => assert_eq!(result.unwrap_err().code(), code, "{at}"),
                    None => {
                        check_realtime(&result.expect(&at), lookup.state, car.energy, &at);
                        reads += 1;
                    }
                }
            }
        }
    }
    assert!(reads > 0);
    replay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thousand_accounts_poll_at_once() {
    const ACCOUNTS: usize = 1000;
    let mut lines = vec![line(
        json!({"type": "scenario", "version": 1, "source": "synthetic",
        "start": START, "duration_s": 3600, "accounts": ACCOUNTS, "cars": ACCOUNTS}),
    )];
    let vin = |i: usize| format!("TESTVIN{:010}", i + 1);
    let soc = |i: usize| (i % 100) as f64;
    for i in 0..ACCOUNTS {
        lines.push(line(
            json!({"type": "account", "account": format!("acct-{:04}", i + 1), "cars": [vin(i)]}),
        ));
    }
    for i in 0..ACCOUNTS {
        lines.push(line(
            json!({"type": "car", "vin": vin(i), "account": format!("acct-{:04}", i + 1),
            "model": "seal", "energy": "ev"}),
        ));
    }
    for i in 0..ACCOUNTS {
        lines.push(state(
            &vin(i),
            0,
            json!({"soc": soc(i), "odometer_km": 1000.0 + i as f64}),
        ));
    }
    let scenario = Scenario::parse(&lines.join("\n")).unwrap();
    let options = ReplayOptions {
        manual: true,
        ..ReplayOptions::default()
    };
    let replay = Arc::new(ReplayServer::start(scenario, options).await.unwrap());
    replay.set_time(60.0).unwrap();
    let pool = HttpPool::new();
    let started = Instant::now();
    // Every account at once, with at most 200 requests in flight, as a
    // worker's connection pool would keep them.
    let in_flight = Arc::new(tokio::sync::Semaphore::new(200));
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..ACCOUNTS {
        let client = client(&replay, &pool, &format!("acct-{:04}", i + 1), None);
        let in_flight = in_flight.clone();
        tasks.spawn(async move {
            let _permit = in_flight.acquire_owned().await.expect("semaphore");
            client.login().await.expect("login");
            let realtime = client
                .get_vehicle_realtime(&vin(i), quick())
                .await
                .expect("realtime");
            (i, realtime.elec_percent, realtime.total_mileage)
        });
    }
    let mut answered = 0;
    while let Some(result) = tasks.join_next().await {
        let (i, soc_read, odometer) = result.expect("task");
        assert_eq!(soc_read, Some(soc(i)), "account {i}");
        assert_eq!(odometer, Some(1000.0 + i as f64), "account {i}");
        answered += 1;
    }
    assert_eq!(answered, ACCOUNTS);
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "took {:?}",
        started.elapsed()
    );
    let stats = replay.stats();
    assert_eq!(stats.server.logins, ACCOUNTS as u64);
    assert_eq!(
        stats.server.by_endpoint["/vehicleInfo/vehicle/vehicleRealTimeResult"],
        ACCOUNTS as u64
    );
    assert!(
        stats.server.by_code.keys().all(|code| code == "0"),
        "{:?}",
        stats.server.by_code
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_other_endpoints_answer_from_templates() {
    let scenario = scenario(&[
        state(EV, 0, json!({})),
        line(json!({"type": "error", "vin": EV, "t": 100, "code": "1008"})),
    ]);
    let options = ReplayOptions {
        manual: true,
        delay: Duration::from_millis(1),
        ..ReplayOptions::default()
    };
    let replay = ReplayServer::start(scenario, options).await.unwrap();
    assert!(replay.template_note(OTHER).is_some());
    assert_eq!(replay.template_note("TESTVIN0000000009"), None);
    let pool = HttpPool::new();
    let owner = client(&replay, &pool, "acct-0001", None);
    owner.login().await.unwrap();
    owner.get_vehicles().await.unwrap();
    let capabilities = owner.get_vehicle_capabilities(EV, false).await;
    assert_eq!(capabilities.source, "latest_config");
    assert_eq!(capabilities.lock, Some(true));
    let configs = owner
        .get_latest_configs(&[EV.to_owned(), HYBRID.to_owned(), OTHER.to_owned()])
        .await
        .unwrap();
    assert_eq!(configs.len(), 2, "only the account's cars");
    let schedule = owner.get_charging_schedule(EV).await.unwrap();
    assert_eq!(schedule.vin, EV);
    assert!(schedule.raw.contains_key("smartChargeDto"));
    let booking = owner.get_booking_list(EV).await.unwrap();
    assert!(!booking.raw.is_empty());
    assert_eq!(booking.vin, EV);
    let push = owner.get_push_state(EV).await.unwrap();
    assert!(!push.switches.is_empty());
    let limit = owner.get_ac_current_limit(EV, None).await.unwrap();
    assert!(!limit.current_limit_list.is_empty());
    owner.get_all_notifications().await.unwrap();
    // A 1008 answers the template endpoints that name the car too.
    replay.set_time(100.0).unwrap();
    assert_eq!(owner.get_booking_list(EV).await.unwrap_err().code(), "1008");
    assert_eq!(
        owner.get_charging_schedule(EV).await.unwrap_err().code(),
        "1008"
    );
    let stats = replay.stats();
    assert_eq!(
        stats.server.by_code.get("9004"),
        None,
        "every call was handled"
    );
    replay.shutdown().await;
}
