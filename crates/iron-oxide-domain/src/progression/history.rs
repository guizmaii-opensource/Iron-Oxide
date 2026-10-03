//! The engine's input: what was prescribed and done for one exercise in past sessions.

use serde::{Deserialize, Serialize};

use super::SetTarget;
use crate::program::{Exercise, Load, Program, ProgressionRule, Work};
use crate::session::{LoggedSet, Session, SessionLog, SessionStatus};
use crate::{DayId, ExerciseId, Reps, Seconds, Weight};

/// One working set as it was performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkingSet {
    /// Position among the session's working sets of the exercise, from 0, as logged
    /// ([`LoggedSet::set_index`]). The sets with an index below the prescribed number of sets are
    /// the prescribed working sets; the others (a top single, a back-off set) are extras.
    pub set_index: u16,
    /// Repetitions done. Zero is a failed attempt.
    pub reps: Reps,
    /// Load, or `None` for body-weight work.
    pub weight: Option<Weight>,
    /// Time of a timed set, or `None`.
    pub duration: Option<Seconds>,
    /// What the app prescribed for this set when it was logged ([`LoggedSet::target`]), or `None`
    /// for a set logged without one (before #60). A training max session's set is judged against
    /// it exactly when it is there.
    pub target: Option<SetTarget>,
}

impl WorkingSet {
    /// A set of `reps` at `weight`, at index 0 (see [`WorkingSet::at`] and
    /// [`PastSession::in_order`]).
    #[must_use]
    pub const fn new(weight: Weight, reps: Reps) -> Self {
        Self {
            set_index: 0,
            reps,
            weight: Some(weight),
            duration: None,
            target: None,
        }
    }

    /// A body-weight set of `reps`, at index 0.
    #[must_use]
    pub const fn bodyweight(reps: Reps) -> Self {
        Self {
            set_index: 0,
            reps,
            weight: None,
            duration: None,
            target: None,
        }
    }

    /// The same set at `set_index`.
    #[must_use]
    pub const fn at(self, set_index: u16) -> Self {
        Self { set_index, ..self }
    }

    /// The same set, logged with `target` as what was prescribed.
    #[must_use]
    pub const fn prescribed(self, target: SetTarget) -> Self {
        Self {
            target: Some(target),
            ..self
        }
    }
}

impl<T> From<&LoggedSet<T>> for WorkingSet {
    fn from(set: &LoggedSet<T>) -> Self {
        Self {
            set_index: set.set_index,
            reps: set.reps,
            weight: set.weight,
            duration: set.duration,
            target: set.target,
        }
    }
}

/// What the program asked for in a past session: the exercise's work, load and progression rule
/// **on the day and program version that session was run from**.
///
/// The same exercise can be prescribed differently on different days (5 × 5 at 80 % on day A,
/// 3 × 3 at 90 % on day B) and in different versions of a program, so each past session is judged
/// against its own prescription, never against the day being planned. Its rule applies the step
/// from the previous judged session to this one, the step whose targets this version showed (see
/// the [module documentation](super)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Prescription {
    /// Sets and reps (or time) asked for.
    pub work: Work,
    /// The load asked for, `None` for body-weight work.
    pub load: Option<Load>,
    /// The progression rule in force.
    pub rule: ProgressionRule,
}

impl Prescription {
    /// The prescription of one exercise line.
    #[must_use]
    pub const fn of(exercise: &Exercise) -> Self {
        Self {
            work: exercise.work,
            load: exercise.load,
            rule: exercise.progression,
        }
    }

    /// The prescription of `exercise` on `day` of `program` (its first line on that day), or
    /// `None` when the day or the exercise is not there.
    #[must_use]
    pub fn in_program(program: &Program, day: &DayId, exercise: &ExerciseId) -> Option<Self> {
        program
            .day(day)?
            .exercises
            .iter()
            .find(|line| &line.id == exercise)
            .map(Self::of)
    }
}

impl From<&Exercise> for Prescription {
    fn from(exercise: &Exercise) -> Self {
        Self::of(exercise)
    }
}

/// One exercise in one past session: what was prescribed, and the working sets done, in set
/// order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PastSession {
    /// What the program asked for in that session, or `None` when it cannot be found any more.
    /// Such a session cannot be judged: it ends a failure streak without progressing.
    pub prescription: Option<Prescription>,
    /// The working sets (never warm-ups), in set order, with their logged `set_index`.
    pub sets: Vec<WorkingSet>,
}

impl PastSession {
    /// A session with this prescription and these working sets, indices as given.
    #[must_use]
    pub const fn new(prescription: Prescription, sets: Vec<WorkingSet>) -> Self {
        Self {
            prescription: Some(prescription),
            sets,
        }
    }

    /// A session with this prescription and these working sets, numbered 0, 1, 2… in order.
    #[must_use]
    pub fn in_order(prescription: Prescription, sets: Vec<WorkingSet>) -> Self {
        let sets = (0..=u16::MAX).zip(sets).map(|(i, set)| set.at(i)).collect();
        Self::new(prescription, sets)
    }

