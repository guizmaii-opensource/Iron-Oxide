//! Properties of the progression engine over random exercises and histories.

// Test helpers outside `#[test]` functions are not covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic)]

use iron_oxide_domain::program::{
    Deload, Exercise, Load, ProgressionRule, RepRange, RepTarget, UnitWeight, WarmupLoad,
    WarmupSet, Work,
};
use iron_oxide_domain::progression::{
    ChangeKind, ExerciseTargets, NextTargets, PastSession, Prescription, ProgressionSettings,
    SessionVerdict, SetGoal, TargetSource, WorkingSet, next_targets,
};
use iron_oxide_domain::{ExerciseId, Percent, Reps, Rounding, Seconds, Unit, Weight};
use proptest::prelude::*;

fn unit() -> impl Strategy<Value = Unit> {
    prop_oneof![Just(Unit::Kg), Just(Unit::Lb)]
}

/// Any weight, from zero to the cap, biased towards the cap now and then.
fn weight() -> impl Strategy<Value = Weight> {
    let max = Weight::MAX.as_nanograms();
    prop_oneof![
        8 => (0_u32..=400_000).prop_map(|g| Weight::from_kg(f64::from(g) / 1_000.0).unwrap()),
        1 => (max - 10_000_000_000_000..=max).prop_map(|ng| Weight::from_nanograms(ng).unwrap()),
        1 => (0..=max).prop_map(|ng| Weight::from_nanograms(ng).unwrap()),
    ]
}

fn positive_weight() -> impl Strategy<Value = Weight> {
    weight().prop_filter("positive", |w| !w.is_zero())
}

fn percent_up_to(max_basis_points: u32) -> impl Strategy<Value = Percent> {
    (1..=max_basis_points).prop_map(|bp| Percent::from_basis_points(bp).unwrap())
}

fn deload() -> impl Strategy<Value = Option<Deload>> {
    proptest::option::of(
        (1_u16..=10, percent_up_to(5_000))
            .prop_map(|(failures, percent)| Deload { failures, percent }),
    )
}

fn rep_target() -> impl Strategy<Value = RepTarget> {
    prop_oneof![
        (1_u16..=20).prop_map(|reps| RepTarget::Fixed(Reps::new(reps))),
        (1_u16..=15, 0_u16..=10).prop_map(|(min, spread)| RepTarget::Range(RepRange {
            min: Reps::new(min),
            max: Reps::new(min + spread),
        })),
    ]
}

/// An exercise with a weight rule or the training max rule, possibly with warm-ups.
fn exercise() -> impl Strategy<Value = Exercise> {
    (
        1_u16..=6,
        rep_target(),
        positive_weight(),
        unit(),
        positive_weight(),
        deload(),
        0_u8..3,
        percent_up_to(20_000),
        proptest::collection::vec(percent_up_to(9_999), 0..3),
    )
        .prop_map(
            |(sets, target, load, unit, increment, deload, rule, percent, warmups)| {
                let increment = UnitWeight::from_weight(increment, unit);
                let (load, progression, target) = match rule {
                    0 => (
                        Load::Weight(UnitWeight::from_weight(load, unit)),
                        ProgressionRule::AddWhenTopOfRange {
                            increment,
                            deload_after_failures: deload,
                        },
                        target,
                    ),
                    1 => (
                        Load::Weight(UnitWeight::from_weight(load, unit)),
                        ProgressionRule::DoubleProgression {
                            increment,
                            deload_after_failures: deload,
                        },
                        RepTarget::Range(RepRange {
                            min: target.min(),
                            max: target.max(),
                        }),
                    ),
                    _ => (
                        Load::PercentOfTrainingMax(percent),
                        ProgressionRule::TrainingMax {
                            increment,
                            deload_after_failures: deload,
                        },
                        target,
                    ),
                };
                Exercise {
                    id: ExerciseId::new("lift").unwrap(),
                    name: "Lift".to_owned(),
                    work: Work::Reps { sets, reps: target },
                    load: Some(load),
                    rest: Seconds::new(90),
                    tempo: None,
                    notes: None,
                    demo_url: None,
                    warmup: warmups
                        .into_iter()
                        .map(|percent| WarmupSet {
                            sets: 1,
                            reps: Reps::new(5),
                            load: WarmupLoad::PercentOfWorkingWeight(percent),
                        })
                        .collect(),
                    superset: None,
                    progression,
                }
            },
        )
}

