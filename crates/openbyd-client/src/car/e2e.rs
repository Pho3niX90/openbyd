//! A car from `BydClient::get_car` on the production environment, against
//! the fake BYD server and the TLS test broker, over real sockets: every
//! update, a command acknowledged over MQTT with its projection applied and
//! then confirmed by a push, and a failing command rolled back.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openbyd_protocol::config::BydConfig;
use openbyd_protocol::error::ErrorKind;
use openbyd_protocol::state::VehicleSnapshot;
use serde_json::{Value, json};

use crate::client::{BydClient, ClientOptions, GetCarOptions, ProductionEnv};
use crate::http::{HttpEnv, HttpPool};
use crate::testing::broker::{TestBroker, runtime_options};
use crate::testing::{Account, FakeBydServer, Reply};

const VIN: &str = "TESTVIN0000000001";
const USER: &str = "user@example.com";
const PASSWORD: &str = "secret";
const WAIT: Duration = Duration::from_secs(10);
const REMOTE_CONTROL: &str = "/control/remoteControl";

const DOORS: [&str; 4] = [
    "left_front_door_lock",
    "right_front_door_lock",
    "left_rear_door_lock",
    "right_rear_door_lock",
];

fn realtime(lock: i64, time: i64) -> Value {
    json!({
        "onlineState": 1, "time": time, "elecPercent": 80,
        "leftFrontDoorLock": lock, "rightFrontDoorLock": lock,
        "leftRearDoorLock": lock, "rightRearDoorLock": lock,
    })
}

fn doors(state: &VehicleSnapshot) -> Vec<Value> {
    let dump = state.dump();
    DOORS
        .iter()
        .map(|field| dump["realtime"][*field].clone())
        .collect()
}

async fn until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

struct World {
    server: FakeBydServer,
    broker: Arc<TestBroker>,
    client: BydClient<ProductionEnv>,
}

impl World {
    async fn start(options: ClientOptions) -> Self {
        let server = FakeBydServer::start([Account::new(USER, PASSWORD)])
            .await
            .expect("server");
        let broker = TestBroker::start().await;
        let data = |path: &str, value: Value| server.reply(path, Reply::Data(value));
        data(
            "/app/emqAuth/getEmqBrokerIp",
            json!({"emqBorker": broker.address()}),
        );
        data(
            "/app/account/getAllListByUserId",
            json!([{"vin": VIN, "energyType": "0", "modelName": "SEAL"}]),
        );
        data(
            "/vehicle/vehicleswitch/getLatestConfig",
            json!({VIN: {"cfFixedList": [{"functionNo": "1005"}, {"functionNo": "1006"}]}}),
        );
        data(
            "/vehicle/vehicleswitch/verifyControlPassword",
            json!({"ok": true}),
        );
        data(
            "/vehicleInfo/vehicle/vehicleRealTimeRequest",
            realtime(1, 1_700_000_000),
        );
        data(
            "/control/getStatusNow",
            json!({"acSwitch": 0, "tempInCar": 21}),
        );
        data(
            "/control/getGpsInfo",
            json!({"latitude": 52.0, "longitude": 4.0, "gpsTimeStamp": 1_700_000_000}),
        );
        data(
            "/control/smartCharge/homePage",
            json!({"elecPercent": 80, "chargingState": 0, "smartChargeDto": {"chargeType": 0}}),
        );
        data(
            "/vehicleInfo/vehicle/getEnergyConsumption",
            json!({"totalEnergy": "15.2kW·h/100km"}),
        );
        let mut config = BydConfig::new(USER, PASSWORD);
        config.base_url = server.url();
        config.control_pin = Some("123456".into());
        config.mqtt_timeout = 5.0;
        let env = ProductionEnv::with_mqtt_options(
            HttpEnv::for_config(&HttpPool::new(), &config),
            runtime_options(),
        );
        let client = BydClient::new(config, env, options);
        Self {
            server,
            broker,
            client,
        }
    }

    fn topic(&self) -> String {
        format!(
            "oversea/res/{}",
            self.client.session().expect("logged in").user_id()
        )
    }

    fn push(&self, body: &Value) {
        let session = self.client.session().expect("logged in");
        let sent = self
            .broker
            .publish_sealed(&self.topic(), body, session.content_key());
        assert_eq!(sent, 1);
    }

