//! A session together with its logged sets: the aggregate the client keeps (and persists to
//! localStorage) while training, and the server rebuilds to save sets and end the session.

use serde::{Deserialize, Serialize};

use crate::ids::{DayId, ProgramVersionId, SessionId};

use super::error::SessionError;
use super::model::{Change, Session, SessionOutcome};
use super::set::LoggedSet;

/// A [`Session`] and the sets logged in it, in the order they were logged.
///
/// Invariants, enforced by every operation and by deserialization (so a corrupted or hand-edited
/// localStorage entry is rejected rather than trusted):
/// - set IDs are unique;
/// - no set is completed before the session started, nor after it ended;
/// - once the session has ended, no new set can be added.
///
/// Every mutation is idempotent so that a retried request is harmless:
/// - adding a set whose ID is already logged with identical values is a no-op
///   ([`Change::Unchanged`]), even after the session ended; with different values it is a
///   [`SessionError::SetConflict`];
/// - ending the session again with the same outcome and time is a no-op.
///
/// Serializes as `{"session": {...}, "sets": [...]}`. Unknown fields, here and in the nested
/// session and sets, are ignored on load, so a localStorage entry written by a newer version of the
/// app still loads in an older one (the extra fields are dropped when it is saved again).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "SessionLogRepr<T>",
    bound(deserialize = "T: Deserialize<'de> + Ord + Copy")
)]
pub struct SessionLog<T> {
    session: Session<T>,
    sets: Vec<LoggedSet<T>>,
}

/// The unchecked shape of a stored [`SessionLog`].
#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de> + Ord + Copy"))]
struct SessionLogRepr<T> {
    session: Session<T>,
    sets: Vec<LoggedSet<T>>,
}

impl<T: Ord + Copy> TryFrom<SessionLogRepr<T>> for SessionLog<T> {
    type Error = SessionError;

    fn try_from(repr: SessionLogRepr<T>) -> Result<Self, Self::Error> {
        Self::from_parts(repr.session, repr.sets)
    }
}

impl<T: Ord + Copy> SessionLog<T> {
    /// Starts a new, empty, in-progress session.
    #[must_use]
    pub const fn start(
        id: SessionId,
        program_version_id: ProgramVersionId,
        day: DayId,
        started_at: T,
    ) -> Self {
        Self {
            session: Session::start(id, program_version_id, day, started_at),
            sets: Vec::new(),
        }
    }

    /// Rebuilds a stored session and its sets, checking every invariant.
    ///
    /// # Errors
    /// - [`SessionError::DuplicateSetId`] when two sets share an ID (even with identical values).
    /// - [`SessionError::SetBeforeStart`] when a set is completed before the session started.
    /// - [`SessionError::EndBeforeSet`] when the session ended before a set was completed.
    pub fn from_parts(session: Session<T>, sets: Vec<LoggedSet<T>>) -> Result<Self, SessionError> {
        for (index, set) in sets.iter().enumerate() {
            if sets[..index].iter().any(|earlier| earlier.id == set.id) {
                return Err(SessionError::DuplicateSetId { set_id: set.id });
            }
            check_set_times(&session, set)?;
        }
        Ok(Self { session, sets })
    }

    /// The session header.
    #[must_use]
    pub const fn session(&self) -> &Session<T> {
        &self.session
    }

    /// The logged sets, in the order they were logged.
    #[must_use]
    pub fn sets(&self) -> &[LoggedSet<T>] {
        &self.sets
    }

    /// Splits the aggregate into its session and sets.
    #[must_use]
    pub fn into_parts(self) -> (Session<T>, Vec<LoggedSet<T>>) {
        (self.session, self.sets)
    }

    /// Logs a set.
    ///
    /// # Errors
    /// - [`SessionError::SetConflict`] when a set with the same ID is logged with different values.
    /// - [`SessionError::AlreadyEnded`] when the set is new and the session has ended.
    /// - [`SessionError::SetBeforeStart`] when the set is completed before the session started.
    pub fn add_set(&mut self, set: LoggedSet<T>) -> Result<Change, SessionError> {
        // Look for the ID first: a retried save of a set logged before the end must still succeed.
        if let Some(existing) = self.sets.iter().find(|logged| logged.id == set.id) {
            return if *existing == set {
                Ok(Change::Unchanged)
            } else {
                Err(SessionError::SetConflict { set_id: set.id })
            };
        }
        if self.session.is_ended() {
            return Err(SessionError::AlreadyEnded {
                session_id: self.session.id(),
                status: self.session.status(),
            });
        }
        check_set_times(&self.session, &set)?;
        self.sets.push(set);
        Ok(Change::Applied)
    }