/// Another version's rule: any kind, increment and deload.
fn other_rule() -> impl Strategy<Value = ProgressionRule> {
    (0_u8..4, positive_weight(), unit(), deload()).prop_map(|(kind, increment, unit, deload)| {
        let increment = UnitWeight::from_weight(increment, unit);
        match kind {
            0 => ProgressionRule::None,
            1 => ProgressionRule::AddWhenTopOfRange {
                increment,
                deload_after_failures: deload,
            },
            2 => ProgressionRule::DoubleProgression {
                increment,
                deload_after_failures: deload,
            },
            _ => ProgressionRule::TrainingMax {
                increment,
                deload_after_failures: deload,
            },
        }
    })
}

/// Another day's (or version's) prescription: other sets, reps, load and rule, or timed work.
fn other_prescription() -> impl Strategy<Value = Prescription> {
    prop_oneof![
        8 => (1_u16..=6, rep_target(), prop_oneof![
            percent_up_to(20_000).prop_map(Load::PercentOfTrainingMax),
            (positive_weight(), unit()).prop_map(|(w, u)| Load::Weight(UnitWeight::from_weight(w, u))),
        ], other_rule())
            .prop_map(|(sets, reps, load, rule)| Prescription {
                work: Work::Reps { sets, reps },
                load: Some(load),
                rule,
            }),
        1 => Just(Prescription {
            work: Work::Hold { sets: 3, seconds: Seconds::new(30) },
            load: None,
            rule: ProgressionRule::None,
        }),
    ]
}

/// Which prescription a past session has.
#[derive(Debug, Clone, Copy)]
enum Given {
    /// The planned exercise's.
    Planned,
    /// Another day's or version's.
    Other(Prescription),
    /// None can be found.
    Missing,
}

/// Past sessions, each with its prescription and working sets.
type RawHistory = Vec<(Given, Vec<WorkingSet>)>;

fn history() -> impl Strategy<Value = RawHistory> {
    let set = (proptest::option::of(weight()), 0_u16..=25).prop_map(|(weight, reps)| WorkingSet {
        set_index: 0,
        reps: Reps::new(reps),
        weight,
        duration: None,
        target: None,
    });
    proptest::collection::vec(
        (
            prop_oneof![
                12 => Just(Given::Planned),
                5 => other_prescription().prop_map(Given::Other),
                1 => Just(Given::Missing),
            ],
            proptest::collection::vec(set, 0..8),
        ),
        0..12,
    )
}

fn resolve(exercise: &Exercise, raw: &RawHistory) -> Vec<PastSession> {
    raw.iter()
        .map(|(given, sets)| match given {
            Given::Planned => PastSession::in_order(Prescription::of(exercise), sets.clone()),
            Given::Other(prescription) => PastSession::in_order(*prescription, sets.clone()),
            Given::Missing => PastSession::without_prescription(sets.clone()),
        })
        .collect()
}

/// Whether a session is judged by a rule: it has sets and a known prescription asking for reps.
fn counts(session: &PastSession) -> bool {
    !session.is_empty()
        && session
            .prescription
            .is_some_and(|prescription| matches!(prescription.work, Work::Reps { .. }))
}

/// The largest step before #60, in nanograms: 2.5 kg. Sets logged without a target are judged
/// with a tolerance that only covers targets rounded to steps up to it.
const LEGACY_MAX_STEP: u64 = 2_500_000_000_000;

