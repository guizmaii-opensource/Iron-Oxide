//! Training programs: the JSON document format, its validation, and the built-in programs.
//!
//! A program is a versioned JSON document (see `schemas/program.schema.json` at the repository
//! root, generated from these types). Uploaded documents are untrusted, so reading one with
//! [`Program::from_json`] is bounded at every step:
//!
//! 1. **Size**: at most [`limits::MAX_DOCUMENT_BYTES`], checked before parsing.
//! 2. **Parsing** with serde. Unknown fields are rejected, every value checks its own format
//!    (slugs, weights, tempo, URLs), and arrays are only accepted where the schema has arrays.
//!    Parsing stops at the first problem and reports a [`ParseError`] with its JSON path, line
//!    and column (when known).
//! 3. **Validation** with [`Program::validate`], which checks the rules that span several values
//!    (rep ranges, rotation, supersets, progression rules against loads, [`limits`]) and reports
//!    every broken rule, up to [`limits::MAX_REPORTED_ERRORS`], as a [`ValidationError`] with its
//!    JSON path, such as `days[1].exercises[2].work.reps.reps: min 12 is greater than max 8`.
//!    Items past a list's limit are not checked.
//!
//! Error messages never repeat more than [`limits::MAX_ECHOED_CHARS`] characters of a user value.
//! The JSON Schema carries the same limits; a differential test keeps the two in agreement.
//!
//! # Training maxes
//!
//! A load written as `{"percent_of_training_max": 75}` refers to the training max of the exercise
//! itself. Training maxes are not part of the program: they are personal numbers that change
//! every few weeks through progression, while a program is shared and versioned. The app keeps
//! them per user and per exercise, and asks for the ones listed by
//! [`Program::training_max_exercises`] before the first session.

mod builtin;
mod error;
mod ids;
pub mod limits;
mod model;
#[cfg(test)]
mod schema;
mod structure;
mod validate;
mod values;
mod whole;

use std::collections::BTreeSet;

pub use builtin::{BuiltinProgram, BuiltinProgramError, builtin_program, builtin_programs};
pub use error::{
    JsonPath, ParseError, PathSegment, ProgramError, ValidationError, ValidationErrorKind,
    ValidationErrors,
};
pub use ids::{BuiltinProgramId, InvalidSlug, SupersetId};
pub use model::{Day, Deload, Exercise, Program, ProgressionRule, WarmupSet, Work};
pub use validate::CURRENT_SCHEMA_VERSION;
pub use values::{
    DemoUrl, InvalidDemoUrl, InvalidTempo, Load, RepRange, RepTarget, SchemaUrl, Tempo, TempoPhase,
    UnitWeight, WarmupLoad,
};

use crate::{DayId, ExerciseId};

/// Where the program JSON Schema is published, for the `$schema` field of a program document.
pub const PROGRAM_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/fe2o3-labs/Iron-Oxide/main/schemas/program.schema.json";

/// The schema URL from before the repository moved to `fe2o3-labs`. Documents stored or saved
/// earlier may still carry it, so it is accepted when reading a `$schema` field. It is never
/// written: a document always serialises with [`PROGRAM_SCHEMA_URL`]. The generated schema
/// (`schemas/program.schema.json`) accepts it too. Apart from these two, the old owner's name
/// appears nowhere.
pub const LEGACY_PROGRAM_SCHEMA_URL: &str = "https://raw.githubusercontent.com/guizmaii-opensource/Iron-Oxide/main/schemas/program.schema.json";

/// The program JSON Schema, as committed in `schemas/program.schema.json`.
pub const PROGRAM_SCHEMA_JSON: &str = include_str!("../../../../schemas/program.schema.json");

/// The 1-based line and column of the first character after leading whitespace.
fn start_of_value(json: &str) -> (usize, usize) {
    let leading = json
        .get(..json.len() - json.trim_start().len())
        .unwrap_or_default();
    let line = leading.matches('\n').count() + 1;
    let column = leading.rsplit('\n').next().map_or(0, str::len) + 1;
    (line, column)
}

