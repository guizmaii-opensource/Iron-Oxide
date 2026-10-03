//! Logged sets (`workout_sets`). Ids are generated on the client, so a retried save is recognised
//! as the same set instead of creating a duplicate (#18, #25).
//!
//! A set can only belong to one of its owner's sessions: the composite foreign key
//! `(session_id, user_id)` makes pointing at another user's session impossible in the database.

use sqlx::{
    PgPool,
    types::{JsonValue, Uuid, time::OffsetDateTime},
};

use super::{
    error::{Change, RepoError, narrow},
    ids::{ProgramId, SessionId, SetId, UserId},
    sessions::Cursor,
};

/// A logged set (the domain `LoggedSet`), as saved and as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedSet {
    /// Generated on the client: the idempotency key.
    pub id: SetId,
    pub session_id: SessionId,
    /// The exercise slug (the domain `ExerciseId`).
    pub exercise_id: String,
    /// Position among the exercise's sets of the same kind in the session, from 0.
    pub set_index: u16,
    pub reps: u16,
    /// Load in nanograms (the domain `Weight`), `None` for body-weight work.
    pub weight_ng: Option<u64>,
    /// Time under tension in seconds (the domain `Seconds`), `None` when not timed.
    pub duration_s: Option<u32>,
    pub warmup: bool,
    pub completed_at: OffsetDateTime,
    /// What the app prescribed for the set when it was logged (#60), or `None` when no target was
    /// recorded (sets logged before #60, extras).
    pub target: Option<Target>,
}

/// A set's prescribed target (the domain `SetTarget`), as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The target load in nanograms (the domain `Weight`), `None` for a body-weight target.
    pub weight_ng: Option<u64>,
    /// The domain `SetGoal` JSON (an object). Validate it with the domain before saving.
    pub goal: JsonValue,
}

/// A `workout_sets` row.
struct SetRow {
    id: Uuid,
    session_id: Uuid,
    exercise_id: String,
    set_index: i32,
    reps: i32,
    weight_ng: Option<i64>,
    duration_s: Option<i64>,
    warmup: bool,
    completed_at: OffsetDateTime,
    target_weight_ng: Option<i64>,
    target_goal: Option<JsonValue>,
}

impl TryFrom<SetRow> for LoggedSet {
    type Error = RepoError;

    fn try_from(row: SetRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: SetId::from_uuid(row.id),
            session_id: SessionId::from_uuid(row.session_id),
            exercise_id: row.exercise_id,
            set_index: narrow(row.set_index.into(), "workout_sets.set_index")?,
            reps: narrow(row.reps.into(), "workout_sets.reps")?,
            weight_ng: row
                .weight_ng
                .map(|weight| narrow(weight, "workout_sets.weight_ng"))
                .transpose()?,
            duration_s: row
                .duration_s
                .map(|duration| narrow(duration, "workout_sets.duration_s"))
                .transpose()?,
            warmup: row.warmup,
            completed_at: row.completed_at,
            target: row
                .target_goal
                .map(|goal| {
                    Ok::<_, RepoError>(Target {
                        weight_ng: row
                            .target_weight_ng
                            .map(|weight| narrow(weight, "workout_sets.target_weight_ng"))
                            .transpose()?,
                        goal,
                    })
                })
                .transpose()?,
        })
    }
}

/// The target columns of a set: `(target_weight_ng, target_goal)`, both `None` without a target.
pub(super) fn target_columns(
    target: Option<&Target>,
) -> Result<(Option<i64>, Option<JsonValue>), RepoError> {
    let Some(target) = target else {
        return Ok((None, None));
    };
    let weight = target
        .weight_ng
        .map(|weight| {
            i64::try_from(weight).map_err(|_| RepoError::Invalid {
                constraint: Some("workout_sets_target_weight_ng_check".to_owned()),
            })
        })
        .transpose()?;
    Ok((weight, Some(target.goal.clone())))
}

