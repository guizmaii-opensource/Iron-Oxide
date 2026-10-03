//! Differential test: the Rust parser and validator against the committed JSON Schema.
//!
//! Starting from valid documents, every node is replaced, one at a time, by each value of a
//! candidate list chosen around the `limits` and the hand-written formats (slugs, tempo, demo
//! URLs, weights per unit, percentages). Objects are also replaced by the array of their values,
//! lose each key, or gain an unknown one. For each document:
//!
//! - the schema accepts it if and only if the app accepts it, **or** the app rejects it only for
//!   rules that span several values (rotation references, supersets, consistency across days,
//!   `min > max`, rule/load fit…), which JSON Schema cannot express.
//!
//! This is what keeps the hand-written parts of `program.schema.json` in step with the Rust
//! checks: changing one without the other fails here.

// Test helpers outside `#[test]` functions are not covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::path::Path;

use iron_oxide_domain::program::{
    LEGACY_PROGRAM_SCHEMA_URL, PROGRAM_SCHEMA_JSON, PROGRAM_SCHEMA_URL, Program, ProgramError,
    ValidationErrorKind as Kind, builtin_programs,
};
use serde_json::{Value, json};

/// Rules the schema cannot express, since they relate several values.
fn is_cross_field(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::DuplicateDayId { .. }
            | Kind::UnknownDay { .. }
            | Kind::DayNotInRotation { .. }
            | Kind::DuplicateExercise { .. }
            | Kind::InconsistentExercise { .. }
            | Kind::RepRangeInverted { .. }
            | Kind::WarmupNotLighter { .. }
            | Kind::WarmupOnTimedWork
            | Kind::WarmupNeedsWorkingLoad
            | Kind::ProgressionNeedsWeightLoad { .. }
            | Kind::ProgressionNeedsTrainingMaxLoad
            | Kind::ProgressionNeedsRepRange { .. }
            | Kind::ProgressionOnTimedWork { .. }
            | Kind::IncrementUnitMismatch { .. }
            | Kind::SupersetNotContiguous { .. }
            | Kind::SupersetTooSmall { .. }
            | Kind::SupersetSetsMismatch { .. }
            | Kind::SupersetWithIntervals
            | Kind::TrainingMaxOnTimedWork
    )
}

