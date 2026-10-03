//! History (#20): the logic behind `crate::api::history`.

use std::collections::{HashMap, HashSet};

use iron_oxide_domain::{
    DayId, E1rmFormula, ExerciseId, ExerciseRecords, LoggedSet, PerformedSet, Reps, Seconds,
    SessionId, Volume, Weight, exercise_series, session_volume, time::Timestamp, top_set,
};
use sqlx::{PgPool, types::time::OffsetDateTime};

use super::{ApiError, sessions::status, timestamp};
use crate::api::history::{
    DEFAULT_PAGE_SIZE, ExerciseLog, ExercisePoint, ExerciseSeries, HistoryCursor, HistoryPage,
    LoggedExercise, MAX_PAGE_SIZE, SeriesKey, SessionDetails, SessionSummary,
};
use crate::server::db::{
    history as repo,
    ids::{self, UserId},
    sessions::{Cursor, SessionStatus as StoredStatus},
    sets,
};

/// The formula of every e1RM in the history: the end-of-session summary's, so the history's PR
/// flags and estimates always agree with it.
const FORMULA: E1rmFormula = E1rmFormula::STANDARD;
/// Nanoseconds in a microsecond, the database's time precision.
const NANOS_PER_MICRO: i128 = 1_000;

pub async fn page(
    pool: &PgPool,
    owner: UserId,
    cursor: Option<HistoryCursor>,
    limit: Option<u32>,
) -> Result<HistoryPage, ApiError> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE);
    if !(1..=MAX_PAGE_SIZE).contains(&limit) {
        return Err(ApiError::invalid(format!(
            "The page size must be between 1 and {MAX_PAGE_SIZE}."
        )));
    }
    let after = cursor.map(repo_cursor).transpose()?;
    let repo::Page { entries, more } = repo::page(pool, owner, after, limit).await?;
    let next = if more {
        entries
            .last()
            .and_then(repo::HistoryEntry::cursor)
            .map(wire_cursor)
            .transpose()?
    } else {
        None
    };
    // Three queries per page, whatever its size: the page, its sets, the record history.
    let ids: Vec<ids::SessionId> = entries.iter().map(|entry| entry.id).collect();
    let sets = by_session(sets::list_for_sessions(pool, owner, &ids).await?)?;
    let prs = sessions_with_prs(pool, owner, &entries, &sets).await?;
    let sessions = entries
        .into_iter()
        .map(|entry| {
            let volume = sets
                .get(&entry.id)
                .map_or(Volume::ZERO, |sets| volume(sets));
            let set_pr = prs.contains(&entry.id);
            summary(entry, volume, set_pr)
        })
        .collect::<Result<_, _>>()?;
    Ok(HistoryPage { sessions, next })
}

pub async fn details(
    pool: &PgPool,
    owner: UserId,
    session_id: SessionId,
) -> Result<SessionDetails, ApiError> {
    let id = session_id.into();
    let entry = repo::entry(pool, owner, id).await?;
    let stored = sets::list_for_session(pool, owner, id).await?;
    let exercises = group_by_exercise(stored.clone())?;
    let sets = by_session(stored)?;
    let prs = sessions_with_prs(pool, owner, std::slice::from_ref(&entry), &sets).await?;
    let set_pr = prs.contains(&entry.id);
    let volume = exercises.iter().map(|log| log.volume).sum();
    Ok(SessionDetails {
        session: summary(entry, volume, set_pr)?,
        exercises,
    })
}

pub async fn series(
    pool: &PgPool,
    owner: UserId,
    exercise_id: &str,
) -> Result<ExerciseSeries, ApiError> {
    let exercise_id = ExerciseId::new(exercise_id)
        .map_err(|_| ApiError::invalid("This is not a valid exercise id."))?;
    let sets = repo::exercise_sets(pool, owner, exercise_id.as_str()).await?;
    Ok(ExerciseSeries {
        points: series_points(sets)?,
        exercise_id,
    })
}

pub async fn exercises(pool: &PgPool, owner: UserId) -> Result<Vec<LoggedExercise>, ApiError> {
    repo::logged_exercises(pool, owner)
        .await?
        .into_iter()
        .map(|row| {
            Ok(LoggedExercise {
                exercise_id: exercise(row.exercise_id)?,
                sessions: row.sessions,
                last_session_at: timestamp(row.last_session_at)?,
            })
        })
        .collect()
}

/// The page cursor for the client, keeping the database's microseconds.
fn wire_cursor(cursor: repo::HistoryCursor) -> Result<HistoryCursor, ApiError> {
    let micros = cursor
        .finished_at
        .unix_timestamp_nanos()
        .div_euclid(NANOS_PER_MICRO);
    let finished_at_us =
        i64::try_from(micros).map_err(|_| ApiError::internal("stored finish time out of range"))?;
    Ok(HistoryCursor::new(finished_at_us, cursor.id.into()))
}

/// The earliest time a `timestamptz` holds (4714-11-24 00:00 UTC BC), in microseconds since the
/// epoch. Every stored finish time is at or after it.
const EARLIEST_TIMESTAMPTZ_US: i64 = -210_866_803_200_000_000;

/// A cursor sent back by the client. `422` if its time is outside what can be stored (it was
/// tampered with): Postgres' `timestamptz` from below, `OffsetDateTime`'s year 9999 from above.
fn repo_cursor(cursor: HistoryCursor) -> Result<repo::HistoryCursor, ApiError> {
    let invalid = || ApiError::invalid("Invalid history cursor.");
    if cursor.finished_at_us() < EARLIEST_TIMESTAMPTZ_US {
        return Err(invalid());
    }
    let nanos = i128::from(cursor.finished_at_us()) * NANOS_PER_MICRO;
    let finished_at = OffsetDateTime::from_unix_timestamp_nanos(nanos).map_err(|_| invalid())?;
    Ok(repo::HistoryCursor {
        finished_at,
        id: cursor.id().into(),
    })
}

