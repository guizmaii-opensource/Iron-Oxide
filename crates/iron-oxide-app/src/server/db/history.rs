//! Read-only queries behind the history screens (#20, #33): the list of ended sessions, one
//! session with its program, the sets of one exercise across sessions (for the charts) and the
//! exercises a user has logged.
//!
//! "History" means the sessions that have ended: completed, skipped or abandoned. A session still
//! in progress appears in none of these queries except [`entry`] (a user may open it).

use sqlx::{
    PgPool,
    types::{Uuid, time::OffsetDateTime},
};

use super::{
    error::{RepoError, narrow},
    ids::{ProgramId, ProgramVersionId, SessionId, UserId},
    sessions::SessionStatus,
};

/// The most sessions one [`page`] returns.
pub const MAX_PAGE: u32 = 100;

/// One session as the history shows it: the session, its program's name and version, and how
/// many working sets were logged in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: SessionId,
    pub program_id: ProgramId,
    /// The program's current name (a rename shows on past sessions too).
    pub program_name: String,
    pub program_version_id: ProgramVersionId,
    /// The version number (1, 2, ...) the session was run from.
    pub program_version: u32,
    pub day_id: String,
    /// The day's name in the session's own program version (as it was when the session ran),
    /// `None` if that version has no such day.
    pub day_name: Option<String>,
    pub status: SessionStatus,
    pub started_at: OffsetDateTime,
    /// Set exactly when the status is not in progress.
    pub finished_at: Option<OffsetDateTime>,
    /// Sets that are not warm-ups.
    pub working_sets: u32,
}

/// Where a [`page`] continues: strictly after this session, in history order (most recently
/// finished first, then by descending id).
///
/// `finished_at` keeps Postgres' full microsecond precision: a cursor rounded to milliseconds would
/// skip the sessions between the rounded and the real value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCursor {
    pub finished_at: OffsetDateTime,
    pub id: SessionId,
}

impl HistoryEntry {
    /// The cursor for the page after this session, `None` while it is in progress (it is not
    /// part of the history).
    pub fn cursor(&self) -> Option<HistoryCursor> {
        self.finished_at.map(|finished_at| HistoryCursor {
            finished_at,
            id: self.id,
        })
    }
}

/// A row of the queries returning [`HistoryEntry`].
struct EntryRow {
    id: Uuid,
    program_id: Uuid,
    program_name: String,
    program_version_id: Uuid,
    program_version: i32,
    day_id: String,
    day_name: Option<String>,
    status: String,
    started_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    working_sets: i64,
}

impl TryFrom<EntryRow> for HistoryEntry {
    type Error = RepoError;

    fn try_from(row: EntryRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: SessionId::from_uuid(row.id),
            program_id: ProgramId::from_uuid(row.program_id),
            program_name: row.program_name,
            program_version_id: ProgramVersionId::from_uuid(row.program_version_id),
            program_version: narrow(row.program_version.into(), "program_versions.version")?,
            day_id: row.day_id,
            day_name: row.day_name,
            status: SessionStatus::parse(&row.status)?,
            started_at: row.started_at,
            finished_at: row.finished_at,
            working_sets: narrow(row.working_sets, "workout_sets count")?,
        })
    }
}

/// One page of [`page`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub entries: Vec<HistoryEntry>,
    /// Whether more sessions follow the last entry (continue from its cursor).
    pub more: bool,
}

