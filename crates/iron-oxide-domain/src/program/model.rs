//! The program document: days, exercises, work, warm-ups and progression rules.
//!
//! The doc comments on these types become the descriptions in `schemas/program.schema.json`,
//! which editors show while a program is written by hand, so they describe the JSON.

use serde::{Deserialize, Serialize};

use super::ids::SupersetId;
use super::values::{DemoUrl, Load, RepTarget, SchemaUrl, Tempo, UnitWeight, WarmupLoad};
use super::whole;
use crate::{DayId, ExerciseId, Percent, Reps, Seconds};

/// A training program: its days, the order to run them in, and what to do on each day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Program {
    /// Optional link to this JSON Schema, so editors can check and complete the document. Only
    /// the published URL is accepted (or the one from before the repository moved).
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaUrl>,
    /// The version of the document format. Must be 1.
    #[serde(deserialize_with = "whole::u32")]
    #[cfg_attr(test, schemars(schema_with = "super::schema::schema_version"))]
    pub schema_version: u32,
    /// The program's name.
    #[cfg_attr(test, schemars(schema_with = "super::schema::name"))]
    pub name: String,
    /// Optional description: who it is for, how to run it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, schemars(schema_with = "super::schema::optional_text"))]
    pub description: Option<String>,
    /// The training days. Each needs a unique id.
    #[cfg_attr(test, schemars(schema_with = "super::schema::days"))]
    pub days: Vec<Day>,
    /// The suggested order of the days, repeated forever: `["a", "b", "c"]`. Every day appears
    /// exactly once.
    #[cfg_attr(test, schemars(schema_with = "super::schema::rotation"))]
    pub rotation: Vec<DayId>,
}

/// One training day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Day {
    /// A slug such as `a` or `upper-1`, referenced by the rotation.
    pub id: DayId,
    /// The name shown in the app, e.g. `Day A`.
    #[cfg_attr(test, schemars(schema_with = "super::schema::name"))]
    pub name: String,
    /// The exercises, in the order they are done.
    #[cfg_attr(test, schemars(schema_with = "super::schema::exercises"))]
    pub exercises: Vec<Exercise>,
}

/// One exercise of a day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Exercise {
    /// A slug such as `back-squat`. Progress is tracked per exercise id, so the same exercise
    /// on several days uses the same id (and the same name and progression rule).
    pub id: ExerciseId,
    /// The name shown in the app, e.g. `Back squat`.
    #[cfg_attr(test, schemars(schema_with = "super::schema::name"))]
    pub name: String,
    /// What to do: sets of reps, timed holds, or work/rest intervals.
    pub work: Work,
    /// The working load. Leave it out for bodyweight work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<Load>,
    /// Seconds of rest after each set (or after the intervals). In a superset, the rest after
    /// each member is the transition to the next member (often 0), and the rest of the last
    /// member is the rest after the whole group.
    #[serde(deserialize_with = "whole::seconds")]
    #[cfg_attr(test, schemars(schema_with = "super::schema::rest_seconds"))]
    pub rest: Seconds,
    /// Optional lifting tempo such as `3-1-X-0`: eccentric, bottom pause, concentric, top pause,
    /// in seconds, `X` meaning explosive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tempo: Option<Tempo>,
    /// Optional coaching notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, schemars(schema_with = "super::schema::optional_text"))]
    pub notes: Option<String>,
    /// Optional link to a demonstration (http or https).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demo_url: Option<DemoUrl>,
    /// Warm-up sets done before the working sets, lightest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(test, schemars(schema_with = "super::schema::warmup"))]
    pub warmup: Vec<WarmupSet>,
    /// Groups this exercise with the next ones carrying the same label into a superset (A1, A2,
    /// …). Members must be next to each other and have the same number of sets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superset: Option<SupersetId>,
    /// How the target moves from one session to the next. Defaults to `"none"`.
    #[serde(default, skip_serializing_if = "ProgressionRule::is_none")]
    pub progression: ProgressionRule,
}

/// What an exercise asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Work {
    /// Sets of repetitions: `{"reps": {"sets": 3, "reps": 5}}` or
    /// `{"reps": {"sets": 3, "reps": {"min": 8, "max": 12}}}`.
    Reps {
        /// Number of working sets.
        #[serde(deserialize_with = "whole::u16")]
        #[cfg_attr(test, schemars(schema_with = "super::schema::sets"))]
        sets: u16,
        /// Reps per set: a count or a `{min, max}` range.
        reps: RepTarget,
    },
    /// Timed holds such as a plank: `{"hold": {"sets": 3, "seconds": 45}}`.
    Hold {
        /// Number of holds.
        #[serde(deserialize_with = "whole::u16")]
        #[cfg_attr(test, schemars(schema_with = "super::schema::sets"))]
        sets: u16,
        /// Target length of each hold.
        #[serde(deserialize_with = "whole::seconds")]
        #[cfg_attr(test, schemars(schema_with = "super::schema::active_seconds"))]
        seconds: Seconds,
    },
    /// Work/rest intervals such as treadmill sprints:
    /// `{"intervals": {"work": 30, "rest": 90, "rounds": 8}}`. There is no rest after the last
    /// round: the exercise's own `rest` follows.
    Intervals {
        /// Seconds of work per round.
        #[serde(deserialize_with = "whole::seconds")]
        #[cfg_attr(test, schemars(schema_with = "super::schema::active_seconds"))]
        work: Seconds,
        /// Seconds of rest between rounds.
        #[serde(deserialize_with = "whole::seconds")]
        #[cfg_attr(test, schemars(schema_with = "super::schema::rest_seconds"))]
        rest: Seconds,
        /// Number of rounds.
        #[serde(deserialize_with = "whole::u16")]
        #[cfg_attr(test, schemars(schema_with = "super::schema::rounds"))]
        rounds: u16,
    },
}

