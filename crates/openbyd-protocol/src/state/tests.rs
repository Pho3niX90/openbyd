//! Unit tests for what the parity vectors do not reach: accessors, argument
//! checks and the engine used directly from Rust.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use super::engine::{dumped_override, py_equals};
use super::validators::{
    RealtimeOverride, drop_zero_value_filter, preserve_when_none_filter, realtime_filter_updates,
    updates_to_json,
};
use super::*;
use crate::models::base::BydModel;
use crate::models::gps::GpsInfo;
use crate::models::hvac::HvacStatus;
use crate::models::realtime::{LockState, TirePressureUnit, VehicleRealtimeData};
use crate::models::vehicle::Vehicle;
use crate::parity;

fn realtime(raw: Value) -> VehicleRealtimeData {
    VehicleRealtimeData::parse(&raw, &Value::Null).unwrap()
}

fn hvac(raw: Value) -> HvacStatus {
    HvacStatus::parse(&raw, &Value::Null).unwrap()
}

fn gps(raw: Value) -> GpsInfo {
    GpsInfo::parse(&raw, &Value::Null).unwrap()
}

fn engine() -> VehicleStateEngine {
    let vehicle = Vehicle::parse(&json!({"vin": "TESTVIN0000000001"}), &Value::Null).unwrap();
    VehicleStateEngine::new("TESTVIN0000000001", vehicle, DEFAULT_PROJECTION_TTL)
}

#[test]
fn per_field_filters() {
    assert_eq!(
        drop_zero_value_filter(Some(2.0), Some(0.0), true),
        Some(Some(2.0))
    );
    assert_eq!(drop_zero_value_filter(Some(2.0), Some(0.0), false), None);
    assert_eq!(drop_zero_value_filter(Some(2.0), Some(1.0), true), None);
    assert_eq!(
        drop_zero_value_filter(Some(LockState::Locked), Some(LockState::Unavailable), true),
        Some(Some(LockState::Locked))
    );
    let previous = Some("a".to_owned());
    assert_eq!(
        preserve_when_none_filter(&None::<String>, &None, false),
        None
    );
    assert_eq!(
        preserve_when_none_filter(&previous, &None, true),
        Some(previous.clone())
    );
    assert_eq!(
        preserve_when_none_filter(&previous, &Some("b".into()), false),
        Some(previous.clone())
    );
    assert_eq!(
        preserve_when_none_filter(&previous, &Some("b".into()), true),
        None
    );
}

#[test]
fn realtime_rules_from_rust() {
    let previous = realtime(json!({
        "tirePressUnit": 2, "leftFrontTirepressure": 38.4, "elecPercent": 80,
        "leftFrontDoorLock": 2, "totalEnergy": "16.6kW·h/100km",
    }));
    let incoming = realtime(json!({
        "tirePressUnit": 3, "leftFrontTirepressure": 0, "elecPercent": 0,
        "leftFrontDoorLock": 0, "totalEnergy": "--",
    }));
    let updates = realtime_filter_updates(Some(&previous), &incoming);
    assert!(updates.contains(&(
        "tire_press_unit",
        RealtimeOverride::TireUnit(Some(TirePressureUnit::Psi))
    )));
    let json = updates_to_json(&updates);
    assert_eq!(json["elec_percent"], json!(80.0));
    assert_eq!(json["left_front_door_lock"], json!(2));
    assert_eq!(json["tire_press_unit"], json!(2));
    assert_eq!(json["total_energy_ev_unit"], json!("kWh/100km"));
    let filtered = apply_realtime_filters(Some(&previous), incoming);
    assert_eq!(filtered.elec_percent, Some(80.0));
    assert_eq!(filtered.left_front_door_lock, Some(LockState::Locked));
    assert_eq!(filtered.total_energy.as_deref(), Some("16.6kW·h/100km"));
    assert_eq!(filtered.tire_press_unit, Some(TirePressureUnit::Psi));

    let first = apply_realtime_filters(
        None,
        realtime(json!({"elecPercent": 0, "tirePressUnit": 3})),
    );
    assert_eq!(first.elec_percent, None);
    assert_eq!(first.tire_press_unit, None);
    assert_eq!(RealtimeOverride::Lock(None).to_json(), Value::Null);
    assert_eq!(RealtimeOverride::TireUnit(None).to_json(), Value::Null);
    assert_eq!(RealtimeOverride::Text(None).to_json(), Value::Null);
}

