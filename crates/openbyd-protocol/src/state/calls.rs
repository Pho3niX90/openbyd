//! Parity calls for the state engine and the validators.
//!
//! The engine is stateful, so `state.VehicleStateEngine` runs a sequence of
//! operations on a fresh engine and reports the state after each one. See
//! `parity/groups/state.py` for the operation vocabulary.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Map, Value, json};

use super::engine::{
    DEFAULT_PROJECTION_TTL, FieldProjection, Projected, ProjectionSpec, SectionModel,
    VehicleSnapshot, VehicleStateEngine,
};
use super::patch::PatchSection;
use super::validators::{
    apply_gps_filters, apply_hvac_filters, apply_model_field_filters, apply_realtime_filters,
    guard_consumption_legs, guard_gps_coordinates, guard_tire_press_unit, has_valid_coordinates,
    updates_to_json,
};
use crate::coerce::IntEnum;
use crate::models::base::BydModel;
use crate::models::charging::ChargingStatus;
use crate::models::energy::EnergyConsumption;
use crate::models::gps::GpsInfo;
use crate::models::hvac::HvacStatus;
use crate::models::realtime::VehicleRealtimeData;
use crate::models::smart_charging::SmartChargingSchedule;
use crate::models::vehicle::Vehicle;
use crate::parity::{Args, Outcome, ParityError};

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    let args = Args(args);
    Some(match name {
        "state.apply_realtime_filters" => realtime_filters(&args),
        "state._apply_model_field_filters" => model_field_filters(&args),
        "state._guard_tire_press_unit" => tire_press_unit(&args),
        "state._guard_consumption_legs" => consumption_legs(&args),
        "state.apply_hvac_filters" => hvac_filters(&args),
        "state.guard_gps_coordinates" => gps_call(&args, guard_gps_coordinates),
        "state.apply_gps_filters" => gps_call(&args, apply_gps_filters),
        "state._has_valid_coordinates" => GpsInfo::parse(args.value("gps"), &Value::Null)
            .map(|gps| json!(has_valid_coordinates(&gps)))
            .map_err(Into::into),
        "state.ProjectionSpec" => projection_spec(&args).map(|spec| spec.to_json()),
        "state.FieldProjection" => field_projection(&args),
        "state._build_section_patch" => section_patch(&args),
        "state.VehicleSnapshot" => snapshot_call(&args),
        "state.VehicleStateEngine" => engine_sequence(&args),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

fn optional<M: BydModel>(raw: &Value, context: &Value) -> Result<Option<M>, ParityError> {
    if raw.is_null() {
        return Ok(None);
    }
    Ok(Some(M::parse(raw, context)?))
}

/// `previous` (or `null`) and `incoming`, each validated with its own context.
fn realtime_pair(
    args: &Args<'_>,
) -> Result<(Option<VehicleRealtimeData>, VehicleRealtimeData), ParityError> {
    let previous = optional(args.value("previous"), args.value("previous_context"))?;
    let incoming = VehicleRealtimeData::parse(args.value("incoming"), args.value("context"))?;
    Ok((previous, incoming))
}

/// Both models, for the private helpers that are only ever called with a baseline.
fn realtime_both(
    args: &Args<'_>,
) -> Result<(VehicleRealtimeData, VehicleRealtimeData), ParityError> {
    let previous =
        VehicleRealtimeData::parse(args.value("previous"), args.value("previous_context"))?;
    let incoming = VehicleRealtimeData::parse(args.value("incoming"), args.value("context"))?;
    Ok((previous, incoming))
}

fn realtime_filters(args: &Args<'_>) -> Outcome {
    let (previous, incoming) = realtime_pair(args)?;
    Ok(apply_realtime_filters(previous.as_ref(), incoming).dump())
}

fn model_field_filters(args: &Args<'_>) -> Outcome {
    let (previous, incoming) = realtime_both(args)?;
    Ok(updates_to_json(&apply_model_field_filters(
        &previous, &incoming,
    )))
}

fn tire_press_unit(args: &Args<'_>) -> Outcome {
    let (previous, incoming) = realtime_both(args)?;
    Ok(match guard_tire_press_unit(&previous, &incoming) {
        None => json!({"missing": true}),
        Some(unit) => json!({"unit": unit.map(IntEnum::value)}),
    })
}

fn consumption_legs(args: &Args<'_>) -> Outcome {
    let (previous, incoming) = realtime_both(args)?;
    Ok(updates_to_json(&guard_consumption_legs(
        &previous, &incoming,
    )))
}

fn hvac_filters(args: &Args<'_>) -> Outcome {
    let previous: Option<HvacStatus> = optional(args.value("previous"), &Value::Null)?;
    let incoming = HvacStatus::parse(args.value("incoming"), &Value::Null)?;
    Ok(apply_hvac_filters(previous.as_ref(), incoming).dump())
}

/// `base` and `partial` validated as `M`, patched, and dumped (`null` when
/// the partial carries nothing).
fn patched<M: PatchSection>(args: &Args<'_>, has_context: bool) -> Outcome {
    let context = |key| {
        if has_context {
            args.value(key)
        } else {
            &Value::Null
        }
    };
    let base = M::parse(args.value("base"), context("base_context"))?;
    let partial = M::parse(args.value("partial"), context("context"))?;
    Ok(M::build_section_patch(&base, partial).map_or(Value::Null, |model| model.dump()))
}

/// `state._build_section_patch`: `section` names the model of `base` and `partial`.
fn section_patch(args: &Args<'_>) -> Outcome {
    match args.str("section")? {
        "realtime" => patched::<VehicleRealtimeData>(args, true),
        "hvac" => patched::<HvacStatus>(args, false),
        "charging" => patched::<ChargingStatus>(args, false),
        other => Err(ParityError::bad_arguments(format!(
            "no patched section {other:?}"
        ))),
    }
}

type GpsFilter = for<'a> fn(Option<&'a GpsInfo>, Option<&'a GpsInfo>) -> Option<&'a GpsInfo>;

/// The kept fix and which argument it was: `"previous"`, `"incoming"` or `null`.
fn gps_call(args: &Args<'_>, filter: GpsFilter) -> Outcome {
    let previous: Option<GpsInfo> = optional(args.value("previous"), &Value::Null)?;
    let incoming: Option<GpsInfo> = optional(args.value("incoming"), &Value::Null)?;
    let kept = filter(previous.as_ref(), incoming.as_ref());
    let from = match kept {
        None => Value::Null,
        Some(kept) if incoming.as_ref().is_some_and(|i| std::ptr::eq(i, kept)) => {
            json!("incoming")
        }
        Some(_) => json!("previous"),
    };
    Ok(json!({"result": kept.map_or(Value::Null, BydModel::dump), "from": from}))
}

// ---------------------------------------------------------------------------
// Data classes
// ---------------------------------------------------------------------------

fn number(args: &Args<'_>, key: &str) -> Result<f64, ParityError> {
    args.value(key)
        .as_f64()
        .ok_or_else(|| ParityError::bad_arguments(format!("{key} must be a number")))
}

fn projection_spec(args: &Args<'_>) -> Result<ProjectionSpec, ParityError> {
    Ok(ProjectionSpec::new(
        args.str("section")?,
        args.str("field_name")?,
        args.value("expected_value").clone(),
    ))
}

fn field_projection(args: &Args<'_>) -> Outcome {
    let spec = projection_spec(args)?;
    let projection = FieldProjection {
        section: spec.section,
        field_name: spec.field_name,
        expected_value: spec.expected_value,
        command_id: args.str("command_id")?.to_owned(),
        created_at: number(args, "created_at")?,
        ttl: number(args, "ttl")?,
    };
    Ok(json!({
        "projection": projection.to_json(),
        "is_expired": projection.is_expired(number(args, "now")?),
    }))
}

fn section<M: SectionModel>(
    raw: &Value,
    context: &Value,
) -> Result<Option<Projected<M>>, ParityError> {
    Ok(optional::<M>(raw, context)?.map(|model| Projected::new(Arc::new(model))))
}

fn snapshot_call(args: &Args<'_>) -> Outcome {
    let snapshot = VehicleSnapshot {
        vehicle: Arc::new(Vehicle::parse(args.value("vehicle"), &Value::Null)?),
        realtime: section(args.value("realtime"), args.value("context"))?,
        hvac: section(args.value("hvac"), &Value::Null)?,
        gps: section(args.value("gps"), &Value::Null)?,
        charging: section(args.value("charging"), &Value::Null)?,
        energy: section(args.value("energy"), &Value::Null)?,
        charging_schedule: section(args.value("charging_schedule"), &Value::Null)?,
    };
    Ok(snapshot.dump())
}

// ---------------------------------------------------------------------------
// Engine sequences
// ---------------------------------------------------------------------------

const SECTIONS: [&str; 6] = [
    "realtime",
    "hvac",
    "gps",
    "charging",
    "energy",
    "charging_schedule",
];

/// The JSON text of `value`; unlike `Value` equality it tells `0.0` and `-0.0` apart.
fn json_text(value: &Value) -> String {
    value.to_string()
}

/// What changed between two snapshot dumps, section by section.
///
/// A section that became `null` is `null`; one that appeared is its whole
/// dump; otherwise only the fields whose JSON text changed. Unchanged
/// sections are left out. Comparing text keeps `1` and `1.0` apart.
fn changes(before: &Value, after: &Value) -> Value {
    let mut changed = Map::new();
    for name in SECTIONS {
        let (old, new) = (&before[name], &after[name]);
        if json_text(old) == json_text(new) {
            continue;
        }
        let entry = match (old, new) {
            (Value::Object(old), Value::Object(new)) => Value::Object(
                new.iter()
                    .filter(|(field, value)| {
                        old.get(*field).map(json_text) != Some(json_text(value))
                    })
                    .map(|(field, value)| (field.clone(), value.clone()))
                    .collect(),
            ),
            _ => new.clone(),
        };
        changed.insert(name.to_owned(), entry);
    }
    Value::Object(changed)
}

/// Parses an operation's payload as `M` and applies it, or reports the validation error.
fn apply<M: BydModel>(
    op: &Args<'_>,
    context: &Value,
    update: impl FnOnce(M),
) -> Result<(), ParityError> {
    let model = M::parse(op.value("raw"), context)?;
    update(model);
    Ok(())
}

/// The partial models of a `patch_status_push` operation, validated in the
/// order realtime, HVAC, charging; a `null` one is not given.
type StatusPush = (
    Option<VehicleRealtimeData>,
    Option<HvacStatus>,
    Option<ChargingStatus>,
);

fn status_push(op: &Args<'_>) -> Result<StatusPush, ParityError> {
    Ok((
        optional(op.value("realtime"), op.value("context"))?,
        optional(op.value("hvac"), &Value::Null)?,
        optional(op.value("charging"), &Value::Null)?,
    ))
}

fn engine_sequence(args: &Args<'_>) -> Outcome {
    let vehicle = Vehicle::parse(args.value("vehicle"), &Value::Null)?;
    let projection_ttl = match args.value("projection_ttl") {
        Value::Null => DEFAULT_PROJECTION_TTL,
        _ => number(args, "projection_ttl")?,
    };
    let mut now = number(args, "start")?;
    let ops = args
        .value("ops")
        .as_array()
        .ok_or_else(|| ParityError::bad_arguments("ops must be a list"))?;

    let notified = Arc::new(AtomicUsize::new(0));
    let mut engine = VehicleStateEngine::new(args.str("vin")?, vehicle, projection_ttl);
    let counter = Arc::clone(&notified);
    engine.set_on_state_changed(Some(Box::new(move |_, _| {
        counter.fetch_add(1, Ordering::SeqCst);
    })));

    let initial = engine.snapshot().dump();
    let mut before = initial.clone();
    let mut steps = Vec::with_capacity(ops.len());
    for op in ops {
        notified.store(0, Ordering::SeqCst);
        let op = Args(op);
        let mut step = Map::new();
        let applied = match op.str("op")? {
            "update_realtime" => apply(&op, op.value("context"), |data| {
                engine.update_realtime(data, now);
            }),
            "update_hvac" => apply(&op, &Value::Null, |data| engine.update_hvac(data, now)),
            "update_gps" => apply(&op, &Value::Null, |data| engine.update_gps(data, now)),
            "update_charging" => apply(&op, &Value::Null, |data: ChargingStatus| {
                engine.update_charging(data, now);
            }),
            "update_energy" => apply(&op, &Value::Null, |data: EnergyConsumption| {
                engine.update_energy(data, now);
            }),
            "update_charging_schedule" => {
                apply(&op, &Value::Null, |data: SmartChargingSchedule| {
                    engine.update_charging_schedule(data, now);
                })
            }
            "patch_realtime" => apply(&op, op.value("context"), |partial| {
                engine.patch_realtime(partial, now);
            }),
            "patch_status_push" => status_push(&op).map(|(realtime, hvac, charging)| {
                engine.patch_status_push(realtime, hvac, charging, now);
            }),
            "register_projections" => {
                let specs = op
                    .value("specs")
                    .as_array()
                    .ok_or_else(|| ParityError::bad_arguments("specs must be a list"))?
                    .iter()
                    .map(|spec| projection_spec(&Args(spec)))
                    .collect::<Result<Vec<_>, _>>()?;
                step.insert(
                    "result".to_owned(),
                    json!(engine.register_projections(&specs, now)),
                );
                Ok(())
            }
            "rollback_projections" => {
                engine.rollback_projections(op.str("command_id")?, now);
                Ok(())
            }
            "advance" => {
                now += number(&op, "seconds")?;
                Ok(())
            }
            "read" => Ok(()),
            other => {
                return Err(ParityError::bad_arguments(format!("unknown op {other:?}")));
            }
        };
        if let Err(error) = applied {
            step.insert("error".to_owned(), json!(error.kind));
        }
        let after = engine.snapshot().dump();
        step.insert("changes".to_owned(), changes(&before, &after));
        step.insert(
            "active".to_owned(),
            Value::Array(
                engine
                    .active_projections(now)
                    .iter()
                    .map(FieldProjection::to_json)
                    .collect(),
            ),
        );
        step.insert("held".to_owned(), json!(engine.held_projections().len()));
        step.insert(
            "notified".to_owned(),
            json!(notified.load(Ordering::SeqCst)),
        );
        steps.push(Value::Object(step));
        before = after;
    }
    Ok(json!({"initial": initial, "steps": steps}))
}