/// Saves a set in one of the user's in-progress sessions. Idempotent on the set id.
///
/// Returns [`Change::Applied`] when the set was saved and [`Change::Unchanged`] when the user
/// already has this exact set (even if the session has ended since, so a queued retry succeeds).
/// Ids are unique per user: another user's set with the same id is a different row, never read,
/// compared or modified.
///
/// # Errors
/// - [`RepoError::Conflict`] when the user already has a different set with this id.
/// - [`RepoError::NotFound`] when the session is not one of the user's.
/// - [`RepoError::SessionEnded`] when the set is new and the session has ended.
/// - [`RepoError::Invalid`] for an exercise id that is not a slug or a weight above 2000 kg.
/// - [`RepoError::Transient`] in the unlikely case that the session kept changing under two
///   attempts: nothing was saved, and a retry is expected to succeed.
pub async fn upsert_idempotent(
    pool: &PgPool,
    user: UserId,
    set: &LoggedSet,
) -> Result<Change, RepoError> {
    let weight_ng = set
        .weight_ng
        .map(|weight| {
            i64::try_from(weight).map_err(|_| RepoError::Invalid {
                constraint: Some("workout_sets_weight_ng_check".to_owned()),
            })
        })
        .transpose()?;
    let duration_s = set.duration_s.map(i64::from);
    let target = target_columns(set.target.as_ref())?;
    // A second attempt covers a save that races its session's start: the insert's snapshot did
    // not see the session yet, the status read afterwards does. The next insert sees it too.
    for _ in 0..2 {
        match save_once(pool, user, set, weight_ng, duration_s, &target).await? {
            Attempt::Done(change) => return Ok(change),
            Attempt::SessionAppeared => {}
        }
    }
    Err(RepoError::Transient)
}

/// What one attempt of [`upsert_idempotent`] found.
enum Attempt {
    Done(Change),
    /// Nothing was inserted, yet the session is in progress now: it was created (committed) during
    /// the attempt.
    SessionAppeared,
}

async fn save_once(
    pool: &PgPool,
    user: UserId,
    set: &LoggedSet,
    weight_ng: Option<i64>,
    duration_s: Option<i64>,
    (target_weight_ng, target_goal): &(Option<i64>, Option<JsonValue>),
) -> Result<Attempt, RepoError> {
    // Inserts only into a session of this user that is still in progress. Ids are unique per user
    // (primary key `(user_id, id)`), so only this user's own set with this id inserts nothing.
    let inserted = sqlx::query!(
        "INSERT INTO workout_sets
             (id, session_id, user_id, exercise_id, set_index, reps, weight_ng, duration_s, warmup,
              completed_at, target_weight_ng, target_goal)
         SELECT $1, s.id, s.user_id, $4, $5, $6, $7, $8, $9, $10, $11, $12
         FROM workout_sessions s
         WHERE s.id = $2 AND s.user_id = $3 AND s.status = 'in_progress'
         ON CONFLICT (user_id, id) DO NOTHING",
        set.id.as_uuid(),
        set.session_id.as_uuid(),
        user.as_uuid(),
        set.exercise_id,
        i32::from(set.set_index),
        i32::from(set.reps),
        weight_ng,
        duration_s,
        set.warmup,
        set.completed_at,
        *target_weight_ng,
        target_goal.as_ref(),
    )
    .execute(pool)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(Attempt::Done(Change::Applied));
    }

    // Nothing inserted: this user already has a set with this id, or the session is not an
    // in-progress one of this user.
    // Only this user's own set is compared.
    let same = sqlx::query_scalar!(
        r#"SELECT (session_id = $3 AND exercise_id = $4 AND set_index = $5 AND reps = $6
                   AND weight_ng IS NOT DISTINCT FROM $7 AND duration_s IS NOT DISTINCT FROM $8
                   AND warmup = $9 AND completed_at = $10
                   AND target_weight_ng IS NOT DISTINCT FROM $11
                   AND target_goal IS NOT DISTINCT FROM $12) AS "same!"
           FROM workout_sets WHERE id = $1 AND user_id = $2"#,
        set.id.as_uuid(),
        user.as_uuid(),
        set.session_id.as_uuid(),
        set.exercise_id,
        i32::from(set.set_index),
        i32::from(set.reps),
        weight_ng,
        duration_s,
        set.warmup,
        set.completed_at,
        *target_weight_ng,
        target_goal.as_ref(),
    )
    .fetch_optional(pool)
    .await?;
    match same {
        Some(true) => return Ok(Attempt::Done(Change::Unchanged)),
        Some(false) => return Err(RepoError::Conflict),
        None => {}
    }

    // The user has no set with this id, so the session is why nothing was inserted.
    let status = sqlx::query_scalar!(
        "SELECT status FROM workout_sessions WHERE id = $1 AND user_id = $2",
        set.session_id.as_uuid(),
        user.as_uuid(),
    )
    .fetch_optional(pool)
    .await?;
    match status.as_deref() {
        None => Err(RepoError::NotFound),
        Some("in_progress") => Ok(Attempt::SessionAppeared),
        Some(_) => Err(RepoError::SessionEnded),
    }
}