/// A step up to the legacy bound, for sets logged without a target.
fn step() -> impl Strategy<Value = Weight> {
    (1..=LEGACY_MAX_STEP).prop_map(|ng| Weight::from_nanograms(ng).unwrap())
}

/// Any step a lifter could have, now that there is no bound (#60): up to 25 kg.
fn any_step() -> impl Strategy<Value = Weight> {
    (1..=10 * LEGACY_MAX_STEP).prop_map(|ng| Weight::from_nanograms(ng).unwrap())
}

/// Settings with steps up to the legacy bound.
fn settings() -> impl Strategy<Value = ProgressionSettings> {
    prop_oneof![
        unit().prop_map(ProgressionSettings::for_unit),
        (unit(), step()).prop_map(|(unit, step)| ProgressionSettings::new(unit, step).unwrap()),
        unit().prop_map(|unit| {
            ProgressionSettings::new(unit, Weight::from_nanograms(LEGACY_MAX_STEP).unwrap())
                .unwrap()
        }),
    ]
}

/// Settings with any step, 5 kg (2.5 kg plates) included.
fn any_settings() -> impl Strategy<Value = ProgressionSettings> {
    prop_oneof![
        settings(),
        (unit(), any_step()).prop_map(|(unit, step)| ProgressionSettings::new(unit, step).unwrap()),
        unit().prop_map(|unit| {
            ProgressionSettings::new(unit, Weight::from_kg(5.0).unwrap()).unwrap()
        }),
    ]
}

fn ready(outcome: NextTargets) -> ExerciseTargets {
    match outcome {
        NextTargets::Ready(targets) => targets,
        NextTargets::NeedsTrainingMax { .. } => panic!("a training max was given"),
    }
}

