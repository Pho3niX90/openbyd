//! A model's key plan: which keys its fields look up, recorded once.
//!
//! [`Fields::cleaned_with_changes`](super::Fields::cleaned_with_changes)
//! builds a hash table of the whole cleaned dict for every parse, and every
//! field read hashes its key (and its camelCase alias) again. A model reads
//! the same fields in the same order every time, so the first successful
//! parse records them ([`Plan`]); every later parse makes one pass over the
//! reply's keys, putting each value a field can ask for in a numbered slot
//! ([`Recorded::fill`]), and the n-th field read is then the n-th recorded
//! one: a few array reads, no hashing.
//!
//! A read the recording does not have at that position (a model whose reads
//! depend on the data, or a duplicated name literal) is looked up elsewhere
//! in the recording, or else answered key by key from the slots and, for a
//! key no recorded field uses, from the full table built once per parse
//! ([`Planned::table`]). Every answer is the one the table gives.

use std::cell::{Cell, OnceCell};
use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::{Map, Value};

use super::{Field, KeyHashing, camel_alias, clean_table, is_sentinel};

/// A model's key aliases (`_KEY_ALIASES`) and the record of its field reads,
/// taken from its first parse in which every field was read and none failed.
///
/// One per model, in a `static`, given to [`super::Fields::planned`]:
///
/// ```ignore
/// static PLAN: Plan = Plan::new(KEY_ALIASES);
/// let mut fields = Fields::planned(&PLAN, raw, &[])?;
/// ```
pub struct Plan {
    aliases: &'static [(&'static str, &'static str)],
    recorded: OnceLock<Recorded>,
}

impl Plan {
    /// A plan for a model with these key aliases (old key, new key), applied
    /// in order as `BydBaseModel._clean_dict` applies them.
    pub const fn new(aliases: &'static [(&'static str, &'static str)]) -> Self {
        Self {
            aliases,
            recorded: OnceLock::new(),
        }
    }

    pub(super) fn aliases(&self) -> &'static [(&'static str, &'static str)] {
        self.aliases
    }

    pub(super) fn recorded(&self) -> Option<&Recorded> {
        self.recorded.get()
    }

    /// Keeps `reads` as the recording, unless another parse got there first
    /// (the two recordings are the same).
    pub(super) fn record(&self, reads: &[Field]) {
        let _ = self.recorded.set(Recorded::new(self.aliases, reads));
    }
}

/// Whether two fields are the same read: the same literal (the usual case,
/// compared by address) or the same name and aliases.
fn same_field(a: Field, b: Field) -> bool {
    (std::ptr::eq(a.name, b.name) && std::ptr::eq(a.aliases, b.aliases))
        || (a.name == b.name && a.aliases == b.aliases)
}

/// The keys a field looks up, in the order [`super::Fields`] tries them: its
/// explicit aliases, or its camelCase alias when that differs from the name,
/// and then the name.
fn keys_of(field: Field) -> impl Iterator<Item = &'static str> {
    let camel = (field.aliases.is_empty() && camel_alias(field.name) != field.name)
        .then(|| camel_alias(field.name));
    field
        .aliases
        .iter()
        .copied()
        .chain(camel)
        .chain(std::iter::once(field.name))
}

/// One recorded read: the field and the slots of its keys, in lookup order.
struct Spec {
    field: Field,
    slots: Box<[usize]>,
}

/// The reads of one model, and a slot for every key they or the key aliases use.
pub(super) struct Recorded {
    specs: Vec<Spec>,
    index: HashMap<&'static str, usize, KeyHashing>,
    raw: usize,
    aliases: Vec<(usize, usize)>,
}

