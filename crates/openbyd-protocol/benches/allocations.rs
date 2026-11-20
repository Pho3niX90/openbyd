//! Heap allocations per call of the realtime hot paths: how many blocks and
//! bytes one call allocates (freed or not), counted by `dhat`'s allocator.
//! See `README.md` in this directory.
//!
//! `cargo bench -p openbyd-protocol --bench allocations`
//!
//! Counts, not times: `dhat` records a backtrace per allocation, so the
//! calls run far slower than normal. The numbers are exact and stable, so
//! they compare across machines and runs.

use std::hint::black_box;
use std::path::PathBuf;

use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::realtime::VehicleRealtimeData;
use openbyd_protocol::pyjson;
use serde_json::{Value, json};

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Calls per row; the counts are the same every call once a model's key plan is recorded.
const CALLS: u64 = 20;

/// The realtime corpus payloads, by note.
fn realtime_corpus() -> Vec<(String, Value)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../parity/corpus/VehicleRealtimeData.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path:?}: {error}"));
    let entries: Vec<Value> = serde_json::from_str(&text).expect("the corpus is JSON");
    entries
        .into_iter()
        .map(|entry| {
            (
                entry["note"].as_str().unwrap_or("").to_owned(),
                entry["payload"].clone(),
            )
        })
        .collect()
}

/// Prints the blocks and bytes one call of `routine` allocates.
fn row<T>(name: &str, mut routine: impl FnMut() -> T) {
    // The first call records the model's key plan; it is not counted.
    black_box(routine());
    let before = dhat::HeapStats::get();
    for _ in 0..CALLS {
        black_box(routine());
    }
    let after = dhat::HeapStats::get();
    let blocks = (after.total_blocks - before.total_blocks) / CALLS;
    let bytes = (after.total_bytes - before.total_bytes) / CALLS;
    println!("{name:<48} {blocks:>6} allocations {bytes:>8} bytes");
}

fn main() {
    let _profiler = dhat::Profiler::builder().testing().build();
    let corpus = realtime_corpus();
    let full = corpus
        .iter()
        .find(|(note, _)| note == "every field present")
        .map(|(_, payload)| payload.clone())
        .expect("the realtime corpus has the full payload");
    let hybrid = json!({"energy_type": {"EnergyType": 2}});

    row("realtime/full parse", || {
        VehicleRealtimeData::parse(black_box(&full), &Value::Null).unwrap()
    });
    row("realtime/full parse, HYBRID context", || {
        VehicleRealtimeData::parse(black_box(&full), &hybrid).unwrap()
    });
    row("realtime/full parse+dump (headline)", || {
        VehicleRealtimeData::parse(black_box(&full), &Value::Null)
            .unwrap()
            .dump()
    });
    row("realtime/full clone (the input of the next row)", || {
        black_box(&full).clone()
    });
    row("realtime/full parse_owned+into_dump", || {
        VehicleRealtimeData::parse_owned(black_box(full.clone()), &Value::Null)
            .unwrap()
            .into_dump()
    });
    let parsed = VehicleRealtimeData::parse(&full, &Value::Null).unwrap();
    row("realtime/full dump", || black_box(&parsed).dump());
    let text = pyjson::dumps_compact(&full);
    row("pyjson/loads (realtime)", || {
        pyjson::loads(black_box(&text)).unwrap()
    });
    row("pyjson/dumps_compact (realtime)", || {
        pyjson::dumps_compact(black_box(&full))
    });
    for (index, (_, payload)) in corpus.iter().enumerate() {
        row(&format!("realtime/corpus[{index}] parse, HYBRID"), || {
            VehicleRealtimeData::parse(black_box(payload), &hybrid).unwrap()
        });
    }
}
