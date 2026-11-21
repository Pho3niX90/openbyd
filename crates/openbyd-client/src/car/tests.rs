//! Unit tests of the car layer beyond the parity sequences: thread safety,
//! real concurrency, and what the sequences cannot reach.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::charging::ChargingStatus;
use openbyd_protocol::models::hvac::HvacStatus;
use openbyd_protocol::models::realtime::VehicleRealtimeData;
use openbyd_protocol::models::vehicle::Vehicle;
use serde_json::{Value, json};

use super::parity::ScriptedBackend;
use super::*;

const VIN: &str = "TESTVIN0000000001";

fn vehicle() -> Vehicle {
    Vehicle::parse(&json!({"vin": VIN}), &Value::Null).unwrap()
}

fn car() -> BydCar<ScriptedBackend> {
    let car = BydCar::new(
        ScriptedBackend::default(),
        VIN,
        vehicle(),
        CarOptions::default(),
    )
    .unwrap();
    ScriptedBackend::attach(&car);
    car
}

fn realtime(raw: Value) -> VehicleRealtimeData {
    VehicleRealtimeData::parse(&raw, &Value::Null).unwrap()
}

fn assert_send_sync<T: Send + Sync>() {}

fn assert_send<T: Send>(_: &T) {}

#[test]
fn cars_and_their_futures_cross_threads() {
    assert_send_sync::<BydCar<ScriptedBackend>>();
    let car = car();
    assert_send(&car.lock.lock());
    assert_send(&car.hvac.start(ClimateStart::default()));
    assert_send(&car.seat.heat(SeatPosition::Driver, SeatLevel::High));
    assert_send(&car.update_charging());
    assert_send(&car.save_charging_schedule("22:00", "06:00", "1", true));
}

#[test]
fn defaults_follow_pybyd() {
    let options = CarOptions::default();
    assert_eq!(options.projection_ttl, 30.0);
    assert!(options.capabilities.is_none());
    let clock = SystemClock::default();
    let first = clock.monotonic();
    assert!(clock.monotonic() >= first);

    let implicit = implicit_capabilities(VIN);
    assert_eq!(implicit.source, "implicit_default");
    assert_eq!(implicit.lock, Some(true));
    assert_eq!(implicit.open_windows, None);

    let car = car();
    assert_eq!(car.vin(), VIN);
    assert!(car.lock.available() && car.hvac.available() && car.steering.available());
    assert!(!car.windows.open_available() && car.windows.close_available());
    assert!(!car.trunk.open_available() && !car.trunk.close_available());
    assert!(
        car.seat
            .is_mode_available(SeatPosition::Driver, SeatMode::Vent)
    );
    assert!(
        !car.seat
            .is_mode_available(SeatPosition::RearLeft, SeatMode::Heat)
    );
    assert_eq!(car.hvac.temp_range(), (15.0, 31.0));
    car.close();
}

#[test]
fn pushes_outside_a_runtime_are_dropped() {
    let car = car();
    car.handle_mqtt_realtime(realtime(json!({"elecPercent": 50})));
    car.handle_mqtt_status_push(realtime(json!({"elecPercent": 51})), None, None);
    assert!(car.state().realtime.is_none());
}

