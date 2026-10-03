//! Scenarios of the progression engine, through its public API.

// Test helpers outside `#[test]` functions are not covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic)]

use iron_oxide_domain::program::{
    Deload, Exercise, Load, ProgressionRule, RepRange, RepTarget, UnitWeight, WarmupLoad,
    WarmupSet, Work, builtin_programs,
};
use iron_oxide_domain::progression::{
    ChangeKind, ExerciseTargets, NextTargets, PastSession, Prescription, ProgressionSettings,
    SessionVerdict, SetGoal, SetTarget, TargetSource, WorkingSet, exercise_history, next_targets,
};
use iron_oxide_domain::{
    DayId, ExerciseId, LoggedSet, Percent, ProgramVersionId, Reps, Seconds, SessionId, SessionLog,
    SetId, Unit, Weight,
};

fn kg(value: f64) -> Weight {
    Weight::from_kg(value).unwrap()
}

fn lb(value: f64) -> Weight {
    Weight::from_lb(value).unwrap()
}

fn pct(value: f64) -> Percent {
    Percent::new(value).unwrap()
}

fn unit_weight(value: f64, unit: Unit) -> UnitWeight {
    UnitWeight::new(value, unit).unwrap()
}

fn kg_settings() -> ProgressionSettings {
    ProgressionSettings::for_unit(Unit::Kg)
}

fn deload(failures: u16, percent: f64) -> Option<Deload> {
    Some(Deload {
        failures,
        percent: pct(percent),
    })
}

fn exercise(name: &str, work: Work, load: Option<Load>, progression: ProgressionRule) -> Exercise {
    Exercise {
        id: ExerciseId::new(name.to_lowercase().replace(' ', "-")).unwrap(),
        name: name.to_owned(),
        work,
        load,
        rest: Seconds::new(120),
        tempo: None,
        notes: None,
        demo_url: None,
        warmup: Vec::new(),
        superset: None,
        progression,
    }
}

fn fixed(sets: u16, reps: u16) -> Work {
    Work::Reps {
        sets,
        reps: RepTarget::Fixed(Reps::new(reps)),
    }
}

fn range(sets: u16, min: u16, max: u16) -> Work {
    Work::Reps {
        sets,
        reps: RepTarget::Range(RepRange {
            min: Reps::new(min),
            max: Reps::new(max),
        }),
    }
}

/// Squat 3 × 5 at 100 kg, +2.5 kg, deload 10 % after 3 failures.
fn squat() -> Exercise {
    exercise(
        "Squat",
        fixed(3, 5),
        Some(Load::Weight(unit_weight(100.0, Unit::Kg))),
        ProgressionRule::AddWhenTopOfRange {
            increment: unit_weight(2.5, Unit::Kg),
            deload_after_failures: deload(3, 10.0),
        },
    )
}

/// Row 3 × 8–12 at 50 kg, double progression +2.5 kg, deload 10 % after 2 failures.
fn row() -> Exercise {
    exercise(
        "Row",
        range(3, 8, 12),
        Some(Load::Weight(unit_weight(50.0, Unit::Kg))),
        ProgressionRule::DoubleProgression {
            increment: unit_weight(2.5, Unit::Kg),
            deload_after_failures: deload(2, 10.0),
        },
    )
}

/// Bench 3 × 5 at 80 % of the training max, +2.5 kg, deload 10 % after 2 failures.
fn bench() -> Exercise {
    exercise(
        "Bench",
        fixed(3, 5),
        Some(Load::PercentOfTrainingMax(pct(80.0))),
        ProgressionRule::TrainingMax {
            increment: unit_weight(2.5, Unit::Kg),
            deload_after_failures: deload(2, 10.0),
        },
    )
}

/// The working sets of one past session.
type Sets = Vec<WorkingSet>;

fn session(weight: Weight, reps: &[u16]) -> Sets {
    reps.iter()
        .map(|&r| WorkingSet::new(weight, Reps::new(r)))
        .collect()
}

fn ready(outcome: NextTargets) -> ExerciseTargets {
    match outcome {
        NextTargets::Ready(targets) => targets,
        NextTargets::NeedsTrainingMax { exercise } => panic!("{exercise} needs a training max"),
    }
}

/// Past sessions of `exercise`, all prescribed like `exercise`.
fn history(exercise: &Exercise, sessions: &[Sets]) -> Vec<PastSession> {
    sessions
        .iter()
        .map(|sets| PastSession::in_order(Prescription::of(exercise), sets.clone()))
        .collect()
}

/// [`next_targets`] with sessions all prescribed like `exercise`.
fn plan(
    exercise: &Exercise,
    training_max: Option<Weight>,
    settings: ProgressionSettings,
    sessions: &[Sets],
) -> NextTargets {
    next_targets(
        exercise,
        training_max,
        settings,
        &history(exercise, sessions),
    )
}

fn targets(exercise: &Exercise, sessions: &[Sets]) -> ExerciseTargets {
    ready(plan(exercise, None, kg_settings(), sessions))
}

/// The single working weight and reps of uniform targets.
fn working(targets: &ExerciseTargets) -> (Weight, Reps) {
    let first = targets.working[0];
    assert!(
        targets.working.iter().all(|set| *set == first),
        "{targets:?}"
    );
    match first {
        SetTarget {
            weight: Some(weight),
            goal: SetGoal::Reps { reps, .. },
        } => (weight, reps),
        other => panic!("not a weighted reps target: {other:?}"),
    }
}

fn change(targets: &ExerciseTargets) -> ChangeKind {
    targets.change.as_ref().unwrap().kind
}

fn describe(targets: &ExerciseTargets, unit: Unit) -> String {
    targets
        .change
        .as_ref()
        .unwrap()
        .display_in(unit)
        .to_string()
}

mod add_when_top_of_range {
    use super::*;

    #[test]
    fn no_history_is_the_program_default() {
        let next = targets(&squat(), &[]);
        assert_eq!(next.source, TargetSource::ProgramDefault);
        assert_eq!(working(&next), (kg(100.0), Reps::new(5)));
        assert_eq!(next.working.len(), 3);
        assert_eq!(next.change, None);
        assert_eq!(next.last_verdict, None);
        assert_eq!(next.failed_sessions, 0);
        assert_eq!(next.training_max, None);
        // Sessions where the exercise was skipped are no history.
        assert_eq!(targets(&squat(), &[Sets::new()]), next);
    }

    #[test]
    fn success_adds_the_increment() {
        let next = targets(&squat(), &[session(kg(100.0), &[5, 5, 5])]);
        assert_eq!(next.source, TargetSource::Progression);
        assert_eq!(next.last_verdict, Some(SessionVerdict::Success));
        assert_eq!(working(&next), (kg(102.5), Reps::new(5)));
        assert_eq!(
            change(&next),
            ChangeKind::WeightIncrease {
                from: kg(100.0),
                to: kg(102.5)
            }
        );
        assert_eq!(describe(&next, Unit::Kg), "Squat: 100 → 102.5 kg");
    }

    #[test]
    fn success_streak_builds_on_the_weight_lifted() {
        let history = [
            session(kg(100.0), &[5, 5, 5]),
            session(kg(102.5), &[5, 5, 5]),
            session(kg(105.0), &[5, 5, 5]),
        ];
        assert_eq!(working(&targets(&squat(), &history)).0, kg(107.5));
    }

    #[test]
    fn the_base_is_the_weight_actually_lifted() {
        // Heavier than the program: progression continues from there.
        let heavier = targets(&squat(), &[session(kg(120.0), &[5, 5, 5])]);
        assert_eq!(working(&heavier).0, kg(122.5));
        // A lighter last set: the lightest working set is the base.
        let mut dropped = session(kg(100.0), &[5, 5]);
        dropped.push(WorkingSet::new(kg(90.0), Reps::new(5)));
        assert_eq!(working(&targets(&squat(), &[dropped])).0, kg(92.5));
    }

    #[test]
    fn sets_logged_without_a_weight_do_not_drop_the_base() {
        let mut slip = session(kg(100.0), &[5, 5]);
        slip.push(WorkingSet::bodyweight(Reps::new(5)));
        assert_eq!(working(&targets(&squat(), &[slip])).0, kg(102.5));
        // No weight at all: the program's load is the base.
        let none = Sets::from(vec![WorkingSet::bodyweight(Reps::new(5)); 3]);
        assert_eq!(working(&targets(&squat(), &[none])).0, kg(102.5));
    }