    async fn until_requests(&self, endpoint: &str, count: usize) {
        until(endpoint, || {
            self.server.requests_to(endpoint).len() >= count
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_car_updates_commands_and_merges_pushes() {
    let changes = Arc::new(Mutex::new(0usize));
    let world = World::start(ClientOptions::default()).await;
    world.client.login().await.expect("login");
    world.broker.until_subscribed(&world.topic()).await;

    let counted = Arc::clone(&changes);
    let car = world
        .client
        .get_car(
            VIN,
            GetCarOptions {
                on_state_changed: Some(Box::new(move |_, _| *counted.lock().unwrap() += 1)),
                ..GetCarOptions::default()
            },
        )
        .await
        .expect("car");
    assert!(car.lock.available());
    assert!(car.capabilities().lock.unwrap_or(false));
    // Kept: the same car again, without a request.
    let again = world
        .client
        .get_car(VIN, GetCarOptions::default())
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&car, &again));
    assert_eq!(
        world
            .server
            .requests_to("/app/account/getAllListByUserId")
            .len(),
        1
    );

    // Every section.
    car.update_realtime().await.expect("realtime");
    car.update_hvac().await.expect("hvac");
    car.update_gps().await.expect("gps");
    car.update_charging().await.expect("charging");
    car.update_energy().await.expect("energy");
    let state = car.state().dump();
    for section in [
        "realtime",
        "hvac",
        "gps",
        "charging",
        "energy",
        "charging_schedule",
    ] {
        assert!(!state[section].is_null(), "{section} was not merged");
    }
    assert_eq!(doors(&car.state()), vec![json!(1); 4]);
    assert!(*changes.lock().unwrap() >= 6);

    // A command acknowledged over MQTT: projected while in flight, kept after.
    world
        .client
        .verify_command_access(VIN, None)
        .await
        .expect("PIN");
    world
        .server
        .reply(REMOTE_CONTROL, Reply::Data(json!({"requestSerial": "C1"})));
    let locking = {
        let car = Arc::clone(&car);
        tokio::spawn(async move { car.lock.lock().await })
    };
    world.until_requests(REMOTE_CONTROL, 1).await;
    assert_eq!(doors(&car.state()), vec![json!(2); 4], "projected");
    tokio::time::sleep(Duration::from_millis(50)).await;
    world.push(&json!({"event": "remoteControl", "vin": VIN,
        "data": {"uuid": "C1", "respondData": {"res": 2}}}));
    tokio::time::timeout(WAIT, locking)
        .await
        .expect("in time")
        .expect("joined")
        .expect("locked");
    assert!(
        world
            .server
            .requests_to("/control/remoteControlResult")
            .is_empty(),
        "acknowledged over MQTT, not polled"
    );
    assert_eq!(car.inner.engine.lock().await.held_projections().len(), 4);
    assert_eq!(doors(&car.state()), vec![json!(2); 4]);

    // A push that still says unlocked is held off by the guard window; one
    // that says locked confirms the projection.
    world.push(&json!({"event": "vehicleInfo", "vin": VIN,
        "data": {"respondData": realtime(1, 1_700_000_100)}}));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(doors(&car.state()), vec![json!(2); 4], "guarded");
    world.push(&json!({"event": "vehicleInfo", "vin": VIN,
        "data": {"respondData": realtime(2, 1_700_000_200)}}));
    let mut confirmed = false;
    let deadline = Instant::now() + WAIT;
    while !confirmed {
        assert!(
            Instant::now() < deadline,
            "the push did not confirm the lock"
        );
        confirmed = car.inner.engine.lock().await.held_projections().is_empty();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(doors(&car.state()), vec![json!(2); 4]);

    // A refused command: projected, then rolled back to the confirmed state.
    world.server.script(REMOTE_CONTROL, [Reply::code("5005")]);
    let error = car.lock.unlock().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydControlPasswordError);
    assert!(car.inner.engine.lock().await.held_projections().is_empty());
    assert_eq!(doors(&car.state()), vec![json!(2); 4], "rolled back");

    // smartCharge and energyConsumption pushes reach the car too.
    world.push(&json!({"event": "smartCharge", "vin": VIN,
        "data": {"respondData": {"elecPercent": 81, "chargingState": 1}}}));
    until("the charging push", || {
        car.state().dump()["charging"]["soc"] == json!(81)
    })
    .await;
    world.push(&json!({"event": "energyConsumption", "vin": VIN,
        "data": {"respondData": {"nearestEnergyConsumption": {"evConsumption": 16.0}}}}));
    until("the energy push", || {
        !car.state().dump()["energy"]["nearest_energy_consumption"].is_null()
    })
    .await;

    // Closing the client forgets its cars; the car's calls then fail.
    world.client.async_close();
    assert!(world.client.car(VIN).is_none());
    let error = car.update_hvac().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydError);
    world.server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_car_outliving_its_client_fails_its_calls() {
    let world = World::start(ClientOptions::default()).await;
    let car = world
        .client
        .get_car(VIN, GetCarOptions::default())
        .await
        .expect("car");
    let handle = world.client.car(VIN).expect("kept");
    assert!(Arc::ptr_eq(&car, &handle));
    drop(handle);
    let World { server, client, .. } = world;
    drop(client);
    let error = car.update_hvac().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydError);
    assert!(error.message().contains("not initialized"));
    // A command fails the same way, and its projections are rolled back.
    let error = car.lock.lock().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydError);
    assert!(car.inner.engine.lock().await.held_projections().is_empty());
    server.shutdown().await;
}
