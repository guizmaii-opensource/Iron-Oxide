//! Program documents end to end: fixtures, error snapshots, round trips and the committed JSON
//! Schema.
//!
//! Each `tests/fixtures/programs/invalid/**/NAME.json` has its expected error output next to it in
//! `NAME.errors`. After an intended change to the messages, regenerate them with
//! `UPDATE_SNAPSHOTS=1 cargo test -p iron-oxide-domain --test program_documents` and review the
//! diff.

// Test helpers outside `#[test]` functions are not covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use iron_oxide_domain::program::{
    AI_PROMPT, AI_PROMPT_SCHEMA_URL, PROGRAM_SCHEMA_JSON, Program, ProgramError,
    ValidationErrorKind, builtin_programs, extract_json, limits,
};
use serde_json::Value;

fn fixtures(dir: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/programs")
        .join(dir);
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures in {}", dir.display());
    paths
}

fn schema_validator() -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(PROGRAM_SCHEMA_JSON).unwrap();
    jsonschema::validator_for(&schema).unwrap()
}

fn schema_errors(validator: &jsonschema::Validator, json: &str) -> Vec<String> {
    let instance: Value = serde_json::from_str(json).unwrap();
    validator
        .iter_errors(&instance)
        .map(|error| format!("{}: {error}", error.instance_path()))
        .collect()
}

#[test]
fn valid_fixtures_parse_validate_round_trip_and_match_the_schema() {
    let validator = schema_validator();
    for path in fixtures("valid") {
        let json = fs::read_to_string(&path).unwrap();
        let program = Program::from_json(&json)
            .unwrap_or_else(|error| panic!("{}:\n{error}", path.display()));
        let again = Program::from_json(&program.to_json_pretty().unwrap()).unwrap();
        assert_eq!(again, program, "{}", path.display());
        assert_eq!(
            schema_errors(&validator, &json),
            Vec::<String>::new(),
            "{}",
            path.display()
        );
    }
}

#[test]
fn builtin_programs_round_trip_and_match_the_schema() {
    let validator = schema_validator();
    for builtin in builtin_programs().unwrap() {
        assert_eq!(
            schema_errors(&validator, builtin.json()),
            Vec::<String>::new(),
            "{}",
            builtin.id()
        );
        let written = builtin.program().to_json_pretty().unwrap();
        assert_eq!(schema_errors(&validator, &written), Vec::<String>::new());
        assert_eq!(Program::from_json(&written).unwrap(), *builtin.program());
    }
}

/// The files in `programs/examples/` with this extension, sorted.
fn examples(extension: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../programs/examples");
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == extension))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no .{extension} in {}", dir.display());
    paths
}

/// The programs written by an AI assistant following the prompt (#108) are valid, and the raw
/// answers they came in give the same program once the JSON is taken out of them.
#[test]
fn ai_written_examples_are_valid_and_extracted_from_the_raw_answers() {
    let validator = schema_validator();
    for path in examples("json") {
        let json = fs::read_to_string(&path).unwrap();
        let program = Program::from_json(&json)
            .unwrap_or_else(|error| panic!("{}:\n{error}", path.display()));
        assert_eq!(
            schema_errors(&validator, &json),
            Vec::<String>::new(),
            "{}",
            path.display()
        );
        let answer = path.with_extension("answer.txt");
        let answer = fs::read_to_string(&answer)
            .unwrap_or_else(|_| panic!("{} is missing", answer.display()));
        let extracted = extract_json(&answer).unwrap();
        assert_eq!(Program::from_json(extracted).unwrap(), program);
    }
}

/// The prompt names the schema and its example is a valid program: the lines from the first
/// `{` alone on its line to the next `}` alone on its line.
#[test]
fn the_ai_prompt_names_the_schema_and_its_example_is_valid() {
    assert!(AI_PROMPT.contains(AI_PROMPT_SCHEMA_URL));
    // No code-host URL: the prompt is shown to users and on the website.
    assert!(!AI_PROMPT.to_lowercase().contains("github"), "{AI_PROMPT}");
    let lines: Vec<&str> = AI_PROMPT.lines().collect();
    let start = lines.iter().position(|line| *line == "{").unwrap();
    let end = start + lines[start..].iter().position(|line| *line == "}").unwrap();
    let example = lines[start..=end].join("\n");
    let program = Program::from_json(&example).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        schema_errors(&schema_validator(), &example),
        Vec::<String>::new()
    );
    assert_eq!(program.rotation.len(), program.days.len());
    // The prompt asks for documents without `$schema`, and they are valid.
    assert!(!example.contains("$schema"));
    assert_eq!(program.schema, None);
}