impl Work {
    /// Whether this is timed work (a hold or intervals) rather than sets of reps.
    #[must_use]
    pub const fn is_timed(self) -> bool {
        !matches!(self, Self::Reps { .. })
    }

    /// Number of sets (holds, or sets of reps). Intervals count as one set.
    #[must_use]
    pub const fn sets(self) -> u16 {
        match self {
            Self::Reps { sets, .. } | Self::Hold { sets, .. } => sets,
            Self::Intervals { .. } => 1,
        }
    }
}

fn one_set() -> u16 {
    1
}

fn is_one_set(sets: &u16) -> bool {
    *sets == 1
}

/// One line of warm-up: `{"reps": 5, "load": {"percent_of_working_weight": 50}}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WarmupSet {
    /// How many sets of this warm-up. Defaults to 1.
    #[serde(
        default = "one_set",
        skip_serializing_if = "is_one_set",
        deserialize_with = "whole::u16"
    )]
    #[cfg_attr(test, schemars(schema_with = "super::schema::warmup_sets"))]
    pub sets: u16,
    /// Reps per warm-up set.
    #[serde(deserialize_with = "whole::reps")]
    #[cfg_attr(test, schemars(schema_with = "super::schema::rep_count"))]
    pub reps: Reps,
    /// A fixed weight (the empty bar) or a percentage of the working weight.
    pub load: WarmupLoad,
}

/// Cuts the load after a streak of failed sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Deload {
    /// Consecutive failed sessions that trigger the deload.
    #[serde(deserialize_with = "whole::u16")]
    #[cfg_attr(test, schemars(schema_with = "super::schema::failures"))]
    pub failures: u16,
    /// How much to take off, in percent of the working weight (10 means 100 kg becomes 90 kg).
    #[serde(deserialize_with = "whole::percent")]
    #[cfg_attr(test, schemars(schema_with = "super::schema::deload_percent"))]
    pub percent: Percent,
}

/// How an exercise progresses. Only the rule's data lives here; the progression engine applies
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgressionRule {
    /// No automatic progression: `"none"`.
    #[default]
    None,
    /// Add `increment` once every working set reaches the top of the rep range (or the fixed rep
    /// count): linear progression. Needs a fixed `kg` or `lb` load.
    /// `{"add_when_top_of_range": {"increment": {"kg": 2.5}}}`
    AddWhenTopOfRange {
        /// Weight added after a successful session.
        increment: UnitWeight,
        /// Optional deload after repeated failures.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deload_after_failures: Option<Deload>,
    },
    /// Double progression: add reps session after session within the range; once every set
    /// reaches the top, add `increment` and go back to the bottom of the range. Needs a rep
    /// range and a fixed `kg` or `lb` load.
    DoubleProgression {
        /// Weight added once the top of the range is reached.
        increment: UnitWeight,
        /// Optional deload after repeated failures.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deload_after_failures: Option<Deload>,
    },
    /// Raise the training max by `increment` once every working set reaches its target. Needs a
    /// `percent_of_training_max` load.
    TrainingMax {
        /// Added to the training max after a successful session.
        increment: UnitWeight,
        /// Optional training max cut after repeated failures.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deload_after_failures: Option<Deload>,
    },
}

impl ProgressionRule {
    /// Whether this is [`ProgressionRule::None`].
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// The weight added on success, if the rule adds any.
    #[must_use]
    pub const fn increment(&self) -> Option<UnitWeight> {
        match self {
            Self::None => None,
            Self::AddWhenTopOfRange { increment, .. }
            | Self::DoubleProgression { increment, .. }
            | Self::TrainingMax { increment, .. } => Some(*increment),
        }
    }

    /// The deload settings, if any.
    #[must_use]
    pub const fn deload(&self) -> Option<Deload> {
        match self {
            Self::None => None,
            Self::AddWhenTopOfRange {
                deload_after_failures,
                ..
            }
            | Self::DoubleProgression {
                deload_after_failures,
                ..
            }
            | Self::TrainingMax {
                deload_after_failures,
                ..
            } => *deload_after_failures,
        }
    }

    /// The rule's name as written in JSON.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AddWhenTopOfRange { .. } => "add_when_top_of_range",
            Self::DoubleProgression { .. } => "double_progression",
            Self::TrainingMax { .. } => "training_max",
        }
    }
}