    /// A session whose prescription cannot be found.
    #[must_use]
    pub const fn without_prescription(sets: Vec<WorkingSet>) -> Self {
        Self {
            prescription: None,
            sets,
        }
    }

    /// Whether no working set was done: the exercise was skipped.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }
}

/// The history of `exercise` for [`next_targets`](super::next_targets), oldest session first.
///
/// From `logs` (in any order), keeps the [`Completed`](SessionStatus::Completed) sessions only
/// (an abandoned session would read as a failure, a skipped one has no sets), and in each the
/// working sets of `exercise` (warm-ups left out), sorted by `set_index`. Sessions where the
/// exercise has no working set are dropped. Sessions are sorted by start time; equal start times
/// keep their order in `logs`.
///
/// `prescription` gives what the program asked for in a session: the caller looks up the
/// program version the session was run from ([`Session::program_version_id`]) and its day
/// ([`Session::day`]), typically with [`Prescription::in_program`]. Sessions it returns `None`
/// for (a version or day that can no longer be found) are kept without a prescription: the engine
/// does not judge them, but they still end a failure streak, so two streaks around them never
/// merge into one.
///
/// It does **not** filter by program: pass the logs of the active program only, and for a load
/// that is a percentage of the training max, only the sessions since the training max was last
/// entered (see the [module documentation](super)).
pub fn exercise_history<'a, T, I, F>(
    exercise: &ExerciseId,
    logs: I,
    mut prescription: F,
) -> Vec<PastSession>
where
    T: Ord + Copy + 'a,
    I: IntoIterator<Item = &'a SessionLog<T>>,
    F: FnMut(&Session<T>) -> Option<Prescription>,
{
    let mut completed: Vec<&SessionLog<T>> = logs
        .into_iter()
        .filter(|log| log.session().status() == SessionStatus::Completed)
        .collect();
    completed.sort_by_key(|log| log.session().started_at());
    completed
        .into_iter()
        .filter_map(|log| {
            let mut sets: Vec<&LoggedSet<T>> = log
                .sets()
                .iter()
                .filter(|set| !set.warm_up && &set.exercise == exercise)
                .collect();
            if sets.is_empty() {
                return None;
            }
            sets.sort_by_key(|set| set.set_index);
            Some(PastSession {
                prescription: prescription(log.session()),
                sets: sets.into_iter().map(WorkingSet::from).collect(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::program::RepTarget;
    use crate::{ProgramVersionId, SessionId, SetId};

    const EVERYTHING: &str = include_str!("../../tests/fixtures/programs/valid/everything.json");

    fn squat() -> ExerciseId {
        ExerciseId::new("squat").unwrap()
    }

    fn five_by_five() -> Prescription {
        Prescription {
            work: Work::Reps {
                sets: 5,
                reps: RepTarget::Fixed(Reps::new(5)),
            },
            load: None,
            rule: ProgressionRule::None,
        }
    }

    fn set(n: u128, exercise: &ExerciseId, index: u16, reps: u16, warm_up: bool) -> LoggedSet<i64> {
        LoggedSet {
            id: SetId::from_uuid(Uuid::from_u128(n)),
            exercise: exercise.clone(),
            set_index: index,
            reps: Reps::new(reps),
            weight: Some(Weight::from_kg(100.0).unwrap()),
            duration: None,
            warm_up,
            completed_at: 10,
            target: None,
        }
    }

    fn log(
        n: u128,
        started_at: i64,
        status: SessionStatus,
        sets: Vec<LoggedSet<i64>>,
    ) -> SessionLog<i64> {
        log_on("a", n, started_at, status, sets)
    }

    fn log_on(
        day: &str,
        n: u128,
        started_at: i64,
        status: SessionStatus,
        sets: Vec<LoggedSet<i64>>,
    ) -> SessionLog<i64> {
        let session = Session::from_parts(
            SessionId::from_uuid(Uuid::from_u128(n)),
            ProgramVersionId::from_uuid(Uuid::from_u128(1)),
            DayId::new(day).unwrap(),
            started_at,
            status,
            status.is_ended().then_some(started_at + 100),
        )
        .unwrap();
        // Shift the sets into the session's time span.
        let sets = sets
            .into_iter()
            .map(|set| LoggedSet {
                completed_at: started_at + set.completed_at,
                ..set
            })
            .collect();
        SessionLog::from_parts(session, sets).unwrap()
    }

    #[test]
    fn working_set_constructors() {
        let weight = Weight::from_kg(60.0).unwrap();
        assert_eq!(
            WorkingSet::new(weight, Reps::new(5)),
            WorkingSet {
                set_index: 0,
                reps: Reps::new(5),
                weight: Some(weight),
                duration: None,
                target: None,
            }
        );
        assert_eq!(WorkingSet::bodyweight(Reps::new(12)).weight, None);
        let logged = set(1, &squat(), 0, 5, false);
        let working = WorkingSet::from(&logged);
        assert_eq!(working.reps, Reps::new(5));
        assert_eq!(working.weight, logged.weight);
        assert_eq!(working.duration, None);
        assert_eq!(set(2, &squat(), 3, 5, false).set_index, 3);
        assert_eq!(
            WorkingSet::from(&set(2, &squat(), 3, 5, false)).set_index,
            3
        );
        assert_eq!(working.at(4).set_index, 4);
        assert!(PastSession::new(five_by_five(), vec![]).is_empty());
        assert!(!PastSession::new(five_by_five(), vec![working]).is_empty());
        let numbered = PastSession::in_order(five_by_five(), vec![working.at(7); 3]);
        let indices: Vec<u16> = numbered.sets.iter().map(|set| set.set_index).collect();
        assert_eq!(indices, [0, 1, 2]);
        assert_eq!(numbered.prescription, Some(five_by_five()));
        assert_eq!(
            PastSession::without_prescription(vec![working]).prescription,
            None
        );
    }

    #[test]
    fn prescriptions_from_a_program() {
        let program = Program::from_json(EVERYTHING).unwrap();
        let day = &program.days[0];
        let bench = &day.exercises[0];
        let found = Prescription::in_program(&program, &day.id, &bench.id).unwrap();
        assert_eq!(found, Prescription::of(bench));
        assert_eq!(found, Prescription::from(bench));
        assert_eq!(found.work, bench.work);
        assert_eq!(found.load, bench.load);
        assert_eq!(found.rule, bench.progression);
        assert_eq!(
            Prescription::in_program(&program, &day.id, &ExerciseId::new("nope").unwrap()),
            None
        );
        assert_eq!(
            Prescription::in_program(&program, &DayId::new("nope").unwrap(), &bench.id),
            None
        );
    }

    #[test]
    fn keeps_completed_sessions_and_working_sets_of_the_exercise_in_order() {
        let bench = ExerciseId::new("bench").unwrap();
        let logs = [
            // Newest first in the input: sorted by start time.
            log(
                1,
                2_000,
                SessionStatus::Completed,
                vec![
                    set(10, &squat(), 1, 4, false),
                    set(11, &squat(), 0, 5, false),
                    set(12, &squat(), 0, 5, true),
                    set(13, &bench, 0, 8, false),
                ],
            ),
            log(
                2,
                1_000,
                SessionStatus::Completed,
                vec![set(20, &squat(), 0, 3, false)],
            ),
            log(
                3,
                3_000,
                SessionStatus::Abandoned,
                vec![set(30, &squat(), 0, 1, false)],
            ),
            log(4, 4_000, SessionStatus::Skipped, vec![]),
            log(
                5,
                5_000,
                SessionStatus::InProgress,
                vec![set(50, &squat(), 0, 9, false)],
            ),
            // The exercise was skipped: dropped.
            log(
                6,
                6_000,
                SessionStatus::Completed,
                vec![set(60, &bench, 0, 8, false)],
            ),
            // No prescription can be found for day `gone`: kept without one.
            log_on(
                "gone",
                7,
                7_000,
                SessionStatus::Completed,
                vec![set(70, &squat(), 0, 7, false)],
            ),
        ];
        let mut asked = Vec::new();
        let history = exercise_history(&squat(), &logs, |session| {
            asked.push(session.id());
            (session.day().as_str() == "a").then(five_by_five)
        });
        let reps: Vec<Vec<u16>> = history
            .iter()
            .map(|session| session.sets.iter().map(|set| set.reps.get()).collect())
            .collect();
        assert_eq!(reps, vec![vec![3], vec![5, 4], vec![7]]);
        assert_eq!(history[0].prescription, Some(five_by_five()));
        assert_eq!(history[1].prescription, Some(five_by_five()));
        assert_eq!(history[2].prescription, None);
        assert_eq!(history[1].sets[1].set_index, 1);
        // Only sessions with sets of the exercise are looked up.
        assert_eq!(asked.len(), 3);
    }

    #[test]
    fn equal_start_times_keep_input_order() {
        let logs = [
            log(
                1,
                1_000,
                SessionStatus::Completed,
                vec![set(10, &squat(), 0, 1, false)],
            ),
            log(
                2,
                1_000,
                SessionStatus::Completed,
                vec![set(20, &squat(), 0, 2, false)],
            ),
        ];
        let history = exercise_history(&squat(), logs.iter(), |_| Some(five_by_five()));
        assert_eq!(history[0].sets[0].reps, Reps::new(1));
        assert_eq!(history[1].sets[0].reps, Reps::new(2));
        assert!(exercise_history::<i64, _, _>(&squat(), [], |_| Some(five_by_five())).is_empty());
    }
}