/// A stored exercise id; not a slug means corrupt data.
fn exercise(id: String) -> Result<ExerciseId, ApiError> {
    ExerciseId::new(id).map_err(ApiError::internal)
}

/// A stored weight; out of range means corrupt data.
fn weight(nanograms: u64) -> Result<Weight, ApiError> {
    Weight::from_nanograms(nanograms).map_err(ApiError::internal)
}

fn summary(
    entry: repo::HistoryEntry,
    volume: Volume,
    set_pr: bool,
) -> Result<SessionSummary, ApiError> {
    Ok(SessionSummary {
        id: entry.id.into(),
        program_id: entry.program_id.into(),
        program_name: entry.program_name,
        program_version_id: entry.program_version_id.into(),
        program_version: entry.program_version,
        day_id: DayId::new(entry.day_id).map_err(ApiError::internal)?,
        day_name: entry.day_name,
        status: status(entry.status),
        started_at: timestamp(entry.started_at)?,
        finished_at: entry.finished_at.map(timestamp).transpose()?,
        working_sets: entry.working_sets,
        volume,
        set_pr,
    })
}

/// Stored sets grouped by session, each group in the stored order.
fn by_session(
    sets: Vec<sets::LoggedSet>,
) -> Result<HashMap<ids::SessionId, Vec<LoggedSet<Timestamp>>>, ApiError> {
    let mut groups: HashMap<ids::SessionId, Vec<LoggedSet<Timestamp>>> = HashMap::new();
    for set in sets {
        let session = set.session_id;
        groups.entry(session).or_default().push(logged_set(set)?);
    }
    Ok(groups)
}

/// A session's volume, as the end-of-session summary counts it.
fn volume(sets: &[LoggedSet<Timestamp>]) -> Volume {
    session_volume(performed(sets))
}

/// Which of `entries` set a personal record, as their end-of-session summaries report them
/// (`server::api::sessions`): completed sessions only, each against the sets of the completed
/// sessions started before it (by start, then id). `sets` holds the entries' own sets.
///
/// One query whatever the number of entries: the record history of the exercises the entries
/// logged, up to the latest-started completed entry, replayed in order.
async fn sessions_with_prs(
    pool: &PgPool,
    owner: UserId,
    entries: &[repo::HistoryEntry],
    sets: &HashMap<ids::SessionId, Vec<LoggedSet<Timestamp>>>,
) -> Result<HashSet<ids::SessionId>, ApiError> {
    let completed: Vec<&repo::HistoryEntry> = entries
        .iter()
        .filter(|entry| entry.status == StoredStatus::Completed)
        .collect();
    let Some(latest) = completed
        .iter()
        .max_by_key(|entry| (entry.started_at, entry.id))
    else {
        return Ok(HashSet::new());
    };
    let mut exercises: Vec<String> = completed
        .iter()
        .filter_map(|entry| sets.get(&entry.id))
        .flatten()
        .map(|set| set.exercise.as_str().to_owned())
        .collect();
    exercises.sort_unstable();
    exercises.dedup();
    if exercises.is_empty() {
        return Ok(HashSet::new());
    }
    let before = Cursor {
        started_at: latest.started_at,
        id: latest.id,
    };
    // Every completed session before the latest entry, the other completed entries included.
    let earlier = sets::completed_for_exercises_before(pool, owner, &exercises, before).await?;
    let mut sweep: Vec<(ids::SessionId, Vec<LoggedSet<Timestamp>>)> = Vec::new();
    for set in earlier {
        let session = set.session_id;
        let set = logged_set(set)?;
        match sweep.last_mut() {
            Some((last, group)) if *last == session => group.push(set),
            _ => sweep.push((session, vec![set])),
        }
    }
    sweep.push((latest.id, sets.get(&latest.id).cloned().unwrap_or_default()));
    let candidates = completed.iter().map(|entry| entry.id).collect();
    Ok(pr_sessions(sweep, &candidates))
}

/// Replays `sweep` (sessions in start order, each with its sets) and returns the `candidates`
/// whose sets beat the records of the sessions before them, as `detect_prs` decides.
fn pr_sessions<S: Copy + Eq + std::hash::Hash>(
    sweep: Vec<(S, Vec<LoggedSet<Timestamp>>)>,
    candidates: &HashSet<S>,
) -> HashSet<S> {
    let mut records: HashMap<ExerciseId, ExerciseRecords> = HashMap::new();
    let mut found = HashSet::new();
    for (session, sets) in sweep {
        if candidates.contains(&session) {
            let mut exercises: Vec<&ExerciseId> = sets.iter().map(|set| &set.exercise).collect();
            exercises.dedup();
            let beat = exercises.into_iter().any(|exercise| {
                let of_exercise = sets
                    .iter()
                    .filter(|set| &set.exercise == exercise)
                    .filter_map(Option::<PerformedSet>::from);
                records
                    .get(exercise)
                    .is_some_and(|records| !records.prs(exercise, of_exercise).is_empty())
            });
            if beat {
                found.insert(session);
            }
        }
        for set in &sets {
            if let Some(performed) = Option::<PerformedSet>::from(set) {
                records
                    .entry(set.exercise.clone())
                    .or_insert_with(|| ExerciseRecords::new(FORMULA))
                    .record(performed);
            }
        }
    }
    found
}

