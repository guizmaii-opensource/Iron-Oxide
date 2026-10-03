//! Generation of `schemas/program.schema.json` from the program types.
//!
//! schemars is a dev-dependency and the derives are `cfg_attr(test, ...)`, so none of this is
//! compiled into the app. The committed file is the product: editors use it through `$schema`,
//! and the server can serve [`PROGRAM_SCHEMA_JSON`](super::PROGRAM_SCHEMA_JSON). CI runs plain
//! `cargo test`, which runs [`committed_schema_is_up_to_date`], so a stale file fails the build.
//!
//! Regenerate with `UPDATE_SCHEMA=1 cargo test -p iron-oxide-domain program::schema`.
//!
//! The types with hand-written serde (the core newtypes and the program values) get hand-written
//! schemas here that mirror what their `Deserialize` accepts. The integration tests validate the
//! built-in programs and the fixtures against the committed schema, which catches drift.

use std::borrow::Cow;
use std::path::PathBuf;

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

use super::ids::SupersetId;
use super::limits::*;
use super::model::{Day, Exercise, WarmupSet};
use super::values::{
    DemoUrl, Load, LoadRepr, RepRange, RepTarget, SchemaUrl, Tempo, UnitWeight, UnitWeightRepr,
    WarmupLoad, WarmupLoadRepr, is_host_char, is_url_char,
};
use super::{CURRENT_SCHEMA_VERSION, LEGACY_PROGRAM_SCHEMA_URL, PROGRAM_SCHEMA_URL, Program};
use crate::{DayId, ExerciseId, Percent, Reps, Seconds};

/// Matches the slug rules of `ExerciseId`, `DayId` and `SupersetId`.
const SLUG_PATTERN: &str = "^[a-z0-9]+(-[a-z0-9]+)*$";

/// A name: no C0 control character (`validate::has_control_character`), and at least one
/// character that `validate::is_blank` does not treat as whitespace. ECMA-262 `\s` is Unicode
/// `White_Space` plus U+FEFF, minus U+0085.
const NAME_PATTERN: &str = "^[^\\u0000-\\u001f]*[^\\s\\u0085\\u0000-\\u001f][^\\u0000-\\u001f]*$";

/// A description or notes: as [`NAME_PATTERN`], but tab, line feed and carriage return are allowed.
const MULTILINE_PATTERN: &str = "^[^\\u0000-\\u0008\\u000b\\u000c\\u000e-\\u001f]*\
     [^\\s\\u0085\\u0000-\\u001f]\
     [^\\u0000-\\u0008\\u000b\\u000c\\u000e-\\u001f]*$";

/// A port from 1 to 65535 without leading zeros, as `values::is_port` accepts.
const PORT_PATTERN: &str =
    "(6553[0-5]|655[0-2][0-9]|65[0-4][0-9]{2}|6[0-4][0-9]{3}|[1-5][0-9]{4}|[1-9][0-9]{0,3})";

/// A regex character class of the ASCII bytes `allowed` accepts, so the schema cannot drift
/// from the Rust predicate.
fn class(allowed: fn(u8) -> bool) -> String {
    let mut class = String::from("[");
    for byte in (0_u8..=0x7f).filter(|&byte| allowed(byte)) {
        if matches!(byte, b'\\' | b']' | b'[' | b'^' | b'-') {
            class.push('\\');
        }
        class.push(char::from(byte));
    }
    class.push(']');
    class
}

/// A `DemoUrl`: `https://`, dot-separated host labels, an optional port, then only the
/// characters `values::is_url_char` allows. Built from the same predicates as the Rust check.
fn demo_url_pattern() -> String {
    let label = class(is_host_char);
    let path = class(is_url_char);
    format!("^https://{label}+(\\.{label}+)*(:{PORT_PATTERN})?([/?#]{path}*)?$")
}

/// The heaviest weight in pounds, rounded down to 4 decimals: 2 000 kg is 4 409.245 243 7… lb.
/// The schema is stricter than the app only between 4 409.2452 and that value.
const MAX_WEIGHT_LB: f64 = 4_409.245_2;

fn slug_schema(description: &str) -> Schema {
    json_schema!({
        "type": "string",
        "pattern": SLUG_PATTERN,
        "minLength": 1,
        "maxLength": crate::SLUG_MAX_LEN,
        "description": description,
    })
}

fn whole(min: u32, max: u32) -> Schema {
    json_schema!({ "type": "integer", "minimum": min, "maximum": max })
}

