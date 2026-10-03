//! History (#20): the logic behind `crate::api::history`.

use iron_oxide_domain::{
    DayId, E1rmFormula, ExerciseId, LoggedSet, PerformedSet, Reps, Seconds, SeriesPoint, SessionId,
    Weight, exercise_series, session_volume, time::Timestamp, top_set,
};
use sqlx::{PgPool, types::time::OffsetDateTime};

use super::{ApiError, sessions::status, timestamp};
use crate::api::history::{
    DEFAULT_PAGE_SIZE, ExerciseLog, ExerciseSeries, HistoryCursor, HistoryPage, LoggedExercise,
    MAX_PAGE_SIZE, SeriesKey, SessionDetails, SessionSummary,
};
use crate::server::db::{history as repo, ids::UserId, sets};

/// The formula of every e1RM in the history.
const FORMULA: E1rmFormula = E1rmFormula::Epley;
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
    let sessions = entries.into_iter().map(summary).collect::<Result<_, _>>()?;
    Ok(HistoryPage { sessions, next })
}

pub async fn details(
    pool: &PgPool,
    owner: UserId,
    session_id: SessionId,
) -> Result<SessionDetails, ApiError> {
    let id = session_id.into();
    let entry = repo::entry(pool, owner, id).await?;
    let sets = sets::list_for_session(pool, owner, id).await?;
    Ok(SessionDetails {
        session: summary(entry)?,
        exercises: group_by_exercise(sets)?,
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

fn summary(entry: repo::HistoryEntry) -> Result<SessionSummary, ApiError> {
    Ok(SessionSummary {
        id: entry.id.into(),
        program_id: entry.program_id.into(),
        program_name: entry.program_name,
        program_version_id: entry.program_version_id.into(),
        program_version: entry.program_version,
        day_id: DayId::new(entry.day_id).map_err(ApiError::internal)?,
        status: status(entry.status),
        started_at: timestamp(entry.started_at)?,
        finished_at: entry.finished_at.map(timestamp).transpose()?,
        working_sets: entry.working_sets,
    })
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

/// The weighted sets, as statistics inputs (body-weight sets have no weight to chart).
fn performed(sets: &[LoggedSet<Timestamp>]) -> impl Iterator<Item = PerformedSet> + '_ {
    sets.iter().filter_map(|set| {
        set.weight.map(|weight| PerformedSet {
            weight,
            reps: set.reps,
            warmup: set.warm_up,
        })
    })
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
pub fn series_points(
    sets: Vec<repo::ExerciseSet>,
) -> Result<Vec<SeriesPoint<SeriesKey>>, ApiError> {
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
    Ok(exercise_series(sessions, FORMULA))
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
        iron_oxide_domain::E1rmFormula::Epley.estimate(weight, Reps::new(reps))
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
            SeriesPoint {
                key: SeriesKey {
                    started_at: Timestamp::from_epoch_millis(0),
                    session_id: SessionId::from_uuid(Uuid::from_u128(1)),
                },
                top_set: Lift {
                    weight: kg(105.0),
                    reps: Reps::new(3),
                },
                best_e1rm: epley(kg(100.0), 5).max(epley(kg(105.0), 3)),
            },
            SeriesPoint {
                key: SeriesKey {
                    started_at: Timestamp::from_epoch_millis(1_000),
                    session_id: SessionId::from_uuid(Uuid::from_u128(3)),
                },
                top_set: Lift {
                    weight: kg(107.5),
                    reps: Reps::new(1),
                },
                best_e1rm: epley(kg(90.0), 12).max(Some(kg(107.5))),
            },
        ];
        assert_eq!(points, expected);
        assert!(series_points(Vec::new()).unwrap().is_empty());
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
                status: iron_oxide_domain::SessionStatus::Completed,
                started_at: timestamp(db_testing::at(4 * 3_600)).unwrap(),
                finished_at: Some(timestamp(db_testing::at(4 * 3_600 + 600)).unwrap()),
                working_sets: 1,
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
}
