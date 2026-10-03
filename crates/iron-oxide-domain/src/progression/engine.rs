//! The rules: judging past sessions and computing the next targets.

use serde::{Deserialize, Serialize};

use super::change::{ChangeKind, ProgressionChange};
use super::history::{PastSession, Prescription, WorkingSet};
use super::settings::ProgressionSettings;
use super::target::{ExerciseTargets, NextTargets, SetGoal, SetTarget, TargetSource};
use crate::program::{
    Deload, Exercise, Load, ProgressionRule, RepTarget, UnitWeight, WarmupLoad, Work,
};
use crate::{Percent, Reps, Rounding, Weight};

/// How a past session went against its own prescription. See the
/// [module documentation](super) for the exact definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionVerdict {
    /// Every prescribed set reached the top of the target.
    Success,
    /// Every prescribed set is within the rep range, but not all at the top.
    Hold,
    /// At least one prescribed set is missing or below the target.
    Failure,
}

/// The next session of `exercise`: the sets to prefill, and what the last session in `history`
/// changed.
///
/// - `training_max`: the training max the lifter last entered for this exercise, needed when its
///   load is a percentage of the training max.
/// - `history`: this exercise's past sessions, **oldest first**, each with its own
///   [`Prescription`], filtered as described in the [module documentation](super) (build it with
///   [`exercise_history`](super::exercise_history)).
///
/// Each past session is judged against its own prescription. The step from one judged session to
/// the next uses the next one's rule (the version that showed its targets); `exercise` gives the
/// rule for the step from the last judged session, and the sets and reps of the result.
///
/// Returns [`NextTargets::NeedsTrainingMax`] when the load is a percentage of the training max
/// and `training_max` is `None`. Never fails otherwise: a program that did not pass validation
/// (e.g. a rule on timed work) is handled as if it had no rule.
#[must_use]
pub fn next_targets(
    exercise: &Exercise,
    training_max: Option<Weight>,
    settings: ProgressionSettings,
    history: &[PastSession],
) -> NextTargets {
    let percent_of_training_max = match exercise.load {
        Some(Load::PercentOfTrainingMax(percent)) => Some(percent),
        Some(Load::Weight(_)) | None => None,
    };
    let training_max = match (percent_of_training_max, training_max) {
        (Some(percent), Some(training_max)) => Some((percent, training_max)),
        (Some(_), None) => {
            return NextTargets::NeedsTrainingMax {
                exercise: exercise.id.clone(),
            };
        }
        (None, _) => None,
    };
    let step = settings.step();
    let sessions: Vec<&PastSession> = history.iter().filter(|s| !s.is_empty()).collect();
    let default_weight = match exercise.load {
        Some(Load::Weight(weight)) => Some(program_weight(weight, settings)),
        Some(Load::PercentOfTrainingMax(_)) => {
            training_max.map(|(percent, training_max)| percent_weight(training_max, percent, step))
        }
        None => None,
    };

    let plan = match (exercise.work, exercise.progression) {
        (
            Work::Reps { sets, reps },
            ProgressionRule::AddWhenTopOfRange {
                increment,
                deload_after_failures,
            },
        ) if !sessions.is_empty() => weight_rule(
            &sessions,
            &WeightRule {
                sets,
                target: reps,
                planned: Outcome {
                    increment: increment.weight(),
                    deload: deload_after_failures,
                    double: false,
                },
                default: default_weight.unwrap_or(Weight::ZERO),
                step,
            },
        ),
        (
            Work::Reps { sets, reps },
            ProgressionRule::DoubleProgression {
                increment,
                deload_after_failures,
            },
        ) if !sessions.is_empty() => weight_rule(
            &sessions,
            &WeightRule {
                sets,
                target: reps,
                planned: Outcome {
                    increment: increment.weight(),
                    deload: deload_after_failures,
                    double: true,
                },
                default: default_weight.unwrap_or(Weight::ZERO),
                step,
            },
        ),
        (
            Work::Reps { sets, reps },
            ProgressionRule::TrainingMax {
                increment,
                deload_after_failures,
            },
        ) => match training_max {
            Some((percent, training_max)) => training_max_rule(
                &sessions,
                &TrainingMaxRule {
                    sets,
                    target: reps,
                    percent,
                    training_max,
                    planned: Outcome {
                        increment: increment.weight(),
                        deload: deload_after_failures,
                        double: false,
                    },
                    step,
                },
            ),
            None => no_rule(exercise, &sessions, default_weight),
        },
        _ => no_rule(exercise, &sessions, default_weight),
    };

    let working_weight = plan.working.iter().filter_map(|set| set.weight).max();
    NextTargets::Ready(ExerciseTargets {
        exercise: exercise.id.clone(),
        source: plan.source,
        warmup: warmup(exercise, working_weight, settings),
        working: plan.working,
        training_max: plan
            .training_max
            .or(training_max.map(|(_, training_max)| training_max)),
        failed_sessions: plan.failed_sessions,
        last_verdict: plan.last_verdict,
        change: plan.change.map(|kind| ProgressionChange {
            exercise: exercise.id.clone(),
            name: exercise.name.clone(),
            kind,
        }),
    })
}

/// The working sets and state computed by one rule.
struct Plan {
    source: TargetSource,
    working: Vec<SetTarget>,
    training_max: Option<Weight>,
    failed_sessions: u16,
    last_verdict: Option<SessionVerdict>,
    change: Option<ChangeKind>,
}

/// A past session a rule can judge: its prescription asks for reps.
struct Judgeable<'a> {
    sets: &'a [WorkingSet],
    /// Prescribed sets and rep target.
    prescribed: u16,
    target: RepTarget,
    prescription: &'a Prescription,
}

/// A past session in a rule's replay.
enum Replayed<'a> {
    /// Judged against its prescription.
    Judged(Judgeable<'a>),
    /// No known prescription, or timed work: not judged, but it ends a failure streak.
    Unjudged,
}

/// Sorts the sessions for a rule's replay, and gives the position of the last judged one.
fn replay<'a>(sessions: &[&'a PastSession]) -> (Vec<Replayed<'a>>, Option<usize>) {
    let replayed: Vec<Replayed<'a>> = sessions
        .iter()
        .map(|session| match session.prescription.as_ref() {
            Some(
                prescription @ Prescription {
                    work: Work::Reps { sets, reps },
                    ..
                },
            ) => Replayed::Judged(Judgeable {
                sets: &session.sets,
                prescribed: *sets,
                target: *reps,
                prescription,
            }),
            _ => Replayed::Unjudged,
        })
        .collect();
    let last = replayed
        .iter()
        .rposition(|entry| matches!(entry, Replayed::Judged(_)));
    (replayed, last)
}