/// A page of the user's ended sessions, most recently finished first (ties broken by descending
/// id). `after` continues from the last entry of the previous page ([`HistoryEntry::cursor`]).
/// `limit` is clamped to `1..=MAX_PAGE`.
pub async fn page(
    pool: &PgPool,
    user: UserId,
    after: Option<HistoryCursor>,
    limit: u32,
) -> Result<Page, RepoError> {
    let limit = limit.clamp(1, MAX_PAGE);
    // One row more than the page, to know whether another page follows.
    let probe = i64::from(limit) + 1;
    let mut entries = sqlx::query_as!(
        EntryRow,
        r#"SELECT s.id, v.program_id, p.name AS program_name, s.program_version_id,
                  v.version AS program_version, s.day_id,
                  (SELECT d ->> 'name' FROM jsonb_array_elements(v.document -> 'days') d
                   WHERE d ->> 'id' = s.day_id LIMIT 1) AS day_name,
                  s.status, s.started_at, s.finished_at,
                  (SELECT count(*) FROM workout_sets st
                   WHERE st.user_id = s.user_id AND st.session_id = s.id AND NOT st.warmup)
                      AS "working_sets!"
           FROM workout_sessions s
           JOIN program_versions v ON v.id = s.program_version_id AND v.user_id = s.user_id
           JOIN programs p ON p.id = v.program_id AND p.user_id = s.user_id
           WHERE s.user_id = $1 AND s.status <> 'in_progress'
             AND ($2::timestamptz IS NULL OR (s.finished_at, s.id) < ($2, $3::uuid))
           ORDER BY s.finished_at DESC, s.id DESC
           LIMIT $4"#,
        user.as_uuid(),
        after.map(|cursor| cursor.finished_at),
        after.map(|cursor| cursor.id.as_uuid()),
        probe,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(HistoryEntry::try_from)
    .collect::<Result<Vec<_>, _>>()?;
    let limit = usize::try_from(limit).map_err(|_| RepoError::Corrupt("page limit"))?;
    let more = entries.len() > limit;
    entries.truncate(limit);
    Ok(Page { entries, more })
}

/// One of the user's sessions (ended or in progress), as the history shows it.
///
/// # Errors
/// [`RepoError::NotFound`] when the user has no session with that id.
pub async fn entry(pool: &PgPool, user: UserId, id: SessionId) -> Result<HistoryEntry, RepoError> {
    let row = sqlx::query_as!(
        EntryRow,
        r#"SELECT s.id, v.program_id, p.name AS program_name, s.program_version_id,
                  v.version AS program_version, s.day_id,
                  (SELECT d ->> 'name' FROM jsonb_array_elements(v.document -> 'days') d
                   WHERE d ->> 'id' = s.day_id LIMIT 1) AS day_name,
                  s.status, s.started_at, s.finished_at,
                  (SELECT count(*) FROM workout_sets st
                   WHERE st.user_id = s.user_id AND st.session_id = s.id AND NOT st.warmup)
                      AS "working_sets!"
           FROM workout_sessions s
           JOIN program_versions v ON v.id = s.program_version_id AND v.user_id = s.user_id
           JOIN programs p ON p.id = v.program_id AND p.user_id = s.user_id
           WHERE s.user_id = $1 AND s.id = $2"#,
        user.as_uuid(),
        id.as_uuid(),
    )
    .fetch_optional(pool)
    .await?
    .ok_or(RepoError::NotFound)?;
    HistoryEntry::try_from(row)
}

/// One weighted set of an exercise, with the session it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExerciseSet {
    pub session_id: SessionId,
    /// When the session started: the x axis of the charts.
    pub session_started_at: OffsetDateTime,
    pub reps: u16,
    /// In nanograms (the domain `Weight`).
    pub weight_ng: u64,
    pub warmup: bool,
}

/// The user's weighted sets of one exercise in ended sessions, grouped by session: sessions in
/// start order (then by id), sets in the order they were completed.
///
/// Sets without a weight (body-weight work) and timed sets (holds) are left out: the charts plot
/// weights lifted, as the domain statistics count them. Sets of every
/// ended session count (a set logged before a session was abandoned was still lifted).
pub async fn exercise_sets(
    pool: &PgPool,
    user: UserId,
    exercise_id: &str,
) -> Result<Vec<ExerciseSet>, RepoError> {
    sqlx::query!(
        r#"SELECT st.session_id, s.started_at, st.reps, st.weight_ng AS "weight_ng!", st.warmup
           FROM workout_sets st
           JOIN workout_sessions s ON s.id = st.session_id AND s.user_id = st.user_id
           WHERE st.user_id = $1 AND st.exercise_id = $2 AND st.weight_ng IS NOT NULL
             AND st.duration_s IS NULL AND s.status <> 'in_progress'
           ORDER BY s.started_at, s.id, st.completed_at, st.id"#,
        user.as_uuid(),
        exercise_id,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| {
        Ok(ExerciseSet {
            session_id: SessionId::from_uuid(row.session_id),
            session_started_at: row.started_at,
            reps: narrow(row.reps.into(), "workout_sets.reps")?,
            weight_ng: narrow(row.weight_ng, "workout_sets.weight_ng")?,
            warmup: row.warmup,
        })
    })
    .collect()
}