fn above_zero(max: f64, exclusive_max: bool) -> Schema {
    if exclusive_max {
        json_schema!({ "type": "number", "exclusiveMinimum": 0, "exclusiveMaximum": max })
    } else {
        json_schema!({ "type": "number", "exclusiveMinimum": 0, "maximum": max })
    }
}

/// A percentage: above zero, with at most two decimals like `Percent`.
fn percent(max: u32, exclusive_max: bool) -> Schema {
    let mut schema = above_zero(f64::from(max), exclusive_max);
    schema.insert("multipleOf".to_owned(), 0.01.into());
    schema
}

fn list<T: JsonSchema>(generator: &mut SchemaGenerator, min: usize, max: usize) -> Schema {
    json_schema!({
        "type": "array",
        "items": generator.subschema_for::<T>(),
        "minItems": min,
        "maxItems": max,
    })
}

// Field schemas, built from `limits` so a changed limit makes the stale-schema test fail.

pub(super) fn schema_version(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "const": CURRENT_SCHEMA_VERSION })
}

pub(super) fn name(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": MAX_NAME_CHARS,
        "pattern": NAME_PATTERN,
    })
}

pub(super) fn optional_text(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": ["string", "null"],
        "minLength": 1,
        "maxLength": MAX_TEXT_CHARS,
        "pattern": MULTILINE_PATTERN,
    })
}

pub(super) fn days(generator: &mut SchemaGenerator) -> Schema {
    list::<Day>(generator, 1, MAX_DAYS)
}

pub(super) fn rotation(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = list::<DayId>(generator, 1, MAX_ROTATION);
    schema.insert("uniqueItems".to_owned(), true.into());
    schema
}

pub(super) fn exercises(generator: &mut SchemaGenerator) -> Schema {
    list::<Exercise>(generator, 1, MAX_EXERCISES_PER_DAY)
}

pub(super) fn warmup(generator: &mut SchemaGenerator) -> Schema {
    list::<WarmupSet>(generator, 0, MAX_WARMUP_LINES)
}

pub(super) fn sets(_: &mut SchemaGenerator) -> Schema {
    whole(1, u32::from(MAX_SETS))
}

pub(super) fn rep_count(_: &mut SchemaGenerator) -> Schema {
    whole(1, u32::from(MAX_REPS))
}

pub(super) fn active_seconds(_: &mut SchemaGenerator) -> Schema {
    whole(1, MAX_SECONDS)
}

pub(super) fn rest_seconds(_: &mut SchemaGenerator) -> Schema {
    whole(0, MAX_SECONDS)
}

pub(super) fn rounds(_: &mut SchemaGenerator) -> Schema {
    whole(1, u32::from(MAX_ROUNDS))
}

pub(super) fn warmup_sets(_: &mut SchemaGenerator) -> Schema {
    let mut schema = whole(1, u32::from(MAX_WARMUP_SETS));
    schema.insert("default".to_owned(), 1.into());
    schema
}

pub(super) fn failures(_: &mut SchemaGenerator) -> Schema {
    whole(1, u32::from(MAX_DELOAD_FAILURES))
}

pub(super) fn deload_percent(_: &mut SchemaGenerator) -> Schema {
    percent(MAX_DELOAD_PERCENT, false)
}

pub(super) fn training_max_percent(_: &mut SchemaGenerator) -> Schema {
    percent(MAX_PERCENT_OF_TRAINING_MAX, false)
}

pub(super) fn warmup_percent(_: &mut SchemaGenerator) -> Schema {
    percent(WARMUP_PERCENT_BELOW, true)
}

pub(super) fn load_kg(_: &mut SchemaGenerator) -> Schema {
    above_zero(f64::from(MAX_WEIGHT_KG), false)
}

pub(super) fn load_lb(_: &mut SchemaGenerator) -> Schema {
    above_zero(MAX_WEIGHT_LB, false)
}

pub(super) fn increment_kg(_: &mut SchemaGenerator) -> Schema {
    above_zero(f64::from(MAX_INCREMENT_KG), false)
}

pub(super) fn increment_lb(_: &mut SchemaGenerator) -> Schema {
    above_zero(f64::from(MAX_INCREMENT_LB), false)
}

/// `inline`: primitives are written in place; the program values get a named `$defs` entry.
macro_rules! manual_schema {
    ($type:ty, $name:literal, inline = $inline:literal, |$generator:ident| $body:expr) => {
        impl JsonSchema for $type {
            fn inline_schema() -> bool {
                $inline
            }

            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed($name)
            }

            fn json_schema($generator: &mut SchemaGenerator) -> Schema {
                $body
            }
        }
    };
}