fn candidates() -> Vec<Value> {
    let numbers = [
        "-1",
        "-0.0",
        "0",
        "0.001",
        "0.01",
        "0.5",
        "1",
        "1.0",
        "2",
        "2.5",
        "3.5",
        "9",
        "10",
        "10.0",
        "11",
        "14",
        "15",
        "19.99",
        "20",
        "20.25",
        "21",
        "28",
        "29",
        "30",
        "31",
        "45",
        "45.5",
        "49.99",
        "50",
        "50.01",
        "99.99",
        "99.999",
        "100",
        "100.0",
        "100.01",
        "101",
        "149.99",
        "150",
        "150.01",
        "151",
        "999",
        "1000",
        "1000.01",
        "1999.5",
        "2000",
        "2000.5",
        "3599",
        "3600",
        "3601",
        "4409.2",
        "4410",
        "65535",
        "65536",
        "4294967295",
        "4294967296",
        "1e300",
    ];
    let mut values: Vec<Value> = numbers
        .iter()
        .map(|text| serde_json::from_str(text).unwrap())
        .collect();
    let strings = [
        "",
        " ",
        "\u{feff}",
        "\u{85}",
        "\u{a0}x",
        "\u{0}",
        "a\u{0}b",
        "\u{1f}x",
        "a\u{1b}b",
        "a\tb",
        "a\nb",
        "a\r\nb",
        "\t",
        "a\u{7f}b",
        "x",
        "a",
        "b",
        "c",
        "upper",
        "lower",
        "none",
        "kg",
        "Bad Id",
        "a--b",
        "-a",
        "a-",
        "bench-press",
        "plank",
        "squat",
        // Tempo.
        "3-1-X-0",
        "10-0-1-99",
        "3-1-x-0",
        "3-1-1",
        "3-1-1-0-0",
        "100-1-1-0",
        "3110",
        // Demo URLs.
        "https://example.com",
        "https://www.youtube.com/watch?v=abc&t=1s",
        "https://[::1]:8443/x",
        "https://example.com:8443",
        "http://example.com",
        "https://:80/x",
        "https://a..b",
        "https://evil.example\\.youtube.com/",
        "https://evil.example/\u{202e}moc",
        "https://a.b/\u{200b}",
        "https://ex\u{e4}mple.com",
        "https://a.b/ x",
        "https://a.b/\u{1}",
        "https://u@a.b",
        "https://a.b:123456",
        "https://a.b:",
        "https://a.b/<x>",
        PROGRAM_SCHEMA_URL,
        // Documents saved before the repository moved (#104).
        LEGACY_PROGRAM_SCHEMA_URL,
    ];
    values.extend(strings.iter().map(|s| Value::from(*s)));
    let long = [
        "a".repeat(64),
        "a".repeat(65),
        "x".repeat(100),
        "x".repeat(101),
        "y".repeat(2_000),
        "y".repeat(2_001),
        format!("https://e.com/{}", "a".repeat(2_048 - 14)),
        format!("https://e.com/{}", "a".repeat(2_048 - 13)),
        format!("{PROGRAM_SCHEMA_URL}x"),
    ];
    values.extend(long.into_iter().map(Value::from));
    values.extend([
        Value::Null,
        json!(true),
        json!([]),
        json!([1]),
        json!({}),
        json!("none"),
        json!({ "kg": 20 }),
        json!({ "kg": 0 }),
        json!({ "lb": 45 }),
        json!({ "lb": 4409.2 }),
        json!({ "percent_of_training_max": 70 }),
        json!({ "percent_of_working_weight": 50 }),
        json!({ "min": 8, "max": 12 }),
        json!({ "min": 12, "max": 8 }),
        json!({ "min": 0, "max": 101 }),
        json!({ "none": null }),
        json!({ "reps": { "sets": 3, "reps": 5 } }),
        json!({ "hold": { "sets": 3, "seconds": 30 } }),
        json!({ "intervals": { "work": 30, "rest": 30, "rounds": 3 } }),
        json!({ "add_when_top_of_range": { "increment": { "kg": 2.5 } } }),
        json!({ "training_max": { "increment": { "lb": 5 } } }),
        json!({ "failures": 3, "percent": 10 }),
        json!(["a", "b"]),
    ]);
    values
}

/// Demo URL candidates, tried at every `demo_url` node.
fn url_candidates() -> Vec<Value> {
    let mut urls = Vec::new();
    // Every printable ASCII character (and a few others) at each position of a demo URL: host,
    // port, path, query and fragment. This catches drift in the schema's URL pattern alone.
    let characters = (0x20_u8..=0x7e)
        .map(char::from)
        .chain(['\u{7f}', '\u{e9}', '\u{202e}']);
    for c in characters {
        for url in [
            format!("https://a{c}b.c/"),
            format!("https://{c}.b/"),
            format!("https://a.b:8{c}/"),
            format!("https://a.b/x{c}y"),
            format!("https://a.b/?q={c}"),
            format!("https://a.b/#{c}"),
        ] {
            urls.push(Value::from(url));
        }
    }
    for port in [
        "0", "1", "01", "080", "8080", "65535", "65536", "99999", "100000",
    ] {
        urls.push(Value::from(format!("https://a.b:{port}/")));
    }
    for host in [
        "[::1]",
        "[1.2.3.4]",
        "[]",
        "1.2.3.4",
        "a-.b",
        "-a.b",
        "xn--bcher-kva.example",
    ] {
        urls.push(Value::from(format!("https://{host}/")));
    }
    urls
}