fn logged_set(set: sets::LoggedSet) -> Result<LoggedSet<Timestamp>, ApiError> {
    Ok(LoggedSet {
        id: set.id.into(),
        exercise: exercise(set.exercise_id)?,
        set_index: set.set_index,
        reps: Reps::new(set.reps),
        weight: set.weight_ng.map(weight).transpose()?,
        duration: set.duration_s.map(Seconds::new),
        warm_up: set.warmup,
        completed_at: timestamp(set.completed_at)?,
        target: set
            .target
            .as_ref()
            .map(super::sessions::domain_target)
            .transpose()?,
    })
}

/// The sets the statistics count, as the domain decides (weighted and not timed): the same
/// inputs as the end-of-session summary.
fn performed(sets: &[LoggedSet<Timestamp>]) -> impl Iterator<Item = PerformedSet> + '_ {
    sets.iter().filter_map(Option::<PerformedSet>::from)
}

/// Groups a session's sets by exercise, in the order each exercise was first logged, with each
/// exercise's top set, best e1RM and volume.
pub fn group_by_exercise(sets: Vec<sets::LoggedSet>) -> Result<Vec<ExerciseLog>, ApiError> {
    let mut groups: Vec<(ExerciseId, Vec<LoggedSet<Timestamp>>)> = Vec::new();
    for set in sets {
        let set = logged_set(set)?;
        match groups.iter_mut().find(|(id, _)| *id == set.exercise) {
            Some((_, group)) => group.push(set),
            None => groups.push((set.exercise.clone(), vec![set])),
        }
    }
    Ok(groups
        .into_iter()
        .map(|(exercise_id, sets)| ExerciseLog {
            top_set: top_set(performed(&sets)),
            best_e1rm: performed(&sets).filter_map(|set| set.e1rm(FORMULA)).max(),
            volume: session_volume(performed(&sets)),
            exercise_id,
            sets,
        })
        .collect())
}