    /// Ends the session with `outcome` at `at`.
    ///
    /// # Errors
    /// - [`SessionError::AlreadyEnded`] when it has already ended with another outcome or time.
    /// - [`SessionError::EndBeforeStart`] when `at` is before the start (checked before the sets).
    /// - [`SessionError::EndBeforeSet`] when `at` is before a logged set was completed.
    pub fn end(&mut self, outcome: SessionOutcome, at: T) -> Result<Change, SessionError> {
        // Once ended, the session alone decides: a retry is a no-op, anything else `AlreadyEnded`.
        if !self.session.is_ended() {
            if at < self.session.started_at() {
                return Err(SessionError::EndBeforeStart {
                    session_id: self.session.id(),
                });
            }
            if let Some(set) = self.sets.iter().find(|set| set.completed_at > at) {
                return Err(SessionError::EndBeforeSet {
                    session_id: self.session.id(),
                    set_id: set.id,
                });
            }
        }
        self.session.end(outcome, at)
    }

    /// Finishes the session normally: [`SessionOutcome::Completed`]. See [`SessionLog::end`].
    ///
    /// # Errors
    /// As [`SessionLog::end`].
    pub fn complete(&mut self, at: T) -> Result<Change, SessionError> {
        self.end(SessionOutcome::Completed, at)
    }

    /// Records the day as deliberately skipped: [`SessionOutcome::Skipped`]. See [`SessionLog::end`].
    ///
    /// # Errors
    /// As [`SessionLog::end`].
    pub fn skip(&mut self, at: T) -> Result<Change, SessionError> {
        self.end(SessionOutcome::Skipped, at)
    }

    /// Gives the session up: [`SessionOutcome::Abandoned`]. See [`SessionLog::end`].
    ///
    /// # Errors
    /// As [`SessionLog::end`].
    pub fn abandon(&mut self, at: T) -> Result<Change, SessionError> {
        self.end(SessionOutcome::Abandoned, at)
    }
}

