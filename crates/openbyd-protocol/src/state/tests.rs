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
use crate::models::charging::ChargingStatus;
use crate::models::gps::GpsInfo;
use crate::models::hvac::{HvacOverallStatus, HvacStatus};
use crate::models::realtime::{
    AirCirculationMode, ChargingState, DoorOpenState, LockState, OnlineState, StearingWheelHeat,
    TirePressureUnit, VehicleRealtimeData,
};
use crate::models::vehicle::Vehicle;
use crate::mqtt::{REALTIME_STATUS_PUSH_DTOS, VehicleStatusPushDto, flatten_vehicle_status_push};
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

fn charging(raw: Value) -> ChargingStatus {
    ChargingStatus::parse(&raw, &Value::Null).unwrap()
}

/// An engine counting its `on_state_changed` calls.
fn counting_engine() -> (VehicleStateEngine, Arc<AtomicUsize>) {
    let mut engine = engine();
    let notified = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&notified);
    engine.set_on_state_changed(Some(Box::new(move |_, _| {
        counter.fetch_add(1, Ordering::SeqCst);
    })));
    (engine, notified)
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
    // Plain ints since pyBYD #94 and #96 (the second is `int | str`).
    for field in ["air_run_state", "repair_mode_switch"] {
        assert_eq!(dumped_override::<R>(field, &json!(true)), json!(1));
        assert_eq!(dumped_override::<R>(field, &json!(false)), json!(0));
        assert_eq!(dumped_override::<R>(field, &json!("x")), json!("x"));
    }
    assert_eq!(
        dumped_override::<HvacStatus>("air_run_state", &json!(false)),
        json!(0)
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

/// Upstream `test_state_engine_patch_realtime_preserves_existing_fields_and_merges_raw`.
#[test]
fn patch_realtime_keeps_the_other_fields_and_merges_raw() {
    let mut engine = engine();
    engine.update_realtime(
        realtime(json!({
            "elecPercent": 75, "enduranceMileage": 400, "tempInCar": 22.5,
            "leftFrontDoor": 0, "leftFrontDoorLock": 2,
        })),
        0.0,
    );
    // A command expects the door to unlock.
    engine.register_projections(
        &[ProjectionSpec::new(
            "realtime",
            "left_front_door_lock",
            json!(1),
        )],
        0.0,
    );
    // The sentinels (-129, -1) must not erase what is known.
    engine.patch_realtime(
        realtime(json!({
            "leftFrontDoor": 1, "leftFrontDoorLock": 1, "tempInCar": -129, "rightFrontDoorLock": -1,
        })),
        1.0,
    );

    let snapshot = engine.snapshot();
    let section = snapshot.realtime.as_ref().unwrap();
    let base = section.base();
    assert_eq!(base.left_front_door, Some(DoorOpenState::Open));
    assert_eq!(base.left_front_door_lock, Some(LockState::Unlocked));
    assert_eq!(base.right_front_door_lock, None);
    assert_eq!(base.elec_percent, Some(75.0));
    assert_eq!(base.endurance_mileage, Some(400.0));
    assert_eq!(base.temp_in_car, Some(22.5));
    assert_eq!(base.raw["enduranceMileage"], json!(400));
    assert_eq!(base.raw["leftFrontDoor"], json!(1));
    assert_eq!(base.raw["tempInCar"], json!(-129), "raw is merged as sent");
    // The push confirmed the projection.
    assert!(!section.is_projected());
    assert!(engine.held_projections().is_empty());
}

/// Upstream `test_state_engine_patch_skips_uninitialized_base_and_patches_hvac_charging`.
#[test]
fn a_status_push_skips_unknown_sections_and_rebuilds_once() {
    let (mut engine, notified) = counting_engine();

    // Before any full payload a partial is ignored.
    engine.patch_realtime(realtime(json!({"elecPercent": 80})), 0.0);
    assert!(engine.snapshot().realtime.is_none());
    assert_eq!(notified.load(Ordering::SeqCst), 0);

    engine.update_realtime(
        realtime(json!({"elecPercent": 70, "chargeState": 1, "chargePower": 5.1})),
        0.0,
    );
    engine.update_hvac(hvac(json!({"status": 2, "mainSettingTempNew": 21.0})), 0.0);
    engine.update_charging(
        charging(json!({"soc": 70, "chargingState": 1, "connectState": 1, "chargePower": 5.1})),
        0.0,
    );
    notified.store(0, Ordering::SeqCst);

    let Value::Object(push) = json!({
        "airStatusDto": {"cycleModel": 2, "airRunState": 1, "mainSettingTempNew": 23.0},
        "chargeStatusDto": {"chargingState": 15, "connectState": 0, "chargeState": 15},
        "can4A505Dto": {"elecPercent": 85},
    }) else {
        unreachable!()
    };
    let flat = flatten_vehicle_status_push(&push, &REALTIME_STATUS_PUSH_DTOS);
    let air = flatten_vehicle_status_push(&push, &[VehicleStatusPushDto::AirStatus]);
    let charge = flatten_vehicle_status_push(&push, &[VehicleStatusPushDto::ChargeStatus]);
    engine.patch_status_push(
        Some(realtime(Value::Object(flat))),
        Some(hvac(Value::Object(air))),
        Some(charging(Value::Object(charge))),
        1.0,
    );

    assert_eq!(
        notified.load(Ordering::SeqCst),
        1,
        "one rebuild for all three"
    );
    let snapshot = engine.snapshot();
    let rt = snapshot.realtime.as_ref().unwrap().base();
    assert_eq!(rt.elec_percent, Some(85.0));
    assert_eq!(rt.charge_state, Some(ChargingState::Connected));
    assert_eq!(rt.charge_power, None, "charging stopped");
    let hv = snapshot.hvac.as_ref().unwrap().base();
    assert_eq!(hv.status, Some(HvacOverallStatus::Off));
    assert!(!hv.raw.contains_key("elecPercent"));
    assert_eq!(hv.cycle_choice, Some(AirCirculationMode::Internal));
    assert_eq!(hv.air_run_state, Some(1));
    assert_eq!(hv.main_setting_temp_new, Some(23.0));
    let ch = snapshot.charging.as_ref().unwrap().base();
    assert_eq!(ch.soc, Some(70));
    assert!(!ch.raw.contains_key("cycleModel"));
    assert_eq!(ch.connect_state, Some(0));
    assert_eq!(ch.charge_power, None);
}

#[test]
fn a_partial_that_carries_nothing_changes_nothing() {
    let (mut engine, notified) = counting_engine();
    engine.update_realtime(realtime(json!({"elecPercent": 70})), 0.0);
    engine.update_hvac(hvac(json!({"status": 1, "pm": 3})), 0.0);
    engine.register_projections(&[ProjectionSpec::new("hvac", "status", json!(2))], 0.0);
    let before = engine.snapshot().clone();
    notified.store(0, Ordering::SeqCst);

    // Unknown keys, sentinels, values at their default, a section never seen
    // (charging): no rebuild, and the expired projection is not cleaned out.
    engine.patch_status_push(
        Some(realtime(
            json!({"foo": 1, "elecPercent": -1, "onlineState": -1, "tempInCar": -129}),
        )),
        Some(hvac(json!({"status": 0, "stearingWheelHeatState": 0}))),
        Some(charging(json!({"soc": 50}))),
        100.0,
    );
    assert_eq!(notified.load(Ordering::SeqCst), 0);
    assert_eq!(engine.snapshot(), &before);
    assert_eq!(engine.held_projections().len(), 1);
    engine.patch_status_push(None, None, None, 100.0);
    assert_eq!(notified.load(Ordering::SeqCst), 0);

    // One carried field rebuilds, which cleans the expired projection out.
    engine.patch_status_push(None, Some(hvac(json!({"pm": 4}))), None, 100.0);
    assert_eq!(notified.load(Ordering::SeqCst), 1);
    assert!(engine.held_projections().is_empty());
    assert_eq!(
        engine.snapshot().hvac.as_ref().unwrap().base().pm,
        Some(4.0)
    );
}

#[test]
fn section_patches_from_rust() {
    let base = realtime(json!({
        "elecPercent": 70, "chargeState": 1, "chargePower": 5.1, "chargePowerUnit": "kW",
        "onlineState": 1, "stearingWheelHeatState": 1, "leftFrontDoorLock": 2,
    }));
    let patch = |partial: Value| VehicleRealtimeData::build_section_patch(&base, realtime(partial));

    // Nothing carried: unknown keys, sentinels, placeholders, `raw` alone.
    for partial in [
        json!({}),
        json!({"foo": 1}),
        json!({"onlineState": -1, "stearingWheelHeatState": 0, "elecPercent": "--"}),
        json!({"raw": {"own": 1}}),
    ] {
        assert!(patch(partial).is_none());
    }

    // A value that is not a member reads ONLINE and is carried; ON (-1) is a
    // reading of the steering wheel, not a placeholder; a lock at 0 is UNAVAILABLE.
    let patched = patch(json!({
        "onlineState": 5, "stearingWheelHeatState": -1, "leftFrontDoorLock": 0, "extra": true,
    }))
    .unwrap();
    assert_eq!(patched.online_state, OnlineState::Online);
    assert_eq!(
        patched.steering_wheel_heat_state,
        Some(StearingWheelHeat::On)
    );
    assert_eq!(patched.left_front_door_lock, Some(LockState::Unavailable));
    assert_eq!(patched.elec_percent, Some(70.0));
    assert_eq!(
        patched.raw.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "elecPercent",
            "chargeState",
            "chargePower",
            "chargePowerUnit",
            "onlineState",
            "stearingWheelHeatState",
            "leftFrontDoorLock",
            "extra",
        ],
        "known keys keep their place, new ones follow"
    );
    assert_eq!(patched.raw["onlineState"], json!(5));

    // The charge power: cleared when charging stops, kept when the partial brings one.
    let stopped = patch(json!({"chargeState": 15})).unwrap();
    assert_eq!(
        (stopped.charge_power, stopped.charge_power_unit),
        (None, None)
    );
    let other_state = patch(json!({"chargingState": 0, "chargePowerUnit": "W"})).unwrap();
    assert_eq!(
        other_state.charge_power_unit, None,
        "the unit goes with the power"
    );
    let still = patch(json!({"chargeState": 15, "chargePower": 1.5})).unwrap();
    assert_eq!(still.charge_power, Some(1.5));
    let charging_on = patch(json!({"chargeState": 1})).unwrap();
    assert_eq!(charging_on.charge_power, Some(5.1));

    // Charging: `vin` is never patched and does not count as carried.
    let status = charging(json!({"vin": "TESTVIN0000000001", "soc": 70, "chargePower": 5.1}));
    assert!(
        ChargingStatus::build_section_patch(&status, charging(json!({"vin": "TESTVIN0000000002"})))
            .is_none()
    );
    let patched = ChargingStatus::build_section_patch(
        &status,
        charging(json!({"vin": "TESTVIN0000000002", "chargingState": 0})),
    )
    .unwrap();
    assert_eq!(patched.vin, "TESTVIN0000000001");
    assert_eq!(patched.charge_power, None);

    // HVAC has no charge power to clear.
    let climate = hvac(json!({"status": 2, "pm": 3}));
    let patched = HvacStatus::build_section_patch(&climate, hvac(json!({"status": 1}))).unwrap();
    assert_eq!(patched.status, Some(HvacOverallStatus::On));
    assert_eq!(patched.pm, Some(3.0));
}