/// JSON pointers (as key/index lists) of every node below the root.
fn nodes(value: &Value, path: &mut Vec<Value>, out: &mut Vec<Vec<Value>>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                path.push(Value::from(key.clone()));
                out.push(path.clone());
                nodes(child, path, out);
                path.pop();
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                path.push(Value::from(index));
                out.push(path.clone());
                nodes(child, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

fn node_mut<'a>(mut value: &'a mut Value, path: &[Value]) -> &'a mut Value {
    for step in path {
        value = match step {
            Value::String(key) => value.get_mut(key.as_str()).unwrap(),
            _ => value
                .get_mut(usize::try_from(step.as_u64().unwrap()).unwrap())
                .unwrap(),
        };
    }
    value
}

struct Checker {
    validator: jsonschema::Validator,
    cases: usize,
}

impl Checker {
    fn check(&mut self, document: &Value, what: &str) {
        self.cases += 1;
        let text = serde_json::to_string(document).unwrap();
        let schema_ok = self.validator.is_valid(document);
        let agree = match Program::from_json(&text) {
            Ok(_) => schema_ok,
            Err(ProgramError::Parse(_)) => !schema_ok,
            Err(ProgramError::Invalid(errors)) => {
                let cross_field_only = errors.as_slice().iter().all(|e| is_cross_field(&e.kind));
                schema_ok == cross_field_only
            }
        };
        if !agree {
            let rust = match Program::from_json(&text) {
                Ok(_) => "accepted".to_owned(),
                Err(error) => error.to_string(),
            };
            let schema: Vec<_> = self
                .validator
                .iter_errors(document)
                .map(|e| format!("{}: {e}", e.instance_path()))
                .take(3)
                .collect();
            panic!("disagreement on {what}\nrust: {rust}\nschema: {schema:?}");
        }
    }
}

fn documents() -> Vec<(String, Value)> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/programs/valid");
    let mut documents: Vec<_> = ["minimal.json", "everything.json"]
        .iter()
        .map(|name| {
            let text = std::fs::read_to_string(fixtures.join(name)).unwrap();
            ((*name).to_owned(), serde_json::from_str(&text).unwrap())
        })
        .collect();
    for builtin in builtin_programs().unwrap() {
        documents.push((
            builtin.id().to_string(),
            serde_json::from_str(builtin.json()).unwrap(),
        ));
    }
    documents
}

#[test]
fn rust_and_schema_agree_on_single_changes() {
    let schema: Value = serde_json::from_str(PROGRAM_SCHEMA_JSON).unwrap();
    let mut checker = Checker {
        validator: jsonschema::validator_for(&schema).unwrap(),
        cases: 0,
    };
    let candidates = candidates();
    let urls = url_candidates();
    for (name, document) in documents() {
        checker.check(&document, &name);
        let mut paths = vec![Vec::new()];
        nodes(&document, &mut Vec::new(), &mut paths);
        for path in &paths {
            let where_ = format!("{name} at {path:?}");
            let is_url = path.last() == Some(&Value::from("demo_url"));
            for candidate in candidates
                .iter()
                .chain(if is_url { &urls[..] } else { &[] })
            {
                let mut changed = document.clone();
                *node_mut(&mut changed, path) = candidate.clone();
                checker.check(&changed, &format!("{where_} = {candidate}"));
            }
            let original = node_mut(&mut document.clone(), path).clone();
            if let Value::Object(map) = &original {
                // The same fields as an array, in order.
                let mut changed = document.clone();
                *node_mut(&mut changed, path) = Value::Array(map.values().cloned().collect());
                checker.check(&changed, &format!("{where_} as an array"));
                // Each key removed, and an unknown key added.
                for key in map.keys() {
                    let mut changed = document.clone();
                    if let Value::Object(map) = node_mut(&mut changed, path) {
                        map.remove(key);
                    }
                    checker.check(&changed, &format!("{where_} without {key}"));
                }
                let mut changed = document.clone();
                if let Value::Object(map) = node_mut(&mut changed, path) {
                    map.insert("zzz".to_owned(), json!(1));
                }
                checker.check(&changed, &format!("{where_} with an unknown key"));
            }
        }
    }
    assert!(checker.cases > 10_000, "{} cases", checker.cases);
}