impl Program {
    /// Parses and validates a program document. Safe on untrusted input: the size is checked
    /// first ([`limits::MAX_DOCUMENT_BYTES`]), and the errors are bounded in number and length.
    ///
    /// A `schema_version` other than [`CURRENT_SCHEMA_VERSION`] is reported on its own, before
    /// the rest of the document is read, since a different version may have a different shape.
    ///
    /// # Errors
    /// [`ProgramError::Parse`] for an oversized document, invalid JSON or a wrong shape (the
    /// first problem only), and [`ProgramError::Invalid`] with every broken rule otherwise.
    pub fn from_json(json: &str) -> Result<Self, ProgramError> {
        if json.len() > limits::MAX_DOCUMENT_BYTES {
            return Err(ProgramError::Parse(ParseError {
                path: JsonPath::root(),
                message: format!(
                    "the document is {} bytes; the limit is {} bytes",
                    json.len(),
                    limits::MAX_DOCUMENT_BYTES
                ),
                line: 0,
                column: 0,
            }));
        }
        let probe: serde_json::Value = serde_json::from_str(json).map_err(|error| {
            ProgramError::Parse(ParseError::from_serde(JsonPath::root(), &error))
        })?;
        if !probe.is_object() {
            let (line, column) = start_of_value(json);
            return Err(ProgramError::Parse(ParseError {
                path: JsonPath::root(),
                message: "a program must be a JSON object".to_owned(),
                line,
                column,
            }));
        }
        if let Some(found) = probe.get("schema_version").and_then(whole::as_whole)
            && found != u64::from(CURRENT_SCHEMA_VERSION)
        {
            let mut errors = ValidationErrors::default();
            errors.push(ValidationError::new(
                JsonPath::root().key("schema_version"),
                ValidationErrorKind::UnsupportedSchemaVersion {
                    found,
                    supported: CURRENT_SCHEMA_VERSION,
                },
            ));
            return Err(ProgramError::Invalid(errors));
        }

        let deserializer = &mut serde_json::Deserializer::from_str(json);
        let program: Self = serde_path_to_error::deserialize(deserializer).map_err(|error| {
            ProgramError::Parse(ParseError::from_serde(error.path().into(), error.inner()))
        })?;
        structure::check(&probe).map_err(ProgramError::Parse)?;
        program.validate().map_err(ProgramError::Invalid)?;
        Ok(program)
    }

    /// Checks every rule that serde cannot: rep ranges, rotation, supersets, progression rules
    /// against loads, cross-day consistency of exercises, and [`limits`].
    ///
    /// # Errors
    /// Every broken rule, in document order.
    pub fn validate(&self) -> Result<(), ValidationErrors> {
        validate::validate(self)
    }

    /// The document as indented JSON.
    ///
    /// # Errors
    /// Only if serde_json fails to write, which the program types never cause.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// The day with this id.
    #[must_use]
    pub fn day(&self, id: &DayId) -> Option<&Day> {
        self.days.iter().find(|day| &day.id == id)
    }

    /// The first occurrence of an exercise. In a valid program, every occurrence has the same
    /// name, progression rule and kind of load.
    #[must_use]
    pub fn exercise(&self, id: &ExerciseId) -> Option<&Exercise> {
        self.exercises().find(|exercise| &exercise.id == id)
    }

    /// Every exercise of every day, in order, repeats included.
    pub fn exercises(&self) -> impl Iterator<Item = &Exercise> {
        self.days.iter().flat_map(|day| &day.exercises)
    }