/// The sets of one of the user's sessions, in the order they were completed.
///
/// # Errors
/// [`RepoError::NotFound`] when the user has no session with that id.
pub async fn list_for_session(
    pool: &PgPool,
    user: UserId,
    session: SessionId,
) -> Result<Vec<LoggedSet>, RepoError> {
    let mut tx = pool.begin().await?;
    let owned = sqlx::query_scalar!(
        r#"SELECT true AS "owned!" FROM workout_sessions WHERE id = $1 AND user_id = $2"#,
        session.as_uuid(),
        user.as_uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?;
    if owned.is_none() {
        return Err(RepoError::NotFound);
    }
    let sets = sqlx::query_as!(
        SetRow,
        "SELECT id, session_id, exercise_id, set_index, reps, weight_ng, duration_s, warmup,
                completed_at, target_weight_ng, target_goal
         FROM workout_sets WHERE session_id = $1 AND user_id = $2
         ORDER BY completed_at, id",
        session.as_uuid(),
        user.as_uuid(),
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(LoggedSet::try_from)
    .collect::<Result<Vec<_>, RepoError>>()?;
    tx.commit().await?;
    Ok(sets)
}

/// The user's sets of one exercise completed strictly after `after`, in sessions that were
/// completed and run from any version of `program`; oldest first. Warm-up sets are included
/// (`warmup` tells them apart).
///
/// This is the progression input (#57): the history after a training max's `set_at`, for the
/// active program. Served by the `(user_id, exercise_id, completed_at)` index. A program that is
/// not the user's gives no sets, like one that does not exist.
pub async fn completed_for_exercise(
    pool: &PgPool,
    user: UserId,
    program: ProgramId,
    exercise_id: &str,
    after: OffsetDateTime,
) -> Result<Vec<LoggedSet>, RepoError> {
    sqlx::query_as!(
        SetRow,
        "SELECT st.id, st.session_id, st.exercise_id, st.set_index, st.reps, st.weight_ng,
                st.duration_s, st.warmup, st.completed_at, st.target_weight_ng, st.target_goal
         FROM workout_sets st
         JOIN workout_sessions s ON s.id = st.session_id AND s.user_id = st.user_id
         JOIN program_versions v ON v.id = s.program_version_id AND v.user_id = s.user_id
         WHERE st.user_id = $1 AND st.exercise_id = $3 AND st.completed_at > $4
           AND s.status = 'completed' AND v.program_id = $2
         ORDER BY st.completed_at, st.id",
        user.as_uuid(),
        program.as_uuid(),
        exercise_id,
        after,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(LoggedSet::try_from)
    .collect()
}

/// Every set (warm-ups included) of the user's **completed** sessions run from any version of
/// `program`, in session order (start, then id), then in logging order. The progression history
/// of #18: the caller groups it by session and exercise. A program that is not the user's gives
/// no sets.
pub async fn completed_in_program(
    pool: &PgPool,
    user: UserId,
    program: ProgramId,
) -> Result<Vec<LoggedSet>, RepoError> {
    sqlx::query_as!(
        SetRow,
        "SELECT st.id, st.session_id, st.exercise_id, st.set_index, st.reps, st.weight_ng,
                st.duration_s, st.warmup, st.completed_at, st.target_weight_ng, st.target_goal
         FROM workout_sets st
         JOIN workout_sessions s ON s.id = st.session_id AND s.user_id = st.user_id
         JOIN program_versions v ON v.id = s.program_version_id AND v.user_id = s.user_id
         WHERE st.user_id = $1 AND s.status = 'completed' AND v.program_id = $2
         ORDER BY s.started_at, s.id, st.completed_at, st.id",
        user.as_uuid(),
        program.as_uuid(),
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(LoggedSet::try_from)
    .collect()
}

/// The user's sets of `exercises` (warm-ups included) in **completed** sessions of any program
/// that started strictly before `before` (by start, then id); oldest first. The personal-record
/// history of a session's summary (#18), which only depends on what came before it, so a retried
/// summary is the same.
pub async fn completed_for_exercises_before(
    pool: &PgPool,
    user: UserId,
    exercises: &[String],
    before: Cursor,
) -> Result<Vec<LoggedSet>, RepoError> {
    sqlx::query_as!(
        SetRow,
        "SELECT st.id, st.session_id, st.exercise_id, st.set_index, st.reps, st.weight_ng,
                st.duration_s, st.warmup, st.completed_at, st.target_weight_ng, st.target_goal
         FROM workout_sets st
         JOIN workout_sessions s ON s.id = st.session_id AND s.user_id = st.user_id
         WHERE st.user_id = $1 AND st.exercise_id = ANY($2) AND s.status = 'completed'
           AND (s.started_at, s.id) < ($3, $4::uuid)
         ORDER BY s.started_at, s.id, st.completed_at, st.id",
        user.as_uuid(),
        exercises,
        before.started_at,
        before.id.as_uuid(),
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(LoggedSet::try_from)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    use crate::server::db::{
        MIGRATOR, programs,
        sessions::{self, SessionOutcome},
        testing::{self, at, document, new_session, new_set, random_uuid},
    };

    fn assert_err<T: std::fmt::Debug>(result: Result<T, RepoError>, expected: &str) {
        let matches = matches!(
            (&result, expected),
            (Err(RepoError::NotFound), "not found")
                | (Err(RepoError::Conflict), "conflict")
                | (Err(RepoError::SessionEnded), "ended")
                | (Err(RepoError::Invalid { .. }), "invalid")
        );
        assert!(matches, "expected {expected}, got {result:?}");
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn a_set_round_trips_with_every_field(pool: PgPool) {
        let user = testing::user(&pool).await;
        let session = testing::session(&pool, user).await;
        let heavy = LoggedSet {
            set_index: u16::MAX,
            reps: u16::MAX,
            weight_ng: Some(2_000_000_000_000_000),
            duration_s: Some(u32::MAX),
            warmup: true,
            completed_at: at(1),
            ..new_set(session)
        };
        let bodyweight = LoggedSet {
            set_index: 0,
            reps: 0,
            weight_ng: None,
            duration_s: None,
            completed_at: at(2),
            ..new_set(session)
        };
        for set in [&heavy, &bodyweight] {
            assert_eq!(
                upsert_idempotent(&pool, user, set).await.unwrap(),
                Change::Applied
            );
        }
        assert_eq!(
            list_for_session(&pool, user, session).await.unwrap(),
            vec![heavy, bodyweight]
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn same_id_and_content_is_one_row_and_different_content_conflicts(pool: PgPool) {
        let user = testing::user(&pool).await;
        // An earlier, ended session (one session in progress at a time).
        let other_session = testing::session(&pool, user).await;
        sessions::finish(&pool, user, other_session, SessionOutcome::Abandoned, at(1))
            .await
            .unwrap();
        let session = testing::session(&pool, user).await;
        let set = new_set(session);
        assert_eq!(
            upsert_idempotent(&pool, user, &set).await.unwrap(),
            Change::Applied
        );
        for _ in 0..3 {
            assert_eq!(
                upsert_idempotent(&pool, user, &set).await.unwrap(),
                Change::Unchanged
            );
        }
        for different in [
            LoggedSet {
                reps: 4,
                ..set.clone()
            },
            LoggedSet {
                set_index: 1,
                ..set.clone()
            },
            LoggedSet {
                weight_ng: None,
                ..set.clone()
            },
            LoggedSet {
                weight_ng: Some(1),
                ..set.clone()
            },
            LoggedSet {
                duration_s: Some(30),
                ..set.clone()
            },
            LoggedSet {
                warmup: true,
                ..set.clone()
            },
            LoggedSet {
                exercise_id: "bench".to_owned(),
                ..set.clone()
            },
            LoggedSet {
                completed_at: at(61),
                ..set.clone()
            },
            LoggedSet {
                session_id: other_session,
                ..set.clone()
            },
        ] {
            assert_err(upsert_idempotent(&pool, user, &different).await, "conflict");
        }
        assert_eq!(
            list_for_session(&pool, user, session).await.unwrap(),
            vec![set]
        );
        assert!(
            list_for_session(&pool, user, other_session)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn concurrent_duplicate_saves_create_one_row(pool: PgPool) {
        let user = testing::user(&pool).await;
        let session = testing::session(&pool, user).await;
        let set = new_set(session);
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (pool, set) = (pool.clone(), set.clone());
                tokio::spawn(async move { upsert_idempotent(&pool, user, &set).await })
            })
            .collect();
        let mut applied = 0;
        for task in tasks {
            if task.await.unwrap().unwrap() == Change::Applied {
                applied += 1;
            }
        }
        assert_eq!(applied, 1);
        assert_eq!(
            list_for_session(&pool, user, session).await.unwrap(),
            vec![set]
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn an_ended_session_takes_retries_but_no_new_sets(pool: PgPool) {
        let user = testing::user(&pool).await;
        let session = testing::session(&pool, user).await;
        let logged = testing::set(&pool, user, session).await;
        sessions::finish(&pool, user, session, SessionOutcome::Completed, at(120))
            .await
            .unwrap();
        assert_eq!(
            upsert_idempotent(&pool, user, &logged).await.unwrap(),
            Change::Unchanged
        );
        assert_err(
            upsert_idempotent(&pool, user, &new_set(session)).await,
            "ended",
        );
        assert_err(
            upsert_idempotent(
                &pool,
                user,
                &LoggedSet {
                    reps: 1,
                    ..logged.clone()
                },
            )
            .await,
            "conflict",
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn invalid_values_are_rejected(pool: PgPool) {
        let user = testing::user(&pool).await;
        let session = testing::session(&pool, user).await;
        for bad in [
            LoggedSet {
                weight_ng: Some(2_000_000_000_000_001),
                ..new_set(session)
            },
            LoggedSet {
                weight_ng: Some(u64::MAX),
                ..new_set(session)
            },
            LoggedSet {
                exercise_id: "Back squat".to_owned(),
                ..new_set(session)
            },
            LoggedSet {
                exercise_id: String::new(),
                ..new_set(session)
            },
        ] {
            assert_err(upsert_idempotent(&pool, user, &bad).await, "invalid");
        }
        assert!(
            list_for_session(&pool, user, session)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_sets_are_invisible_and_untouchable(pool: PgPool) {
        let (a, b) = testing::users_a_and_b(&pool).await;
        let session_a = testing::session(&pool, a).await;
        let set_a = testing::set(&pool, a, session_a).await;
        let session_b = testing::session(&pool, b).await;
        let guessed_session = SessionId::from_uuid(random_uuid());

        // Reading A's session's sets: the same answer as for a session that does not exist.
        for session in [session_a, guessed_session] {
            assert_err(list_for_session(&pool, b, session).await, "not found");
        }
        // Logging into A's session: the same answer as into a session that does not exist, even
        // with A's exact set.
        for session in [session_a, guessed_session] {
            assert_err(
                upsert_idempotent(&pool, b, &new_set(session)).await,
                "not found",
            );
        }
        assert_err(upsert_idempotent(&pool, b, &set_a).await, "not found");
        // A's set id in B's own session: B's own, independent set. Retries and conflicts are
        // judged against B's row only.
        let reused = LoggedSet {
            session_id: session_b,
            ..set_a.clone()
        };
        assert_eq!(
            upsert_idempotent(&pool, b, &reused).await.unwrap(),
            Change::Applied
        );
        assert_eq!(
            upsert_idempotent(&pool, b, &reused).await.unwrap(),
            Change::Unchanged
        );
        let changed = LoggedSet {
            reps: 1,
            ..reused.clone()
        };
        assert_err(upsert_idempotent(&pool, b, &changed).await, "conflict");
        assert_eq!(
            list_for_session(&pool, b, session_b).await.unwrap(),
            vec![reused]
        );
        assert_eq!(
            list_for_session(&pool, a, session_a).await.unwrap(),
            vec![set_a]
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn completed_history_of_one_exercise_across_program_versions(pool: PgPool) {
        let user = testing::user(&pool).await;
        let (program, v1) = testing::program(&pool, user).await;
        let v2 = programs::add_version(&pool, user, program, &document("v2"))
            .await
            .unwrap()
            .1
            .id;
        let (_, other_version) = testing::program(&pool, user).await;

        // Logs a set of `exercise` at `minute` in a new session, then ends the session.
        let log =
            |version, exercise: &'static str, minute: i64, outcome: Option<SessionOutcome>| {
                let pool = pool.clone();
                async move {
                    let session = new_session(version);
                    sessions::start(&pool, user, &session).await.unwrap();
                    let set = LoggedSet {
                        exercise_id: exercise.to_owned(),
                        completed_at: at(minute * 60),
                        ..new_set(session.id)
                    };
                    upsert_idempotent(&pool, user, &set).await.unwrap();
                    if let Some(outcome) = outcome {
                        sessions::finish(&pool, user, session.id, outcome, at(minute * 60 + 1))
                            .await
                            .unwrap();
                    }
                    set
                }
            };
        let done = Some(SessionOutcome::Completed);
        let _before_anchor = log(v1, "back-squat", 1, done).await;
        let at_anchor = log(v1, "back-squat", 10, done).await;
        let from_v1 = log(v1, "back-squat", 11, done).await;
        let from_v2 = log(v2, "back-squat", 12, done).await;
        let _other_exercise = log(v2, "bench", 13, done).await;
        let _other_program = log(other_version, "back-squat", 14, done).await;
        let _abandoned = log(v2, "back-squat", 15, Some(SessionOutcome::Abandoned)).await;
        let _skipped = log(v2, "back-squat", 16, Some(SessionOutcome::Skipped)).await;
        let _in_progress = log(v2, "back-squat", 17, None).await;

        let history = completed_for_exercise(&pool, user, program, "back-squat", at(600))
            .await
            .unwrap();
        assert_eq!(history, vec![from_v1.clone(), from_v2.clone()]);
        // Strictly after: the anchor itself is excluded, and moving it earlier includes it.
        let history = completed_for_exercise(&pool, user, program, "back-squat", at(599))
            .await
            .unwrap();
        assert_eq!(history, vec![at_anchor, from_v1, from_v2]);
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_history_is_invisible(pool: PgPool) {
        let (a, b) = testing::users_a_and_b(&pool).await;
        let (program_a, version_a) = testing::program(&pool, a).await;
        let session = new_session(version_a);
        sessions::start(&pool, a, &session).await.unwrap();
        testing::set(&pool, a, session.id).await;
        sessions::finish(&pool, a, session.id, SessionOutcome::Completed, at(3_600))
            .await
            .unwrap();
        assert_eq!(
            completed_for_exercise(&pool, a, program_a, "back-squat", at(-1))
                .await
                .unwrap()
                .len(),
            1
        );
        let exercises = ["back-squat".to_owned()];
        let end = sessions::Cursor {
            started_at: at(1_000_000),
            id: SessionId::from_uuid(random_uuid()),
        };
        assert_eq!(
            completed_in_program(&pool, a, program_a)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            completed_for_exercises_before(&pool, a, &exercises, end)
                .await
                .unwrap()
                .len(),
            1
        );
        for program in [program_a, ProgramId::from_uuid(random_uuid())] {
            assert!(
                completed_for_exercise(&pool, b, program, "back-squat", at(-1))
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                completed_in_program(&pool, b, program)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            completed_for_exercises_before(&pool, b, &exercises, end)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn completed_sets_of_a_program_and_before_a_session(pool: PgPool) {
        let user = testing::user(&pool).await;
        let (program, v1) = testing::program(&pool, user).await;
        let v2 = programs::add_version(&pool, user, program, &document("v2"))
            .await
            .unwrap()
            .1
            .id;
        let (_, other_version) = testing::program(&pool, user).await;

        // A session of `version` started at `hour` with one set of `exercise`, then ended.
        let log = |version, exercise: &'static str, hour: i64, outcome: Option<SessionOutcome>| {
            let pool = pool.clone();
            async move {
                let session = sessions::NewSession {
                    started_at: at(hour * 3_600),
                    ..new_session(version)
                };
                sessions::start(&pool, user, &session).await.unwrap();
                let set = LoggedSet {
                    exercise_id: exercise.to_owned(),
                    completed_at: at(hour * 3_600 + 60),
                    ..new_set(session.id)
                };
                upsert_idempotent(&pool, user, &set).await.unwrap();
                if let Some(outcome) = outcome {
                    sessions::finish(&pool, user, session.id, outcome, at(hour * 3_600 + 120))
                        .await
                        .unwrap();
                }
                (session, set)
            }
        };
        let done = Some(SessionOutcome::Completed);
        // Logged out of order: the results are in session order.
        let (_, v2_squat) = log(v2, "back-squat", 3, done).await;
        let (_, v1_squat) = log(v1, "back-squat", 1, done).await;
        let (_, v1_bench) = log(v1, "bench", 2, done).await;
        let (_, other_squat) = log(other_version, "back-squat", 4, done).await;
        let (_, _abandoned) = log(v2, "back-squat", 5, Some(SessionOutcome::Abandoned)).await;
        let (_, _skipped) = log(v2, "back-squat", 6, Some(SessionOutcome::Skipped)).await;
        // Logged before the one in progress: one session in progress at a time.
        let (_, _later) = log(v1, "back-squat", 8, done).await;
        let (current, _in_progress) = log(v2, "back-squat", 7, None).await;

        let in_program = completed_in_program(&pool, user, program).await.unwrap();
        assert_eq!(
            in_program[..3],
            [v1_squat.clone(), v1_bench, v2_squat.clone()]
        );
        assert_eq!(in_program.len(), 4);

        let before = sessions::Cursor {
            started_at: current.started_at,
            id: current.id,
        };
        let squats = ["back-squat".to_owned()];
        assert_eq!(
            completed_for_exercises_before(&pool, user, &squats, before)
                .await
                .unwrap(),
            vec![v1_squat, v2_squat, other_squat]
        );
        assert!(
            completed_for_exercises_before(&pool, user, &[], before)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn two_users_can_use_the_same_ids_independently(pool: PgPool) {
        let (a, b) = testing::users_a_and_b(&pool).await;
        let (_, version_a) = testing::program(&pool, a).await;
        let (_, version_b) = testing::program(&pool, b).await;
        let session_id = SessionId::from_uuid(random_uuid());
        let set_id = SetId::from_uuid(random_uuid());
        let mut logged = Vec::new();
        for (user, version, reps) in [(a, version_a, 5), (b, version_b, 8)] {
            let session = sessions::NewSession {
                id: session_id,
                ..new_session(version)
            };
            assert_eq!(
                sessions::start(&pool, user, &session).await.unwrap(),
                Change::Applied
            );
            let set = LoggedSet {
                id: set_id,
                reps,
                ..new_set(session_id)
            };
            assert_eq!(
                upsert_idempotent(&pool, user, &set).await.unwrap(),
                Change::Applied
            );
            logged.push(set);
        }
        // Each user sees exactly their own rows, and ending one session leaves the other open.
        sessions::finish(&pool, a, session_id, SessionOutcome::Completed, at(120))
            .await
            .unwrap();
        for (user, set, status) in [
            (a, &logged[0], sessions::SessionStatus::Completed),
            (b, &logged[1], sessions::SessionStatus::InProgress),
        ] {
            assert_eq!(
                list_for_session(&pool, user, session_id).await.unwrap(),
                vec![set.clone()]
            );
            assert_eq!(
                sessions::get(&pool, user, session_id).await.unwrap().status,
                status
            );
        }
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn errors_never_contain_ids(pool: PgPool) {
        let (a, b) = testing::users_a_and_b(&pool).await;
        let (program_a, version_a) = testing::program(&pool, a).await;
        let session_a = testing::session(&pool, a).await;
        let set_a = testing::set(&pool, a, session_a).await;
        let session_b = testing::session(&pool, b).await;
        let set_b = testing::set(&pool, b, session_b).await;
        sessions::finish(&pool, b, session_b, SessionOutcome::Completed, at(120))
            .await
            .unwrap();

        let errors: Vec<RepoError> = vec![
            // Not found: A's ids used by B.
            upsert_idempotent(&pool, b, &set_a).await.unwrap_err(),
            list_for_session(&pool, b, session_a).await.unwrap_err(),
            sessions::get(&pool, b, session_a).await.unwrap_err(),
            sessions::start(&pool, b, &new_session(version_a))
                .await
                .unwrap_err(),
            programs::get(&pool, b, program_a).await.unwrap_err(),
            // Conflict, session ended and invalid value: B's own rows.
            upsert_idempotent(
                &pool,
                b,
                &LoggedSet {
                    reps: 1,
                    ..set_b.clone()
                },
            )
            .await
            .unwrap_err(),
            upsert_idempotent(&pool, b, &new_set(session_b))
                .await
                .unwrap_err(),
            sessions::start(
                &pool,
                b,
                &sessions::NewSession {
                    day_id: "Bad Day".to_owned(),
                    ..new_session(testing::program(&pool, b).await.1)
                },
            )
            .await
            .unwrap_err(),
        ];
        let ids = [
            a.as_uuid(),
            b.as_uuid(),
            program_a.as_uuid(),
            version_a.as_uuid(),
            session_a.as_uuid(),
            session_b.as_uuid(),
            set_a.id.as_uuid(),
            set_b.id.as_uuid(),
        ];
        for error in &errors {
            let text = format!("{error} {error:?}");
            for id in ids {
                for form in [id.to_string(), id.simple().to_string()] {
                    assert!(!text.contains(&form), "{text} contains {form}");
                }
            }
        }
        for expected in ["not found", "conflict", "ended", "invalid"] {
            assert!(
                errors.iter().any(|error| {
                    matches!(
                        (error, expected),
                        (RepoError::NotFound, "not found")
                            | (RepoError::Conflict, "conflict")
                            | (RepoError::SessionEnded, "ended")
                            | (RepoError::Invalid { .. }, "invalid")
                    )
                }),
                "{expected}: {errors:?}"
            );
        }
    }

    /// A set saved while its session's start is still committing must never come back as a
    /// conflict: it is saved (the second attempt sees the session) or, at worst, reported as
    /// transient. Reproducer from the review of #58.
    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn a_set_racing_its_sessions_start_is_saved(pool: PgPool) {
        let pool = PgPoolOptions::new()
            .max_connections(40)
            .connect_with((*pool.connect_options()).clone())
            .await
            .unwrap();
        let user = testing::user(&pool).await;
        let (_, version) = testing::program(&pool, user).await;
        let mut not_found = 0;
        for _ in 0..300 {
            let session = new_session(version);
            let set = new_set(session.id);
            let (pool_1, session_1) = (pool.clone(), session.clone());
            let start =
                tokio::spawn(async move { sessions::start(&pool_1, user, &session_1).await });
            let (pool_2, set_2) = (pool.clone(), set.clone());
            let save = tokio::spawn(async move { upsert_idempotent(&pool_2, user, &set_2).await });
            assert_eq!(start.await.unwrap().unwrap(), Change::Applied);
            let saved = save.await.unwrap();
            // Ended, so the next round can start one (one session in progress at a time).
            sessions::finish(&pool, user, session.id, SessionOutcome::Abandoned, at(120))
                .await
                .unwrap();
            match saved {
                Ok(change) => {
                    assert_eq!(change, Change::Applied);
                    assert_eq!(
                        list_for_session(&pool, user, session.id).await.unwrap(),
                        vec![set]
                    );
                }
                // The save ran entirely before the session existed: an honest answer.
                Err(RepoError::NotFound) => not_found += 1,
                Err(error) => panic!("{error:?}"),
            }
        }
        assert!(not_found < 300, "the race was never exercised");
    }
}