fn on_step(weight: Weight, step: Weight) -> bool {
    weight.round_to(step, Rounding::Down) == Ok(weight)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn targets_are_consistent(
        exercise in exercise(),
        training_max in weight(),
        settings in settings(),
        raw in history(),
    ) {
        let history = resolve(&exercise, &raw);
        let outcome = next_targets(&exercise, Some(training_max), settings, &history);
        // Deterministic.
        prop_assert_eq!(&outcome, &next_targets(&exercise, Some(training_max), settings, &history));
        let NextTargets::Ready(targets) = outcome else {
            return Err(TestCaseError::fail("a training max was given"));
        };
        let has_history = history.iter().any(counts);
        let is_training_max = exercise.progression.name() == "training_max";
        prop_assert_eq!(targets.change.is_some(), has_history);
        prop_assert_eq!(targets.last_verdict.is_some(), has_history);
        prop_assert_eq!(
            targets.source,
            if has_history { TargetSource::Progression } else { TargetSource::ProgramDefault }
        );
        prop_assert_eq!(targets.training_max.is_some(), is_training_max);

        // One weight and one rep target for every prescribed set, within the range.
        let Work::Reps { sets, reps: target } = exercise.work else { unreachable!() };
        prop_assert_eq!(targets.working.len(), usize::from(sets));
        let first = targets.working[0];
        prop_assert!(targets.working.iter().all(|set| *set == first));
        let Some(working) = first.weight else {
            return Err(TestCaseError::fail("weight rules always have a weight"));
        };
        prop_assert!(working <= Weight::MAX);
        let SetGoal::Reps { reps, .. } = first.goal else { unreachable!() };
        prop_assert!(reps >= target.min() && reps <= target.max());

        // Warm-ups never reach the working weight.
        prop_assert_eq!(targets.warmup.len(), exercise.warmup.len());
        for warmup in &targets.warmup {
            let weight = warmup.weight.unwrap();
            prop_assert!(weight <= working);
            prop_assert!(weight < working || working.as_nanograms() < 10_000);
        }

        // The failure count stays below the deload threshold, when the last judged session ran
        // under a rule of the same kind (under another kind, it moves nothing, not even a deload).
        let last_rule = history
            .iter()
            .rev()
            .find(|session| counts(session))
            .and_then(|session| session.prescription)
            .map(|prescription| prescription.rule.name());
        if let Some(deload) = exercise.progression.deload()
            && last_rule.is_none_or(|name| name == exercise.progression.name())
        {
            prop_assert!(targets.failed_sessions < deload.failures);
        }

        let step = settings.step();
        match targets.change.as_ref().map(|change| change.kind) {
            Some(ChangeKind::WeightIncrease { from, to }
                | ChangeKind::WeightIncreaseRepsReset { from, to, .. }) => {
                prop_assert!(to > from);
                prop_assert_eq!(to, working);
                // On the step, unless the cap left no step above.
                prop_assert!(on_step(to, step) || to.checked_add(step).is_err());
            }
            Some(ChangeKind::Deload { from, to }) => {
                prop_assert!(to <= from);
                prop_assert_eq!(to, working);
                prop_assert!(on_step(to, step) || to < step);
            }
            Some(ChangeKind::TrainingMaxIncrease { from, to }) => {
                prop_assert!(to > from);
                prop_assert_eq!(Some(to), targets.training_max);
            }
            Some(ChangeKind::TrainingMaxDeload { from, to }) => {
                prop_assert!(to <= from);
                prop_assert_eq!(Some(to), targets.training_max);
            }
            Some(ChangeKind::Unchanged { weight, .. }) => prop_assert_eq!(weight, working),
            Some(ChangeKind::RepsIncrease { weight, from, to }) => {
                prop_assert_eq!(weight, working);
                prop_assert!(to > from);
            }
            Some(ChangeKind::TrainingMaxUnchanged { training_max, .. }) => {
                prop_assert_eq!(Some(training_max), targets.training_max);
            }
            None => {}
        }
        // A weight computed from the training max is on the step, or below one step.
        if is_training_max {
            prop_assert!(on_step(working, step) || working < step || working == Weight::MAX);
        }
    }

    /// Appending a failed session never increases the working weight or the training max. (When
    /// the last judged session ran under another rule, appending a session makes that session
    /// apply its own rule instead of the planned one, which may add more: left out.)
    #[test]
    fn a_failure_never_increases(
        exercise in exercise(),
        training_max in weight(),
        settings in settings(),
        raw in history(),
        weight in weight(),
    ) {
        let history = resolve(&exercise, &raw);
        let last_rule = history
            .iter()
            .rev()
            .find(|session| counts(session))
            .and_then(|session| session.prescription)
            .map(|prescription| prescription.rule);
        prop_assume!(last_rule.is_none_or(|rule| rule == exercise.progression));
        let before = next_targets(&exercise, Some(training_max), settings, &history);
        let NextTargets::Ready(before) = before else { unreachable!() };
        let target = before.working[0].weight.unwrap();
        // Every set lifted at the target (or the given weight for weight rules) with 0 reps.
        let lifted = if exercise.progression.name() == "training_max" { target } else { weight };
        let mut longer = history.clone();
        longer.push(PastSession::in_order(
            Prescription::of(&exercise),
            vec![WorkingSet::new(lifted, Reps::ZERO); 3],
        ));
        let NextTargets::Ready(after) = next_targets(&exercise, Some(training_max), settings, &longer)
        else { unreachable!() };
        prop_assert!(after.working[0].weight.unwrap() <= lifted.max(target));
        if let (Some(tm_before), Some(tm_after)) = (before.training_max, after.training_max) {
            prop_assert!(tm_after <= tm_before);
        }
    }

    /// Past verdicts, the failure count and the training max depend only on what was prescribed
    /// and lifted, never on today's step or unit.
    #[test]
    fn replay_does_not_depend_on_the_settings(
        exercise in exercise(),
        training_max in weight(),
        first in any_settings(),
        second in any_settings(),
        raw in history(),
    ) {
        let history = resolve(&exercise, &raw);
        let a = ready(next_targets(&exercise, Some(training_max), first, &history));
        let b = ready(next_targets(&exercise, Some(training_max), second, &history));
        prop_assert_eq!(a.last_verdict, b.last_verdict);
        prop_assert_eq!(a.failed_sessions, b.failed_sessions);
        prop_assert_eq!(a.training_max, b.training_max);
        if exercise.progression.name() == "training_max" {
            prop_assert_eq!(a.change, b.change);
        }
    }

    /// Doing exactly what was prescribed (the target weight, every set at the top of the range)
    /// is a success, session after session, for every rule and any settings, up to the cap.
    #[test]
    fn doing_the_prescription_is_a_success(
        exercise in exercise(),
        training_max in weight(),
        settings in settings(),
        sessions in 1_usize..=5,
    ) {
        let mut history = Vec::new();
        for _ in 0..sessions {
            let target = ready(next_targets(&exercise, Some(training_max), settings, &history));
            let Work::Reps { reps, .. } = exercise.work else { unreachable!() };
            let sets = target
                .working
                .iter()
                .map(|set| WorkingSet::new(set.weight.unwrap(), reps.max()))
                .collect();
            history.push(PastSession::in_order(Prescription::of(&exercise), sets));
            let after = ready(next_targets(&exercise, Some(training_max), settings, &history));
            prop_assert_eq!(after.last_verdict, Some(SessionVerdict::Success));
            prop_assert_eq!(after.failed_sessions, 0);
        }
    }

    /// Doing exactly what was shown, with each set logged with its target (#60), is a success
    /// session after session, for every rule and **any** step, 5 kg and up included: the verdict
    /// compares with the target stored with each set, not with a tolerance.
    #[test]
    fn doing_the_prescription_logged_with_its_targets_is_a_success_with_any_step(
        exercise in exercise(),
        training_max in weight(),
        settings in any_settings(),
        later in any_settings(),
        sessions in 1_usize..=5,
    ) {
        let mut history = Vec::new();
        for _ in 0..sessions {
            let target = ready(next_targets(&exercise, Some(training_max), settings, &history));
            let Work::Reps { reps, .. } = exercise.work else { unreachable!() };
            let sets = target
                .working
                .iter()
                .map(|set| WorkingSet::new(set.weight.unwrap(), reps.max()).prescribed(*set))
                .collect();
            history.push(PastSession::in_order(Prescription::of(&exercise), sets));
            let after = ready(next_targets(&exercise, Some(training_max), settings, &history));
            prop_assert_eq!(after.last_verdict, Some(SessionVerdict::Success));
            prop_assert_eq!(after.failed_sessions, 0);
            // And it stays so whatever the settings become.
            let changed = ready(next_targets(&exercise, Some(training_max), later, &history));
            prop_assert_eq!(changed.last_verdict, after.last_verdict);
            prop_assert_eq!(changed.training_max, after.training_max);
        }
    }

    /// Extra sets logged after the prescribed ones (a top single, a failed attempt, back-off
    /// sets) change nothing, with or without a rule.
    #[test]
    fn extra_sets_change_nothing(
        exercise in prop_oneof![exercise(), exercise().prop_map(without_rule)],
        training_max in weight(),
        settings in settings(),
        raw in history(),
        extras in proptest::collection::vec((weight(), 0_u16..=25), 1..4),
    ) {
        let history = resolve(&exercise, &raw);
        let before = ready(next_targets(&exercise, Some(training_max), settings, &history));
        let planned = exercise.work.sets();
        let with_extras: Vec<PastSession> = history
            .iter()
            .map(|session| {
                let prescribed = match session.prescription.map(|p| p.work) {
                    Some(Work::Reps { sets, .. } | Work::Hold { sets, .. }) => sets,
                    Some(Work::Intervals { .. }) | None => 1,
                };
                let mut extended = session.clone();
                if !session.is_empty() {
                    let first_extra = prescribed
                        .max(planned)
                        .max(u16::try_from(session.sets.len()).unwrap());
                    extended.sets.extend(extras.iter().zip(first_extra..).map(|((w, r), i)| {
                        WorkingSet::new(*w, Reps::new(*r)).at(i)
                    }));
                }
                extended
            })
            .collect();
        let after = ready(next_targets(&exercise, Some(training_max), settings, &with_extras));
        prop_assert_eq!(before, after);
    }
}