    #[test]
    fn a_missed_rep_is_a_failure() {
        let next = targets(&squat(), &[session(kg(100.0), &[5, 5, 4])]);
        assert_eq!(next.last_verdict, Some(SessionVerdict::Failure));
        assert_eq!(working(&next), (kg(100.0), Reps::new(5)));
        assert_eq!(next.failed_sessions, 1);
        assert_eq!(
            change(&next),
            ChangeKind::Unchanged {
                weight: kg(100.0),
                failed_sessions: 1
            }
        );
        assert_eq!(
            describe(&next, Unit::Kg),
            "Squat: stays at 100 kg (1 failed session)"
        );
    }

    #[test]
    fn a_missing_set_is_a_failure() {
        let next = targets(&squat(), &[session(kg(100.0), &[5, 5])]);
        assert_eq!(next.last_verdict, Some(SessionVerdict::Failure));
    }

    #[test]
    fn deload_after_the_failure_streak_then_the_count_restarts() {
        let fail = || session(kg(100.0), &[5, 5, 3]);
        let two = targets(&squat(), &[fail(), fail()]);
        assert_eq!(working(&two).0, kg(100.0));
        assert_eq!(two.failed_sessions, 2);

        let three = targets(&squat(), &[fail(), fail(), fail()]);
        assert_eq!(working(&three).0, kg(90.0));
        assert_eq!(three.failed_sessions, 0);
        assert_eq!(
            change(&three),
            ChangeKind::Deload {
                from: kg(100.0),
                to: kg(90.0)
            }
        );
        assert_eq!(describe(&three, Unit::Kg), "Squat: deload 100 → 90 kg");

        // One more failure at the deloaded weight: no second deload.
        let four = targets(
            &squat(),
            &[fail(), fail(), fail(), session(kg(90.0), &[5, 5, 4])],
        );
        assert_eq!(working(&four).0, kg(90.0));
        assert_eq!(four.failed_sessions, 1);
        assert!(!change(&four).is_deload());
    }

    #[test]
    fn a_success_breaks_the_failure_streak() {
        let history = [
            session(kg(100.0), &[5, 5, 3]),
            session(kg(100.0), &[5, 5, 3]),
            session(kg(100.0), &[5, 5, 5]),
            session(kg(102.5), &[5, 5, 3]),
            session(kg(102.5), &[5, 4, 3]),
        ];
        let next = targets(&squat(), &history);
        assert_eq!(next.failed_sessions, 2);
        assert_eq!(working(&next).0, kg(102.5));
    }

    #[test]
    fn skipped_sessions_do_not_break_or_extend_the_streak() {
        let fail = || session(kg(100.0), &[5, 5, 3]);
        let history = [fail(), Sets::new(), fail(), Sets::new(), fail()];
        let next = targets(&squat(), &history);
        assert!(change(&next).is_deload());
    }

    #[test]
    fn without_deload_failures_accumulate() {
        let mut no_deload = squat();
        no_deload.progression = ProgressionRule::AddWhenTopOfRange {
            increment: unit_weight(2.5, Unit::Kg),
            deload_after_failures: None,
        };
        let history = vec![session(kg(100.0), &[5, 5, 3]); 12];
        let next = targets(&no_deload, &history);
        assert_eq!(working(&next).0, kg(100.0));
        assert_eq!(next.failed_sessions, 12);
    }

    #[test]
    fn with_a_range_the_top_is_the_goal_and_the_middle_holds() {
        let mut curl = squat();
        curl.work = range(3, 8, 12);
        let default = targets(&curl, &[]);
        assert_eq!(
            default.working[0].goal,
            SetGoal::Reps {
                reps: Reps::new(12),
                range: Some(RepRange {
                    min: Reps::new(8),
                    max: Reps::new(12)
                })
            }
        );
        let fail = session(kg(100.0), &[8, 8, 7]);
        let hold = targets(&curl, &[fail.clone(), session(kg(100.0), &[12, 10, 9])]);
        assert_eq!(hold.last_verdict, Some(SessionVerdict::Hold));
        assert_eq!(working(&hold), (kg(100.0), Reps::new(12)));
        assert_eq!(hold.failed_sessions, 0, "a hold ends the failure streak");
        assert_eq!(describe(&hold, Unit::Kg), "Squat: stays at 100 kg");
        let top = targets(&curl, &[session(kg(100.0), &[12, 12, 12])]);
        assert_eq!(working(&top), (kg(102.5), Reps::new(12)));
    }

    #[test]
    fn pounds_round_to_five() {
        let mut press = squat();
        press.load = Some(Load::Weight(unit_weight(95.0, Unit::Lb)));
        press.progression = ProgressionRule::AddWhenTopOfRange {
            increment: unit_weight(5.0, Unit::Lb),
            deload_after_failures: deload(1, 10.0),
        };
        let settings = ProgressionSettings::for_unit(Unit::Lb);
        let up = ready(plan(
            &press,
            None,
            settings,
            &[session(lb(95.0), &[5, 5, 5])],
        ));
        assert_eq!(working(&up).0, lb(100.0));
        assert_eq!(describe(&up, Unit::Lb), "Squat: 95 → 100 lb");
        // 135 lb − 10 % = 121.5 lb → 120 lb.
        let down = ready(plan(
            &press,
            None,
            settings,
            &[session(lb(135.0), &[5, 5, 1])],
        ));
        assert_eq!(working(&down).0, lb(120.0));
        assert_eq!(describe(&down, Unit::Lb), "Squat: deload 135 → 120 lb");
    }

    #[test]
    fn a_custom_step() {
        let mut press = squat();
        press.progression = ProgressionRule::AddWhenTopOfRange {
            increment: unit_weight(1.0, Unit::Kg),
            deload_after_failures: None,
        };
        let history = [session(kg(40.0), &[5, 5, 5])];
        // The program's increment, in the lifter's unit, wins over the step (#120): +1 kg is
        // +1 kg with the default 2.5 kg step, with 1 kg micro-plates, and with a 5 kg step.
        assert_eq!(working(&targets(&press, &history)).0, kg(41.0));
        for step in [1.0, 5.0] {
            let settings = ProgressionSettings::new(Unit::Kg, kg(step)).unwrap();
            let next = ready(plan(&press, None, settings, &history));
            assert_eq!(working(&next).0, kg(41.0), "{step} kg step");
        }
        // Written in the other unit (2.5 lb for a lifter in kg), it is a conversion: rounded to
        // the step, and at least one step up.
        press.progression = ProgressionRule::AddWhenTopOfRange {
            increment: unit_weight(2.5, Unit::Lb),
            deload_after_failures: None,
        };
        assert_eq!(working(&targets(&press, &history)).0, kg(42.5));
        let five = ProgressionSettings::new(Unit::Kg, kg(5.0)).unwrap();
        assert_eq!(
            working(&ready(plan(&press, None, five, &history))).0,
            kg(45.0)
        );
    }
}

mod double_progression {
    use super::*;

    #[test]
    fn starts_at_the_bottom_of_the_range() {
        let next = targets(&row(), &[]);
        assert_eq!(next.source, TargetSource::ProgramDefault);
        assert_eq!(working(&next), (kg(50.0), Reps::new(8)));
    }

    #[test]
    fn reps_climb_then_the_weight_goes_up() {
        let steps: [(&[u16], f64, u16, &str); 4] = [
            (&[8, 8, 8], 50.0, 9, "Row: reps 8 → 9 at 50 kg"),
            (&[11, 10, 9], 50.0, 10, "Row: reps 9 → 10 at 50 kg"),
            (&[12, 12, 11], 50.0, 12, "Row: reps 11 → 12 at 50 kg"),
            (&[12, 12, 12], 52.5, 8, "Row: 50 → 52.5 kg, reps 12 → 8"),
        ];
        let mut history = Vec::new();
        for (reps, weight, aim, description) in steps {
            history.push(session(kg(50.0), reps));
            let next = targets(&row(), &history);
            assert_eq!(
                working(&next),
                (kg(weight), Reps::new(aim)),
                "{description}"
            );
            assert_eq!(describe(&next, Unit::Kg), description);
            assert_eq!(next.failed_sessions, 0);
        }
        let last = targets(&row(), &history);
        assert_eq!(last.last_verdict, Some(SessionVerdict::Success));
        assert_eq!(
            change(&last),
            ChangeKind::WeightIncreaseRepsReset {
                from: kg(50.0),
                to: kg(52.5),
                reps_from: Reps::new(12),
                reps_to: Reps::new(8)
            }
        );
    }