#[test]
fn gps_guard_returns_one_of_its_arguments() {
    let good = gps(json!({"latitude": 52.0, "longitude": 4.0}));
    let island = gps(json!({"latitude": 0.0, "longitude": 0.0}));
    assert!(std::ptr::eq(
        guard_gps_coordinates(Some(&good), Some(&island)).unwrap(),
        &good
    ));
    assert!(std::ptr::eq(
        apply_gps_filters(None, Some(&good)).unwrap(),
        &good
    ));
    assert!(guard_gps_coordinates(None, Some(&island)).is_none());
    assert!(has_valid_coordinates(&gps(
        json!({"latitude": "NAN", "longitude": "NAN"})
    )));
}

#[test]
fn hvac_placeholder_clears_the_triad() {
    let placeholder = apply_hvac_filters(None, hvac(json!({"tempInCar": 0, "pm": 0, "status": 1})));
    assert_eq!(
        (
            placeholder.temp_in_car,
            placeholder.temp_out_car,
            placeholder.pm
        ),
        (None, None, None)
    );
    let mixed = apply_hvac_filters(None, hvac(json!({"tempInCar": 0, "pm": 3})));
    assert_eq!(mixed.temp_in_car, Some(0.0));
}

#[test]
fn python_equality() {
    assert!(py_equals(&json!(1), &json!(1.0)));
    assert!(py_equals(&json!(true), &json!(1)));
    assert!(py_equals(
        &json!(u64::MAX),
        &json!(18_446_744_073_709_551_615_u64)
    ));
    assert!(!py_equals(&json!(1), &json!("1")));
    assert!(!py_equals(&json!(1.5), &json!(1)));
    assert!(!py_equals(&json!(1e300), &json!(1)));
    assert!(py_equals(
        &json!([1, {"a": null}]),
        &json!([1.0, {"a": null}])
    ));
    assert!(!py_equals(&json!({"a": 1}), &json!({"b": 1})));
    assert!(!py_equals(&json!([1]), &json!([1, 2])));
    assert!(py_equals(&json!(-0.0), &json!(0)));
}

#[test]
fn overrides_dump_like_pydantic() {
    type R = VehicleRealtimeData;
    assert_eq!(
        dumped_override::<R>("elec_percent", &json!(90)),
        json!(90.0)
    );
    assert_eq!(
        dumped_override::<R>("elec_percent", &json!(true)),
        json!(1.0)
    );
    assert_eq!(
        dumped_override::<R>("elec_percent", &json!(false)),
        json!(0.0)
    );
    assert_eq!(
        dumped_override::<R>("elec_percent", &json!("x")),
        json!("x")
    );
    assert_eq!(
        dumped_override::<R>("main_setting_temp", &json!(true)),
        json!(1)
    );
    assert_eq!(
        dumped_override::<R>("main_setting_temp", &json!(2.5)),
        json!(2.5)
    );
    assert_eq!(
        dumped_override::<R>("left_front_door_lock", &json!(true)),
        json!(true)
    );
}