/// An exercise the user has logged sets of, in ended sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedExercise {
    /// The exercise slug (the domain `ExerciseId`).
    pub exercise_id: String,
    /// In how many ended sessions it was logged.
    pub sessions: u32,
    /// The start of the most recent of those sessions.
    pub last_session_at: OffsetDateTime,
}

/// The exercises the user has logged in ended sessions, most recently trained first (then by id).
/// Body-weight and warm-up-only exercises are included.
pub async fn logged_exercises(
    pool: &PgPool,
    user: UserId,
) -> Result<Vec<LoggedExercise>, RepoError> {
    sqlx::query!(
        r#"SELECT st.exercise_id, count(DISTINCT st.session_id) AS "sessions!",
                  max(s.started_at) AS "last_session_at!"
           FROM workout_sets st
           JOIN workout_sessions s ON s.id = st.session_id AND s.user_id = st.user_id
           WHERE st.user_id = $1 AND s.status <> 'in_progress'
           GROUP BY st.exercise_id
           ORDER BY max(s.started_at) DESC, st.exercise_id"#,
        user.as_uuid(),
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| {
        Ok(LoggedExercise {
            exercise_id: row.exercise_id,
            sessions: narrow(row.sessions, "workout_sets count")?,
            last_session_at: row.last_session_at,
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::{
        MIGRATOR,
        sessions::{self, NewSession, SessionOutcome},
        sets::{self, LoggedSet},
        testing::{self, at, new_session, new_set, random_uuid},
    };
    use time::Duration;

    /// Starts a session of `version` at `start` and returns its id.
    async fn start(
        pool: &PgPool,
        user: UserId,
        version: ProgramVersionId,
        start: OffsetDateTime,
    ) -> SessionId {
        let new = NewSession {
            started_at: start,
            ..new_session(version)
        };
        sessions::start(pool, user, &new).await.unwrap();
        new.id
    }

    async fn finish(pool: &PgPool, user: UserId, id: SessionId, at: OffsetDateTime) {
        sessions::finish(pool, user, id, SessionOutcome::Completed, at)
            .await
            .unwrap();
    }

    async fn log(pool: &PgPool, user: UserId, set: LoggedSet) {
        sets::upsert_idempotent(pool, user, &set).await.unwrap();
    }

    /// Every page of the history, `limit` at a time.
    async fn all_pages(pool: &PgPool, user: UserId, limit: u32) -> Vec<SessionId> {
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let page = page(pool, user, after, limit).await.unwrap();
            seen.extend(page.entries.iter().map(|entry| entry.id));
            if !page.more {
                break;
            }
            after = page.entries.last().and_then(HistoryEntry::cursor);
        }
        seen
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn page_lists_ended_sessions_by_finish_time_with_their_program(pool: PgPool) {
        let user = testing::user(&pool).await;
        let (program, version) = testing::program(&pool, user).await;
        // Started in one order, finished in another (each ended before the next starts: one
        // session in progress at a time).
        let early = start(&pool, user, version, at(0)).await;
        sessions::finish(&pool, user, early, SessionOutcome::Abandoned, at(200))
            .await
            .unwrap();
        let late = start(&pool, user, version, at(10)).await;
        finish(&pool, user, late, at(100)).await;
        let running = start(&pool, user, version, at(20)).await;
        log(
            &pool,
            user,
            LoggedSet {
                warmup: true,
                ..new_set(running)
            },
        )
        .await;

        let entries = page(&pool, user, None, 10).await.unwrap().entries;
        assert_eq!(
            entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![early, late]
        );
        assert_eq!(
            entries[0],
            HistoryEntry {
                id: early,
                program_id: program,
                program_name: "Program".to_owned(),
                program_version_id: version,
                program_version: 1,
                day_id: "a".to_owned(),
                day_name: None,
                status: SessionStatus::Abandoned,
                started_at: at(0),
                finished_at: Some(at(200)),
                working_sets: 0,
            }
        );
        // The in-progress session joins the history once it ends.
        finish(&pool, user, running, at(300)).await;
        assert_eq!(
            page(&pool, user, None, 10).await.unwrap().entries[0].id,
            running
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn entry_counts_working_sets_and_includes_sessions_in_progress(pool: PgPool) {
        let user = testing::user(&pool).await;
        let session = testing::session(&pool, user).await;
        log(&pool, user, new_set(session)).await;
        log(
            &pool,
            user,
            LoggedSet {
                warmup: true,
                ..new_set(session)
            },
        )
        .await;
        let found = entry(&pool, user, session).await.unwrap();
        assert_eq!(found.status, SessionStatus::InProgress);
        assert_eq!(found.finished_at, None);
        assert_eq!(found.cursor(), None);
        assert_eq!(found.working_sets, 1);
        let missing = entry(&pool, user, SessionId::from_uuid(random_uuid())).await;
        assert!(matches!(missing, Err(RepoError::NotFound)), "{missing:?}");
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn pages_split_ties_and_sub_millisecond_times_without_gaps_or_repeats(pool: PgPool) {
        let user = testing::user(&pool).await;
        let (_, version) = testing::program(&pool, user).await;
        // Three sessions finished at the same instant, and three finished within the same
        // millisecond, one microsecond apart.
        let mut expected = Vec::new();
        for offset in [0, 0, 0, 1, 2, 3] {
            let id = start(&pool, user, version, at(0)).await;
            finish(&pool, user, id, at(60) + Duration::microseconds(offset)).await;
            expected.push((offset, id));
        }
        expected.sort_by(|x, y| y.cmp(x));
        let expected: Vec<SessionId> = expected.into_iter().map(|(_, id)| id).collect();
        for limit in 1..=7 {
            assert_eq!(
                all_pages(&pool, user, limit).await,
                expected,
                "limit {limit}"
            );
        }
        // The limit is clamped to 1..=MAX_PAGE.
        let first = page(&pool, user, None, 0).await.unwrap();
        assert_eq!((first.entries.len(), first.more), (1, true));
        let all = page(&pool, user, None, u32::MAX).await.unwrap();
        assert_eq!((all.entries.len(), all.more), (6, false));
        // Exactly one full page: nothing more.
        let exact = page(&pool, user, None, 6).await.unwrap();
        assert_eq!((exact.entries.len(), exact.more), (6, false));
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn a_full_page_of_the_largest_size_still_reports_more(pool: PgPool) {
        let user = testing::user(&pool).await;
        let (_, version) = testing::program(&pool, user).await;
        for n in 0..=i64::from(MAX_PAGE) {
            let id = start(&pool, user, version, at(n)).await;
            finish(&pool, user, id, at(n + 1)).await;
        }
        let first = page(&pool, user, None, MAX_PAGE).await.unwrap();
        assert_eq!(first.entries.len(), 100);
        assert!(first.more);
        let rest = page(&pool, user, first.entries[99].cursor(), MAX_PAGE)
            .await
            .unwrap();
        assert_eq!((rest.entries.len(), rest.more), (1, false));
        assert_eq!(rest.entries[0].started_at, at(0));
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn exercise_sets_leave_out_weighted_holds(pool: PgPool) {
        let user = testing::user(&pool).await;
        let (_, version) = testing::program(&pool, user).await;
        let session = start(&pool, user, version, at(0)).await;
        // A weighted plank: a weight and a duration. Not a lift for the charts or the volume.
        let hold = LoggedSet {
            exercise_id: "plank".to_owned(),
            reps: 1,
            weight_ng: Some(20_000_000_000_000),
            duration_s: Some(60),
            completed_at: at(10),
            ..new_set(session)
        };
        let lifted = LoggedSet {
            exercise_id: "plank".to_owned(),
            reps: 8,
            weight_ng: Some(10_000_000_000_000),
            completed_at: at(20),
            ..new_set(session)
        };
        for set in [hold, lifted] {
            log(&pool, user, set).await;
        }
        finish(&pool, user, session, at(100)).await;
        let found = exercise_sets(&pool, user, "plank").await.unwrap();
        assert_eq!(
            found,
            vec![ExerciseSet {
                session_id: session,
                session_started_at: at(0),
                reps: 8,
                weight_ng: 10_000_000_000_000,
                warmup: false,
            }]
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn exercise_sets_come_from_ended_sessions_and_have_a_weight(pool: PgPool) {
        let user = testing::user(&pool).await;
        let (_, version) = testing::program(&pool, user).await;
        // Each session ends before the next starts: one session in progress at a time.
        let second = start(&pool, user, version, at(1_000)).await;
        let heavy = LoggedSet {
            reps: 3,
            weight_ng: Some(120_000_000_000_000),
            completed_at: at(1_100),
            ..new_set(second)
        };
        let light = LoggedSet {
            warmup: true,
            weight_ng: Some(60_000_000_000_000),
            completed_at: at(1_050),
            ..new_set(second)
        };
        for set in [heavy.clone(), light.clone()] {
            log(&pool, user, set).await;
        }
        finish(&pool, user, second, at(3_000)).await;
        let first = start(&pool, user, version, at(0)).await;
        for set in [
            new_set(first),
            LoggedSet {
                weight_ng: None,
                ..new_set(first)
            },
            LoggedSet {
                exercise_id: "bench".to_owned(),
                ..new_set(first)
            },
        ] {
            log(&pool, user, set).await;
        }
        finish(&pool, user, first, at(3_000)).await;
        let running = start(&pool, user, version, at(2_000)).await;
        log(&pool, user, new_set(running)).await;
        let found = exercise_sets(&pool, user, "back-squat").await.unwrap();
        assert_eq!(
            found,
            vec![
                ExerciseSet {
                    session_id: first,
                    session_started_at: at(0),
                    reps: 5,
                    weight_ng: 100_000_000_000_000,
                    warmup: false,
                },
                ExerciseSet {
                    session_id: second,
                    session_started_at: at(1_000),
                    reps: 5,
                    weight_ng: 60_000_000_000_000,
                    warmup: true,
                },
                ExerciseSet {
                    session_id: second,
                    session_started_at: at(1_000),
                    reps: 3,
                    weight_ng: 120_000_000_000_000,
                    warmup: false,
                },
            ]
        );
        assert!(
            exercise_sets(&pool, user, "deadlift")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn logged_exercises_are_most_recent_first(pool: PgPool) {
        let user = testing::user(&pool).await;
        let (_, version) = testing::program(&pool, user).await;
        assert!(logged_exercises(&pool, user).await.unwrap().is_empty());
        // Each session ends before the next starts: one session in progress at a time.
        for (start_at, exercises, ended) in [
            (0, &["back-squat", "back-squat", "bench"][..], true),
            (1_000, &["back-squat", "pull-up"][..], true),
            (2_000, &["deadlift"][..], false),
        ] {
            let session = start(&pool, user, version, at(start_at)).await;
            for exercise in exercises {
                log(
                    &pool,
                    user,
                    LoggedSet {
                        exercise_id: (*exercise).to_owned(),
                        ..new_set(session)
                    },
                )
                .await;
            }
            if ended {
                finish(&pool, user, session, at(3_000)).await;
            }
        }
        let found = logged_exercises(&pool, user).await.unwrap();
        let summary: Vec<(&str, u32, OffsetDateTime)> = found
            .iter()
            .map(|e| (e.exercise_id.as_str(), e.sessions, e.last_session_at))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("back-squat", 2, at(1_000)),
                ("pull-up", 1, at(1_000)),
                ("bench", 1, at(0)),
            ]
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_history_is_invisible(pool: PgPool) {
        let (a, b) = testing::users_a_and_b(&pool).await;
        let (_, version) = testing::program(&pool, a).await;
        let session = start(&pool, a, version, at(0)).await;
        log(&pool, a, new_set(session)).await;
        finish(&pool, a, session, at(60)).await;

        assert!(page(&pool, b, None, 100).await.unwrap().entries.is_empty());
        let result = entry(&pool, b, session).await;
        assert!(matches!(result, Err(RepoError::NotFound)), "{result:?}");
        assert!(
            exercise_sets(&pool, b, "back-squat")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(logged_exercises(&pool, b).await.unwrap().is_empty());
        // A's cursor gives B nothing either.
        let cursor = entry(&pool, a, session).await.unwrap().cursor();
        let later = HistoryCursor {
            finished_at: at(1_000_000),
            ..cursor.unwrap()
        };
        assert!(
            page(&pool, b, Some(later), 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
    }
}