    #[test]
    fn only_the_prescribed_sets_count() {
        // An extra fourth set, however it went, is ignored.
        let next = targets(&row(), &[session(kg(50.0), &[10, 10, 10, 5])]);
        assert_eq!(working(&next), (kg(50.0), Reps::new(11)));
        // It cannot make up for a missed prescribed set either.
        let next = targets(&row(), &[session(kg(50.0), &[10, 5, 10, 10])]);
        assert_eq!(next.last_verdict, Some(SessionVerdict::Failure));
        assert_eq!(working(&next), (kg(50.0), Reps::new(8)));
    }

    #[test]
    fn a_set_below_the_range_is_a_failure_and_resets_the_reps() {
        let history = [
            session(kg(50.0), &[10, 10, 10]),
            session(kg(50.0), &[10, 9, 7]),
        ];
        let next = targets(&row(), &history);
        assert_eq!(next.last_verdict, Some(SessionVerdict::Failure));
        assert_eq!(working(&next), (kg(50.0), Reps::new(8)));
        assert_eq!(next.failed_sessions, 1);
    }

    #[test]
    fn deload_then_a_fresh_streak() {
        let fail = || session(kg(50.0), &[8, 8, 6]);
        let deloaded = targets(&row(), &[fail(), fail()]);
        assert_eq!(working(&deloaded), (kg(45.0), Reps::new(8)));
        assert_eq!(describe(&deloaded, Unit::Kg), "Row: deload 50 → 45 kg");
        let again = targets(&row(), &[fail(), fail(), session(kg(45.0), &[8, 8, 6])]);
        assert_eq!(working(&again).0, kg(45.0));
        assert_eq!(again.failed_sessions, 1);
    }
}

mod training_max {
    use super::*;

    fn with_tm(training_max: Weight, history: &[Sets]) -> ExerciseTargets {
        ready(plan(&bench(), Some(training_max), kg_settings(), history))
    }

    #[test]
    fn needs_a_training_max() {
        let outcome = plan(&bench(), None, kg_settings(), &[]);
        assert_eq!(
            outcome,
            NextTargets::NeedsTrainingMax {
                exercise: bench().id
            }
        );
        assert_eq!(outcome.ready(), None);
        assert_eq!(outcome.exercise(), &bench().id);
        // Whatever the rule.
        let mut no_rule = bench();
        no_rule.progression = ProgressionRule::None;
        assert!(matches!(
            plan(&no_rule, None, kg_settings(), &[session(kg(80.0), &[5])]),
            NextTargets::NeedsTrainingMax { .. }
        ));
    }

    #[test]
    fn no_history_uses_the_percentage() {
        let next = with_tm(kg(100.0), &[]);
        assert_eq!(next.source, TargetSource::ProgramDefault);
        assert_eq!(working(&next), (kg(80.0), Reps::new(5)));
        assert_eq!(next.training_max, Some(kg(100.0)));
        assert_eq!(next.change, None);
        let outcome = plan(&bench(), Some(kg(100.0)), kg_settings(), &[]);
        assert_eq!(outcome.ready(), Some(&next));
        assert_eq!(outcome.exercise(), &bench().id);
    }

    #[test]
    fn success_raises_the_training_max() {
        let next = with_tm(kg(100.0), &[session(kg(80.0), &[5, 5, 5])]);
        assert_eq!(next.source, TargetSource::Progression);
        assert_eq!(next.training_max, Some(kg(102.5)));
        // 80 % of 102.5 kg is 82 kg: 82.5 kg to the nearest step.
        assert_eq!(working(&next).0, kg(82.5));
        assert_eq!(
            describe(&next, Unit::Kg),
            "Bench: training max 100 → 102.5 kg"
        );
    }

    #[test]
    fn replaying_is_idempotent() {
        let history = [session(kg(80.0), &[5, 5, 5]), session(kg(82.5), &[5, 5, 5])];
        let first = with_tm(kg(100.0), &history);
        let second = with_tm(kg(100.0), &history);
        assert_eq!(first, second);
        assert_eq!(first.training_max, Some(kg(105.0)));
        assert_eq!(working(&first).0, kg(85.0));
    }

    #[test]
    fn lighter_than_the_target_does_not_count() {
        let next = with_tm(kg(100.0), &[session(kg(70.0), &[5, 5, 5])]);
        assert_eq!(next.last_verdict, Some(SessionVerdict::Failure));
        assert_eq!(next.training_max, Some(kg(100.0)));
        assert_eq!(
            describe(&next, Unit::Kg),
            "Bench: training max stays at 100 kg (1 failed session)"
        );
        // Heavier is fine.
        let heavier = with_tm(kg(100.0), &[session(kg(85.0), &[5, 5, 5])]);
        assert_eq!(heavier.training_max, Some(kg(102.5)));
    }

    #[test]
    fn half_a_step_of_tolerance_on_the_target() {
        // 80 % of 102.5 kg is exactly 82 kg; the target shown was 82.5 kg.
        let history = [session(kg(80.0), &[5, 5, 5]), session(kg(82.0), &[5, 5, 5])];
        assert_eq!(with_tm(kg(100.0), &history).training_max, Some(kg(105.0)));
        // A lifter in lb loaded 180 lb (81.65 kg): within half a 2.5 kg step of 82 kg.
        let pounds = [
            session(kg(80.0), &[5, 5, 5]),
            session(lb(180.0), &[5, 5, 5]),
        ];
        assert_eq!(with_tm(kg(100.0), &pounds).training_max, Some(kg(105.0)));
        // A full step lighter does not count.
        let lighter = [session(kg(80.0), &[5, 5, 5]), session(kg(80.0), &[5, 5, 5])];
        let next = with_tm(kg(100.0), &lighter);
        assert_eq!(next.training_max, Some(kg(102.5)));
        assert_eq!(next.last_verdict, Some(SessionVerdict::Failure));
    }

    #[test]
    fn deload_cuts_the_training_max_then_the_count_restarts() {
        let fail = |weight| session(kg(weight), &[5, 5, 2]);
        let next = with_tm(kg(100.0), &[fail(80.0), fail(80.0)]);
        assert_eq!(next.training_max, Some(kg(90.0)));
        assert_eq!(working(&next).0, kg(72.5), "80 % of 90 kg is 72 kg");
        assert_eq!(
            describe(&next, Unit::Kg),
            "Bench: deload, training max 100 → 90 kg"
        );
        let again = with_tm(kg(100.0), &[fail(80.0), fail(80.0), fail(72.5)]);
        assert_eq!(again.training_max, Some(kg(90.0)));
        assert_eq!(again.failed_sessions, 1);
    }

    #[test]
    fn with_a_range_the_middle_holds() {
        let mut bench = bench();
        bench.work = range(3, 3, 5);
        let next = ready(plan(
            &bench,
            Some(kg(100.0)),
            kg_settings(),
            &[session(kg(80.0), &[5, 4, 3])],
        ));
        assert_eq!(next.last_verdict, Some(SessionVerdict::Hold));
        assert_eq!(next.training_max, Some(kg(100.0)));
        assert_eq!(working(&next), (kg(80.0), Reps::new(5)));
    }

    #[test]
    fn pounds() {
        let mut bench = bench();
        bench.progression = ProgressionRule::TrainingMax {
            increment: unit_weight(5.0, Unit::Lb),
            deload_after_failures: None,
        };
        bench.load = Some(Load::PercentOfTrainingMax(pct(75.0)));
        let settings = ProgressionSettings::for_unit(Unit::Lb);
        // 75 % of 225 lb is 168.75 lb: 170 lb.
        let first = ready(plan(&bench, Some(lb(225.0)), settings, &[]));
        assert_eq!(working(&first).0, lb(170.0));
        let next = ready(plan(
            &bench,
            Some(lb(225.0)),
            settings,
            &[session(lb(170.0), &[5, 5, 5])],
        ));
        assert_eq!(next.training_max, Some(lb(230.0)));
        // 75 % of 230 lb is 172.5 lb: 175 lb (halfway rounds up).
        assert_eq!(working(&next).0, lb(175.0));
    }
}

mod no_rule {
    use super::*;

    fn pullup() -> Exercise {
        exercise("Pull-up", range(3, 5, 10), None, ProgressionRule::None)
    }

