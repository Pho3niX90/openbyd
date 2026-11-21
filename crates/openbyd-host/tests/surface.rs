//! `bindings/surface.json`, which the packages' typed layers are generated
//! from, describes exactly the methods and events this crate has.
//!
//! A method added here without a line in `bindings/spec.py` (or the other
//! way round) fails this test, before any package is built.

use std::collections::BTreeSet;
use std::path::Path;

use openbyd_host::{ASYNC_METHODS, EVENTS, SYNC_METHODS};
use serde_json::Value;

/// Methods every package writes by hand, so the surface does not list them:
/// the client's lifecycle, the answer to a question, and the car's factory.
const HAND_WRITTEN: [&str; 4] = ["start", "close", "resolve", "get_car"];

/// Events that are not asked for by name: a car's state changes (asked for
/// by `get_car`) and the three questions (asked for by an option).
const UNLISTED_EVENTS: [&str; 4] = ["state_changed", "gate_policy", "pin_resolver", "relogin"];

fn surface() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bindings/surface.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).expect("surface.json is JSON")
}

/// Every method of the surface as `(wire name, sync)`.
fn described(surface: &Value) -> Vec<(String, bool)> {
    let methods = |list: &Value| -> Vec<(String, bool)> {
        list.as_array()
            .expect("a list of methods")
            .iter()
            .map(|method| {
                (
                    method["wire"].as_str().expect("wire").to_owned(),
                    method["sync"].as_bool().expect("sync"),
                )
            })
            .collect()
    };
    let mut all = methods(&surface["client"]);
    all.extend(methods(&surface["car"]));
    for feature in surface["features"].as_array().expect("features") {
        all.extend(methods(&feature["methods"]));
    }
    all
}

fn set<'a>(names: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    names.into_iter().map(str::to_owned).collect()
}

#[test]
fn the_surface_lists_the_methods_this_crate_dispatches() {
    let described = described(&surface());
    let sync: BTreeSet<String> = described
        .iter()
        .filter(|(_, sync)| *sync)
        .map(|(wire, _)| wire.clone())
        .collect();
    let waiting: BTreeSet<String> = described
        .iter()
        .filter(|(_, sync)| !*sync)
        .map(|(wire, _)| wire.clone())
        .collect();
    assert_eq!(
        sync.len() + waiting.len(),
        described.len(),
        "a method is described twice"
    );

    let hand_written = set(HAND_WRITTEN);
    let dispatched_sync: BTreeSet<String> = set(SYNC_METHODS)
        .difference(&hand_written)
        .cloned()
        .collect();
    let dispatched_waiting: BTreeSet<String> = set(ASYNC_METHODS)
        .difference(&hand_written)
        .cloned()
        .collect();
    assert_eq!(
        sync, dispatched_sync,
        "sync methods: spec.py against client.rs"
    );
    assert_eq!(
        waiting, dispatched_waiting,
        "methods that wait: spec.py against client.rs"
    );
    for name in HAND_WRITTEN {
        assert!(
            SYNC_METHODS.contains(&name) || ASYNC_METHODS.contains(&name),
            "{name} is not dispatched"
        );
    }
}

#[test]
fn the_surface_lists_the_events_this_crate_sends() {
    let surface = surface();
    let described: BTreeSet<String> = surface["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["wire"].as_str().expect("wire").to_owned())
        .collect();
    let sent: BTreeSet<String> = set(EVENTS).union(&set(UNLISTED_EVENTS)).cloned().collect();
    assert_eq!(described, sent, "events: spec.py against client.rs");
}

#[test]
fn the_surface_is_for_this_version_and_this_pybyd() {
    let surface = surface();
    assert_eq!(surface["version"], openbyd_host::VERSION);
    let pinned = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parity/UPSTREAM_COMMIT");
    let pinned = std::fs::read_to_string(pinned).expect("parity/UPSTREAM_COMMIT");
    assert_eq!(
        surface["pybyd_commit"],
        pinned.trim(),
        "run `python bindings/generate.py` after moving the pin"
    );
}