/// The rule that applies the outcome of the judged session at `index`: the rule of the version
/// that produced the target shown for the next judged session (that session's own
/// prescription), or `None` for the last judged session, whose outcome the planned rule applies.
/// A transition is so always replayed with the rule that produced the target the lifter saw next,
/// and never changes once a later session is logged.
fn next_rule(replayed: &[Replayed<'_>], index: usize) -> Option<ProgressionRule> {
    replayed
        .get(index + 1..)?
        .iter()
        .find_map(|entry| match entry {
            Replayed::Judged(session) => Some(session.prescription.rule),
            Replayed::Unjudged => None,
        })
}

/// Makes the summary agree with the failure count when sessions that cannot be judged ended the
/// streak after the last judged one.
fn with_failures(change: Option<ChangeKind>, failures: u16) -> Option<ChangeKind> {
    change.map(|kind| match kind {
        ChangeKind::Unchanged { weight, .. } => ChangeKind::Unchanged {
            weight,
            failed_sessions: failures,
        },
        ChangeKind::TrainingMaxUnchanged { training_max, .. } => ChangeKind::TrainingMaxUnchanged {
            training_max,
            failed_sessions: failures,
        },
        other => other,
    })
}

/// The prescribed working sets of a session: those with a `set_index` below the prescribed
/// number of sets, in the given order. Extra sets (a top single, a back-off set) are left out,
/// and when an index appears more than once (a retried set), only its first occurrence counts.
fn prescribed_sets(sets: &[WorkingSet], prescribed: u16) -> Vec<&WorkingSet> {
    let mut seen = vec![false; usize::from(prescribed.max(1))];
    sets.iter()
        .filter(|set| {
            seen.get_mut(usize::from(set.set_index))
                .is_some_and(|slot| !std::mem::replace(slot, true))
        })
        .collect()
}

/// The verdict from the rep counts of the sets that count: the `n`-th best decides. Also
/// returns that rep count.
fn verdict(mut reps: Vec<Reps>, prescribed: u16, target: RepTarget) -> (SessionVerdict, Reps) {
    reps.sort_unstable_by(|a, b| b.cmp(a));
    let n = usize::from(prescribed.max(1));
    let Some(&reached) = reps.get(n - 1) else {
        return (SessionVerdict::Failure, Reps::ZERO);
    };
    let verdict = if reached.is_zero() || reached < target.min() {
        SessionVerdict::Failure
    } else if reached >= target.max() {
        SessionVerdict::Success
    } else {
        SessionVerdict::Hold
    };
    (verdict, reached)
}

/// Judges a session under a weight rule, on its prescribed working sets only. The session's
/// weight (the base) is the lightest of them that has a weight; with none, `fallback`.
fn judge_weighted(session: &Judgeable<'_>, fallback: Weight) -> (SessionVerdict, Reps, Weight) {
    let base = prescribed_sets(session.sets, session.prescribed)
        .into_iter()
        .filter_map(|set| set.weight)
        .min()
        .unwrap_or(fallback);
    let reps = prescribed_sets(session.sets, session.prescribed)
        .into_iter()
        .map(|set| set.reps)
        .collect();
    let (verdict, reached) = verdict(reps, session.prescribed, session.target);
    (verdict, reached, base)
}

/// Judges a session under the training max rule: only prescribed working sets at least as heavy
/// as what they were prescribed count. A set logged with a weighted target
/// ([`WorkingSet::target`]) counts when it is at least that target's weight, exactly; a set logged
/// without one (before #60), or with a weightless target (never a training max prescription, so
/// not one to trust: any lift would beat it), when it is at least `legacy_at_least` (the
/// [`training_max_threshold`]).
fn judge_at_least(session: &Judgeable<'_>, legacy_at_least: Weight) -> SessionVerdict {
    let reps = prescribed_sets(session.sets, session.prescribed)
        .into_iter()
        .filter(|set| {
            let at_least = set
                .target
                .and_then(|target| target.weight)
                .unwrap_or(legacy_at_least);
            set.weight.unwrap_or(Weight::ZERO) >= at_least
        })
        .map(|set| set.reps)
        .collect();
    verdict(reps, session.prescribed, session.target).0
}

/// The largest step the settings allowed before #60, in nanograms: 2.5 kg. Every set logged
/// without a target was shown a weight rounded to a step at most this large.
const LEGACY_MAX_STEP_NANOGRAMS: u64 = 2_500_000_000_000;

/// How far below the exact prescribed weight a training max set logged **without a target**
/// (before #60) may be and still count: half of the legacy largest step, 1.25 kg, whatever the
/// settings. Any target shown then (rounded to the nearest step of at most 2.5 kg) is within it.
fn legacy_tolerance() -> Weight {
    Weight::from_nanograms(LEGACY_MAX_STEP_NANOGRAMS / 2).unwrap_or(Weight::ZERO)
}

/// The lightest weight that counts, for a past training max session's sets logged without a
/// target (before #60): the exact weight its prescription asked for (a percentage of the training
/// max at that point of the replay, or a fixed weight), less the [`legacy_tolerance`]. It depends
/// only on that session and the replayed training max, never on the current settings. Near
/// [`Weight::MAX`], where the app rounds targets down instead, it is lowered so that any target
/// the app could have shown still counts.
fn training_max_threshold(prescription: &Prescription, training_max: Weight) -> Weight {
    let exact = match prescription.load {
        Some(Load::PercentOfTrainingMax(percent)) => {
            percent.of(training_max).unwrap_or(Weight::MAX)
        }
        Some(Load::Weight(weight)) => weight.weight(),
        None => Weight::ZERO,
    };
    let legacy_max_step = Weight::from_nanograms(LEGACY_MAX_STEP_NANOGRAMS).unwrap_or(Weight::MAX);
    let below_cap = Weight::MAX.saturating_sub(legacy_max_step);
    exact.saturating_sub(legacy_tolerance()).min(below_cap)
}

/// Counts a failure, and says whether it triggers a deload.
fn count_failure(failures: &mut u16, deload: Option<Deload>) -> Option<Percent> {
    *failures = failures.saturating_add(1);
    match deload {
        Some(deload) if *failures >= deload.failures.max(1) => {
            *failures = 0;
            Some(deload.percent)
        }
        _ => None,
    }
}

fn reps_goal(target: RepTarget, aim: Reps) -> SetGoal {
    SetGoal::Reps {
        reps: aim,
        range: match target {
            RepTarget::Fixed(_) => None,
            RepTarget::Range(range) => Some(range),
        },
    }
}

fn uniform_sets(sets: u16, weight: Option<Weight>, goal: SetGoal) -> Vec<SetTarget> {
    vec![SetTarget { weight, goal }; usize::from(sets)]
}

/// The parts of a rule that apply a session's outcome.
#[derive(Clone, Copy)]
struct Outcome {
    increment: Weight,
    deload: Option<Deload>,
    double: bool,
}

impl Outcome {
    /// Nothing: no increment, no deload.
    const NOTHING: Self = Self {
        increment: Weight::ZERO,
        deload: None,
        double: false,
    };

    /// Whether this moves nothing: the rule was of another kind (or none).
    fn is_nothing(self) -> bool {
        self.increment.is_zero() && self.deload.is_none() && !self.double
    }

    /// A version's rule applied to a weight: only a weight rule moves the weight.
    fn for_weight(rule: ProgressionRule) -> Self {
        match rule {
            ProgressionRule::AddWhenTopOfRange {
                increment,
                deload_after_failures,
            } => Self {
                increment: increment.weight(),
                deload: deload_after_failures,
                double: false,
            },
            ProgressionRule::DoubleProgression {
                increment,
                deload_after_failures,
            } => Self {
                increment: increment.weight(),
                deload: deload_after_failures,
                double: true,
            },
            ProgressionRule::TrainingMax { .. } | ProgressionRule::None => Self::NOTHING,
        }
    }

    /// A version's rule applied to the training max: only the training max rule moves it.
    fn for_training_max(rule: ProgressionRule) -> Self {
        match rule {
            ProgressionRule::TrainingMax {
                increment,
                deload_after_failures,
            } => Self {
                increment: increment.weight(),
                deload: deload_after_failures,
                double: false,
            },
            ProgressionRule::AddWhenTopOfRange { .. }
            | ProgressionRule::DoubleProgression { .. }
            | ProgressionRule::None => Self::NOTHING,
        }
    }
}

struct WeightRule {
    /// The planned sets and rep target.
    sets: u16,
    target: RepTarget,
    /// The planned rule, applied to the last judged session.
    planned: Outcome,
    /// The base when a session has no weighted prescribed set and no fixed prescribed load.
    default: Weight,
    step: Weight,
}

/// Where the reps of the planned sets go after the last session, for double progression.
#[derive(Clone, Copy)]
enum NextReps {
    /// The bottom of the planned range.
    Bottom,
    /// The top of the planned range.
    Top,
    /// This many, clamped into the planned range.
    Exactly(Reps),
}

/// `add_when_top_of_range` and `double_progression`, from a non-empty history.
fn weight_rule(sessions: &[&PastSession], rule: &WeightRule) -> Plan {
    let (replayed, last) = replay(sessions);
    let mut failures = 0;
    let mut weight = rule.default;
    let mut next_reps = NextReps::Bottom;
    let mut last_verdict = None;
    let mut change = None;
    for (index, entry) in replayed.iter().enumerate() {
        let Replayed::Judged(session) = entry else {
            failures = 0;
            continue;
        };
        // A session done under a version without a weight rule moves nothing.
        let outcome = if Outcome::for_weight(session.prescription.rule).is_nothing() {
            Outcome::NOTHING
        } else {
            next_rule(&replayed, index).map_or(rule.planned, Outcome::for_weight)
        };
        let fallback = match session.prescription.load {
            Some(Load::Weight(load)) => load.weight(),
            Some(Load::PercentOfTrainingMax(_)) | None => rule.default,
        };
        let (min, max) = (session.target.min(), session.target.max());
        let (verdict, reached, base) = judge_weighted(session, fallback);
        weight = base;
        let unchanged = |failed_sessions| ChangeKind::Unchanged {
            weight: base,
            failed_sessions,
        };
        let kind = match verdict {
            SessionVerdict::Success => {
                failures = 0;
                let to = increase(base, outcome.increment, rule.step);
                if to > base {
                    weight = to;
                    next_reps = NextReps::Bottom;
                    if outcome.double {
                        ChangeKind::WeightIncreaseRepsReset {
                            from: base,
                            to,
                            reps_from: reached,
                            reps_to: min,
                        }
                    } else {
                        ChangeKind::WeightIncrease { from: base, to }
                    }
                } else {
                    // At the cap: nothing heavier to load, so stay at the top of the range.
                    next_reps = NextReps::Top;
                    unchanged(0)
                }
            }
            SessionVerdict::Hold => {
                failures = 0;
                // reached < max here, so one more rep stays within the session's range.
                let more = Reps::new(reached.get().saturating_add(1)).min(max);
                next_reps = NextReps::Exactly(more);
                if outcome.double {
                    ChangeKind::RepsIncrease {
                        weight: base,
                        from: reached,
                        to: more,
                    }
                } else {
                    unchanged(0)
                }
            }
            SessionVerdict::Failure => {
                next_reps = NextReps::Bottom;
                match count_failure(&mut failures, outcome.deload) {
                    Some(percent) => {
                        weight = deload_weight(base, percent, rule.step);
                        ChangeKind::Deload {
                            from: base,
                            to: weight,
                        }
                    }
                    None => unchanged(failures),
                }
            }
        };
        if Some(index) == last {
            last_verdict = Some(verdict);
            change = Some(kind);
        }
    }
    let (min, max) = (rule.target.min(), rule.target.max());
    let aim = if rule.planned.double {
        match next_reps {
            NextReps::Bottom => min,
            NextReps::Top => max,
            NextReps::Exactly(reps) => reps.clamp(min, max.max(min)),
        }
    } else {
        max
    };
    Plan {
        source: if last.is_some() {
            TargetSource::Progression
        } else {
            TargetSource::ProgramDefault
        },
        working: uniform_sets(rule.sets, Some(weight), reps_goal(rule.target, aim)),
        training_max: None,
        failed_sessions: failures,
        last_verdict,
        change: with_failures(change, failures),
    }
}

struct TrainingMaxRule {
    /// The planned sets and rep target.
    sets: u16,
    target: RepTarget,
    /// The planned percentage of the training max.
    percent: Percent,
    training_max: Weight,
    /// The planned rule, applied to the last judged session.
    planned: Outcome,
    step: Weight,
}

/// `training_max`: replays the history on the training max, starting from the one entered.
/// Nothing in the replay depends on the settings: sessions are judged against the target stored
/// with each set (or, for sets logged before #60, their own prescription with a fixed tolerance),
/// and each step uses the rule that showed the next targets
/// (increments added exactly, deloads exact).
fn training_max_rule(sessions: &[&PastSession], rule: &TrainingMaxRule) -> Plan {
    let (replayed, last) = replay(sessions);
    let mut training_max = rule.training_max;
    let mut failures = 0;
    let mut last_verdict = None;
    let mut change = None;
    for (index, entry) in replayed.iter().enumerate() {
        let Replayed::Judged(session) = entry else {
            failures = 0;
            continue;
        };
        // A session done under a version without the training max rule moves nothing.
        let outcome = if Outcome::for_training_max(session.prescription.rule).is_nothing() {
            Outcome::NOTHING
        } else {
            next_rule(&replayed, index).map_or(rule.planned, Outcome::for_training_max)
        };
        let at_least = training_max_threshold(session.prescription, training_max);
        let verdict = judge_at_least(session, at_least);
        let before = training_max;
        let unchanged = |failed_sessions| ChangeKind::TrainingMaxUnchanged {
            training_max: before,
            failed_sessions,
        };
        let kind = match verdict {
            SessionVerdict::Success => {
                failures = 0;
                training_max = before.checked_add(outcome.increment).unwrap_or(Weight::MAX);
                if training_max > before {
                    ChangeKind::TrainingMaxIncrease {
                        from: before,
                        to: training_max,
                    }
                } else {
                    unchanged(0)
                }
            }
            SessionVerdict::Hold => {
                failures = 0;
                unchanged(0)
            }
            SessionVerdict::Failure => match count_failure(&mut failures, outcome.deload) {
                Some(percent) => {
                    training_max = exact_deload(before, percent);
                    ChangeKind::TrainingMaxDeload {
                        from: before,
                        to: training_max,
                    }
                }
                None => unchanged(failures),
            },
        };
        if Some(index) == last {
            last_verdict = Some(verdict);
            change = Some(kind);
        }
    }
    let weight = percent_weight(training_max, rule.percent, rule.step);
    Plan {
        source: if last.is_some() {
            TargetSource::Progression
        } else {
            TargetSource::ProgramDefault
        },
        working: uniform_sets(
            rule.sets,
            Some(weight),
            reps_goal(rule.target, rule.target.max()),
        ),
        training_max: Some(training_max),
        failed_sessions: failures,
        last_verdict,
        change: with_failures(change, failures),
    }
}

/// No rule (or timed work): the last performance, else the program default.
fn no_rule(exercise: &Exercise, sessions: &[&PastSession], default_weight: Option<Weight>) -> Plan {
    let (count, goal) = match exercise.work {
        Work::Reps { sets, reps } => {
            let aim = match (reps, exercise.progression) {
                (RepTarget::Range(range), ProgressionRule::DoubleProgression { .. }) => range.min,
                _ => reps.max(),
            };
            (sets, reps_goal(reps, aim))
        }
        Work::Hold { sets, seconds } => (sets, SetGoal::Hold { seconds }),
        Work::Intervals { work, rest, rounds } => (1, SetGoal::Intervals { work, rest, rounds }),
    };
    let Some(last) = sessions.last() else {
        return Plan {
            source: TargetSource::ProgramDefault,
            working: uniform_sets(count, default_weight, goal),
            training_max: None,
            failed_sessions: 0,
            last_verdict: None,
            change: None,
        };
    };
    // The last session's working sets by index; extras (index at or above the planned count)
    // are left out. A set not done repeats the last one done before it, else the first one.
    let done_sets = prescribed_sets(&last.sets, count);
    let working = (0..count)
        .map(|index| {
            let done = done_sets
                .iter()
                .filter(|set| set.set_index <= index)
                .max_by_key(|set| set.set_index)
                .or_else(|| done_sets.iter().min_by_key(|set| set.set_index))
                .copied();
            let weight = done.and_then(|set| set.weight).or(default_weight);
            let goal = match (goal, exercise.work, done) {
                (
                    SetGoal::Reps { range, .. },
                    Work::Reps {
                        reps: RepTarget::Range(bounds),
                        ..
                    },
                    Some(set),
                ) => SetGoal::Reps {
                    reps: set.reps.clamp(bounds.min, bounds.max.max(bounds.min)),
                    range,
                },
                _ => goal,
            };
            SetTarget { weight, goal }
        })
        .collect();
    Plan {
        source: TargetSource::LastPerformance,
        working,
        training_max: None,
        failed_sessions: 0,
        last_verdict: None,
        change: None,
    }
}

/// The warm-up sets, from the heaviest working weight.
fn warmup(
    exercise: &Exercise,
    working: Option<Weight>,
    settings: ProgressionSettings,
) -> Vec<SetTarget> {
    exercise
        .warmup
        .iter()
        .flat_map(|line| {
            let weight = match line.load {
                WarmupLoad::Weight(weight) => Some(fixed_warmup(weight, working, settings)),
                WarmupLoad::PercentOfWorkingWeight(percent) => {
                    working.map(|working| lighter_share(working, percent, settings.step()))
                }
            };
            let target = SetTarget {
                weight,
                goal: SetGoal::Reps {
                    reps: line.reps,
                    range: None,
                },
            };
            std::iter::repeat_n(target, usize::from(line.sets))
        })
        .collect()
}

/// `exact` rounded to `step`, falling back to rounding down if rounding up passes
/// [`Weight::MAX`], and to `exact` itself if the rounding gives zero.
fn round_or_exact(exact: Weight, step: Weight, rounding: Rounding) -> Weight {
    let rounded = exact
        .round_to(step, rounding)
        .or_else(|_| exact.round_to(step, Rounding::Down))
        .unwrap_or(exact);
    if rounded.is_zero() { exact } else { rounded }
}

/// A weight written in the program: as written in the lifter's unit, and rounded to the nearest
/// step in the other unit (a 100 kg load is 220 lb for a lifter who loads in pounds).
fn program_weight(weight: UnitWeight, settings: ProgressionSettings) -> Weight {
    if weight.unit() == settings.unit() {
        weight.weight()
    } else {
        round_or_exact(weight.weight(), settings.step(), Rounding::Nearest)
    }
}

/// A fixed warm-up weight, converted like [`program_weight`] (the 20 kg bar is 45 lb), unless
/// rounding would make it as heavy as the working weight while the written weight was lighter:
/// then it is kept exact.
fn fixed_warmup(
    weight: UnitWeight,
    working: Option<Weight>,
    settings: ProgressionSettings,
) -> Weight {
    let converted = program_weight(weight, settings);
    match working {
        Some(working) if converted >= working && weight.weight() < working => weight.weight(),
        _ => converted,
    }
}

/// `base + increment`, rounded to the nearest step, or up when the nearest does not move it (an
/// increment smaller than half a step), or down when rounding up passes [`Weight::MAX`], or the
/// exact sum as a last resort. Returns `base` when nothing heavier can be loaded (at
/// [`Weight::MAX`], or a zero increment).
fn increase(base: Weight, increment: Weight, step: Weight) -> Weight {
    if increment.is_zero() {
        return base;
    }
    let raw = base.checked_add(increment).unwrap_or(Weight::MAX);
    [Rounding::Nearest, Rounding::Up, Rounding::Down]
        .into_iter()
        .filter_map(|rounding| raw.round_to(step, rounding).ok())
        .chain([raw])
        .find(|&weight| weight > base)
        .unwrap_or(base)
}

/// `weight × (1 − percent)`, rounded like a warm-up: see [`lighter_share`].
fn deload_weight(weight: Weight, percent: Percent, step: Weight) -> Weight {
    lighter_share(weight, kept_share(percent), step)
}

/// The share of `weight` a deload of `percent` keeps.
fn kept_share(percent: Percent) -> Percent {
    let kept = Percent::HUNDRED
        .basis_points()
        .saturating_sub(percent.basis_points());
    // `kept` is at most 100 %, so the conversion cannot fail.
    Percent::from_basis_points(kept).unwrap_or(Percent::HUNDRED)
}

/// `training_max × (1 − percent)`, exact to the nanogram and unrounded, so the replayed training
/// max never depends on the step setting.
fn exact_deload(training_max: Weight, percent: Percent) -> Weight {
    kept_share(percent)
        .of(training_max)
        .unwrap_or(training_max)
        .min(training_max)
}

/// `percent` of the training max, rounded to the nearest step.
fn percent_weight(training_max: Weight, percent: Percent, step: Weight) -> Weight {
    let exact = percent.of(training_max).unwrap_or(Weight::MAX);
    round_or_exact(exact, step, Rounding::Nearest)
}

/// `percent` of `weight` (a warm-up share, or what a deload keeps), rounded to the nearest step,
/// or down when the nearest would not be lighter than `weight`. Never heavier than `weight`.
fn lighter_share(weight: Weight, percent: Percent, step: Weight) -> Weight {
    let exact = percent.of(weight).unwrap_or(weight);
    [Rounding::Nearest, Rounding::Down]
        .into_iter()
        .map(|rounding| round_or_exact(exact, step, rounding))
        .find(|&share| share < weight)
        .unwrap_or_else(|| exact.min(weight))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Unit;

    fn kg(value: f64) -> Weight {
        Weight::from_kg(value).unwrap()
    }

    fn lb(value: f64) -> Weight {
        Weight::from_lb(value).unwrap()
    }

    fn pct(value: f64) -> Percent {
        Percent::new(value).unwrap()
    }

    fn sets(weight: f64, reps: &[u16]) -> Vec<WorkingSet> {
        reps.iter()
            .map(|&r| WorkingSet::new(kg(weight), Reps::new(r)))
            .collect()
    }

    const STEP_KG: f64 = 2.5;

    const HUNDRED: f64 = 100.0;

    fn prescription(sets: u16, target: RepTarget, load: Option<Load>) -> Prescription {
        Prescription {
            work: Work::Reps { sets, reps: target },
            load,
            rule: ProgressionRule::None,
        }
    }

    /// Numbers the sets 0, 1, 2… as logged.
    fn numbered(sets: &[WorkingSet]) -> Vec<WorkingSet> {
        (0..).zip(sets).map(|(i, set)| set.at(i)).collect()
    }

    fn weighted(sets: &[WorkingSet], n: u16, target: RepTarget) -> (SessionVerdict, Reps, Weight) {
        let sets = numbered(sets);
        let prescription = prescription(n, target, None);
        let session = Judgeable {
            sets: &sets,
            prescribed: n,
            target,
            prescription: &prescription,
        };
        judge_weighted(&session, kg(40.0))
    }

    fn at_least(sets: &[WorkingSet], n: u16, target: RepTarget, minimum: Weight) -> SessionVerdict {
        let sets = numbered(sets);
        let prescription = prescription(n, target, None);
        let session = Judgeable {
            sets: &sets,
            prescribed: n,
            target,
            prescription: &prescription,
        };
        judge_at_least(&session, minimum)
    }

    #[test]
    fn judging_fixed_reps() {
        let five = RepTarget::Fixed(Reps::new(5));
        let judge = |reps: &[u16]| weighted(&sets(HUNDRED, reps), 3, five).0;
        assert_eq!(judge(&[5, 5, 5]), SessionVerdict::Success);
        assert_eq!(judge(&[6, 5, 7]), SessionVerdict::Success);
        assert_eq!(judge(&[5, 5, 4]), SessionVerdict::Failure);
        assert_eq!(judge(&[5, 5]), SessionVerdict::Failure, "missing set");
        assert_eq!(judge(&[5, 5, 0]), SessionVerdict::Failure, "failed attempt");
        // Only the prescribed sets (indices 0 to 2) are judged: an extra set cannot make up
        // for a missed one, nor spoil a success.
        assert_eq!(judge(&[5, 3, 5, 5]), SessionVerdict::Failure, "extra set");
        assert_eq!(judge(&[5, 5, 5, 0]), SessionVerdict::Success, "extra set");
        assert_eq!(judge(&[]), SessionVerdict::Failure);
    }

    #[test]
    fn judging_ranges() {
        let range = RepTarget::Range(crate::program::RepRange {
            min: Reps::new(8),
            max: Reps::new(12),
        });
        let judge = |reps: &[u16]| {
            let (verdict, reached, _) = weighted(&sets(50.0, reps), 3, range);
            (verdict, reached)
        };
        assert_eq!(
            judge(&[12, 12, 12]),
            (SessionVerdict::Success, Reps::new(12))
        );
        assert_eq!(judge(&[12, 10, 9]), (SessionVerdict::Hold, Reps::new(9)));
        assert_eq!(judge(&[8, 8, 8]), (SessionVerdict::Hold, Reps::new(8)));
        assert_eq!(judge(&[12, 12, 7]), (SessionVerdict::Failure, Reps::new(7)));
    }

    #[test]
    fn only_the_prescribed_sets_are_judged() {
        let five = RepTarget::Fixed(Reps::new(5));
        let set = |weight, reps| WorkingSet::new(kg(weight), Reps::new(reps));
        // A heavier single after the work set is an extra.
        assert_eq!(
            weighted(&[set(100.0, 5), set(110.0, 1)], 1, five),
            (SessionVerdict::Success, Reps::new(5), kg(100.0))
        );
        // Logged first, the single is the prescribed set.
        assert_eq!(
            weighted(&[set(110.0, 1), set(100.0, 5)], 1, five),
            (SessionVerdict::Failure, Reps::new(1), kg(110.0))
        );
        // A failed heavier attempt after the work sets.
        let attempt = [set(100.0, 5), set(100.0, 5), set(100.0, 5), set(110.0, 0)];
        assert_eq!(
            weighted(&attempt, 3, five),
            (SessionVerdict::Success, Reps::new(5), kg(100.0))
        );
        // A back-off set neither counts nor lowers the base.
        let backoff = [set(100.0, 5), set(100.0, 5), set(100.0, 5), set(60.0, 10)];
        assert_eq!(
            weighted(&backoff, 3, five),
            (SessionVerdict::Success, Reps::new(5), kg(100.0))
        );
        // Nor does it rescue a missing working set: two sets at 100 kg are judged.
        let short = [set(100.0, 5), set(100.0, 5)];
        let rescued = [set(100.0, 5), set(100.0, 5), set(60.0, 10).at(3)];
        assert_eq!(weighted(&short, 3, five).0, SessionVerdict::Failure);
        let sets_with_gap: Vec<WorkingSet> =
            numbered(&short).into_iter().chain([rescued[2]]).collect();
        let prescription = prescription(3, five, None);
        let session = Judgeable {
            sets: &sets_with_gap,
            prescribed: 3,
            target: five,
            prescription: &prescription,
        };
        assert_eq!(
            judge_weighted(&session, kg(40.0)),
            (SessionVerdict::Failure, Reps::ZERO, kg(100.0))
        );
        // Dropping the weight on the last prescribed set lowers the base.
        let dropped = [set(100.0, 5), set(100.0, 5), set(90.0, 5)];
        assert_eq!(weighted(&dropped, 3, five).2, kg(90.0));
        // Sets without a weight count for reps, not for the base; with none weighted, the
        // fallback.
        let slip = [
            set(100.0, 5),
            set(100.0, 5),
            WorkingSet::bodyweight(Reps::new(5)),
        ];
        assert_eq!(
            weighted(&slip, 3, five),
            (SessionVerdict::Success, Reps::new(5), kg(100.0))
        );
        let none = [WorkingSet::bodyweight(Reps::new(5)); 3];
        assert_eq!(
            weighted(&none, 3, five),
            (SessionVerdict::Success, Reps::new(5), kg(40.0))
        );
    }

    #[test]
    fn judging_ignores_sets_lighter_than_the_threshold() {
        let five = RepTarget::Fixed(Reps::new(5));
        let mut done = sets(HUNDRED, &[5, 5]);
        done.push(WorkingSet::new(kg(90.0), Reps::new(5)));
        assert_eq!(
            at_least(&done, 3, five, kg(HUNDRED)),
            SessionVerdict::Failure
        );
        assert_eq!(at_least(&done, 3, five, kg(90.0)), SessionVerdict::Success);
        // An extra set at the target does not stand in for a light prescribed one.
        done.push(WorkingSet::new(kg(HUNDRED), Reps::new(5)));
        assert_eq!(
            at_least(&done, 3, five, kg(HUNDRED)),
            SessionVerdict::Failure
        );
        let bodyweight = [WorkingSet::bodyweight(Reps::new(5)); 3];
        assert_eq!(
            at_least(&bodyweight, 3, five, Weight::ZERO),
            SessionVerdict::Success
        );
        assert_eq!(
            at_least(&bodyweight, 3, five, kg(1.0)),
            SessionVerdict::Failure
        );
    }

    #[test]
    fn a_set_logged_with_its_target_is_judged_against_it_exactly() {
        let five = RepTarget::Fixed(Reps::new(5));
        let shown = |weight| SetTarget {
            weight: Some(kg(weight)),
            goal: crate::progression::SetGoal::Reps {
                reps: Reps::new(5),
                range: None,
            },
        };
        let lifted = |weight, target| WorkingSet::new(kg(weight), Reps::new(5)).prescribed(target);
        // 80 % of 96.75 kg is 77.4 kg; with a 5 kg step the app showed 75 kg. The legacy
        // threshold (77.4 − 1.25 = 76.15 kg) would refuse it; the stored target accepts it.
        let legacy = kg(76.15);
        let as_shown = [lifted(75.0, shown(75.0)); 3];
        assert_eq!(
            at_least(&as_shown, 3, five, legacy),
            SessionVerdict::Success
        );
        // Lighter than its own target: does not count, even within the legacy tolerance.
        let light = [
            lifted(80.0, shown(80.0)),
            lifted(80.0, shown(80.0)),
            lifted(77.5, shown(80.0)),
        ];
        assert_eq!(at_least(&light, 3, five, legacy), SessionVerdict::Failure);
        // Heavier than the target counts.
        let heavy = [lifted(82.5, shown(80.0)); 3];
        assert_eq!(at_least(&heavy, 3, five, legacy), SessionVerdict::Success);
        // Mixed: sets without a target (logged before #60) keep the legacy threshold.
        let mut mixed = vec![lifted(75.0, shown(75.0)), lifted(75.0, shown(75.0))];
        mixed.push(WorkingSet::new(kg(77.5), Reps::new(5)));
        assert_eq!(at_least(&mixed, 3, five, legacy), SessionVerdict::Success);
        mixed[2] = WorkingSet::new(kg(75.0), Reps::new(5));
        assert_eq!(at_least(&mixed, 3, five, legacy), SessionVerdict::Failure);
        // A weightless target is no training max prescription: the legacy threshold applies, so
        // a light set does not count just because it beats "nothing".
        let weightless = SetTarget {
            weight: None,
            ..shown(0.0)
        };
        let any = [lifted(20.0, weightless); 3];
        assert_eq!(at_least(&any, 3, five, legacy), SessionVerdict::Failure);
        let enough = [lifted(77.5, weightless); 3];
        assert_eq!(at_least(&enough, 3, five, legacy), SessionVerdict::Success);
    }

    #[test]
    fn thresholds_do_not_depend_on_the_settings() {
        assert_eq!(legacy_tolerance(), kg(1.25));
        let five = RepTarget::Fixed(Reps::new(5));
        let percent = |value| prescription(3, five, Some(Load::PercentOfTrainingMax(pct(value))));
        // 65 % of 121 kg is 78.65 kg: 77.4 kg and up count.
        assert_eq!(training_max_threshold(&percent(65.0), kg(121.0)), kg(77.4));
        let fixed = prescription(
            3,
            five,
            Some(Load::Weight(
                UnitWeight::new(100.0, crate::Unit::Kg).unwrap(),
            )),
        );
        assert_eq!(training_max_threshold(&fixed, kg(121.0)), kg(98.75));
        assert_eq!(
            training_max_threshold(&prescription(3, five, None), kg(121.0)),
            Weight::ZERO
        );
        // Near the cap, lowered below any target rounded down from there.
        let below_cap = Weight::MAX.saturating_sub(kg(2.5));
        assert_eq!(
            training_max_threshold(&percent(100.0), Weight::MAX),
            below_cap
        );
        assert_eq!(
            training_max_threshold(&percent(1_000.0), Weight::MAX),
            below_cap
        );
        assert_eq!(
            training_max_threshold(&percent(1.0), kg(50.0)),
            Weight::ZERO
        );
    }

    #[test]
    fn replay_order_and_unjudged_sessions() {
        let five = RepTarget::Fixed(Reps::new(5));
        let judged = PastSession::new(prescription(3, five, None), sets(HUNDRED, &[5]));
        let hold = PastSession::new(
            Prescription {
                work: Work::Hold {
                    sets: 3,
                    seconds: crate::Seconds::new(30),
                },
                load: None,
                rule: ProgressionRule::None,
            },
            sets(HUNDRED, &[1]),
        );
        let unknown = PastSession::without_prescription(sets(HUNDRED, &[5]));
        let (replayed, last) = replay(&[&judged, &hold, &judged, &unknown]);
        assert_eq!(last, Some(2));
        let kinds: Vec<bool> = replayed
            .iter()
            .map(|entry| matches!(entry, Replayed::Judged(_)))
            .collect();
        assert_eq!(kinds, [true, false, true, false]);
        assert_eq!(replay(&[&hold, &unknown]).1, None);
    }

    #[test]
    fn outcomes_of_rules() {
        let increment = UnitWeight::new(2.5, crate::Unit::Kg).unwrap();
        let deload = Some(Deload {
            failures: 2,
            percent: pct(10.0),
        });
        let add = ProgressionRule::AddWhenTopOfRange {
            increment,
            deload_after_failures: deload,
        };
        let double = ProgressionRule::DoubleProgression {
            increment,
            deload_after_failures: deload,
        };
        let tm = ProgressionRule::TrainingMax {
            increment,
            deload_after_failures: deload,
        };
        let check = |outcome: Outcome, increment: Weight, deload: Option<Deload>, double: bool| {
            assert_eq!(outcome.increment, increment);
            assert_eq!(outcome.deload, deload);
            assert_eq!(outcome.double, double);
        };
        check(Outcome::for_weight(add), kg(2.5), deload, false);
        check(Outcome::for_weight(double), kg(2.5), deload, true);
        check(Outcome::for_training_max(tm), kg(2.5), deload, false);
        // A rule of another kind moves nothing.
        for (outcome, _) in [
            (Outcome::for_weight(tm), ()),
            (Outcome::for_weight(ProgressionRule::None), ()),
            (Outcome::for_training_max(add), ()),
            (Outcome::for_training_max(double), ()),
            (Outcome::for_training_max(ProgressionRule::None), ()),
        ] {
            check(outcome, Weight::ZERO, None, false);
        }
    }

    #[test]
    fn duplicate_indices_count_once() {
        let set = |index, reps| WorkingSet::new(kg(HUNDRED), Reps::new(reps)).at(index);
        let sets = [set(0, 0), set(0, 5), set(1, 5), set(1, 1), set(5, 9)];
        let reps: Vec<u16> = prescribed_sets(&sets, 3)
            .iter()
            .map(|set| set.reps.get())
            .collect();
        assert_eq!(reps, [0, 5], "first occurrence of 0 and 1; 5 is an extra");
        assert!(prescribed_sets(&sets, 0).len() == 1, "at least one set");
        assert!(prescribed_sets(&[set(u16::MAX, 5)], u16::MAX).is_empty());
    }

    #[test]
    fn transitions_use_the_next_sessions_rule() {
        let five = RepTarget::Fixed(Reps::new(5));
        let with_rule = |rule| {
            PastSession::new(
                Prescription {
                    rule,
                    ..prescription(3, five, None)
                },
                sets(HUNDRED, &[5]),
            )
        };
        let increment = UnitWeight::new(2.5, crate::Unit::Kg).unwrap();
        let tm = ProgressionRule::TrainingMax {
            increment,
            deload_after_failures: None,
        };
        let first = with_rule(ProgressionRule::None);
        let second = with_rule(tm);
        let unknown = PastSession::without_prescription(sets(HUNDRED, &[5]));
        let sessions = [&first, &unknown, &second];
        let (replayed, _) = replay(&sessions);
        assert_eq!(
            next_rule(&replayed, 0),
            Some(tm),
            "skips the unjudged session"
        );
        assert_eq!(
            next_rule(&replayed, 2),
            None,
            "the last one: the planned rule"
        );
        assert_eq!(next_rule(&replayed, 9), None);
    }

    #[test]
    fn summaries_agree_with_the_failure_count() {
        let unchanged = ChangeKind::Unchanged {
            weight: kg(HUNDRED),
            failed_sessions: 1,
        };
        assert_eq!(
            with_failures(Some(unchanged), 0),
            Some(ChangeKind::Unchanged {
                weight: kg(HUNDRED),
                failed_sessions: 0
            })
        );
        let tm = ChangeKind::TrainingMaxUnchanged {
            training_max: kg(HUNDRED),
            failed_sessions: 2,
        };
        assert_eq!(
            with_failures(Some(tm), 0),
            Some(ChangeKind::TrainingMaxUnchanged {
                training_max: kg(HUNDRED),
                failed_sessions: 0
            })
        );
        let up = ChangeKind::WeightIncrease {
            from: kg(HUNDRED),
            to: kg(102.5),
        };
        assert_eq!(with_failures(Some(up), 0), Some(up));
        assert_eq!(with_failures(None, 0), None);
    }

    #[test]
    fn exact_deloads() {
        assert_eq!(exact_deload(kg(121.0), pct(10.0)), kg(108.9));
        assert_eq!(exact_deload(kg(100.0), Percent::ZERO), kg(100.0));
        assert_eq!(exact_deload(kg(100.0), Percent::MAX), Weight::ZERO);
        assert_eq!(kept_share(pct(12.5)), pct(87.5));
    }

    #[test]
    fn increase_rounds_to_the_step() {
        let step = kg(STEP_KG);
        assert_eq!(increase(kg(100.0), kg(2.5), step), kg(102.5));
        assert_eq!(increase(kg(100.0), kg(5.0), step), kg(105.0));
        // Smaller than the step: goes up to the next step instead of stalling.
        assert_eq!(increase(kg(100.0), kg(1.0), step), kg(102.5));
        // Off the grid: nearest step above.
        assert_eq!(increase(kg(101.0), kg(2.5), step), kg(102.5));
        assert_eq!(increase(kg(101.0), kg(1.0), step), kg(102.5));
        // Pounds.
        let step = lb(5.0);
        assert_eq!(increase(lb(225.0), lb(5.0), step), lb(230.0));
        assert_eq!(increase(lb(225.0), lb(2.5), step), lb(230.0));
        // A kg increment on a lb grid.
        assert_eq!(increase(lb(225.0), kg(2.5), step), lb(230.0));
        // Zero increment and the cap.
        assert_eq!(increase(kg(100.0), Weight::ZERO, kg(STEP_KG)), kg(100.0));
        assert_eq!(increase(Weight::MAX, kg(2.5), kg(STEP_KG)), Weight::MAX);
        let top = Weight::MAX.round_to(step, Rounding::Down).unwrap();
        assert_eq!(
            increase(top, lb(5.0), step),
            Weight::MAX,
            "the exact sum, capped"
        );
        assert_eq!(increase(Weight::MAX, lb(5.0), step), Weight::MAX);
        // Rounding up would pass the cap, rounding down still moves: the step below the cap.
        let below = top.saturating_sub(lb(1.0));
        assert_eq!(increase(below, lb(5.0), step), top);
        // From zero (a body-weight log), a small increment goes to the first step.
        assert_eq!(increase(Weight::ZERO, kg(0.5), kg(STEP_KG)), kg(2.5));
    }

    #[test]
    fn deload_rounds_to_a_lighter_step() {
        let step = kg(STEP_KG);
        assert_eq!(deload_weight(kg(100.0), pct(10.0), step), kg(90.0));
        assert_eq!(deload_weight(kg(80.0), pct(10.0), step), kg(72.5));
        assert_eq!(deload_weight(kg(82.5), pct(10.0), step), kg(75.0));
        // Off the grid, a small cut: the nearest step is heavier, so round down.
        assert_eq!(deload_weight(kg(24.0), pct(1.0), step), kg(22.5));
        // Tiny weights keep the exact value rather than zero.
        assert_eq!(deload_weight(kg(2.0), pct(10.0), step), kg(1.8));
        assert_eq!(deload_weight(kg(100.0), Percent::ZERO, step), kg(100.0));
        assert_eq!(
            deload_weight(kg(100.0), Percent::HUNDRED, step),
            Weight::ZERO
        );
        assert_eq!(deload_weight(kg(100.0), Percent::MAX, step), Weight::ZERO);
        assert_eq!(deload_weight(Weight::ZERO, pct(10.0), step), Weight::ZERO);
        assert_eq!(deload_weight(lb(135.0), pct(10.0), lb(5.0)), lb(120.0));
        assert_eq!(deload_weight(lb(225.0), pct(10.0), lb(5.0)), lb(205.0));
    }

    #[test]
    fn percentages_round_to_the_nearest_step() {
        let step = kg(STEP_KG);
        assert_eq!(percent_weight(kg(100.0), pct(77.5), step), kg(77.5));
        assert_eq!(percent_weight(kg(103.0), pct(75.0), step), kg(77.5));
        assert_eq!(percent_weight(kg(20.0), pct(1.0), step), kg(0.2));
        assert_eq!(
            percent_weight(Weight::MAX, Percent::MAX, step),
            Weight::MAX,
            "saturates"
        );
        assert_eq!(percent_weight(lb(200.0), pct(80.0), lb(5.0)), lb(160.0));
        assert_eq!(percent_weight(lb(210.0), pct(75.0), lb(5.0)), lb(160.0));
    }

    #[test]
    fn warmups_stay_lighter_than_the_working_weight() {
        let step = kg(STEP_KG);
        assert_eq!(lighter_share(kg(100.0), pct(50.0), step), kg(50.0));
        assert_eq!(lighter_share(kg(102.5), pct(60.0), step), kg(62.5));
        // Nearest would reach the working weight: rounded down instead.
        assert_eq!(lighter_share(kg(20.0), pct(95.0), step), kg(17.5));
        // Down would be zero: the exact value.
        assert_eq!(lighter_share(kg(2.5), pct(90.0), step), kg(2.25));
        assert_eq!(lighter_share(kg(2.5), pct(40.0), step), kg(1.0));
        assert_eq!(lighter_share(Weight::ZERO, pct(50.0), step), Weight::ZERO);
        assert_eq!(lighter_share(lb(135.0), pct(50.0), lb(5.0)), lb(70.0));
    }

    #[test]
    fn rounding_falls_back_near_the_cap() {
        let step = ProgressionSettings::for_unit(Unit::Lb).step();
        let rounded = round_or_exact(Weight::MAX, step, Rounding::Up);
        assert!(rounded <= Weight::MAX && rounded > Weight::ZERO);
        assert_eq!(rounded.round_to(step, Rounding::Down).unwrap(), rounded);
    }

    #[test]
    fn failures_count_up_to_the_deload() {
        let deload = Some(Deload {
            failures: 2,
            percent: pct(10.0),
        });
        let mut failures = 0;
        assert_eq!(count_failure(&mut failures, deload), None);
        assert_eq!(failures, 1);
        assert_eq!(count_failure(&mut failures, deload), Some(pct(10.0)));
        assert_eq!(failures, 0);
        assert_eq!(count_failure(&mut failures, None), None);
        assert_eq!(failures, 1);
        // A (non-validated) zero means every failure deloads.
        let zero = Some(Deload {
            failures: 0,
            percent: pct(10.0),
        });
        assert_eq!(count_failure(&mut 0, zero), Some(pct(10.0)));
        let mut many = u16::MAX;
        assert_eq!(count_failure(&mut many, None), None);
        assert_eq!(many, u16::MAX);
    }
}