/// The same exercise without a progression rule.
fn without_rule(mut exercise: Exercise) -> Exercise {
    exercise.progression = ProgressionRule::None;
    exercise
}

/// What the lifter does with the targets shown.
#[derive(Debug, Clone, Copy)]
enum Act {
    /// Every set at the target weight, at the top of the range.
    Top,
    /// Every set at the target weight and the rep goal shown.
    Goal,
    /// One set short.
    Short,
    /// The last set one rep short.
    Fewer,
    /// As `Top`, plus a light back-off set.
    BackOff,
    /// As `Top`, plus a failed heavy attempt.
    Attempt,
    /// The exercise skipped.
    Skip,
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        4 => Just(Act::Top),
        2 => Just(Act::Goal),
        1 => Just(Act::Short),
        1 => Just(Act::Fewer),
        1 => Just(Act::BackOff),
        1 => Just(Act::Attempt),
        1 => Just(Act::Skip),
    ]
}

/// A version's rule of `kind` (0: add when top of range, 1: double progression, 2: training max).
fn rule_of_kind(kind: u8) -> impl Strategy<Value = ProgressionRule> {
    (
        1_u32..=10_000,
        proptest::option::of((1_u16..=4, 1_u32..=5_000)),
    )
        .prop_map(move |(grams, deload)| {
            let increment = UnitWeight::new(f64::from(grams) / 1_000.0, Unit::Kg).unwrap();
            let deload_after_failures = deload.map(|(failures, bp)| Deload {
                failures,
                percent: Percent::from_basis_points(bp).unwrap(),
            });
            match kind {
                0 => ProgressionRule::AddWhenTopOfRange {
                    increment,
                    deload_after_failures,
                },
                1 => ProgressionRule::DoubleProgression {
                    increment,
                    deload_after_failures,
                },
                _ => ProgressionRule::TrainingMax {
                    increment,
                    deload_after_failures,
                },
            }
        })
}