fn check_snapshot(path: &Path, actual: &str) {
    let snapshot = path.with_extension("errors");
    let actual = format!("{actual}\n");
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        fs::write(&snapshot, &actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&snapshot).unwrap_or_default();
    assert!(
        expected == actual,
        "{} does not match.\n--- expected\n{expected}--- actual\n{actual}\
         (run with UPDATE_SNAPSHOTS=1 to accept)",
        snapshot.display()
    );
}

/// Documents that serde rejects. The schema rejects them too, so an editor flags them as well.
#[test]
fn parse_errors_have_a_path_and_a_position() {
    let validator = schema_validator();
    for path in fixtures("invalid/parse") {
        let json = fs::read_to_string(&path).unwrap();
        let error = Program::from_json(&json).unwrap_err();
        let ProgramError::Parse(parse) = &error else {
            panic!("{}: expected a parse error, got {error}", path.display());
        };
        assert!(parse.line > 0 && parse.column > 0, "{}", path.display());
        check_snapshot(&path, &error.to_string());
        if serde_json::from_str::<Value>(&json).is_ok() {
            assert!(
                !schema_errors(&validator, &json).is_empty(),
                "{}: the schema accepts what serde rejects",
                path.display()
            );
        }
    }
}

/// Documents that parse but break rules: every broken rule is reported.
#[test]
fn validation_errors_list_every_problem_with_its_path() {
    for path in fixtures("invalid/rules") {
        let json = fs::read_to_string(&path).unwrap();
        let error = Program::from_json(&json).unwrap_err();
        let ProgramError::Invalid(errors) = &error else {
            panic!(
                "{}: expected validation errors, got {error}",
                path.display()
            );
        };
        assert!(!errors.as_slice().is_empty());
        check_snapshot(&path, &error.to_string());
    }
}

#[test]
fn the_ticket_example_reads_as_expected() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/programs/invalid/rules/rep-range-inverted.json");
    let error = Program::from_json(&fs::read_to_string(path).unwrap()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "days[1].exercises[2].work.reps.reps: min 12 is greater than max 8"
    );
}

#[test]
fn unsupported_version_is_reported_alone() {
    let error = Program::from_json(r#"{"schema_version": 7, "whatever": true}"#).unwrap_err();
    let ProgramError::Invalid(errors) = error else {
        panic!("expected validation errors");
    };
    assert_eq!(errors.as_slice().len(), 1);
    assert_eq!(
        errors.as_slice()[0].kind,
        ValidationErrorKind::UnsupportedSchemaVersion {
            found: 7,
            supported: 1
        }
    );
    // A version that is not a number is left to the full parse.
    let error = Program::from_json(r#"{"schema_version": "1"}"#).unwrap_err();
    assert!(matches!(error, ProgramError::Parse(_)), "{error}");
}

/// Building a program in code and breaking it: validate() is usable without JSON.
#[test]
fn validate_works_on_programs_built_in_code() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/programs/valid/everything.json");
    let mut program = Program::from_json(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(program.validate(), Ok(()));
    program.schema_version = 2;
    program.rotation.clear();
    let errors = program.validate().unwrap_err();
    let messages: Vec<_> = errors.as_slice().iter().map(ToString::to_string).collect();
    assert_eq!(
        messages,
        [
            "schema_version: unsupported schema_version 2 (this app reads version 1)",
            "days[0].id: day `upper` is not in the rotation",
            "days[1].id: day `lower` is not in the rotation",
            "rotation: must contain at least one day",
        ]
    );
}

/// Arrays where the schema has objects, and `{"none": …}`: serde's derives accept them, the
/// structural check rejects them with the path of the offending node.
#[test]
fn shape_errors_point_at_the_offending_node() {
    let validator = schema_validator();
    for path in fixtures("invalid/shape") {
        let json = fs::read_to_string(&path).unwrap();
        let error = Program::from_json(&json).unwrap_err();
        assert!(
            matches!(error, ProgramError::Parse(_)),
            "{}: {error}",
            path.display()
        );
        check_snapshot(&path, &error.to_string());
        assert!(
            !schema_errors(&validator, &json).is_empty(),
            "{}: the schema accepts what serde rejects",
            path.display()
        );
    }
}

#[test]
fn oversized_documents_are_rejected_before_parsing() {
    let padding = " ".repeat(limits::MAX_DOCUMENT_BYTES);
    let error = Program::from_json(&format!("{{{padding}}}")).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "the document is {} bytes; the limit is {} bytes",
            limits::MAX_DOCUMENT_BYTES + 2,
            limits::MAX_DOCUMENT_BYTES
        )
    );
    // Exactly at the limit, the document is read.
    let padding = " ".repeat(limits::MAX_DOCUMENT_BYTES - 2);
    let error = Program::from_json(&format!("{{{padding}}}")).unwrap_err();
    assert!(error.to_string().starts_with("missing field"), "{error}");
}