/// Lets the tasks the car spawned on this (current-thread) runtime finish.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn a_status_push_is_laid_over_what_the_car_knows() {
    let car = car();
    // Nothing is known yet: a partial push shows nothing.
    car.handle_mqtt_status_push(realtime(json!({"elecPercent": 80})), None, None);
    settle().await;
    assert!(car.state().realtime.is_none());

    let backend = &car.inner.backend;
    backend.queue(json!({"result": {"elecPercent": 70, "leftFrontDoor": 0, "chargeState": 1, "chargePower": 5.1}}));
    car.update_realtime().await.unwrap();
    backend.queue(json!({"result": {"status": 2, "tempInCar": 21.0}}));
    car.update_hvac().await.unwrap();
    backend.queue(json!({"result": {"soc": 70, "chargingState": 1, "chargePower": 5.1}}));
    car.update_charging().await.unwrap();

    let hvac = HvacStatus::parse(&json!({"cycleModel": 2}), &Value::Null).unwrap();
    let charging = ChargingStatus::parse(&json!({"chargingState": 15}), &Value::Null).unwrap();
    car.handle_mqtt_status_push(
        realtime(
            json!({"elecPercent": 85, "leftFrontDoor": 1, "chargeState": 15, "tempInCar": -129}),
        ),
        Some(hvac),
        Some(charging),
    );
    settle().await;

    let state = car.state();
    let realtime = state.realtime.as_ref().unwrap().base();
    assert_eq!(realtime.elec_percent, Some(85.0));
    assert_eq!(realtime.charge_power, None);
    assert_eq!(realtime.raw["leftFrontDoor"], json!(1));
    assert_eq!(
        state.realtime.as_ref().unwrap().field("left_front_door"),
        Some(json!(1))
    );
    let hvac = state.hvac.as_ref().unwrap();
    assert_eq!(hvac.field("cycle_choice"), Some(json!(2)));
    assert_eq!(hvac.base().temp_in_car, Some(21.0));
    let charging = state.charging.as_ref().unwrap().base();
    assert_eq!(
        (charging.soc, charging.charging_state),
        (Some(70), Some(15))
    );
    assert_eq!(charging.charge_power, None);
    assert!(
        state.charging_schedule.is_some(),
        "the schedule is not a push's to change"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushes_merge_in_the_order_they_arrive() {
    let car = car();
    for round in 0..40 {
        // Hold the engine so every push of the round is waiting at once,
        // and the workers pick their tasks up in whatever order they like.
        let engine = car.inner.engine.lock().await;
        let last = 60 + round % 30;
        for step in 0..12 {
            let percent = if step == 11 { last } else { step };
            car.handle_mqtt_realtime(realtime(
                json!({"elecPercent": percent, "leftFrontDoor": 0}),
            ));
            car.handle_mqtt_status_push(
                realtime(json!({"elecPercent": percent, "leftFrontDoor": step % 2})),
                None,
                None,
            );
        }
        drop(engine);
        while !car.inner.pushes.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        drop(car.inner.engine.lock().await);

        let state = car.state();
        let realtime = state.realtime.as_ref().unwrap();
        assert_eq!(realtime.base().elec_percent, Some(f64::from(last)));
        // The last push of the round, a partial one, said the door was open.
        assert_eq!(realtime.field("left_front_door"), Some(json!(1)));
    }
}

#[test]
fn a_push_left_behind_by_a_dropped_runtime_merges_before_a_later_one() {
    let car = car();
    // Hold the engine lock so the task spawned on the first runtime cannot run
    // before that runtime is dropped.
    let engine = car.inner.engine.try_lock().unwrap();
    let first = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    {
        let _entered = first.enter();
        car.handle_mqtt_realtime(realtime(json!({"elecPercent": 42, "leftFrontDoor": 0})));
    }
    drop(first);
    drop(engine);

    assert!(car.state().realtime.is_none());
    assert_eq!(car.inner.pushes.lock().unwrap().len(), 1);

    // A partial status push on a new runtime only has a base to patch if the
    // orphaned full push ahead of it drains first.
    let second = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    second.block_on(async {
        car.handle_mqtt_status_push(realtime(json!({"leftFrontDoor": 1})), None, None);
        settle().await;
    });

    assert!(car.inner.pushes.lock().unwrap().is_empty());
    let state = car.state();
    let realtime = state.realtime.as_ref().unwrap();
    assert_eq!(realtime.base().elec_percent, Some(42.0));
    assert_eq!(realtime.field("left_front_door"), Some(json!(1)));
}

#[test]
fn unavailable_features_fail_before_sending() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let car = car();
    let error = runtime.block_on(car.trunk.open()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::BydEndpointNotSupportedError);
    assert_eq!(error.code(), "capability_unsupported");
    assert!(car.inner.backend.take_calls().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_push_during_a_failed_command_merges_after_the_rollback() {
    let notified = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&notified);
    let car = BydCar::new(
        ScriptedBackend::default(),
        VIN,
        vehicle(),
        CarOptions {
            on_state_changed: Some(Box::new(move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
            })),
            ..CarOptions::default()
        },
    )
    .unwrap();
    ScriptedBackend::attach(&car);
    let car = Arc::new(car);

    car.inner
        .backend
        .queue(json!({"result": {"leftFrontDoorLock": 2, "elecPercent": 70}}));
    car.update_realtime().await.unwrap();
    car.inner.backend.queue(json!({
        "error": {"class": "BydRemoteControlError", "code": "timeout"},
        "pushes": [{"kind": "realtime", "raw": {"leftFrontDoorLock": 2, "elecPercent": 60}}],
    }));
    let error = car.lock.unlock().await.unwrap_err();
    assert_eq!(error.code(), "timeout");

    // The push waits for the command's lock, then merges over the rolled-back state.
    for _ in 0..100 {
        if car
            .state()
            .realtime
            .as_ref()
            .and_then(|r| r.base().elec_percent)
            == Some(60.0)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let state = car.state();
    let realtime = state.realtime.as_ref().unwrap();
    assert_eq!(realtime.base().elec_percent, Some(60.0));
    assert!(!realtime.is_projected());
    assert_eq!(realtime.field("left_front_door_lock"), Some(json!(2)));
    assert!(notified.load(Ordering::SeqCst) >= 4);

    // Commands from many tasks run one at a time.
    let mut tasks = Vec::new();
    for _ in 0..8 {
        car.inner
            .backend
            .queue(json!({"result": {"controlState": 1}}));
        let car = Arc::clone(&car);
        tasks.push(tokio::spawn(async move { car.lock.lock().await }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(car.inner.backend.take_calls().len(), 10);
    let state = car.state();
    assert_eq!(
        state.realtime.as_ref().unwrap().overrides().len(),
        4,
        "only the latest lock projection per door"
    );
}

#[test]
fn sequences_reject_malformed_arguments() {
    let base = json!({"vin": VIN, "vehicle": {"vin": VIN}, "start": 0.0, "ops": []});
    let with = |key: &str, value: Value| {
        let mut args = base.clone();
        args[key] = value;
        parity::parity_call("car.BydCar", &args).unwrap()
    };
    assert!(with("ops", json!([])).is_ok());
    assert_eq!(with("ops", json!(1)).unwrap_err().kind, "BadArguments");
    assert_eq!(with("start", json!("x")).unwrap_err().kind, "BadArguments");
    assert_eq!(
        with("projection_ttl", json!("x")).unwrap_err().kind,
        "BadArguments"
    );
    assert_eq!(
        with("ops", json!([{"op": "nope"}])).unwrap_err().kind,
        "BadArguments"
    );
    assert_eq!(
        with(
            "ops",
            json!([{"op": "call", "target": "car", "method": "nope"}])
        )
        .unwrap_err()
        .kind,
        "BadArguments"
    );
    let bad_error = with(
        "ops",
        json!([{"op": "call", "target": "lock", "method": "lock",
                "replies": [{"error": {"class": "Nope"}}]}]),
    )
    .unwrap();
    assert_eq!(
        bad_error["steps"][0]["error"]["class"],
        json!("AssertionError")
    );
    assert!(parity::parity_call("car.nothing", &base).is_none());
}
