//! Rendering: JSON for programs, tables for people.

use clap::ValueEnum;
use serde_json::Value;

/// `--format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// JSON (pretty on a terminal, compact otherwise).
    Json,
    /// A human-readable table.
    Table,
}

/// Renders `value` as JSON text.
pub fn render_json(value: &Value, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(value).expect("JSON values serialise")
    } else {
        value.to_string()
    }
}

/// Renders `value` as a table: a list of flat objects becomes one row each
/// under a header of its keys; anything else becomes two columns, the path
/// of each value (`vehicles[0].realtime.elec_percent`) and the value.
pub fn render_table(value: &Value) -> String {
    match columns_of(value) {
        Some((headers, rows)) => grid(&headers, &rows),
        None => {
            let mut rows = Vec::new();
            flatten("", value, &mut rows);
            let rows: Vec<Vec<String>> = rows
                .into_iter()
                .map(|(path, text)| vec![path, text])
                .collect();
            grid(&["FIELD".to_owned(), "VALUE".to_owned()], &rows)
        }
    }
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("-".to_owned()),
        Value::String(text) => Some(text.clone()),
        Value::Bool(_) | Value::Number(_) => Some(value.to_string()),
        Value::Array(items) if items.is_empty() => Some("[]".to_owned()),
        Value::Object(map) if map.is_empty() => Some("{}".to_owned()),
        _ => None,
    }
}

/// The headers and rows of a non-empty list of objects whose values are all scalars.
fn columns_of(value: &Value) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    let items = value.as_array().filter(|items| !items.is_empty())?;
    let first = items[0].as_object()?;
    let headers: Vec<String> = first.keys().cloned().collect();
    let mut rows = Vec::new();
    for item in items {
        let object = item.as_object()?;
        if object.len() != headers.len() {
            return None;
        }
        let mut row = Vec::new();
        for header in &headers {
            row.push(scalar(object.get(header)?)?);
        }
        rows.push(row);
    }
    Some((headers.iter().map(|h| h.to_uppercase()).collect(), rows))
}

fn flatten(prefix: &str, value: &Value, rows: &mut Vec<(String, String)>) {
    if let Some(text) = scalar(value) {
        rows.push((prefix.to_owned(), text));
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, item, rows);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                flatten(&format!("{prefix}[{index}]"), item, rows);
            }
        }
        _ => {}
    }
}

fn grid(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        let mut out = String::new();
        for (index, (cell, width)) in cells.iter().zip(&widths).enumerate() {
            if index > 0 {
                out.push_str("  ");
            }
            out.push_str(cell);
            if index + 1 < cells.len() {
                out.push_str(&" ".repeat(width - cell.chars().count()));
            }
        }
        out
    };
    let mut lines = vec![line(headers)];
    lines.extend(rows.iter().map(|row| line(row)));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn json_is_compact_or_pretty() {
        let value = json!({"a": [1, 2]});
        assert_eq!(render_json(&value, false), r#"{"a":[1,2]}"#);
        assert!(render_json(&value, true).contains("\n  \"a\""));
    }

    #[test]
    fn a_list_of_flat_objects_is_a_table() {
        let value = json!([
            {"vin": "V1", "alias": "", "n": 3, "ok": true, "x": null},
            {"vin": "LONGER", "alias": "car", "n": 10, "ok": false, "x": null},
        ]);
        let text = render_table(&value);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "VIN     ALIAS  N   OK     X");
        assert_eq!(lines[1], "V1             3   true   -");
        assert_eq!(lines[2], "LONGER  car    10  false  -");
    }

    #[test]
    fn anything_else_lists_paths_and_values() {
        let value = json!({"a": {"b": [1, {"c": "x"}], "e": [], "f": {}}, "n": null});
        let text = render_table(&value);
        assert_eq!(
            text,
            "FIELD     VALUE
a.b[0]    1
a.b[1].c  x
a.e       []
a.f       {}
n         -"
        );
    }

    #[test]
    fn ragged_lists_fall_back_to_paths() {
        for value in [
            json!([]),
            json!([1, 2]),
            json!([{"a": 1}, {"a": 1, "b": 2}]),
            json!([{"a": {"x": 1}}]),
            json!([{"a": 1}, {"b": 1}]),
        ] {
            assert!(render_table(&value).starts_with("FIELD"), "{value}");
        }
    }
}