    #[test]
    fn program_default_then_last_performance() {
        let press = exercise(
            "Press",
            fixed(3, 5),
            Some(Load::Weight(unit_weight(40.0, Unit::Kg))),
            ProgressionRule::None,
        );
        let first = targets(&press, &[]);
        assert_eq!(first.source, TargetSource::ProgramDefault);
        assert_eq!(working(&first), (kg(40.0), Reps::new(5)));

        let last = Sets::from(vec![
            WorkingSet::new(kg(42.0), Reps::new(5)),
            WorkingSet::new(kg(41.0), Reps::new(3)),
        ]);
        let next = targets(&press, &[session(kg(30.0), &[5, 5, 5]), last]);
        assert_eq!(next.source, TargetSource::LastPerformance);
        let weights: Vec<_> = next.working.iter().map(|set| set.weight).collect();
        // Set by set, the last one repeated for the missing third set.
        assert_eq!(weights, [Some(kg(42.0)), Some(kg(41.0)), Some(kg(41.0))]);
        // A fixed count stays the program's.
        assert!(next.working.iter().all(|set| set.goal
            == SetGoal::Reps {
                reps: Reps::new(5),
                range: None
            }));
        assert_eq!(next.change, None);
        assert_eq!(next.last_verdict, None);
    }

    #[test]
    fn last_performance_follows_set_indices() {
        let press = exercise(
            "Press",
            fixed(3, 5),
            Some(Load::Weight(unit_weight(40.0, Unit::Kg))),
            ProgressionRule::None,
        );
        let weights = |sets: Vec<WorkingSet>| {
            let history = [PastSession::new(Prescription::of(&press), sets)];
            ready(next_targets(&press, None, kg_settings(), &history))
                .working
                .iter()
                .map(|set| set.weight.unwrap())
                .collect::<Vec<_>>()
        };
        let set = |weight, index| WorkingSet::new(kg(weight), Reps::new(5)).at(index);
        // Back-off sets (index 3) are extras, never working sets.
        assert_eq!(weights(vec![set(100.0, 0), set(60.0, 3)]), [kg(100.0); 3]);
        assert_eq!(
            weights(vec![set(100.0, 0), set(100.0, 1), set(60.0, 3)]),
            [kg(100.0); 3]
        );
        // A skipped first set takes the first one done; later ones repeat the one before.
        assert_eq!(
            weights(vec![set(90.0, 1), set(95.0, 2)]),
            [kg(90.0), kg(90.0), kg(95.0)]
        );
        // Only extras: the program's load.
        assert_eq!(weights(vec![set(60.0, 4)]), [kg(40.0); 3]);
    }