impl Recorded {
    fn new(aliases: &'static [(&'static str, &'static str)], reads: &[Field]) -> Self {
        let mut index: HashMap<&'static str, usize, KeyHashing> =
            HashMap::with_hasher(KeyHashing::Fast);
        let mut slot = |key: &'static str| -> usize {
            let next = index.len();
            *index.entry(key).or_insert(next)
        };
        let raw = slot("raw");
        let aliases = aliases
            .iter()
            .map(|&(old, new)| (slot(old), slot(new)))
            .collect();
        let specs = reads
            .iter()
            .map(|&field| Spec {
                field,
                slots: keys_of(field).map(&mut slot).collect(),
            })
            .collect();
        Self {
            specs,
            index,
            raw,
            aliases,
        }
    }

    /// The value of every slot: what `clean_table(entries, changes, whole,
    /// aliases)` holds under the slot's key (sentinels included).
    fn fill<'a>(
        &self,
        entries: &'a Map<String, Value>,
        changes: &'a [(&'a str, Value)],
        whole: &'a Value,
    ) -> Vec<Option<&'a Value>> {
        let mut found = vec![None; self.index.len()];
        let changes = changes.iter().map(|(key, value)| (*key, value));
        // Changes are laid over the entries: a later key replaces an earlier one.
        for (key, value) in entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
            .chain(changes)
        {
            if let Some(&slot) = self.index.get(key) {
                found[slot] = Some(value);
            }
        }
        // Whether the dict has its own `raw` is decided before the aliases run.
        let own_raw = found[self.raw].is_some();
        for &(old, new) in &self.aliases {
            if found[new].is_none()
                && let Some(value) = found[old].take()
            {
                found[new] = Some(value);
            }
        }
        if !own_raw {
            found[self.raw] = Some(whole);
        }
        found
    }
}

/// The keys of one parse that follows a recorded plan.
pub(super) struct Planned<'a> {
    recorded: &'a Recorded,
    found: Vec<Option<&'a Value>>,
    /// The position of the next read in the recording.
    cursor: Cell<usize>,
    entries: &'a Map<String, Value>,
    changes: &'a [(&'a str, Value)],
    whole: &'a Value,
    aliases: &'static [(&'static str, &'static str)],
    /// The full table, for a key the plan has no slot for; built at most once.
    table: OnceCell<HashMap<&'a str, &'a Value, KeyHashing>>,
}

impl<'a> Planned<'a> {
    pub(super) fn new(
        recorded: &'a Recorded,
        aliases: &'static [(&'static str, &'static str)],
        entries: &'a Map<String, Value>,
        changes: &'a [(&'a str, Value)],
        whole: &'a Value,
    ) -> Self {
        Self {
            recorded,
            found: recorded.fill(entries, changes, whole),
            cursor: Cell::new(0),
            entries,
            changes,
            whole,
            aliases,
            table: OnceCell::new(),
        }
    }

    fn table(&self) -> &HashMap<&'a str, &'a Value, KeyHashing> {
        self.table.get_or_init(|| {
            counters::table_built();
            clean_table(self.entries, self.changes, self.whole, self.aliases)
        })
    }

    /// The value under `key`, or `None` if it is absent or a sentinel.
    pub(super) fn lookup(&self, key: &str) -> Option<&'a Value> {
        match self.recorded.index.get(key) {
            Some(&slot) => self.found[slot],
            None => self.table().get(key).copied(),
        }
        .filter(|value| !is_sentinel(value))
    }

    /// The value `field` reads, or `None`.
    pub(super) fn find(&self, field: Field) -> Option<&'a Value> {
        let specs = &self.recorded.specs;
        let at = self.cursor.get();
        let position = match specs.get(at) {
            Some(spec) if same_field(spec.field, field) => Some(at),
            _ => {
                counters::read_elsewhere();
                specs.iter().position(|spec| same_field(spec.field, field))
            }
        };
        match position {
            Some(position) => {
                self.cursor.set(position + 1);
                specs[position]
                    .slots
                    .iter()
                    .find_map(|&slot| self.found[slot].filter(|value| !is_sentinel(value)))
            }
            None => keys_of(field).find_map(|key| self.lookup(key)),
        }
    }
}

/// How often a followed plan could not answer from the recorded position,
/// for the tests: reads found elsewhere (or not at all) in the recording, and
/// full tables built.
#[cfg(test)]
pub(super) mod counters {
    use std::cell::Cell;

    thread_local! {
        static READS_ELSEWHERE: Cell<usize> = const { Cell::new(0) };
        static TABLES_BUILT: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn read_elsewhere() {
        READS_ELSEWHERE.set(READS_ELSEWHERE.get() + 1);
    }

    pub(crate) fn table_built() {
        TABLES_BUILT.set(TABLES_BUILT.get() + 1);
    }

    /// `(reads elsewhere, tables built)` on this thread since the last call.
    pub(crate) fn take() -> (usize, usize) {
        (READS_ELSEWHERE.replace(0), TABLES_BUILT.replace(0))
    }
}

#[cfg(not(test))]
mod counters {
    pub(super) fn read_elsewhere() {}
    pub(super) fn table_built() {}
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};

