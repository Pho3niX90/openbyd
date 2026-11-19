//! Runs every recorded parity vector.
//!
//! `parity/generate.py` records what pyBYD answers for a list of calls and
//! writes one file per group into `tests/vectors/`. This test replays each
//! call against the Rust port and compares.

use std::fs;
use std::path::Path;

use openbyd_client::parity;
use serde_json::Value;

#[test]
fn recorded_vectors_match_pybyd() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
    let mut files: Vec<_> = fs::read_dir(&directory)
        .expect("tests/vectors exists")
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no vector files in {}",
        directory.display()
    );

    let mut failures = Vec::new();
    let mut total = 0;
    for file in &files {
        let text = fs::read_to_string(file).expect("vector file is readable");
        let document: Value = serde_json::from_str(&text).expect("vector file is JSON");
        let group = document["group"].as_str().expect("group is a string");
        let cases = document["cases"].as_array().expect("cases is a list");
        assert!(!cases.is_empty(), "group {group} has no cases");
        for (index, case) in cases.iter().enumerate() {
            total += 1;
            let name = case["call"].as_str().expect("call is a string");
            let got = parity::call(name, &case["args"]);
            let problem = match (case.get("error").and_then(Value::as_str), got) {
                (None, Ok(value)) if value == case["ok"] => continue,
                (None, Ok(value)) => format!("expected {}\n       got {value}", case["ok"]),
                (None, Err(error)) => format!("expected {}\n       got error {error}", case["ok"]),
                (Some(kind), Err(error)) if error.kind == kind => continue,
                (Some(kind), Err(error)) => format!("expected error {kind}, got error {error}"),
                (Some(kind), Ok(value)) => format!("expected error {kind}, got {value}"),
            };
            failures.push(format!(
                "{group} #{index} {name} {}\n  {problem}",
                case["args"]
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {total} vectors differ from pyBYD:\n{}",
        failures.len(),
        failures
            .iter()
            .take(25)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