manual_schema!(Percent, "Percent", inline = true, |_g| json_schema!({
    "type": "number",
    "minimum": 0,
    "maximum": 1000,
}));
manual_schema!(Reps, "Reps", inline = true, |_g| json_schema!({
    "type": "integer",
    "minimum": 0,
    "maximum": u16::MAX,
}));
manual_schema!(Seconds, "Seconds", inline = true, |_g| json_schema!({
    "type": "integer",
    "minimum": 0,
    "maximum": u32::MAX,
}));
manual_schema!(ExerciseId, "ExerciseId", inline = true, |_g| slug_schema(
    "Lowercase letters and digits in words separated by single hyphens, e.g. `back-squat`."
));
manual_schema!(DayId, "DayId", inline = true, |_g| slug_schema(
    "Lowercase letters and digits in words separated by single hyphens, e.g. `a` or `upper-1`."
));
manual_schema!(SupersetId, "SupersetId", inline = true, |_g| slug_schema(
    "Lowercase letters and digits in words separated by single hyphens, e.g. `a`."
));
manual_schema!(Tempo, "Tempo", inline = false, |_g| json_schema!({
    "type": "string",
    "pattern": "^([0-9]{1,2}|X)-([0-9]{1,2}|X)-([0-9]{1,2}|X)-([0-9]{1,2}|X)$",
    "examples": ["3-1-X-0"],
}));
manual_schema!(DemoUrl, "DemoUrl", inline = false, |_g| json_schema!({
    "type": "string",
    "pattern": demo_url_pattern(),
    "maxLength": DemoUrl::MAX_LEN,
}));
// The legacy URL is accepted (documents saved before the repository moved), never written.
manual_schema!(SchemaUrl, "SchemaUrl", inline = true, |_g| json_schema!({
    "enum": [PROGRAM_SCHEMA_URL, LEGACY_PROGRAM_SCHEMA_URL],
}));
manual_schema!(UnitWeight, "UnitWeight", inline = false, |g| {
    UnitWeightRepr::json_schema(g)
});
manual_schema!(Load, "Load", inline = false, |g| LoadRepr::json_schema(g));
manual_schema!(WarmupLoad, "WarmupLoad", inline = false, |g| {
    WarmupLoadRepr::json_schema(g)
});
manual_schema!(RepTarget, "RepTarget", inline = false, |g| json_schema!({
    "oneOf": [
        {
            "type": "integer",
            "minimum": 1,
            "maximum": MAX_REPS,
            "description": "Exactly this many reps.",
        },
        g.subschema_for::<RepRange>(),
    ],
}));

/// The schema document, as committed.
fn generate() -> String {
    let mut schema = SchemaSettings::draft2020_12()
        .for_deserialize()
        .into_generator()
        .into_root_schema_for::<Program>();
    schema.insert("$id".to_owned(), PROGRAM_SCHEMA_URL.into());
    schema.insert("title".to_owned(), "Iron Oxide training program".into());
    let mut json = serde_json::to_string_pretty(&schema).unwrap();
    json.push('\n');
    json
}

fn committed_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/program.schema.json")
}

#[test]
fn committed_schema_is_up_to_date() {
    let generated = generate();
    let path = committed_path();
    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        std::fs::write(&path, &generated).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "{} is stale: regenerate it with \
         `UPDATE_SCHEMA=1 cargo test -p iron-oxide-domain program::schema` and commit it",
        path.display()
    );
    assert_eq!(super::PROGRAM_SCHEMA_JSON, committed);
}

#[test]
fn character_classes_list_exactly_the_allowed_bytes() {
    assert_eq!(
        class(is_host_char),
        "[\\-0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz]"
    );
    assert_eq!(class(|byte| byte == b'^' || byte == b']'), "[\\]\\^]");
    let path = class(is_url_char);
    let listed: Vec<u8> = path
        .trim_start_matches('[')
        .trim_end_matches(']')
        .replace('\\', "")
        .into_bytes();
    let expected: Vec<u8> = (0_u8..=0x7f).filter(|&byte| is_url_char(byte)).collect();
    assert_eq!(listed, expected);
}

#[test]
fn schema_describes_the_document() {
    let schema: serde_json::Value = serde_json::from_str(&generate()).unwrap();
    assert_eq!(schema["$id"], PROGRAM_SCHEMA_URL);
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["additionalProperties"], false);
    let required = schema["required"].as_array().unwrap();
    for field in ["schema_version", "name", "days", "rotation"] {
        assert!(required.contains(&field.into()), "{field}");
    }
    assert!(schema["properties"]["$schema"].is_object());
}