/// 1 to 3 versions of one exercise, all with a rule of `kind`, each with 1 or 2 days.
fn versions(kind: u8) -> impl Strategy<Value = Vec<Vec<Exercise>>> {
    let day = (1_u16..=5, rep_target(), 40_u32..=110, 20_u32..=150);
    let version = (rule_of_kind(kind), proptest::collection::vec(day, 1..=2)).prop_map(
        move |(rule, days)| {
            days.into_iter()
                .map(|(sets, target, percent, load)| {
                    let reps = if kind == 1 {
                        RepTarget::Range(RepRange {
                            min: target.min(),
                            max: Reps::new(target.max().get().max(target.min().get() + 1)),
                        })
                    } else {
                        target
                    };
                    let load = if kind == 2 {
                        Load::PercentOfTrainingMax(Percent::new(f64::from(percent)).unwrap())
                    } else {
                        Load::Weight(UnitWeight::new(f64::from(load), Unit::Kg).unwrap())
                    };
                    Exercise {
                        id: ExerciseId::new("lift").unwrap(),
                        name: "Lift".to_owned(),
                        work: Work::Reps { sets, reps },
                        load: Some(load),
                        rest: Seconds::new(90),
                        tempo: None,
                        notes: None,
                        demo_url: None,
                        warmup: Vec::new(),
                        superset: None,
                        progression: rule,
                    }
                })
                .collect::<Vec<_>>()
        },
    );
    proptest::collection::vec(version, 1..=3)
}