    #[test]
    fn bodyweight_reps_are_clamped_into_the_range() {
        let first = targets(&pullup(), &[]);
        assert_eq!(first.working[0].weight, None);
        assert_eq!(
            first.working[0].goal,
            SetGoal::Reps {
                reps: Reps::new(10),
                range: Some(RepRange {
                    min: Reps::new(5),
                    max: Reps::new(10)
                })
            }
        );
        let last = Sets::from(
            [12, 7, 2]
                .map(|r| WorkingSet::bodyweight(Reps::new(r)))
                .to_vec(),
        );
        let next = targets(&pullup(), &[last]);
        let reps: Vec<_> = next
            .working
            .iter()
            .map(|set| match set.goal {
                SetGoal::Reps { reps, .. } => reps.get(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(reps, [10, 7, 5]);
        assert!(next.working.iter().all(|set| set.weight.is_none()));
    }

    #[test]
    fn a_weight_logged_on_bodyweight_work_is_kept() {
        let last = Sets::from(vec![WorkingSet::new(kg(10.0), Reps::new(6))]);
        let next = targets(&pullup(), &[last]);
        assert!(next.working.iter().all(|set| set.weight == Some(kg(10.0))));
    }

    #[test]
    fn percent_of_training_max_without_a_rule() {
        let mut press = bench();
        press.progression = ProgressionRule::None;
        let next = ready(plan(&press, Some(kg(60.0)), kg_settings(), &[]));
        assert_eq!(next.source, TargetSource::ProgramDefault);
        assert_eq!(working(&next).0, kg(47.5), "80 % of 60 kg is 48 kg");
        assert_eq!(next.training_max, Some(kg(60.0)));
        let later = ready(plan(
            &press,
            Some(kg(60.0)),
            kg_settings(),
            &[session(kg(50.0), &[5, 5, 5])],
        ));
        assert_eq!(later.source, TargetSource::LastPerformance);
        assert_eq!(working(&later).0, kg(50.0));
    }
}

mod timed {
    use super::*;

    #[test]
    fn holds_keep_the_program_duration() {
        let plank = exercise(
            "Plank",
            Work::Hold {
                sets: 3,
                seconds: Seconds::new(45),
            },
            None,
            ProgressionRule::None,
        );
        let hold = SetTarget {
            weight: None,
            goal: SetGoal::Hold {
                seconds: Seconds::new(45),
            },
        };
        let first = targets(&plank, &[]);
        assert_eq!(first.source, TargetSource::ProgramDefault);
        assert_eq!(first.working, vec![hold; 3]);
        // A short hold last time does not shorten the target.
        let short = Sets::from(vec![
            WorkingSet {
                set_index: 0,
                reps: Reps::new(1),
                weight: None,
                duration: Some(Seconds::new(30)),
                target: None,
            };
            3
        ]);
        let next = targets(&plank, &[short]);
        assert_eq!(next.source, TargetSource::LastPerformance);
        assert_eq!(next.working, vec![hold; 3]);
        assert_eq!(next.change, None);
        assert!(next.warmup.is_empty());
    }

    #[test]
    fn weighted_holds_take_the_last_weight() {
        let carry = exercise(
            "Carry",
            Work::Hold {
                sets: 2,
                seconds: Seconds::new(40),
            },
            Some(Load::Weight(unit_weight(24.0, Unit::Kg))),
            ProgressionRule::None,
        );
        assert_eq!(targets(&carry, &[]).working[0].weight, Some(kg(24.0)));
        let last = Sets::from(vec![WorkingSet {
            set_index: 0,
            reps: Reps::new(1),
            weight: Some(kg(32.0)),
            duration: Some(Seconds::new(40)),
            target: None,
        }]);
        let next = targets(&carry, &[last]);
        assert!(next.working.iter().all(|set| set.weight == Some(kg(32.0))));
    }

    #[test]
    fn intervals_are_one_target() {
        let sprints = exercise(
            "Sprints",
            Work::Intervals {
                work: Seconds::new(30),
                rest: Seconds::new(90),
                rounds: 8,
            },
            None,
            ProgressionRule::None,
        );
        let expected = vec![SetTarget {
            weight: None,
            goal: SetGoal::Intervals {
                work: Seconds::new(30),
                rest: Seconds::new(90),
                rounds: 8,
            },
        }];
        assert_eq!(targets(&sprints, &[]).working, expected);
        let done = Sets::from(vec![WorkingSet {
            set_index: 0,
            reps: Reps::new(6),
            weight: None,
            duration: Some(Seconds::new(180)),
            target: None,
        }]);
        assert_eq!(targets(&sprints, &[done]).working, expected);
    }

    #[test]
    fn a_rule_on_timed_work_is_ignored() {
        // Rejected by program validation; the engine still answers.
        let plank = exercise(
            "Plank",
            Work::Hold {
                sets: 1,
                seconds: Seconds::new(60),
            },
            Some(Load::Weight(unit_weight(10.0, Unit::Kg))),
            ProgressionRule::AddWhenTopOfRange {
                increment: unit_weight(2.5, Unit::Kg),
                deload_after_failures: None,
            },
        );
        let done = Sets::from(vec![WorkingSet {
            set_index: 0,
            reps: Reps::new(1),
            weight: Some(kg(10.0)),
            duration: Some(Seconds::new(60)),
            target: None,
        }]);
        let next = targets(&plank, &[done]);
        assert_eq!(next.source, TargetSource::LastPerformance);
        assert_eq!(next.working[0].weight, Some(kg(10.0)));
        assert_eq!(next.change, None);
    }
}

mod warmups {
    use super::*;

    fn with_warmup(mut exercise: Exercise) -> Exercise {
        exercise.warmup = vec![
            WarmupSet {
                sets: 2,
                reps: Reps::new(5),
                load: WarmupLoad::Weight(unit_weight(20.0, Unit::Kg)),
            },
            WarmupSet {
                sets: 1,
                reps: Reps::new(5),
                load: WarmupLoad::PercentOfWorkingWeight(pct(60.0)),
            },
            WarmupSet {
                sets: 1,
                reps: Reps::new(3),
                load: WarmupLoad::PercentOfWorkingWeight(pct(80.0)),
            },
        ];
        exercise
    }

    fn weights(targets: &ExerciseTargets) -> Vec<Option<Weight>> {
        targets.warmup.iter().map(|set| set.weight).collect()
    }

    #[test]
    fn follow_the_progressed_working_weight() {
        let squat = with_warmup(squat());
        let first = targets(&squat, &[]);
        assert_eq!(
            weights(&first),
            [
                Some(kg(20.0)),
                Some(kg(20.0)),
                Some(kg(60.0)),
                Some(kg(80.0))
            ]
        );
        assert_eq!(
            first.warmup[3].goal,
            SetGoal::Reps {
                reps: Reps::new(3),
                range: None
            }
        );
        // 102.5 kg: 61.5 → 62.5 kg and 82 → 82.5 kg.
        let next = targets(&squat, &[session(kg(100.0), &[5, 5, 5])]);
        assert_eq!(
            weights(&next),
            [
                Some(kg(20.0)),
                Some(kg(20.0)),
                Some(kg(62.5)),
                Some(kg(82.5))
            ]
        );
    }

    #[test]
    fn follow_the_training_max() {
        let bench = with_warmup(bench());
        let next = ready(plan(&bench, Some(kg(100.0)), kg_settings(), &[]));
        assert_eq!(
            weights(&next),
            [
                Some(kg(20.0)),
                Some(kg(20.0)),
                Some(kg(47.5)),
                Some(kg(65.0))
            ]
        );
        assert!(matches!(
            plan(&bench, None, kg_settings(), &[]),
            NextTargets::NeedsTrainingMax { .. }
        ));
    }

    #[test]
    fn fixed_warmups_round_to_the_step() {
        let squat = with_warmup(squat());
        let settings = ProgressionSettings::for_unit(Unit::Lb);
        let next = ready(plan(&squat, None, settings, &[]));
        // 100 kg is 220.46 lb: 220 lb, and the 20 kg bar is 45 lb.
        assert_eq!(working(&next).0, lb(220.0));
        assert_eq!(
            weights(&next),
            [
                Some(lb(45.0)),
                Some(lb(45.0)),
                Some(lb(130.0)),
                Some(lb(175.0))
            ]
        );
        // Rounding would make the warm-up as heavy as the working weight: kept as written.
        let mut light = with_warmup(squat.clone());
        light.load = Some(Load::Weight(unit_weight(45.0, Unit::Lb)));
        let next = ready(plan(&light, None, settings, &[]));
        assert_eq!(working(&next).0, lb(45.0));
        assert_eq!(next.warmup[0].weight, Some(kg(20.0)));
    }

    #[test]
    fn percentages_need_a_working_weight() {
        // A percentage warm-up on body-weight work is rejected by validation; no weight here.
        let pullup = with_warmup(exercise(
            "Pull-up",
            fixed(3, 5),
            None,
            ProgressionRule::None,
        ));
        assert_eq!(
            weights(&targets(&pullup, &[])),
            [Some(kg(20.0)), Some(kg(20.0)), None, None]
        );
    }

    #[test]
    fn never_as_heavy_as_a_light_working_weight() {
        let mut light = with_warmup(squat());
        light.load = Some(Load::Weight(unit_weight(22.5, Unit::Kg)));
        light.warmup[2].load = WarmupLoad::PercentOfWorkingWeight(pct(95.0));
        let next = targets(&light, &[]);
        // 95 % of 22.5 kg is 21.375 kg: 20 kg rather than 22.5 kg.
        assert_eq!(next.warmup[3].weight, Some(kg(20.0)));
    }
}

mod review_cases {
    use super::*;

    fn with_prescription(exercise: &Exercise, sets: Sets) -> PastSession {
        PastSession::in_order(Prescription::of(exercise), sets)
    }

    /// Bench on day A: 5 × 5 at 80 %; on day B: 3 × 3 at 90 %. Same rule everywhere.
    fn bench_days() -> (Exercise, Exercise) {
        let mut a = bench();
        a.work = fixed(5, 5);
        a.load = Some(Load::PercentOfTrainingMax(pct(80.0)));
        let mut b = bench();
        b.work = fixed(3, 3);
        b.load = Some(Load::PercentOfTrainingMax(pct(90.0)));
        (a, b)
    }

    #[test]
    fn each_session_is_judged_against_its_own_day() {
        let (day_a, day_b) = bench_days();
        let settings = kg_settings();
        // Alternate A, B, A, B, each done exactly as prescribed at the time.
        let mut history: Vec<PastSession> = Vec::new();
        for day in [&day_a, &day_b, &day_a, &day_b] {
            let target = ready(next_targets(day, Some(kg(100.0)), settings, &history));
            let (weight, reps) = working(&target);
            let sets = vec![WorkingSet::new(weight, reps); target.working.len()];
            history.push(with_prescription(day, sets));
        }
        let plan_a = ready(next_targets(&day_a, Some(kg(100.0)), settings, &history));
        let plan_b = ready(next_targets(&day_b, Some(kg(100.0)), settings, &history));
        for plan in [&plan_a, &plan_b] {
            assert_eq!(plan.last_verdict, Some(SessionVerdict::Success));
            assert_eq!(plan.failed_sessions, 0);
            assert_eq!(plan.training_max, Some(kg(110.0)), "four successes");
        }
        assert_eq!(plan_a.change, plan_b.change, "no conflicting summaries");
        // Each day keeps its own sets, reps and percentage.
        assert_eq!(
            working(&plan_a),
            (kg(87.5), Reps::new(5)),
            "80 % of 110 kg is 88 kg"
        );
        assert_eq!(plan_a.working.len(), 5);
        assert_eq!(
            working(&plan_b),
            (kg(100.0), Reps::new(3)),
            "90 % of 110 kg is 99 kg"
        );
        assert_eq!(plan_b.working.len(), 3);
    }

    #[test]
    fn each_day_is_judged_on_its_own_sets_under_a_weight_rule() {
        let day_a = squat();
        let mut day_b = squat();
        day_b.work = fixed(3, 3);
        let history = [
            with_prescription(&day_b, session(kg(100.0), &[3, 3, 3])),
            with_prescription(&day_b, session(kg(102.5), &[3, 3, 3])),
        ];
        let plan_a = ready(next_targets(&day_a, None, kg_settings(), &history));
        let plan_b = ready(next_targets(&day_b, None, kg_settings(), &history));
        assert_eq!(working(&plan_a), (kg(105.0), Reps::new(5)));
        assert_eq!(working(&plan_b), (kg(105.0), Reps::new(3)));
        assert_eq!(plan_a.change, plan_b.change);
        assert_eq!(plan_a.failed_sessions, 0);
        // 3 sets of 3 on day A (3 × 5) is a failure, whichever day is planned.
        let short = [with_prescription(&day_a, session(kg(100.0), &[3, 3, 3]))];
        for day in [&day_a, &day_b] {
            let plan = ready(next_targets(day, None, kg_settings(), &short));
            assert_eq!(plan.last_verdict, Some(SessionVerdict::Failure));
        }
    }

    #[test]
    fn double_progression_reps_follow_the_planned_range() {
        // Day A: 3 × 8–12; day B: 3 × 5–8.
        let day_a = row();
        let mut day_b = row();
        day_b.work = range(3, 5, 8);
        let history = [with_prescription(&day_a, session(kg(50.0), &[10, 10, 10]))];
        let plan_a = ready(next_targets(&day_a, None, kg_settings(), &history));
        let plan_b = ready(next_targets(&day_b, None, kg_settings(), &history));
        assert_eq!(working(&plan_a), (kg(50.0), Reps::new(11)));
        // 11 is above day B's range: its top.
        assert_eq!(working(&plan_b), (kg(50.0), Reps::new(8)));
        assert_eq!(plan_a.change, plan_b.change);
        assert_eq!(describe(&plan_b, Unit::Kg), "Row: reps 10 → 11 at 50 kg");
        // Below the range: its bottom.
        let low = [with_prescription(&day_b, session(kg(50.0), &[5, 5, 5]))];
        let plan_a = ready(next_targets(&day_a, None, kg_settings(), &low));
        assert_eq!(working(&plan_a), (kg(50.0), Reps::new(8)));
    }

    #[test]
    fn timed_prescriptions_are_left_out_of_a_rule() {
        // An older program version had the exercise as a timed hold.
        let mut old = squat();
        old.work = Work::Hold {
            sets: 3,
            seconds: Seconds::new(30),
        };
        let history = [
            with_prescription(&squat(), session(kg(100.0), &[5, 5, 5])),
            with_prescription(&old, session(kg(100.0), &[1, 1, 1])),
        ];
        let plan = ready(next_targets(&squat(), None, kg_settings(), &history));
        assert_eq!(working(&plan).0, kg(102.5));
        assert_eq!(plan.last_verdict, Some(SessionVerdict::Success));
        let only_old = [with_prescription(&old, session(kg(100.0), &[1, 1, 1]))];
        let plan = ready(next_targets(&squat(), None, kg_settings(), &only_old));
        assert_eq!(plan.source, TargetSource::ProgramDefault);
        assert_eq!(plan.change, None);
        let tm = ready(next_targets(
            &bench(),
            Some(kg(100.0)),
            kg_settings(),
            &only_old,
        ));
        assert_eq!(tm.source, TargetSource::ProgramDefault);
        assert_eq!(tm.training_max, Some(kg(100.0)));
    }

    #[test]
    fn past_verdicts_do_not_depend_on_todays_settings() {
        // 3 × 5 at 65 % of a 121 kg training max: exactly 78.65 kg, shown as 77.5 kg with a
        // 2.5 kg step, and done at 77.5 kg. Deload 10 % after one failure.
        let mut bench = bench();
        bench.load = Some(Load::PercentOfTrainingMax(pct(65.0)));
        bench.progression = ProgressionRule::TrainingMax {
            increment: unit_weight(2.5, Unit::Kg),
            deload_after_failures: deload(1, 10.0),
        };
        let history = [session(kg(77.5), &[5, 5, 5])];
        let all = [
            kg_settings(),
            ProgressionSettings::new(Unit::Kg, kg(1.25)).unwrap(),
            ProgressionSettings::new(Unit::Kg, kg(1.0)).unwrap(),
            ProgressionSettings::for_unit(Unit::Lb),
        ];
        for settings in all {
            let next = ready(plan(&bench, Some(kg(121.0)), settings, &history));
            assert_eq!(
                next.last_verdict,
                Some(SessionVerdict::Success),
                "{settings:?}"
            );
            assert_eq!(next.training_max, Some(kg(123.5)), "{settings:?}");
        }
        // The deloaded training max is exact too: 10 % off 121 kg is 108.9 kg in every setting.
        let failed = [session(kg(77.5), &[5, 5, 2])];
        for settings in all {
            let next = ready(plan(&bench, Some(kg(121.0)), settings, &failed));
            assert_eq!(next.training_max, Some(kg(108.9)), "{settings:?}");
        }
    }

    /// #120: training max weights follow the lifter's step, 1.25 / 2.5 / 5 kg and 2.5 / 5 / 10 lb;
    /// the training max itself never depends on it.
    #[test]
    fn training_max_weights_follow_the_lifters_step() {
        let cases = [
            (kg(121.0), Unit::Kg, kg(1.25), kg(96.25)),
            (kg(121.0), Unit::Kg, kg(2.5), kg(97.5)),
            (kg(121.0), Unit::Kg, kg(5.0), kg(95.0)),
            (lb(318.0), Unit::Lb, lb(2.5), lb(255.0)),
            (lb(318.0), Unit::Lb, lb(5.0), lb(255.0)),
            (lb(318.0), Unit::Lb, lb(10.0), lb(250.0)),
        ];
        for (training_max, unit, step, expected) in cases {
            let settings = ProgressionSettings::new(unit, step).unwrap();
            let first = ready(plan(&bench(), Some(training_max), settings, &[]));
            let (weight, _) = working(&first);
            assert_eq!(weight, expected, "{step:?}");
            // Done as shown (logged with its target): a success, the training max +2.5 kg.
            let done: Sets = first
                .working
                .iter()
                .map(|target| WorkingSet::new(weight, Reps::new(5)).prescribed(*target))
                .collect();
            let next = ready(plan(&bench(), Some(training_max), settings, &[done]));
            assert_eq!(next.last_verdict, Some(SessionVerdict::Success), "{step:?}");
            assert_eq!(
                next.training_max,
                Some(training_max.checked_add(kg(2.5)).unwrap()),
                "{step:?}"
            );
        }
    }

    /// #60: a lifter whose smallest plates are 2.5 kg steps by 5 kg. The target shown, 75 kg
    /// (80 % of 96.75 kg is 77.4 kg, rounded to 5 kg), is 2.4 kg below the exact weight, beyond
    /// the legacy 1.25 kg tolerance; logged with its target, doing it is a success.
    #[test]
    fn a_five_kg_step_done_as_shown_is_a_success() {
        let settings = ProgressionSettings::new(Unit::Kg, kg(5.0)).unwrap();
        let first = ready(plan(&bench(), Some(kg(96.75)), settings, &[]));
        let (weight, _) = working(&first);
        assert_eq!(weight, kg(75.0));
        let done: Sets = first
            .working
            .iter()
            .map(|target| WorkingSet::new(weight, Reps::new(5)).prescribed(*target))
            .collect();
        let next = ready(plan(
            &bench(),
            Some(kg(96.75)),
            settings,
            std::slice::from_ref(&done),
        ));
        assert_eq!(next.last_verdict, Some(SessionVerdict::Success));
        assert_eq!(next.training_max, Some(kg(99.25)));
        // The same sets logged without their target (before #60) fall to the legacy tolerance.
        let legacy: Sets = done
            .iter()
            .map(|set| WorkingSet {
                target: None,
                ..*set
            })
            .collect();
        let next = ready(plan(&bench(), Some(kg(96.75)), settings, &[legacy]));
        assert_eq!(next.last_verdict, Some(SessionVerdict::Failure));
    }

    #[test]
    fn the_prescribed_target_counts_at_the_cap() {
        let settings = ProgressionSettings::for_unit(Unit::Lb);
        for (percent, training_max) in [(100.0, 2_000.0), (150.0, 1_400.0), (109.32, 1_826.0)] {
            let mut bench = bench();
            bench.load = Some(Load::PercentOfTrainingMax(pct(percent)));
            let first = ready(plan(&bench, Some(kg(training_max)), settings, &[]));
            let (weight, _) = working(&first);
            let done = [session(weight, &[5, 5, 5])];
            let next = ready(plan(&bench, Some(kg(training_max)), settings, &done));
            assert_eq!(
                next.last_verdict,
                Some(SessionVerdict::Success),
                "{percent} % of {training_max} kg, done at {weight:?}"
            );
        }
    }

    #[test]
    fn a_top_single_after_the_work_set_is_ignored() {
        let mut deadlift = squat();
        deadlift.work = fixed(1, 5);
        let done = [session(kg(100.0), &[5])
            .into_iter()
            .chain([WorkingSet::new(kg(110.0), Reps::new(1))])
            .collect()];
        let next = targets(&deadlift, &done);
        assert_eq!(next.last_verdict, Some(SessionVerdict::Success));
        assert_eq!(describe(&next, Unit::Kg), "Squat: 100 → 102.5 kg");
        // A failed attempt after it too.
        let done = [session(kg(100.0), &[5])
            .into_iter()
            .chain([WorkingSet::new(kg(110.0), Reps::ZERO)])
            .collect()];
        assert_eq!(working(&targets(&deadlift, &done)).0, kg(102.5));
    }

    #[test]
    fn a_backoff_set_does_not_rescue_a_missing_working_set() {
        // The third working set was skipped; the back-off is logged after the prescribed sets.
        let done = PastSession::new(
            Prescription::of(&squat()),
            vec![
                WorkingSet::new(kg(100.0), Reps::new(5)).at(0),
                WorkingSet::new(kg(100.0), Reps::new(5)).at(1),
                WorkingSet::new(kg(60.0), Reps::new(10)).at(3),
            ],
        );
        let next = ready(next_targets(&squat(), None, kg_settings(), &[done]));
        assert_eq!(next.last_verdict, Some(SessionVerdict::Failure));
        assert_eq!(
            describe(&next, Unit::Kg),
            "Squat: stays at 100 kg (1 failed session)"
        );
    }

    #[test]
    fn a_backoff_set_does_not_drag_the_next_target_down() {
        let mut done = session(kg(100.0), &[5, 5, 5]);
        done.push(WorkingSet::new(kg(60.0), Reps::new(10)));
        let next = targets(&squat(), &[done]);
        assert_eq!(working(&next).0, kg(102.5));
        assert_eq!(describe(&next, Unit::Kg), "Squat: 100 → 102.5 kg");
    }

    #[test]
    fn a_training_max_hold_or_success_ends_the_failure_streak() {
        let mut bench = bench();
        bench.work = range(3, 3, 5);
        let fail = || session(kg(80.0), &[5, 5, 2]);
        // Deload after 2 failures: fail, hold, fail is no deload.
        let hold = session(kg(80.0), &[4, 4, 4]);
        let next = ready(plan(
            &bench,
            Some(kg(100.0)),
            kg_settings(),
            &[fail(), hold, fail()],
        ));
        assert_eq!(next.failed_sessions, 1);
        assert_eq!(next.training_max, Some(kg(100.0)));
        // Fail, success, fail: no deload either.
        let success = session(kg(80.0), &[5, 5, 5]);
        let next = ready(plan(
            &bench,
            Some(kg(100.0)),
            kg_settings(),
            &[fail(), success, session(kg(82.5), &[5, 5, 2])],
        ));
        assert_eq!(next.failed_sessions, 1);
        assert_eq!(next.training_max, Some(kg(102.5)));
    }

    #[test]
    fn double_progression_at_the_cap_stays_at_the_top_of_the_range() {
        let next = targets(&row(), &[session(Weight::MAX, &[12, 12, 12])]);
        assert_eq!(working(&next), (Weight::MAX, Reps::new(12)));
        assert_eq!(
            change(&next),
            ChangeKind::Unchanged {
                weight: Weight::MAX,
                failed_sessions: 0
            }
        );
    }

    #[test]
    fn percentage_warmups_use_the_heaviest_working_set() {
        let mut press = exercise(
            "Press",
            fixed(2, 5),
            Some(Load::Weight(unit_weight(40.0, Unit::Kg))),
            ProgressionRule::None,
        );
        press.warmup = vec![WarmupSet {
            sets: 1,
            reps: Reps::new(5),
            load: WarmupLoad::PercentOfWorkingWeight(pct(50.0)),
        }];
        let last = vec![
            WorkingSet::new(kg(60.0), Reps::new(5)),
            WorkingSet::new(kg(40.0), Reps::new(5)),
        ];
        let next = targets(&press, &[last]);
        assert_eq!(next.warmup[0].weight, Some(kg(30.0)));
    }
}

mod program_versions {
    use super::*;

    fn tm_bench(increment: f64, deload_after: u16) -> Exercise {
        let mut bench = bench();
        bench.load = Some(Load::PercentOfTrainingMax(pct(90.0)));
        bench.progression = ProgressionRule::TrainingMax {
            increment: unit_weight(increment, Unit::Kg),
            deload_after_failures: deload(deload_after, 10.0),
        };
        bench
    }

    fn done(version: &Exercise, weight: Weight, reps: &[u16]) -> PastSession {
        PastSession::in_order(Prescription::of(version), session(weight, reps))
    }

    /// Plans `version` from `history`, does it exactly as shown (every set at the top of the
    /// range), and returns what was shown and what the history then gives.
    fn done_as_shown(
        version: &Exercise,
        history: &mut Vec<PastSession>,
    ) -> (ExerciseTargets, ExerciseTargets) {
        let shown = ready(next_targets(
            version,
            Some(kg(100.0)),
            kg_settings(),
            history,
        ));
        let Work::Reps { reps, .. } = version.work else {
            panic!("not reps work")
        };
        let sets = shown
            .working
            .iter()
            .map(|set| WorkingSet::new(set.weight.unwrap(), reps.max()))
            .collect();
        history.push(PastSession::in_order(Prescription::of(version), sets));
        let after = ready(next_targets(
            version,
            Some(kg(100.0)),
            kg_settings(),
            history,
        ));
        (shown, after)
    }

    #[test]
    fn a_smaller_increment_in_a_new_version_does_not_fail_a_session_done_as_shown() {
        let v1 = tm_bench(10.0, 3);
        let v2 = tm_bench(2.5, 3);
        let mut history = Vec::new();
        let (_, after) = done_as_shown(&v1, &mut history);
        assert_eq!(after.training_max, Some(kg(110.0)));
        // Planned from version 2, the step from session 1 uses +2.5 kg.
        let (shown, after) = done_as_shown(&v2, &mut history);
        assert_eq!(shown.training_max, Some(kg(102.5)));
        assert_eq!(working(&shown).0, kg(92.5));
        assert_eq!(after.last_verdict, Some(SessionVerdict::Success));
        // The step from session 1 is still replayed with version 2's rule, the one that showed
        // 92.5 kg: 100 → 102.5 → 105.
        assert_eq!(after.training_max, Some(kg(105.0)));
        assert_eq!(
            describe(&after, Unit::Kg),
            "Bench: training max 102.5 → 105 kg"
        );

        // v1, v1, then v2.
        let mut history = Vec::new();
        done_as_shown(&v1, &mut history);
        done_as_shown(&v1, &mut history);
        let (shown, after) = done_as_shown(&v2, &mut history);
        assert_eq!(shown.training_max, Some(kg(112.5)));
        assert_eq!(after.last_verdict, Some(SessionVerdict::Success));
        assert_eq!(after.training_max, Some(kg(115.0)));
    }

    #[test]
    fn a_bigger_increment_applies_from_the_step_it_planned() {
        let v1 = tm_bench(2.5, 3);
        let v2 = tm_bench(10.0, 3);
        let mut history = Vec::new();
        done_as_shown(&v1, &mut history);
        done_as_shown(&v1, &mut history);
        // Planned from version 1: +2.5 kg twice.
        let from_v1 = ready(next_targets(&v1, Some(kg(100.0)), kg_settings(), &history));
        assert_eq!(from_v1.training_max, Some(kg(105.0)));
        // Planned from version 2: only the last step uses +10 kg.
        let from_v2 = ready(next_targets(&v2, Some(kg(100.0)), kg_settings(), &history));
        assert_eq!(from_v2.training_max, Some(kg(112.5)));
        assert_eq!(
            describe(&from_v2, Unit::Kg),
            "Bench: training max 102.5 → 112.5 kg"
        );
        let (_, after) = done_as_shown(&v2, &mut history);
        assert_eq!(after.last_verdict, Some(SessionVerdict::Success));
        assert_eq!(after.training_max, Some(kg(122.5)));
    }

    #[test]
    fn a_new_deload_applies_from_the_step_it_planned() {
        let mut v1 = tm_bench(2.5, 3);
        v1.progression = ProgressionRule::TrainingMax {
            increment: unit_weight(2.5, Unit::Kg),
            deload_after_failures: None,
        };
        let v2 = tm_bench(2.5, 1);
        let mut history = vec![done(&v1, kg(90.0), &[5, 5, 3])];
        let shown = ready(next_targets(&v2, Some(kg(100.0)), kg_settings(), &history));
        assert_eq!(
            describe(&shown, Unit::Kg),
            "Bench: deload, training max 100 → 90 kg"
        );
        let (_, after) = done_as_shown(&v2, &mut history);
        assert_eq!(after.last_verdict, Some(SessionVerdict::Success));
        assert_eq!(
            describe(&after, Unit::Kg),
            "Bench: training max 90 → 92.5 kg"
        );
        // Without a version change, the old deload settings hold.
        let history = [
            done(&v1, kg(90.0), &[5, 5, 2]),
            done(&v1, kg(90.0), &[5, 5, 2]),
        ];
        let next = ready(next_targets(&v1, Some(kg(100.0)), kg_settings(), &history));
        assert_eq!(next.failed_sessions, 2);
        assert_eq!(next.training_max, Some(kg(100.0)));
    }

    #[test]
    fn weight_rules_apply_the_rule_that_showed_the_next_target() {
        let mut v1 = squat();
        v1.progression = ProgressionRule::AddWhenTopOfRange {
            increment: unit_weight(2.5, Unit::Kg),
            deload_after_failures: deload(1, 10.0),
        };
        let v2 = squat(); // deload after 3 failures
        // Planned from version 2 after a version 1 failure: no deload, 100 kg shown.
        let failed = [PastSession::in_order(
            Prescription::of(&v1),
            session(kg(100.0), &[5, 5, 2]),
        )];
        let shown = targets_from(&v2, &failed);
        assert_eq!(working(&shown).0, kg(100.0));
        assert_eq!(shown.failed_sessions, 1);
        // Failed again as shown: two failures, still no deload.
        let history = [
            failed[0].clone(),
            PastSession::in_order(Prescription::of(&v2), session(kg(100.0), &[5, 5, 2])),
        ];
        let next = targets_from(&v2, &history);
        assert_eq!(next.failed_sessions, 2);
        assert_eq!(working(&next).0, kg(100.0));
        // Planned from version 1 instead, the last failure deloads at once.
        assert_eq!(working(&targets_from(&v1, &history)).0, kg(90.0));
    }

    #[test]
    fn sessions_under_another_kind_of_rule_do_not_move_the_training_max() {
        let tm = tm_bench(2.5, 1);
        let mut fixed_version = squat();
        fixed_version.load = Some(Load::Weight(unit_weight(90.0, Unit::Kg)));
        // A success under a fixed-load version adds nothing to the training max.
        let history = [PastSession::in_order(
            Prescription::of(&fixed_version),
            session(kg(90.0), &[5, 5, 5]),
        )];
        let next = ready(next_targets(&tm, Some(kg(100.0)), kg_settings(), &history));
        assert_eq!(next.training_max, Some(kg(100.0)));
        // Nor does a failure deload it.
        let history = [PastSession::in_order(
            Prescription::of(&fixed_version),
            session(kg(90.0), &[5, 5, 1]),
        )];
        let next = ready(next_targets(&tm, Some(kg(100.0)), kg_settings(), &history));
        assert_eq!(next.training_max, Some(kg(100.0)));
        // A training max success followed by a fixed-load version: that step moves nothing.
        let history = [
            done(&tm, kg(90.0), &[5, 5, 5]),
            PastSession::in_order(
                Prescription::of(&fixed_version),
                session(kg(90.0), &[5, 5, 5]),
            ),
        ];
        let next = ready(next_targets(&tm, Some(kg(100.0)), kg_settings(), &history));
        assert_eq!(next.training_max, Some(kg(100.0)));
    }

    #[test]
    fn an_unweighted_session_falls_back_to_its_own_fixed_load() {
        let mut light_day = squat();
        light_day.load = Some(Load::Weight(unit_weight(80.0, Unit::Kg)));
        let history = [PastSession::in_order(
            Prescription::of(&light_day),
            vec![WorkingSet::bodyweight(Reps::new(5)); 3],
        )];
        let next = targets_from(&squat(), &history);
        assert_eq!(working(&next).0, kg(82.5));
    }

    #[test]
    fn a_retried_set_counts_its_first_attempt() {
        let mut single = squat();
        single.work = fixed(1, 5);
        let history = [PastSession::new(
            Prescription::of(&single),
            vec![
                WorkingSet::new(kg(110.0), Reps::ZERO).at(0),
                WorkingSet::new(kg(100.0), Reps::new(5)).at(0),
            ],
        )];
        let next = targets_from(&single, &history);
        assert_eq!(next.last_verdict, Some(SessionVerdict::Failure));
        assert_eq!(working(&next).0, kg(110.0));
    }

    fn targets_from(exercise: &Exercise, history: &[PastSession]) -> ExerciseTargets {
        ready(next_targets(exercise, None, kg_settings(), history))
    }

    #[test]
    fn a_session_without_a_prescription_ends_a_failure_streak() {
        let fail =
            || PastSession::in_order(Prescription::of(&row()), session(kg(50.0), &[8, 8, 6]));
        let unknown = PastSession::without_prescription(session(kg(50.0), &[8, 8, 6]));
        // Row deloads after 2 failures: F, unknown, F is one failure, not a deload.
        let next = targets_from(&row(), &[fail(), unknown.clone(), fail()]);
        assert_eq!(next.failed_sessions, 1);
        assert_eq!(working(&next).0, kg(50.0));
        // Last in the history, it resets the count without a verdict of its own, and the
        // summary agrees.
        let next = targets_from(&row(), &[fail(), unknown]);
        assert_eq!(next.failed_sessions, 0);
        assert_eq!(next.last_verdict, Some(SessionVerdict::Failure));
        assert_eq!(describe(&next, Unit::Kg), "Row: stays at 50 kg");
        // The training max rule too.
        let fail = || done(&tm_bench(2.5, 2), kg(90.0), &[5, 5, 2]);
        let unknown = PastSession::without_prescription(session(kg(90.0), &[5, 5, 2]));
        let next = ready(next_targets(
            &tm_bench(2.5, 2),
            Some(kg(100.0)),
            kg_settings(),
            &[fail(), unknown, fail()],
        ));
        assert_eq!(next.failed_sessions, 1);
        assert_eq!(next.training_max, Some(kg(100.0)));
    }

    #[test]
    fn double_progression_describes_the_reset_in_the_sessions_own_range() {
        let day_a = row(); // 8–12
        let mut day_b = row();
        day_b.work = range(3, 5, 8);
        let history = [PastSession::in_order(
            Prescription::of(&day_a),
            session(kg(50.0), &[12, 12, 12]),
        )];
        let next = targets_from(&day_b, &history);
        assert_eq!(working(&next), (kg(52.5), Reps::new(5)));
        assert_eq!(describe(&next, Unit::Kg), "Row: 50 → 52.5 kg, reps 12 → 8");
    }

    #[test]
    fn a_training_max_success_at_the_cap_is_unchanged() {
        let mut bench = bench();
        bench.load = Some(Load::PercentOfTrainingMax(pct(100.0)));
        let settings = kg_settings();
        let first = ready(plan(&bench, Some(Weight::MAX), settings, &[]));
        let (weight, _) = working(&first);
        let next = ready(plan(
            &bench,
            Some(Weight::MAX),
            settings,
            &[session(weight, &[5, 5, 5])],
        ));
        assert_eq!(next.last_verdict, Some(SessionVerdict::Success));
        assert_eq!(
            change(&next),
            ChangeKind::TrainingMaxUnchanged {
                training_max: Weight::MAX,
                failed_sessions: 0
            }
        );
    }
}

mod history_from_logs {
    use uuid::Uuid;

    use super::*;

    fn logged(
        n: u128,
        exercise: &ExerciseId,
        index: u16,
        reps: u16,
        weight: f64,
    ) -> LoggedSet<i64> {
        LoggedSet {
            id: SetId::from_uuid(Uuid::from_u128(n)),
            exercise: exercise.clone(),
            set_index: index,
            reps: Reps::new(reps),
            weight: Some(kg(weight)),
            duration: None,
            warm_up: false,
            completed_at: n as i64,
            target: None,
        }
    }

    #[test]
    fn end_to_end() {
        let squat = squat();
        let mut logs = Vec::new();
        for (n, weight) in [(1_u128, 100.0), (2, 102.5)] {
            let start = (n as i64) * 1_000;
            let mut log = SessionLog::start(
                SessionId::from_uuid(Uuid::from_u128(n)),
                ProgramVersionId::from_uuid(Uuid::from_u128(9)),
                DayId::new("a").unwrap(),
                start,
            );
            for index in 0..3 {
                let id = n * 100 + u128::from(index);
                let mut set = logged(id, &squat.id, index, 5, weight);
                set.completed_at = start + i64::from(index) + 1;
                log.add_set(set).unwrap();
            }
            log.complete(start + 10).unwrap();
            logs.push(log);
        }
        let history = exercise_history(&squat.id, &logs, |_| Some(Prescription::of(&squat)));
        assert_eq!(history.len(), 2);
        let next = ready(next_targets(&squat, None, kg_settings(), &history));
        assert_eq!(working(&next).0, kg(105.0));
        assert_eq!(describe(&next, Unit::Kg), "Squat: 102.5 → 105 kg");
    }
}

#[test]
fn every_builtin_exercise_has_loadable_targets() {
    for (builtin, unit) in builtin_programs()
        .unwrap()
        .into_iter()
        .flat_map(|builtin| Unit::ALL.map(|unit| (builtin.clone(), unit)))
    {
        let settings = ProgressionSettings::for_unit(unit);
        let step = settings.step();
        let program = builtin.program();
        let training_maxes = program.training_max_exercises();
        for exercise in program.exercises() {
            let training_max = training_maxes.contains(&exercise.id).then(|| kg(100.0));
            let next = ready(plan(exercise, training_max, settings, &[]));
            assert_eq!(next.source, TargetSource::ProgramDefault, "{}", exercise.id);
            assert_eq!(
                next.working.len(),
                usize::from(exercise.work.sets()),
                "{}",
                exercise.id
            );
            let on_step = |weight: Weight| {
                weight.round_to(step, iron_oxide_domain::Rounding::Down) == Ok(weight)
            };
            for set in next.working.iter().chain(&next.warmup) {
                if let Some(weight) = set.weight {
                    assert!(on_step(weight), "{} in {unit}: {weight:?}", exercise.id);
                }
            }
            let working = next.working.iter().filter_map(|set| set.weight).max();
            for warmup in &next.warmup {
                if let (Some(warmup), Some(working)) = (warmup.weight, working) {
                    assert!(warmup < working, "{} in {unit}", exercise.id);
                }
            }
            // A session at the target that hits the top of the range progresses every rule.
            if let (Some(working), Work::Reps { sets, reps }) = (working, exercise.work)
                && !exercise.progression.is_none()
            {
                let top = Sets::from(vec![
                    WorkingSet::new(working, reps.max());
                    usize::from(sets)
                ]);
                let after = ready(plan(exercise, training_max, settings, &[top]));
                let kind = after.change.unwrap().kind;
                assert!(kind.is_progress(), "{} in {unit}: {kind:?}", exercise.id);
                assert!(
                    after.working[0].weight.unwrap() >= working,
                    "{} in {unit}",
                    exercise.id
                );
            }
        }
    }
}