    use serde_json::{Value, json};

    use super::super::{BydModel, Field, Fields, aliased};
    use super::{Plan, counters};
    use crate::coerce::{self, ValidationError};
    use crate::models::{
        booking, charging, command_gating, control, energy, gps, hvac, latest_config,
        message_center, push_notification, realtime, smart_charging, standby, vehicle,
    };

    // -----------------------------------------------------------------
    // Every planned model, over every corpus payload
    // -----------------------------------------------------------------

    fn corpus_payloads() -> Vec<Value> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/corpus");
        let mut payloads = Vec::new();
        for entry in std::fs::read_dir(dir).expect("the corpus directory exists") {
            let path = entry.expect("a readable directory entry").path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let text = std::fs::read_to_string(&path).expect("a readable corpus file");
                let entries: Vec<Value> =
                    serde_json::from_str(&text).expect("corpus files are JSON");
                payloads.extend(entries.into_iter().map(|entry| entry["payload"].clone()));
            }
        }
        assert!(payloads.len() >= 20, "the corpus has payloads");
        payloads
    }

    /// Parses every payload twice with every context, and returns how many
    /// payloads parsed and the fallbacks of the second round.
    fn second_parse_fallbacks<M: BydModel>(payloads: &[Value]) -> (usize, (usize, usize)) {
        let contexts = [Value::Null, json!({"energy_type": {"EnergyType": 2}})];
        let mut parsed = 0;
        for payload in payloads {
            for context in &contexts {
                let _ = M::parse(payload, context);
            }
        }
        counters::take();
        for payload in payloads {
            for context in &contexts {
                // Both entry points: the owned parse skips the `raw` read.
                if M::parse(payload, context).is_ok() {
                    parsed += 1;
                }
                let _ = M::parse_owned(payload.clone(), context);
            }
        }
        (parsed, counters::take())
    }

    macro_rules! planned_models {
        ($($model:ty),+ $(,)?) => {{
            let payloads = corpus_payloads();
            let mut parsed = 0;
            $(
                let (count, fallbacks) = second_parse_fallbacks::<$model>(&payloads);
                assert_eq!(fallbacks, (0, 0), "{} fell back", stringify!($model));
                parsed += count;
            )+
            parsed
        }};
    }

    #[test]
    fn a_second_parse_of_every_corpus_payload_follows_the_plan() {
        let parsed = planned_models!(
            booking::ClimateBooking,
            booking::BookingList,
            charging::ChargingStatus,
            command_gating::CommandGateRule,
            command_gating::CommandGateVerdict,
            control::RemoteControlResult,
            control::CommandAck,
            control::ChargeChangeResult,
            control::CommandAckEvent,
            control::CommandLifecycleEvent,
            control::CommandAckDiagnostics,
            control::VerifyControlPasswordResponse,
            energy::EnergyConsumptionGraph,
            energy::CumulativeEnergyConsumption,
            energy::NearestEnergyConsumption,
            energy::EnergyConsumption,
            gps::GpsInfo,
            hvac::HvacStatus,
            latest_config::LatestConfigFunction,
            latest_config::VehicleLatestConfig,
            latest_config::VehicleCapabilities,
            message_center::VehicleMsgItemInfo,
            message_center::VehicleMsgListResponse,
            push_notification::PushSwitch,
            push_notification::PushNotificationState,
            realtime::VehicleRealtimeData,
            smart_charging::SmartChargeDto,
            smart_charging::SmartJourneyDto,
            smart_charging::SmartChargingSchedule,
            smart_charging::AcCurrentLimit,
            standby::StandbyPlanCommand,
            standby::StandbyPlan,
            standby::StandbyPlanList,
            standby::StandbyPlanRunResult,
            vehicle::EmpowerRange,
            vehicle::Vehicle,
        );
        // Most models accept most payloads (unknown keys are ignored).
        assert!(parsed > 1000, "only {parsed} parses succeeded");
    }

    // -----------------------------------------------------------------
    // The plan answers what the table answers
    // -----------------------------------------------------------------

    const ALIASES: &[(&str, &str)] = &[
        ("oldKey", "newKey"),
        ("first", "second"),
        ("second", "third"),
        ("x", "raw"),
        ("same", "same"),
    ];

    const fn plain(name: &'static str) -> Field {
        aliased(name, &[])
    }

    const READS: &[Field] = &[
        plain("new_key"),
        plain("old_key"),
        plain("first"),
        plain("second"),
        plain("third"),
        aliased("picked", &["choiceA", "choice_b"]),
        plain("plain"),
        plain("same"),
        plain("left_front_tire_pressure"),
    ];

    /// Every read of `READS` (in `order`), then `raw`, as answered.
    fn read_all(mut fields: Fields<'_>, order: &[usize]) -> Vec<Option<Value>> {
        let mut answers: Vec<Option<Value>> = order
            .iter()
            .map(|&index| fields.opt(READS[index], coerce::any))
            .collect();
        answers.push(Some(Value::Object(fields.raw())));
        answers.push(fields.finish().err().map(|error| json!(error.error_type)));
        answers
    }

    fn awkward_dicts() -> Vec<(Value, Vec<(&'static str, Value)>)> {
        let dicts = [
            json!({}),
            json!({"oldKey": 1, "newKey": 2}),
            json!({"oldKey": 1, "newKey": "--"}),
            json!({"oldKey": 1, "newKey": null}),
            json!({"oldKey": " nan ", "new_key": 5}),
            json!({"first": 1}),
            json!({"first": 1, "second": 2}),
            json!({"first": 1, "third": "NaN"}),
            json!({"second": "x", "third": 3}),
            json!({"x": {"from": "x"}}),
            json!({"x": {"from": "x"}, "raw": {"own": 1}}),
            json!({"raw": "--", "plain": 1}),
            json!({"raw": [1], "plain": 1}),
            json!({"same": 4, "unknown": 1, "other": [1, 2]}),
            json!({"choiceA": "--", "choice_b": 2, "picked": 3}),
            json!({"choice_b": null, "picked": 3}),
            json!({"leftFrontTirePressure": 2.5, "left_front_tire_pressure": 1}),
            json!({"leftFrontTirePressure": "", "left_front_tire_pressure": 1}),
        ];
        let changes: [Vec<(&'static str, Value)>; 4] = [
            vec![],
            vec![("plain", json!("changed"))],
            vec![("newKey", json!("--")), ("first", json!(7))],
            vec![("raw", json!({"by": "change"})), ("unknown", json!(1))],
        ];
        let mut cases = Vec::new();
        for dict in &dicts {
            for change in &changes {
                cases.push((dict.clone(), change.clone()));
            }
        }
        cases
    }

    #[test]
    fn planned_reads_answer_what_the_table_answers() {
        static PLAN: Plan = Plan::new(ALIASES);
        let in_order: Vec<usize> = (0..READS.len()).collect();
        let reversed: Vec<usize> = in_order.iter().rev().copied().collect();
        let repeated = [0, 0, 3, 8, 8, 1];
        // The first parse records the plan.
        let first = json!({"plain": 1});
        let recorded = read_all(Fields::planned(&PLAN, &first, &[]).unwrap(), &in_order);
        assert!(PLAN.recorded().is_some());
        assert_eq!(
            recorded,
            read_all(Fields::cleaned(&first, ALIASES).unwrap(), &in_order)
        );
        for (dict, changes) in awkward_dicts() {
            for order in [&in_order[..], &reversed, &repeated] {
                counters::take();
                let planned = read_all(Fields::planned(&PLAN, &dict, &changes).unwrap(), order);
                let fallbacks = counters::take();
                let table = read_all(
                    Fields::cleaned_with_changes(&dict, &changes, ALIASES).unwrap(),
                    order,
                );
                assert_eq!(planned, table, "{dict} with {changes:?}, order {order:?}");
                if order == in_order {
                    assert_eq!(fallbacks, (0, 0), "{dict}");
                }
            }
        }
        assert!(Fields::planned(&PLAN, &json!([1]), &[]).is_err());
    }

    #[test]
    fn reads_the_recording_lacks_are_answered_with_one_table_per_parse() {
        static PLAN: Plan = Plan::new(ALIASES);
        let dict = json!({"oldKey": 1, "unrecorded": 2, "otherKey": "--", "camelName": 3});
        read_all(Fields::planned(&PLAN, &dict, &[]).unwrap(), &[0, 1]);
        counters::take();
        let mut fields = Fields::planned(&PLAN, &dict, &[]).unwrap();
        let mut table = Fields::cleaned(&dict, ALIASES).unwrap();
        for field in [
            "unrecorded",
            "other_key",
            "camel_name",
            "missing",
            "new_key",
        ] {
            assert_eq!(
                fields.opt(field, coerce::any),
                table.opt(field, coerce::any),
                "{field}"
            );
        }
        let (elsewhere, tables) = counters::take();
        assert_eq!(tables, 1, "the table is built once");
        assert!(elsewhere >= 4);
    }

    #[test]
    fn a_duplicated_name_literal_is_the_same_read() {
        static PLAN: Plan = Plan::new(&[]);
        let dict = json!({"someName": 1});
        let name = String::from("some_name");
        // A name with the same text at another address (and so, after
        // `Box::leak`, never the literal the plan recorded).
        let copy: &'static str = Box::leak(name.into_boxed_str());
        let mut fields = Fields::planned(&PLAN, &dict, &[]).unwrap();
        assert_eq!(fields.opt("some_name", coerce::int), Some(1));
        fields.finish().unwrap();
        counters::take();
        let mut fields = Fields::planned(&PLAN, &dict, &[]).unwrap();
        assert_eq!(fields.opt(copy, coerce::int), Some(1));
        fields.finish().unwrap();
        assert_eq!(counters::take(), (0, 0));
    }

    // -----------------------------------------------------------------
    // Recording
    // -----------------------------------------------------------------

    /// One parse of a three-field model, with `third` required.
    fn parse_three(
        plan: &'static Plan,
        dict: &Value,
    ) -> Result<Vec<Option<Value>>, ValidationError> {
        let mut fields = Fields::planned(plan, dict, &[])?;
        let first = fields.opt("first", |value| match value {
            Value::String(text) if text == "escape" => {
                Err(ValidationError::raised("OverflowError", "int"))
            }
            other => coerce::any(other),
        });
        let second = fields.opt("second", coerce::int).map(Value::from);
        let third: Value = fields.required("third", coerce::any);
        fields.finish()?;
        Ok(vec![first, second, Some(third)])
    }

    #[test]
    fn a_failed_or_cut_short_parse_records_nothing() {
        static PLAN: Plan = Plan::new(&[]);
        // `third` is missing: a validation error.
        assert!(parse_three(&PLAN, &json!({"first": 1})).is_err());
        assert!(PLAN.recorded().is_none());
        // An escaping error stops the reads.
        let error = parse_three(&PLAN, &json!({"first": "escape", "third": 1})).unwrap_err();
        assert!(error.is_escaping());
        assert!(PLAN.recorded().is_none());
        // A bad value is an error too.
        assert!(parse_three(&PLAN, &json!({"second": "x", "third": 1})).is_err());
        assert!(PLAN.recorded().is_none());
        // The first good parse records; later parses, good or bad, are right.
        let good = json!({"first": 1, "second": 2, "third": 3});
        let answers = parse_three(&PLAN, &good).unwrap();
        assert!(PLAN.recorded().is_some());
        counters::take();
        assert_eq!(parse_three(&PLAN, &good).unwrap(), answers);
        assert!(parse_three(&PLAN, &json!({"first": 1})).is_err());
        assert_eq!(
            parse_three(&PLAN, &json!({"third": "--", "second": "4"}))
                .map_err(|error| error.error_type),
            Err("missing")
        );
        let error = parse_three(&PLAN, &json!({"first": "escape", "third": 1})).unwrap_err();
        assert!(error.is_escaping());
        assert_eq!(counters::take(), (0, 0));
    }

    #[test]
    fn two_threads_racing_the_first_parse() {
        static PLAN: Plan = Plan::new(&[("old", "third")]);
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|index| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let dict = json!({"first": index, "second": 2, "old": "o"});
                    barrier.wait();
                    let answers = parse_three(&PLAN, &dict).unwrap();
                    assert_eq!(
                        answers,
                        vec![Some(json!(index)), Some(json!(2)), Some(json!("o"))]
                    );
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("no thread panicked");
        }
        assert!(PLAN.recorded().is_some());
        counters::take();
        let answers = parse_three(&PLAN, &json!({"first": 9, "third": 3, "old": 0})).unwrap();
        assert_eq!(answers, vec![Some(json!(9)), None, Some(json!(3))]);
        assert_eq!(counters::take(), (0, 0));
    }
}