fn chain_settings() -> Vec<ProgressionSettings> {
    let kg = |value| Weight::from_kg(value).unwrap();
    let lb = |value| Weight::from_lb(value).unwrap();
    vec![
        ProgressionSettings::for_unit(Unit::Kg),
        ProgressionSettings::for_unit(Unit::Lb),
        ProgressionSettings::new(Unit::Kg, kg(1.25)).unwrap(),
        ProgressionSettings::new(Unit::Kg, kg(1.0)).unwrap(),
        ProgressionSettings::new(Unit::Kg, kg(0.25)).unwrap(),
        ProgressionSettings::new(Unit::Lb, lb(2.5)).unwrap(),
        ProgressionSettings::new(Unit::Lb, lb(5.5)).unwrap(),
        ProgressionSettings::new(Unit::Kg, kg(2.5)).unwrap(),
        ProgressionSettings::new(Unit::Kg, Weight::from_nanograms(1).unwrap()).unwrap(),
        // Above the legacy bound (#60): every set is logged with its target below.
        ProgressionSettings::new(Unit::Kg, kg(5.0)).unwrap(),
        ProgressionSettings::new(Unit::Lb, lb(10.0)).unwrap(),
        ProgressionSettings::new(Unit::Kg, kg(20.0)).unwrap(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3_000))]

    /// Sessions planned from random versions and days, under random settings, and done from the
    /// targets shown: done as shown at the top of the range is a success, at the rep goal never a
    /// failure, one set short a failure; and every version and day agrees, under every setting,
    /// on the verdict, the failure count and the training max.
    #[test]
    fn chains_across_versions_done_as_shown(
        versions in (0_u8..3).prop_flat_map(versions),
        training_max in weight(),
        steps in proptest::collection::vec((0_usize..3, 0_usize..2, 0_usize..12, act()), 1..=8),
    ) {
        let all = chain_settings();
        let tm = Some(training_max);
        let mut history: Vec<PastSession> = Vec::new();
        for (v, d, s, act) in steps {
            let days = &versions[v % versions.len()];
            let planned = &days[d % days.len()];
            let shown = ready(next_targets(planned, tm, all[s], &history));
            let Work::Reps { reps: target, .. } = planned.work else { unreachable!() };
            let mut sets: Vec<WorkingSet> = shown
                .working
                .iter()
                .map(|set| {
                    let reps = match (act, set.goal) {
                        (Act::Goal, SetGoal::Reps { reps, .. }) => reps,
                        _ => target.max(),
                    };
                    WorkingSet::new(set.weight.unwrap(), reps).prescribed(*set)
                })
                .collect();
            match act {
                Act::Short => { sets.pop(); }
                Act::Fewer => {
                    let last = sets.last_mut().unwrap();
                    last.reps = Reps::new(last.reps.get().saturating_sub(1));
                }
                Act::BackOff => sets.push(WorkingSet::new(Weight::from_kg(20.0).unwrap(), Reps::new(10))),
                Act::Attempt => sets.push(WorkingSet::new(Weight::MAX, Reps::ZERO)),
                Act::Skip => sets.clear(),
                Act::Top | Act::Goal => {}
            }
            let skipped = sets.is_empty();
            history.push(PastSession::in_order(Prescription::of(planned), sets));
            if !skipped {
                let after = ready(next_targets(planned, tm, all[0], &history));
                match act {
                    Act::Top | Act::BackOff | Act::Attempt => {
                        prop_assert_eq!(after.last_verdict, Some(SessionVerdict::Success));
                    }
                    Act::Goal => prop_assert_ne!(after.last_verdict, Some(SessionVerdict::Failure)),
                    Act::Short => prop_assert_eq!(after.last_verdict, Some(SessionVerdict::Failure)),
                    Act::Fewer | Act::Skip => {}
                }
            }
            for day in versions.iter().flatten() {
                let first = ready(next_targets(day, tm, all[0], &history));
                for settings in &all[1..] {
                    let other = ready(next_targets(day, tm, *settings, &history));
                    prop_assert_eq!(other.last_verdict, first.last_verdict);
                    prop_assert_eq!(other.failed_sessions, first.failed_sessions);
                    prop_assert_eq!(other.training_max, first.training_max);
                }
            }
        }
    }
}