    /// The exercises whose load is a percentage of the training max, so the lifter must enter a
    /// training max for each before training.
    #[must_use]
    pub fn training_max_exercises(&self) -> BTreeSet<&ExerciseId> {
        self.exercises()
            .filter(|exercise| exercise.load.is_some_and(Load::is_percent_of_training_max))
            .map(|exercise| &exercise.id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::{Percent, Reps, Seconds, Unit};

    const EVERYTHING: &str = include_str!("../../tests/fixtures/programs/valid/everything.json");

    #[test]
    fn lookups() {
        let program = Program::from_json(EVERYTHING).unwrap();
        let upper = DayId::new("upper").unwrap();
        assert_eq!(program.day(&upper).unwrap().name, "Upper");
        assert!(program.day(&DayId::new("legs").unwrap()).is_none());
        let bench = ExerciseId::new("bench-press").unwrap();
        assert_eq!(program.exercise(&bench).unwrap().name, "Bench press");
        assert!(
            program
                .exercise(&ExerciseId::new("nope").unwrap())
                .is_none()
        );
        assert_eq!(program.exercises().count(), 6);
        assert_eq!(
            program
                .training_max_exercises()
                .into_iter()
                .collect::<Vec<_>>(),
            [&bench]
        );
    }

    #[test]
    fn parses_every_value_of_the_everything_fixture() {
        let program = Program::from_json(EVERYTHING).unwrap();
        let bench = &program.days[0].exercises[0];
        assert_eq!(
            bench.load,
            Some(Load::PercentOfTrainingMax(Percent::new(77.5).unwrap()))
        );
        assert_eq!(bench.tempo.unwrap().to_string(), "2-1-X-0");
        assert_eq!(
            bench.demo_url.as_ref().unwrap().as_str(),
            "https://example.com/bench"
        );
        assert_eq!(bench.warmup[0].sets, 2);
        assert_eq!(bench.warmup[1].sets, 1);
        assert_eq!(
            bench.warmup[0].load,
            WarmupLoad::Weight(UnitWeight::new(45.0, Unit::Lb).unwrap())
        );
        assert_eq!(
            bench.progression,
            ProgressionRule::TrainingMax {
                increment: UnitWeight::new(5.0, Unit::Lb).unwrap(),
                deload_after_failures: Some(Deload {
                    failures: 2,
                    percent: Percent::new(10.0).unwrap(),
                }),
            }
        );
        let row = &program.days[0].exercises[1];
        assert_eq!(row.superset, Some(SupersetId::new("a").unwrap()));
        assert_eq!(
            row.work,
            Work::Reps {
                sets: 3,
                reps: RepTarget::Range(RepRange {
                    min: Reps::new(8),
                    max: Reps::new(12)
                }),
            }
        );
        assert_eq!(row.progression.name(), "double_progression");
        assert_eq!(row.progression.deload(), None);
        let treadmill = &program.days[1].exercises[1];
        assert_eq!(
            treadmill.work,
            Work::Intervals {
                work: Seconds::new(30),
                rest: Seconds::new(90),
                rounds: 8,
            }
        );
        assert!(treadmill.work.is_timed());
        assert_eq!(treadmill.work.sets(), 1);
        assert_eq!(treadmill.progression, ProgressionRule::None);
        assert_eq!(treadmill.progression.increment(), None);
        assert_eq!(program.rotation.len(), 2);
    }

    #[test]
    fn serialization_omits_defaults() {
        let program = Program::from_json(EVERYTHING).unwrap();
        let json = program.to_json_pretty().unwrap();
        assert!(
            !json.contains("\"none\""),
            "progression none is the default"
        );
        assert!(!json.contains("null"));
        assert!(json.contains(r#""$schema": "https://raw.githubusercontent.com"#));
        assert!(json.contains(r#""lb": 52.5"#));
        assert_eq!(Program::from_json(&json).unwrap(), program);
    }

    #[test]
    fn position_of_a_non_object_document() {
        assert_eq!(start_of_value("[]"), (1, 1));
        assert_eq!(start_of_value("  \n\n   1"), (3, 4));
        let error = Program::from_json("\"program\"").unwrap_err();
        assert_eq!(
            error.to_string(),
            "a program must be a JSON object (line 1, column 1)"
        );
    }

    fn unit_weight() -> impl Strategy<Value = UnitWeight> {
        prop_oneof![
            (1_u32..=400_000)
                .prop_map(|g| UnitWeight::new(f64::from(g) / 1_000.0, Unit::Kg).unwrap()),
            (1_u32..=8_000)
                .prop_map(|tenth| UnitWeight::new(f64::from(tenth) / 10.0, Unit::Lb).unwrap()),
        ]
    }

    /// A valid exercise with a random prescription, load and rule.
    fn exercise(index: usize) -> impl Strategy<Value = Exercise> {
        let work = prop_oneof![
            (1_u16..=limits::MAX_SETS, 1_u16..=limits::MAX_REPS).prop_map(|(sets, reps)| {
                Work::Reps {
                    sets,
                    reps: RepTarget::Fixed(Reps::new(reps)),
                }
            }),
            (1_u16..=limits::MAX_SETS, 1_u16..=50, 0_u16..=50).prop_map(|(sets, min, spread)| {
                Work::Reps {
                    sets,
                    reps: RepTarget::Range(RepRange {
                        min: Reps::new(min),
                        max: Reps::new(min + spread),
                    }),
                }
            }),
        ];
        (
            work,
            unit_weight(),
            0_u32..=limits::MAX_SECONDS,
            any::<bool>(),
            1_u16..=10,
            // Quarter kilos up to 20 kg, half pounds up to 45 lb.
            1_u32..=80,
        )
            .prop_map(move |(work, load, rest, progress, failures, step)| {
                let increment = match load.unit() {
                    Unit::Kg => UnitWeight::new(f64::from(step) / 4.0, Unit::Kg).unwrap(),
                    Unit::Lb => UnitWeight::new(f64::from(step.min(90)) / 2.0, Unit::Lb).unwrap(),
                };
                let progression = if progress {
                    ProgressionRule::AddWhenTopOfRange {
                        increment,
                        deload_after_failures: Some(Deload {
                            failures,
                            percent: Percent::new(10.0).unwrap(),
                        }),
                    }
                } else {
                    ProgressionRule::None
                };
                Exercise {
                    id: ExerciseId::new(format!("exercise-{index}")).unwrap(),
                    name: format!("Exercise {index}"),
                    work,
                    load: Some(Load::Weight(load)),
                    rest: Seconds::new(rest),
                    tempo: None,
                    notes: None,
                    demo_url: None,
                    warmup: Vec::new(),
                    superset: None,
                    progression,
                }
            })
    }

    proptest! {
        #[test]
        fn generated_programs_validate_and_round_trip(
            exercises in (1_usize..=5).prop_flat_map(|n| (0..n).map(exercise).collect::<Vec<_>>())
        ) {
            let program = Program {
                schema: None,
                schema_version: CURRENT_SCHEMA_VERSION,
                name: "Generated".to_owned(),
                description: None,
                days: vec![Day {
                    id: DayId::new("a").unwrap(),
                    name: "A".to_owned(),
                    exercises,
                }],
                rotation: vec![DayId::new("a").unwrap()],
            };
            prop_assert_eq!(program.validate(), Ok(()));
            let json = program.to_json_pretty().unwrap();
            prop_assert_eq!(Program::from_json(&json).unwrap(), program);
        }
    }
}