#[test]
fn engine_from_rust() {
    let mut engine = engine();
    assert_eq!(engine.vin(), "TESTVIN0000000001");
    assert_eq!(engine.projection_ttl(), 30.0);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    engine.set_on_state_changed(Some(Box::new(move |vin, snapshot| {
        assert_eq!(vin, "TESTVIN0000000001");
        assert_eq!(snapshot.vehicle.vin, "TESTVIN0000000001");
        counter.fetch_add(1, Ordering::SeqCst);
    })));

    engine.update_realtime(
        realtime(json!({"leftFrontDoorLock": 1, "elecPercent": 70})),
        0.0,
    );
    let spec = ProjectionSpec::new("realtime", "left_front_door_lock", json!(2));
    assert_eq!(spec.to_json()["field_name"], json!("left_front_door_lock"));
    let command = engine.register_projections(&[spec], 0.0);
    assert_eq!(command, "cmd-1");

    let section = engine.snapshot().realtime.clone().unwrap();
    assert!(section.is_projected());
    assert_eq!(section.overrides()["left_front_door_lock"], json!(2));
    assert_eq!(
        section.base().left_front_door_lock,
        Some(LockState::Unlocked)
    );
    assert_eq!(section.field("left_front_door_lock"), Some(json!(2)));
    assert_eq!(section.field("no_such_field"), None);
    assert!(Arc::ptr_eq(
        section.base_arc(),
        engine.snapshot().realtime.as_ref().unwrap().base_arc()
    ));

    // A contradicting payload is held off; a confirming one drops the projection.
    engine.update_realtime(realtime(json!({"leftFrontDoorLock": 1})), 1.0);
    assert_eq!(engine.active_projections(1.0).len(), 1);
    assert_eq!(
        engine
            .snapshot()
            .realtime
            .as_ref()
            .unwrap()
            .field("left_front_door_lock"),
        Some(json!(2))
    );
    engine.update_realtime(realtime(json!({"leftFrontDoorLock": 2})), 2.0);
    assert!(engine.held_projections().is_empty());

    // Expiry and rollback.
    engine.register_projections(&[ProjectionSpec::new("hvac", "status", json!(1))], 10.0);
    assert_eq!(engine.active_projections(40.0).len(), 1);
    assert!(engine.active_projections(40.5).is_empty());
    assert_eq!(engine.held_projections().len(), 1);
    engine.rollback_projections("cmd-2", 11.0);
    assert!(engine.held_projections().is_empty());

    engine.update_gps(gps(json!({"latitude": 0.0, "longitude": 0.0})), 12.0);
    assert!(engine.snapshot().gps.is_none());
    engine.update_gps(gps(json!({"latitude": 52.0, "longitude": 4.0})), 12.0);
    engine.update_gps(gps(json!({"latitude": 52.0})), 12.0);
    assert_eq!(
        engine.snapshot().gps.as_ref().unwrap().base().longitude,
        Some(4.0)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 9);

    engine.set_on_state_changed(None);
    engine.update_hvac(hvac(json!({"pm": 0})), 13.0);
    assert_eq!(calls.load(Ordering::SeqCst), 9);
    let dump = engine.snapshot().dump();
    assert_eq!(dump["hvac"]["pm"], Value::Null);
    assert_eq!(dump["charging"], Value::Null);

    let projection = &FieldProjection {
        section: "gps".into(),
        field_name: "speed".into(),
        expected_value: json!(0),
        command_id: "cmd-9".into(),
        created_at: 1.0,
        ttl: 2.0,
    };
    assert!(!projection.is_expired(3.0));
    assert!(projection.is_expired(3.5));
    assert_eq!(projection.to_json()["created_at"], json!(1.0));
}

#[test]
fn parity_arguments_are_checked() {
    let vehicle = json!({"vin": "TESTVIN0000000001"});
    let bad = |args: Value| {
        parity::call("state.VehicleStateEngine", &args)
            .unwrap_err()
            .kind
    };
    let base = json!({"vin": "V", "vehicle": vehicle, "start": 0.0, "ops": []});
    assert!(parity::call("state.VehicleStateEngine", &base).is_ok());
    let with = |key: &str, value: Value| {
        let mut args = base.clone();
        args[key] = value;
        args
    };
    assert_eq!(bad(with("ops", json!(5))), "BadArguments");
    assert_eq!(bad(with("start", json!("x"))), "BadArguments");
    assert_eq!(bad(with("projection_ttl", json!("x"))), "BadArguments");
    assert_eq!(bad(with("vin", json!(5))), "BadArguments");
    assert_eq!(bad(with("ops", json!([{"op": "jump"}]))), "BadArguments");
    assert_eq!(bad(with("ops", json!([{"op": 5}]))), "BadArguments");
    assert_eq!(
        bad(with(
            "ops",
            json!([{"op": "register_projections", "specs": 5}])
        )),
        "BadArguments"
    );
    assert_eq!(
        bad(with(
            "ops",
            json!([{"op": "register_projections", "specs": [{"section": 1}]}])
        )),
        "BadArguments"
    );
    assert_eq!(
        bad(with("ops", json!([{"op": "rollback_projections"}]))),
        "BadArguments"
    );
    assert_eq!(
        bad(with("ops", json!([{"op": "advance", "seconds": null}]))),
        "BadArguments"
    );
    let projection = json!({"section": "a", "field_name": "b", "expected_value": 1, "command_id": "c",
                            "created_at": 0.0, "ttl": 1.0, "now": "x"});
    assert_eq!(
        parity::call("state.FieldProjection", &projection)
            .unwrap_err()
            .kind,
        "BadArguments"
    );
    assert_eq!(
        parity::call("state.ProjectionSpec", &json!({"section": "a"}))
            .unwrap_err()
            .kind,
        "BadArguments"
    );
}