/// The chart points of one exercise, from its sets grouped by session (as
/// [`repo::exercise_sets`] returns them).
pub fn series_points(sets: Vec<repo::ExerciseSet>) -> Result<Vec<ExercisePoint>, ApiError> {
    let mut sessions: Vec<(SeriesKey, Vec<PerformedSet>)> = Vec::new();
    for set in sets {
        let key = SeriesKey {
            started_at: timestamp(set.session_started_at)?,
            session_id: set.session_id.into(),
        };
        let performed = PerformedSet {
            weight: weight(set.weight_ng)?,
            reps: Reps::new(set.reps),
            warmup: set.warmup,
        };
        match sessions.last_mut() {
            Some((last, group)) if *last == key => group.push(performed),
            _ => sessions.push((key, vec![performed])),
        }
    }
    let volumes: HashMap<SeriesKey, Volume> = sessions
        .iter()
        .map(|(key, sets)| (*key, session_volume(sets.iter().copied())))
        .collect();
    Ok(exercise_series(sessions, FORMULA)
        .into_iter()
        .map(|point| ExercisePoint {
            volume: volumes.get(&point.key).copied().unwrap_or(Volume::ZERO),
            key: point.key,
            top_set: point.top_set,
            best_e1rm: point.best_e1rm,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use iron_oxide_domain::{Lift, Volume};
    use sqlx::types::{Uuid, time::OffsetDateTime};

    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::server::{
        api::testing::{self, TestApi, TestUser},
        db::{
            history::ExerciseSet,
            ids,
            sessions::{self as db_sessions, NewSession, SessionOutcome},
            sets, testing as db_testing,
        },
    };

    const PAGE: &str = "/api/history/page";
    const DETAILS: &str = "/api/history/session";
    const SERIES: &str = "/api/history/exercise-series";
    const EXERCISES: &str = "/api/history/exercises";

    fn kg(value: f64) -> Weight {
        Weight::from_kg(value).unwrap()
    }

    fn time(millis: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).unwrap()
    }

    fn set(
        n: u128,
        exercise: &str,
        weight: Option<f64>,
        reps: u16,
        warmup: bool,
    ) -> sets::LoggedSet {
        sets::LoggedSet {
            id: ids::SetId::from_uuid(Uuid::from_u128(n)),
            session_id: ids::SessionId::from_uuid(Uuid::from_u128(1)),
            exercise_id: exercise.to_owned(),
            set_index: 0,
            reps,
            weight_ng: weight.map(|w| kg(w).as_nanograms()),
            duration_s: None,
            warmup,
            completed_at: time(i64::try_from(n).unwrap()),
            target: None,
        }
    }

    fn epley(weight: Weight, reps: u16) -> Option<Weight> {
        FORMULA.estimate(weight, Reps::new(reps))
    }

    #[test]
    fn cursors_keep_microseconds_and_reject_out_of_range_times() {
        let stored = repo::HistoryCursor {
            finished_at: time(1_790_000_000_123) + time::Duration::microseconds(456),
            id: ids::SessionId::from_uuid(Uuid::from_u128(9)),
        };
        let wire = wire_cursor(stored).unwrap();
        assert_eq!(wire.finished_at_us(), 1_790_000_000_123_456);
        assert_eq!(repo_cursor(wire).unwrap(), stored);
        let before_epoch = repo::HistoryCursor {
            finished_at: time(-1),
            ..stored
        };
        assert_eq!(
            repo_cursor(wire_cursor(before_epoch).unwrap()).unwrap(),
            before_epoch
        );
        let id = SessionId::from_uuid(Uuid::from_u128(9));
        for accepted in [
            EARLIEST_TIMESTAMPTZ_US,
            -1,
            0,
            // 9999-12-31 23:59:59.999999 UTC.
            253_402_300_799_999_999,
        ] {
            assert!(
                repo_cursor(HistoryCursor::new(accepted, id)).is_ok(),
                "{accepted}"
            );
        }
        for tampered in [
            i64::MIN,
            -377_705_116_800_000_001,
            -377_705_116_800_000_000,
            -300_000_000_000_000_000,
            EARLIEST_TIMESTAMPTZ_US - 1,
            253_402_300_800_000_000,
            i64::MAX,
        ] {
            assert_eq!(
                repo_cursor(HistoryCursor::new(tampered, id))
                    .unwrap_err()
                    .public(),
                (422, "Invalid history cursor."),
                "{tampered}"
            );
        }
    }

    #[test]
    fn sets_are_grouped_by_exercise_in_first_logged_order_with_their_stats() {
        let logs = group_by_exercise(vec![
            set(1, "back-squat", Some(60.0), 5, true),
            set(2, "bench", Some(80.0), 5, false),
            set(3, "back-squat", Some(100.0), 5, false),
            set(4, "back-squat", Some(100.0), 3, false),
            set(5, "pull-up", None, 8, false),
            set(6, "bench", Some(80.0), 0, false),
        ])
        .unwrap();
        let summary: Vec<(&str, usize)> = logs
            .iter()
            .map(|log| (log.exercise_id.as_str(), log.sets.len()))
            .collect();
        assert_eq!(
            summary,
            vec![("back-squat", 3), ("bench", 2), ("pull-up", 1)]
        );

        let squat = &logs[0];
        assert_eq!(
            squat.top_set,
            Some(Lift {
                weight: kg(100.0),
                reps: Reps::new(5)
            })
        );
        assert_eq!(squat.best_e1rm, epley(kg(100.0), 5));
        assert_eq!(squat.volume, Volume::of(kg(100.0), Reps::new(8)));
        assert_eq!(squat.sets[0].completed_at, Timestamp::from_epoch_millis(1));
        assert!(squat.sets[0].warm_up);

        let pull_up = &logs[2];
        assert_eq!(pull_up.sets[0].weight, None);
        assert_eq!(pull_up.top_set, None);
        assert_eq!(pull_up.best_e1rm, None);
        assert_eq!(pull_up.volume, Volume::ZERO);
        assert!(group_by_exercise(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn corrupt_stored_values_are_errors_not_panics() {
        let bad_exercise = set(1, "Back Squat", Some(60.0), 5, false);
        assert!(group_by_exercise(vec![bad_exercise]).is_err());
        let too_heavy = sets::LoggedSet {
            weight_ng: Some(u64::MAX),
            ..set(1, "bench", None, 5, false)
        };
        assert!(group_by_exercise(vec![too_heavy]).is_err());
    }

    fn chart_set(session: u128, started: i64, weight: f64, reps: u16, warmup: bool) -> ExerciseSet {
        ExerciseSet {
            session_id: ids::SessionId::from_uuid(Uuid::from_u128(session)),
            session_started_at: time(started),
            reps,
            weight_ng: kg(weight).as_nanograms(),
            warmup,
        }
    }

    #[test]
    fn series_has_one_point_per_session_with_a_working_set() {
        let points = series_points(vec![
            chart_set(1, 0, 100.0, 5, false),
            chart_set(1, 0, 105.0, 3, false),
            // Two sessions started in the same millisecond stay apart.
            chart_set(2, 1_000, 60.0, 5, true),
            chart_set(3, 1_000, 107.5, 1, false),
            chart_set(3, 1_000, 90.0, 12, false),
            // Only a failed attempt: no point.
            chart_set(4, 2_000, 110.0, 0, false),
        ])
        .unwrap();
        let expected = vec![
            ExercisePoint {
                key: SeriesKey {
                    started_at: Timestamp::from_epoch_millis(0),
                    session_id: SessionId::from_uuid(Uuid::from_u128(1)),
                },
                top_set: Lift {
                    weight: kg(105.0),
                    reps: Reps::new(3),
                },
                best_e1rm: epley(kg(100.0), 5).max(epley(kg(105.0), 3)),
                // 100 × 5 + 105 × 3.
                volume: Volume::of(kg(815.0), Reps::new(1)),
            },
            ExercisePoint {
                key: SeriesKey {
                    started_at: Timestamp::from_epoch_millis(1_000),
                    session_id: SessionId::from_uuid(Uuid::from_u128(3)),
                },
                top_set: Lift {
                    weight: kg(107.5),
                    reps: Reps::new(1),
                },
                best_e1rm: epley(kg(90.0), 12).max(Some(kg(107.5))),
                // 107.5 × 1 + 90 × 12; the warm-up of session 2 adds nothing anywhere.
                volume: Volume::of(kg(1187.5), Reps::new(1)),
            },
        ];
        assert_eq!(points, expected);
        assert!(series_points(Vec::new()).unwrap().is_empty());
    }

    fn performed_set(n: u128, exercise: &str, weight: f64, reps: u16) -> LoggedSet<Timestamp> {
        logged_set(set(n, exercise, Some(weight), reps, false)).unwrap()
    }

    #[test]
    fn pr_sessions_replays_the_history_in_order() {
        let sweep = vec![
            // Not a candidate (an older page): its sets still count as history.
            (1, vec![performed_set(1, "squat", 100.0, 5)]),
            // Same lift: no record.
            (2, vec![performed_set(2, "squat", 100.0, 5)]),
            // Heavier: a record.
            (3, vec![performed_set(3, "squat", 105.0, 5)]),
            // A first bench session is no record (nothing to beat), squat 100 x 6 is (reps).
            (
                4,
                vec![
                    performed_set(4, "bench", 80.0, 5),
                    performed_set(5, "squat", 100.0, 6),
                ],
            ),
            // A first ever exercise alone: no record.
            (5, vec![performed_set(6, "row", 60.0, 5)]),
        ];
        let candidates: HashSet<u32> = [2, 3, 4, 5].into_iter().collect();
        let mut found: Vec<u32> = pr_sessions(sweep, &candidates).into_iter().collect();
        found.sort_unstable();
        assert_eq!(found, [3, 4]);
        assert!(pr_sessions(Vec::<(u32, _)>::new(), &candidates).is_empty());
    }

    // --- Endpoints, against Postgres.

    /// Starts a session of `version` at `start` seconds, logs `sets` (exercise, kg, reps, warm-up)
    /// in it and, with `finish`, ends it (completed) that many seconds after the start.
    async fn seed(
        db: &PgPool,
        owner: UserId,
        version: ids::ProgramVersionId,
        start: i64,
        sets: &[(&str, Option<f64>, u16, bool)],
        finish: Option<i64>,
    ) -> ids::SessionId {
        let new = NewSession {
            started_at: db_testing::at(start),
            ..db_testing::new_session(version)
        };
        db_sessions::start(db, owner, &new).await.unwrap();
        for (n, &(exercise, weight, reps, warmup)) in (0_i64..).zip(sets) {
            let set = sets::LoggedSet {
                exercise_id: exercise.to_owned(),
                weight_ng: weight.map(|w| kg(w).as_nanograms()),
                reps,
                warmup,
                completed_at: db_testing::at(start + n + 1),
                ..db_testing::new_set(new.id)
            };
            sets::upsert_idempotent(db, owner, &set).await.unwrap();
        }
        if let Some(after) = finish {
            db_sessions::finish(
                db,
                owner,
                new.id,
                SessionOutcome::Completed,
                db_testing::at(start + after),
            )
            .await
            .unwrap();
        }
        new.id
    }

    /// Every page of `user`'s history, `limit` at a time.
    async fn all_pages(user: &mut TestUser, limit: u32) -> Vec<Vec<Uuid>> {
        let mut pages = Vec::new();
        let mut cursor: Option<HistoryCursor> = None;
        loop {
            let page: HistoryPage = user
                .call(PAGE, json!({ "cursor": cursor, "limit": limit }))
                .await
                .unwrap();
            pages.push(page.sessions.iter().map(|s| s.id.as_uuid()).collect());
            match page.next {
                Some(next) => cursor = Some(next),
                None => return pages,
            }
        }
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn history_pages_ended_sessions_newest_first(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let empty: HistoryPage = a.call(PAGE, json!({})).await.unwrap();
        assert_eq!(
            empty,
            HistoryPage {
                sessions: Vec::new(),
                next: None
            }
        );

        let (program, version) = db_testing::program(&api.db, a.id).await;
        let mut ended = Vec::new();
        for hour in 0..5 {
            let sets = [
                ("back-squat", Some(100.0), 5, false),
                ("back-squat", Some(60.0), 5, true),
            ];
            ended.push(seed(&api.db, a.id, version, hour * 3_600, &sets, Some(600)).await);
        }
        let running = seed(&api.db, a.id, version, 20_000, &[], None).await;
        let newest_first: Vec<Uuid> = ended.iter().rev().map(|id| id.as_uuid()).collect();

        // Exactly one full page: no next cursor.
        assert_eq!(all_pages(&mut a, 5).await, vec![newest_first.clone()]);
        assert_eq!(
            all_pages(&mut a, 2).await,
            vec![
                newest_first[0..2].to_vec(),
                newest_first[2..4].to_vec(),
                newest_first[4..].to_vec()
            ]
        );
        // The default page size (20) holds them all.
        let page: HistoryPage = a.call(PAGE, json!({ "cursor": null })).await.unwrap();
        assert_eq!(page.next, None);
        assert_eq!(
            page.sessions[0],
            SessionSummary {
                id: ended[4].into(),
                program_id: program.into(),
                program_name: "Program".to_owned(),
                program_version_id: version.into(),
                program_version: 1,
                day_id: DayId::new("a").unwrap(),
                // The test program has no days.
                day_name: None,
                status: iron_oxide_domain::SessionStatus::Completed,
                started_at: timestamp(db_testing::at(4 * 3_600)).unwrap(),
                finished_at: Some(timestamp(db_testing::at(4 * 3_600 + 600)).unwrap()),
                working_sets: 1,
                // 100 kg × 5; the warm-up adds nothing.
                volume: Volume::of(kg(100.0), Reps::new(5)),
                // The same lift as the four sessions before it: no record.
                set_pr: false,
            }
        );
        assert!(
            !page
                .sessions
                .iter()
                .any(|s| s.id.as_uuid() == running.as_uuid())
        );
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn history_rejects_bad_page_sizes_and_cursors(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        for limit in [0, MAX_PAGE_SIZE + 1] {
            let error = a.call_err(PAGE, json!({ "limit": limit })).await;
            assert_eq!(error.status.as_u16(), 422, "{error:?}");
            assert_eq!(error.message, "The page size must be between 1 and 100.");
        }
        let _: HistoryPage = a
            .call(PAGE, json!({ "limit": MAX_PAGE_SIZE }))
            .await
            .unwrap();
        let cursor = |finished_at_us: i64| json!({ "cursor": { "finished_at_us": finished_at_us, "id": Uuid::now_v7() } });
        for tampered in [
            i64::MIN,
            -377_705_116_800_000_000,
            -300_000_000_000_000_000,
            -210_866_803_200_000_001,
            253_402_300_800_000_000,
            i64::MAX,
        ] {
            let error = a.call_err(PAGE, cursor(tampered)).await;
            assert_eq!(
                (error.status.as_u16(), error.message.as_str()),
                (422, "Invalid history cursor."),
                "{tampered}"
            );
        }
        // The extremes that can be stored are valid positions (nothing is before or after them).
        for edge in [-210_866_803_200_000_000, 253_402_300_799_999_999] {
            let page: HistoryPage = a.call(PAGE, cursor(edge)).await.unwrap();
            assert!(page.sessions.is_empty(), "{edge}");
        }
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn the_largest_page_size_still_reaches_every_session(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let (_, version) = db_testing::program(&api.db, a.id).await;
        for n in 0..=i64::from(MAX_PAGE_SIZE) {
            seed(&api.db, a.id, version, n * 10, &[], Some(1)).await;
        }
        let pages = all_pages(&mut a, MAX_PAGE_SIZE).await;
        let sizes: Vec<usize> = pages.iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![100, 1]);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn session_details_groups_sets_by_exercise(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let (_, version) = db_testing::program(&api.db, a.id).await;
        let session = seed(
            &api.db,
            a.id,
            version,
            0,
            &[
                ("back-squat", Some(60.0), 5, true),
                ("bench", Some(80.0), 5, false),
                ("back-squat", Some(100.0), 5, false),
                ("pull-up", None, 8, false),
            ],
            None,
        )
        .await;
        let details: SessionDetails = a
            .call(DETAILS, json!({ "session_id": session.as_uuid() }))
            .await
            .unwrap();
        // In progress sessions can be opened too.
        assert_eq!(details.session.finished_at, None);
        assert_eq!(details.session.working_sets, 3);
        let exercises: Vec<(&str, usize)> = details
            .exercises
            .iter()
            .map(|e| (e.exercise_id.as_str(), e.sets.len()))
            .collect();
        assert_eq!(
            exercises,
            vec![("back-squat", 2), ("bench", 1), ("pull-up", 1)]
        );
        assert_eq!(
            details.exercises[0].top_set,
            Some(Lift {
                weight: kg(100.0),
                reps: Reps::new(5)
            })
        );
        assert_eq!(details.exercises[0].best_e1rm, epley(kg(100.0), 5));
        assert_eq!(details.exercises[2].sets[0].weight, None);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn exercise_series_and_logged_exercises(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let (_, version) = db_testing::program(&api.db, a.id).await;
        let first = seed(
            &api.db,
            a.id,
            version,
            0,
            &[
                ("back-squat", Some(100.0), 5, false),
                ("bench", Some(60.0), 5, false),
            ],
            Some(600),
        )
        .await;
        let second = seed(
            &api.db,
            a.id,
            version,
            86_400,
            &[
                ("back-squat", Some(60.0), 5, true),
                ("back-squat", Some(105.0), 3, false),
            ],
            Some(600),
        )
        .await;
        // Not ended yet: not charted.
        seed(
            &api.db,
            a.id,
            version,
            200_000,
            &[("back-squat", Some(150.0), 1, false)],
            None,
        )
        .await;

        let series: ExerciseSeries = a
            .call(SERIES, json!({ "exercise_id": "back-squat" }))
            .await
            .unwrap();
        assert_eq!(series.exercise_id.as_str(), "back-squat");
        let points: Vec<(Uuid, Lift, Option<Weight>)> = series
            .points
            .iter()
            .map(|p| (p.key.session_id.as_uuid(), p.top_set, p.best_e1rm))
            .collect();
        let lift = |weight, reps| Lift {
            weight: kg(weight),
            reps: Reps::new(reps),
        };
        assert_eq!(
            points,
            vec![
                (first.as_uuid(), lift(100.0, 5), epley(kg(100.0), 5)),
                (second.as_uuid(), lift(105.0, 3), epley(kg(105.0), 3)),
            ]
        );
        let never: ExerciseSeries = a
            .call(SERIES, json!({ "exercise_id": "deadlift" }))
            .await
            .unwrap();
        assert!(never.points.is_empty());
        for bad in ["Back Squat", ""] {
            let error = a.call_err(SERIES, json!({ "exercise_id": bad })).await;
            assert_eq!(
                (error.status.as_u16(), error.message.as_str()),
                (422, "This is not a valid exercise id.")
            );
        }

        let exercises: Vec<LoggedExercise> = a.call(EXERCISES, json!({})).await.unwrap();
        let summary: Vec<(&str, u32)> = exercises
            .iter()
            .map(|e| (e.exercise_id.as_str(), e.sessions))
            .collect();
        assert_eq!(summary, vec![("back-squat", 2), ("bench", 1)]);
        assert_eq!(
            exercises[0].last_session_at,
            timestamp(db_testing::at(86_400)).unwrap()
        );
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_session_details_are_not_found(db: PgPool) {
        let api = TestApi::new(db).await;
        let (mut a, mut b) = api.users_a_and_b().await;
        let (_, version) = db_testing::program(&api.db, a.id).await;
        let session = seed(
            &api.db,
            a.id,
            version,
            0,
            &[("bench", Some(60.0), 5, false)],
            Some(60),
        )
        .await;
        let body = |id: Uuid| json!({ "session_id": id });
        testing::assert_not_found_for_other_user(&mut b, DETAILS, session.as_uuid(), body).await;
        let details: Result<SessionDetails, _> = a.call(DETAILS, body(session.as_uuid())).await;
        assert!(details.is_ok(), "{details:?}");
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_history_series_and_exercises_are_invisible(db: PgPool) {
        let api = TestApi::new(db).await;
        let (mut a, mut b) = api.users_a_and_b().await;
        let (_, version) = db_testing::program(&api.db, a.id).await;
        for start in [0, 1_000] {
            seed(
                &api.db,
                a.id,
                version,
                start,
                &[("bench", Some(60.0), 5, false)],
                Some(60),
            )
            .await;
        }
        let a_page: HistoryPage = a.call(PAGE, json!({ "limit": 1 })).await.unwrap();
        assert_eq!(a_page.sessions.len(), 1);

        // B's own history is empty, even when paging from A's cursor.
        for body in [json!({}), json!({ "cursor": a_page.next })] {
            let page: HistoryPage = b.call(PAGE, body).await.unwrap();
            assert_eq!(
                page,
                HistoryPage {
                    sessions: Vec::new(),
                    next: None
                }
            );
        }
        let series: ExerciseSeries = b
            .call(SERIES, json!({ "exercise_id": "bench" }))
            .await
            .unwrap();
        assert!(series.points.is_empty());
        let exercises: Vec<LoggedExercise> = b.call(EXERCISES, json!({})).await.unwrap();
        assert!(exercises.is_empty());
        // A still sees everything.
        let series: ExerciseSeries = a
            .call(SERIES, json!({ "exercise_id": "bench" }))
            .await
            .unwrap();
        assert_eq!(series.points.len(), 2);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn history_needs_a_signed_in_user(db: PgPool) {
        let api = TestApi::new(db).await;
        let bodies: [(&str, Value); 4] = [
            (PAGE, json!({})),
            (DETAILS, json!({ "session_id": Uuid::now_v7() })),
            (SERIES, json!({ "exercise_id": "bench" })),
            (EXERCISES, json!({})),
        ];
        for (path, body) in bodies {
            testing::assert_unauthorized_when_signed_out(&api, path, body).await;
        }
    }

    /// Ends `session` as `outcome`, `after` seconds after the fixed test time.
    async fn end(
        db: &PgPool,
        owner: UserId,
        session: ids::SessionId,
        outcome: SessionOutcome,
        after: i64,
    ) {
        db_sessions::finish(db, owner, session, outcome, db_testing::at(after))
            .await
            .unwrap();
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn history_items_carry_their_volume_and_whether_they_set_a_pr(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let (_, version) = db_testing::program(&api.db, a.id).await;
        let squat = |weight: f64, reps: u16| ("back-squat", Some(weight), reps, false);
        let mut sessions = Vec::new();
        // 0: the first squat session sets no record (nothing to beat).
        sessions.push(seed(&api.db, a.id, version, 0, &[squat(100.0, 5)], Some(600)).await);
        // 1: heavier, a record.
        sessions.push(seed(&api.db, a.id, version, 1_000, &[squat(105.0, 5)], Some(600)).await);
        // 2: a lighter session with a warm-up, body-weight work and a weighted hold: no record,
        // and only 100 x 5 counts in its volume.
        let light = seed(
            &api.db,
            a.id,
            version,
            2_000,
            &[
                ("back-squat", Some(60.0), 5, true),
                squat(100.0, 5),
                ("pull-up", None, 8, false),
            ],
            None,
        )
        .await;
        let hold = sets::LoggedSet {
            exercise_id: "plank".to_owned(),
            weight_ng: Some(kg(20.0).as_nanograms()),
            reps: 1,
            duration_s: Some(60),
            completed_at: db_testing::at(2_100),
            ..db_testing::new_set(light)
        };
        sets::upsert_idempotent(&api.db, a.id, &hold).await.unwrap();
        end(&api.db, a.id, light, SessionOutcome::Completed, 2_600).await;
        sessions.push(light);
        // 3: abandoned: never a record, and not a record to beat later.
        let abandoned = seed(&api.db, a.id, version, 3_000, &[squat(200.0, 1)], None).await;
        end(&api.db, a.id, abandoned, SessionOutcome::Abandoned, 3_600).await;
        sessions.push(abandoned);
        // 4: one more rep at 100 kg, a record.
        sessions.push(seed(&api.db, a.id, version, 4_000, &[squat(100.0, 6)], Some(600)).await);
        // 5: 150 kg beats 105 kg; it would not beat the abandoned 200 kg.
        sessions.push(seed(&api.db, a.id, version, 5_000, &[squat(150.0, 1)], Some(600)).await);
        let expected_prs = [false, true, false, false, true, true];

        // The same flags whatever the page size, across page boundaries too.
        for limit in [1, 2, 4, 20] {
            let mut flags = Vec::new();
            let mut cursor: Option<HistoryCursor> = None;
            loop {
                let page: HistoryPage = a
                    .call(PAGE, json!({ "cursor": cursor, "limit": limit }))
                    .await
                    .unwrap();
                flags.extend(page.sessions.iter().map(|s| (s.id.as_uuid(), s.set_pr)));
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            let expected: Vec<(Uuid, bool)> = sessions
                .iter()
                .zip(expected_prs)
                .rev()
                .map(|(id, pr)| (id.as_uuid(), pr))
                .collect();
            assert_eq!(flags, expected, "limit {limit}");
        }

        let page: HistoryPage = a.call(PAGE, json!({})).await.unwrap();
        let volume_of = |id: ids::SessionId| {
            page.sessions
                .iter()
                .find(|s| s.id.as_uuid() == id.as_uuid())
                .unwrap()
                .volume
        };
        assert_eq!(volume_of(light), Volume::of(kg(100.0), Reps::new(5)));
        assert_eq!(volume_of(abandoned), Volume::of(kg(200.0), Reps::new(1)));
        assert_eq!(volume_of(sessions[1]), Volume::of(kg(105.0), Reps::new(5)));

        // The details say the same as the list.
        for listed in &page.sessions {
            let details: SessionDetails = a
                .call(DETAILS, json!({ "session_id": listed.id.as_uuid() }))
                .await
                .unwrap();
            assert_eq!(&details.session, listed);
            let sum: Volume = details.exercises.iter().map(|e| e.volume).sum();
            assert_eq!(sum, listed.volume);
        }
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_lifts_never_make_or_break_a_record(db: PgPool) {
        let api = TestApi::new(db).await;
        let (mut a, mut b) = api.users_a_and_b().await;
        let (_, a_version) = db_testing::program(&api.db, a.id).await;
        let (_, b_version) = db_testing::program(&api.db, b.id).await;
        // B lifts far more, earlier.
        let b_session = seed(
            &api.db,
            b.id,
            b_version,
            0,
            &[("back-squat", Some(250.0), 5, false)],
            Some(600),
        )
        .await;
        for (start, weight) in [(1_000, 100.0), (2_000, 101.0)] {
            seed(
                &api.db,
                a.id,
                a_version,
                start,
                &[("back-squat", Some(weight), 5, false)],
                Some(600),
            )
            .await;
        }
        let a_page: HistoryPage = a.call(PAGE, json!({})).await.unwrap();
        // A's second session is a record against A's first only.
        let flags: Vec<bool> = a_page.sessions.iter().map(|s| s.set_pr).collect();
        assert_eq!(flags, [true, false]);
        let b_page: HistoryPage = b.call(PAGE, json!({})).await.unwrap();
        assert_eq!(b_page.sessions.len(), 1);
        assert!(!b_page.sessions[0].set_pr);
        // A's sets query never returns B's sets, even given B's session id.
        let leaked = sets::list_for_sessions(&api.db, a.id, &[b_session])
            .await
            .unwrap();
        assert!(leaked.is_empty());
        assert_eq!(
            sets::list_for_sessions(&api.db, b.id, &[b_session])
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn sessions_keep_the_day_name_of_their_own_program_version(db: PgPool) {
        use crate::server::db::programs;

        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let document = |day: &str| json!({ "schema_version": 1, "name": "P", "days": [{ "id": "a", "name": day }] });
        let (_, program, v1) = programs::create(
            &api.db,
            a.id,
            db_testing::creation(),
            "P",
            &document("Heavy day"),
            db_testing::unlimited,
        )
        .await
        .unwrap();
        let old = seed(&api.db, a.id, v1.id, 0, &[], Some(600)).await;
        let (_, v2) = programs::add_version(&api.db, a.id, program.id, &document("Light day"))
            .await
            .unwrap();
        let new = seed(&api.db, a.id, v2.id, 1_000, &[], Some(600)).await;

        let page: HistoryPage = a.call(PAGE, json!({})).await.unwrap();
        let names: Vec<Option<&str>> = page
            .sessions
            .iter()
            .map(|s| s.day_name.as_deref())
            .collect();
        assert_eq!(names, [Some("Light day"), Some("Heavy day")]);
        for (session, name) in [(old, "Heavy day"), (new, "Light day")] {
            let details: SessionDetails = a
                .call(DETAILS, json!({ "session_id": session.as_uuid() }))
                .await
                .unwrap();
            assert_eq!(details.session.day_name.as_deref(), Some(name));
        }
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn the_series_carries_each_sessions_volume(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let (_, version) = db_testing::program(&api.db, a.id).await;
        seed(
            &api.db,
            a.id,
            version,
            0,
            &[
                ("back-squat", Some(60.0), 5, true),
                ("back-squat", Some(100.0), 5, false),
                ("back-squat", Some(100.0), 4, false),
                ("bench", Some(80.0), 5, false),
            ],
            Some(600),
        )
        .await;
        let series: ExerciseSeries = a
            .call(SERIES, json!({ "exercise_id": "back-squat" }))
            .await
            .unwrap();
        assert_eq!(series.points.len(), 1);
        // 100 x 5 + 100 x 4; the warm-up and the bench add nothing.
        assert_eq!(series.points[0].volume, Volume::of(kg(100.0), Reps::new(9)));
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn sessions_started_and_finished_together_page_stably(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let (_, version) = db_testing::program(&api.db, a.id).await;
        // An earlier session to beat, then two sessions with the same start and the same finish
        // (one in progress at a time: each ends before the next starts, at the same instants).
        let base = seed(
            &api.db,
            a.id,
            version,
            0,
            &[("back-squat", Some(100.0), 5, false)],
            Some(600),
        )
        .await;
        let mut twins = Vec::new();
        for weight in [105.0, 110.0] {
            twins.push(
                seed(
                    &api.db,
                    a.id,
                    version,
                    1_000,
                    &[("back-squat", Some(weight), 5, false)],
                    Some(600),
                )
                .await,
            );
        }
        // History order: by finish then id, both descending; the twins tie on the finish.
        let mut by_id = twins.clone();
        by_id.sort_by_key(|id| std::cmp::Reverse(id.as_uuid()));
        let expected: Vec<Uuid> = by_id.iter().chain([&base]).map(|id| id.as_uuid()).collect();

        let mut flags_by_size = Vec::new();
        for limit in [1, 2, 3] {
            let mut seen = Vec::new();
            let mut cursor: Option<HistoryCursor> = None;
            loop {
                let page: HistoryPage = a
                    .call(PAGE, json!({ "cursor": cursor, "limit": limit }))
                    .await
                    .unwrap();
                seen.extend(page.sessions.iter().map(|s| (s.id.as_uuid(), s.set_pr)));
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            let ids: Vec<Uuid> = seen.iter().map(|(id, _)| *id).collect();
            // No session skipped or repeated across pages, always in the same order.
            assert_eq!(ids, expected, "limit {limit}");
            flags_by_size.push(seen);
        }
        // The PR flags do not depend on where the page breaks fall.
        assert!(
            flags_by_size.windows(2).all(|pair| pair[0] == pair[1]),
            "{flags_by_size:?}"
        );
        // Both twins lift more than every session before them, whichever way the tie breaks.
        let flags: Vec<bool> = flags_by_size[0].iter().map(|(_, pr)| *pr).collect();
        assert_eq!(
            flags.last(),
            Some(&false),
            "the first session has nothing to beat"
        );
        assert!(flags[..2].iter().all(|pr| *pr), "{flags:?}");
    }
}