#[test]
fn patch_calls_check_their_arguments() {
    let call = |args: Value| parity::call("state._build_section_patch", &args);
    assert_eq!(
        call(json!({"section": "gps", "base": {}, "partial": {}}))
            .unwrap_err()
            .kind,
        "BadArguments"
    );
    assert_eq!(
        call(json!({"base": {}, "partial": {}})).unwrap_err().kind,
        "BadArguments"
    );
    assert_eq!(
        call(json!({"section": "hvac", "base": {"pm": 1}, "partial": {}})).unwrap(),
        Value::Null
    );
    let engine = |ops: Value| {
        parity::call(
            "state.VehicleStateEngine",
            &json!({"vin": "TESTVIN0000000001", "vehicle": {"vin": "TESTVIN0000000001"},
                    "start": 0.0, "ops": ops}),
        )
        .unwrap()
    };
    let answer = engine(json!([
        {"op": "update_realtime", "raw": {"elecPercent": 70}},
        {"op": "patch_realtime", "raw": {"elecPercent": 71}},
        {"op": "patch_status_push", "realtime": {"elecPercent": 72}, "hvac": []},
        {"op": "patch_status_push"},
    ]));
    assert_eq!(
        answer["steps"][1]["changes"]["realtime"]["elec_percent"],
        json!(71.0)
    );
    assert_eq!(answer["steps"][2]["error"], json!("ValidationError"));
    assert_eq!(answer["steps"][3]["notified"], json!(0));
}