#[test]
fn integral_floats_count_as_whole_numbers() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/programs/valid/minimal.json");
    let json = fs::read_to_string(path).unwrap();
    let floats = json
        .replace(r#""schema_version": 1"#, r#""schema_version": 1.0"#)
        .replace(r#""sets": 3"#, r#""sets": 3.0"#)
        .replace(r#""rest": 60"#, r#""rest": 60.0"#);
    assert_ne!(floats, json);
    assert_eq!(
        Program::from_json(&floats).unwrap(),
        Program::from_json(&json).unwrap()
    );
    let error =
        Program::from_json(&json.replace(r#""schema_version": 1"#, r#""schema_version": 2.0"#))
            .unwrap_err();
    assert_eq!(
        error.to_string(),
        "schema_version: unsupported schema_version 2 (this app reads version 1)"
    );
}

/// Every error is bounded: at most `MAX_REPORTED_ERRORS` are listed, and items past a list's
/// limit are not checked.
#[test]
fn errors_are_bounded() {
    let bad_exercise =
        r#"{"id": "x", "name": "", "work": {"reps": {"sets": 0, "reps": 0}}, "rest": 0}"#;
    let exercises = vec![bad_exercise; 1_000].join(",");
    let json = format!(
        r#"{{"schema_version": 1, "name": "Big", "days": [{{"id": "a", "name": "A", "exercises": [{exercises}]}}], "rotation": ["a"]}}"#
    );
    let ProgramError::Invalid(errors) = Program::from_json(&json).unwrap_err() else {
        panic!("expected validation errors");
    };
    assert_eq!(errors.as_slice().len(), limits::MAX_REPORTED_ERRORS);
    assert!(errors.omitted() > 0);
    // 30 exercises checked (3 errors each, plus duplicates), not 1 000.
    let total = errors.as_slice().len() + errors.omitted();
    assert!(total < 200, "{total}");
    assert!(
        errors
            .to_string()
            .ends_with(&format!("… and {} more errors", errors.omitted()))
    );

    // Days past the limit: no errors from them, and rotation entries naming them are fine.
    let days: Vec<_> = (0..40)
        .map(|i| format!(r#"{{"id": "d{i}", "name": "", "exercises": []}}"#))
        .collect();
    let json = format!(
        r#"{{"schema_version": 1, "name": "Many days", "days": [{}], "rotation": ["d39"]}}"#,
        days.join(",")
    );
    let ProgramError::Invalid(errors) = Program::from_json(&json).unwrap_err() else {
        panic!("expected validation errors");
    };
    let messages = errors.to_string();
    assert!(
        messages.starts_with("days: must contain at most 14 days (got 40)"),
        "{messages}"
    );
    assert!(!messages.contains("days[14]"), "{messages}");
    assert!(!messages.contains("unknown day"), "{messages}");
}