/// Checks that `set` falls within the session's time span.
fn check_set_times<T: Ord + Copy>(
    session: &Session<T>,
    set: &LoggedSet<T>,
) -> Result<(), SessionError> {
    if set.completed_at < session.started_at() {
        return Err(SessionError::SetBeforeStart { set_id: set.id });
    }
    if session
        .finished_at()
        .is_some_and(|end| set.completed_at > end)
    {
        return Err(SessionError::EndBeforeSet {
            session_id: session.id(),
            set_id: set.id,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ExerciseId, SetId};
    use crate::reps::Reps;
    use crate::session::SessionStatus;
    use crate::weight::Weight;
    use uuid::Uuid;

    const START: i64 = 1_000;

    fn session_id() -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(1))
    }

    fn start() -> SessionLog<i64> {
        SessionLog::start(
            session_id(),
            ProgramVersionId::from_uuid(Uuid::from_u128(2)),
            DayId::new("day-a").unwrap(),
            START,
        )
    }

    fn set(id: u128, completed_at: i64) -> LoggedSet<i64> {
        LoggedSet {
            id: SetId::from_uuid(Uuid::from_u128(id)),
            exercise: ExerciseId::new("back-squat").unwrap(),
            set_index: 0,
            reps: Reps::new(5),
            weight: Some(Weight::from_kg(100.0).unwrap()),
            duration: None,
            warm_up: false,
            completed_at,
            target: None,
        }
    }

    fn set_id(id: u128) -> SetId {
        SetId::from_uuid(Uuid::from_u128(id))
    }

    #[test]
    fn start_is_empty_and_in_progress() {
        let log = start();
        assert!(log.sets().is_empty());
        assert_eq!(log.session().status(), SessionStatus::InProgress);
        assert_eq!(log.session().started_at(), START);
    }

    #[test]
    fn add_set_keeps_logging_order() {
        let mut log = start();
        assert_eq!(log.add_set(set(2, 2_000)).unwrap(), Change::Applied);
        assert_eq!(log.add_set(set(1, 3_000)).unwrap(), Change::Applied);
        // A set at the exact start is fine.
        assert_eq!(log.add_set(set(3, START)).unwrap(), Change::Applied);
        let ids: Vec<_> = log.sets().iter().map(|s| s.id).collect();
        assert_eq!(ids, [set_id(2), set_id(1), set_id(3)]);
    }

    #[test]
    fn re_adding_an_identical_set_is_a_no_op() {
        let mut log = start();
        assert_eq!(log.add_set(set(1, 2_000)).unwrap(), Change::Applied);
        let before = log.clone();
        assert_eq!(log.add_set(set(1, 2_000)).unwrap(), Change::Unchanged);
        assert_eq!(log, before);
    }

    #[test]
    fn re_adding_a_set_id_with_other_values_is_a_conflict() {
        let mut log = start();
        log.add_set(set(1, 2_000)).unwrap();
        let before = log.clone();
        let mut changed = set(1, 2_000);
        changed.reps = Reps::new(4);
        assert_eq!(
            log.add_set(changed),
            Err(SessionError::SetConflict { set_id: set_id(1) })
        );
        assert_eq!(
            log.add_set(set(1, 2_001)),
            Err(SessionError::SetConflict { set_id: set_id(1) })
        );
        assert_eq!(log, before);
    }

    #[test]
    fn rejects_a_set_before_the_start() {
        let mut log = start();
        assert_eq!(
            log.add_set(set(1, START - 1)),
            Err(SessionError::SetBeforeStart { set_id: set_id(1) })
        );
        assert!(log.sets().is_empty());
    }

    #[test]
    fn no_new_set_after_the_end_but_retries_still_succeed() {
        let mut log = start();
        log.add_set(set(1, 2_000)).unwrap();
        assert_eq!(log.complete(3_000).unwrap(), Change::Applied);
        let err = log.add_set(set(2, 2_500)).unwrap_err();
        assert_eq!(
            err,
            SessionError::AlreadyEnded {
                session_id: session_id(),
                status: SessionStatus::Completed
            }
        );
        assert_eq!(
            err.to_string(),
            format!("session {} has already ended (completed)", session_id())
        );
        // A retried save of a set logged before the end is still a no-op success.
        assert_eq!(log.add_set(set(1, 2_000)).unwrap(), Change::Unchanged);
        // But a conflicting one is still a conflict.
        assert_eq!(
            log.add_set(set(1, 2_001)),
            Err(SessionError::SetConflict { set_id: set_id(1) })
        );
        assert_eq!(log.sets().len(), 1);
    }

    #[test]
    fn each_outcome_ends_the_session() {
        type End = fn(&mut SessionLog<i64>, i64) -> Result<Change, SessionError>;
        let cases: [(End, SessionStatus); 3] = [
            (SessionLog::complete, SessionStatus::Completed),
            (SessionLog::skip, SessionStatus::Skipped),
            (SessionLog::abandon, SessionStatus::Abandoned),
        ];
        for (end, status) in cases {
            let mut log = start();
            assert_eq!(end(&mut log, 5_000), Ok(Change::Applied));
            assert_eq!(log.session().status(), status);
            assert_eq!(log.session().finished_at(), Some(5_000));
            // Retrying the same end is a no-op.
            assert_eq!(end(&mut log, 5_000), Ok(Change::Unchanged));
        }
        let mut log = start();
        assert_eq!(log.end(SessionOutcome::Skipped, START), Ok(Change::Applied));
    }

    #[test]
    fn ending_differently_a_second_time_is_rejected() {
        let mut log = start();
        log.abandon(5_000).unwrap();
        let already = Err(SessionError::AlreadyEnded {
            session_id: session_id(),
            status: SessionStatus::Abandoned,
        });
        assert_eq!(log.complete(5_000), already);
        assert_eq!(log.abandon(6_000), already);
        // Even a time that would otherwise be invalid reports the session as already ended.
        assert_eq!(log.abandon(0), already);
    }

    #[test]
    fn re_ending_an_ended_session_earlier_is_already_ended_even_with_sets() {
        let mut log = start();
        log.add_set(set(1, 2_000)).unwrap();
        log.complete(3_000).unwrap();
        let already = Err(SessionError::AlreadyEnded {
            session_id: session_id(),
            status: SessionStatus::Completed,
        });
        // Before the last set, and before the start: the session has ended, that is the error.
        assert_eq!(log.complete(1_500), already);
        assert_eq!(log.complete(START - 1), already);
        assert_eq!(log.session().finished_at(), Some(3_000));
    }

    #[test]
    fn ending_before_the_start_is_reported_as_such_even_with_sets() {
        let mut log = start();
        log.add_set(set(1, 2_000)).unwrap();
        assert_eq!(
            log.complete(START - 1),
            Err(SessionError::EndBeforeStart {
                session_id: session_id()
            })
        );
        assert!(!log.session().is_ended());
    }

    #[test]
    fn unknown_json_fields_are_ignored_on_load() {
        let mut log = start();
        log.add_set(set(1, 2_000)).unwrap();
        let mut json = serde_json::to_value(&log).unwrap();
        json["from_the_future"] = true.into();
        json["session"]["notes"] = "felt strong".into();
        json["sets"][0]["rpe"] = 8.into();
        assert_eq!(
            serde_json::from_value::<SessionLog<i64>>(json).unwrap(),
            log
        );
    }

    #[test]
    fn end_rejects_a_time_before_the_start_or_a_set() {
        let mut log = start();
        assert_eq!(
            log.complete(START - 1),
            Err(SessionError::EndBeforeStart {
                session_id: session_id()
            })
        );
        log.add_set(set(1, 2_000)).unwrap();
        log.add_set(set(2, 3_000)).unwrap();
        let err = log.complete(2_500).unwrap_err();
        assert_eq!(
            err,
            SessionError::EndBeforeSet {
                session_id: session_id(),
                set_id: set_id(2)
            }
        );
        assert!(err.to_string().ends_with(&format!(
            "cannot end before set {} was completed",
            set_id(2)
        )));
        assert!(!log.session().is_ended());
        // Ending exactly when the last set was completed is fine.
        assert_eq!(log.complete(3_000), Ok(Change::Applied));
    }

    #[test]
    fn from_parts_accepts_a_consistent_log() {
        let mut expected = start();
        expected.add_set(set(1, 2_000)).unwrap();
        expected.add_set(set(2, 3_000)).unwrap();
        expected.complete(3_000).unwrap();
        let (session, sets) = expected.clone().into_parts();
        assert_eq!(SessionLog::from_parts(session, sets).unwrap(), expected);
    }

    #[test]
    fn from_parts_rejects_duplicate_ids_even_when_identical() {
        let (session, _) = start().into_parts();
        let err =
            SessionLog::from_parts(session, vec![set(1, 2_000), set(2, 2_000), set(1, 2_000)])
                .unwrap_err();
        assert_eq!(err, SessionError::DuplicateSetId { set_id: set_id(1) });
        assert_eq!(
            err.to_string(),
            format!("set {} appears more than once", set_id(1))
        );
    }

    #[test]
    fn from_parts_rejects_sets_outside_the_session() {
        let (session, _) = start().into_parts();
        let err = SessionLog::from_parts(session, vec![set(1, START - 1)]).unwrap_err();
        assert_eq!(err, SessionError::SetBeforeStart { set_id: set_id(1) });
        assert_eq!(
            err.to_string(),
            format!("set {} was completed before the session started", set_id(1))
        );

        let mut ended = start();
        ended.complete(2_000).unwrap();
        let (session, _) = ended.into_parts();
        assert_eq!(
            SessionLog::from_parts(session, vec![set(1, 2_001)]),
            Err(SessionError::EndBeforeSet {
                session_id: session_id(),
                set_id: set_id(1)
            })
        );
    }

    #[test]
    fn serde_round_trips() {
        let mut log = start();
        log.add_set(set(1, 2_000)).unwrap();
        let mut timed = set(2, 2_500);
        timed.exercise = ExerciseId::new("plank").unwrap();
        timed.weight = None;
        timed.duration = Some(crate::Seconds::new(45));
        timed.warm_up = true;
        log.add_set(timed).unwrap();
        for log in [start(), log.clone(), {
            let mut ended = log;
            ended.skip(3_000).unwrap();
            ended
        }] {
            let json = serde_json::to_string(&log).unwrap();
            assert_eq!(serde_json::from_str::<SessionLog<i64>>(&json).unwrap(), log);
        }
    }

    #[test]
    fn deserialization_rejects_broken_invariants() {
        let mut log = start();
        log.add_set(set(1, 2_000)).unwrap();
        let good = serde_json::to_value(&log).unwrap();
        assert_eq!(good["sets"].as_array().unwrap().len(), 1);
        assert_eq!(good["session"]["status"], "in_progress");

        let mut duplicate = good.clone();
        let first = duplicate["sets"][0].clone();
        duplicate["sets"].as_array_mut().unwrap().push(first);
        let err = serde_json::from_value::<SessionLog<i64>>(duplicate).unwrap_err();
        assert!(err.to_string().contains("appears more than once"), "{err}");

        let mut early = good.clone();
        early["sets"][0]["completed_at"] = (START - 1).into();
        let err = serde_json::from_value::<SessionLog<i64>>(early).unwrap_err();
        assert!(
            err.to_string().contains("before the session started"),
            "{err}"
        );

        let mut after_end = good.clone();
        after_end["session"]["status"] = "completed".into();
        after_end["session"]["finished_at"] = 1_500.into();
        let err = serde_json::from_value::<SessionLog<i64>>(after_end).unwrap_err();
        assert!(err.to_string().contains("cannot end before set"), "{err}");

        let mut no_end = good;
        no_end["session"]["status"] = "completed".into();
        let err = serde_json::from_value::<SessionLog<i64>>(no_end).unwrap_err();
        assert!(err.to_string().contains("end time is missing"), "{err}");
    }
}
