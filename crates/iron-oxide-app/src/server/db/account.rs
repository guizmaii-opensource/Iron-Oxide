//! The whole account (#22): reading everything a user owns for an export, writing an import, and
//! deleting the account.
//!
//! Every function takes the caller's [`UserId`] and a connection inside the caller's transaction,
//! and scopes every statement by the user, like the rest of the repository. Imports never update
//! a row: they insert what the user does not have yet and leave everything else as it is
//! (`ON CONFLICT DO NOTHING`, or a lookup first), which is what makes them idempotent.

use sqlx::{
    Connection, PgConnection, PgPool,
    types::{JsonValue, Uuid, time::OffsetDateTime},
};

use super::{
    error::{RepoError, narrow},
    ids::UserId,
    settings::UserSettings,
};

/// The database deadlines of an import or an account deletion (#22), instead of the pool's 5 s
/// (`db::STATEMENT_DEADLINE`): a whole account is written or deleted in one transaction. The
/// largest import measured (40,000 sets) took up to 10.8 s on a debug build; 60 s leaves room for
/// a slow machine and a large account's cascade.
pub const ACCOUNT_DEADLINE: &str = "60s";

/// The `application_name` of a [`long_connection`], to tell them apart in `pg_stat_activity`.
pub const ACCOUNT_CONNECTION_NAME: &str = "iron-oxide-account";

/// A connection of its own for an import or an account deletion, with the three deadlines
/// (`statement_timeout`, `idle_in_transaction_session_timeout`, `transaction_timeout`) at
/// [`ACCOUNT_DEADLINE`] instead of the pool's 5 s.
///
/// `SET LOCAL` inside the transaction is not enough: `transaction_timeout` is armed when the
/// transaction starts, so raising it later does not extend it. The deadlines are therefore set on
/// the session before `BEGIN`, on a connection taken out of the pool (`detach`): it is closed when
/// dropped, never returned to the pool with the longer deadlines, whatever happens. Close it with
/// [`close`] after the transaction.
pub async fn long_connection(pool: &PgPool) -> Result<PgConnection, RepoError> {
    let mut conn = pool.acquire().await?.detach();
    sqlx::query!(
        "SELECT set_config('statement_timeout', $1, false) AS statement,
                set_config('idle_in_transaction_session_timeout', $1, false) AS idle,
                set_config('transaction_timeout', $1, false) AS transaction,
                set_config('application_name', $2, false) AS name",
        ACCOUNT_DEADLINE,
        ACCOUNT_CONNECTION_NAME
    )
    .fetch_one(&mut conn)
    .await?;
    Ok(conn)
}

/// Closes a [`long_connection`] (an error closing it only means it is already gone).
pub async fn close(conn: PgConnection) {
    if let Err(error) = conn.close().await {
        dioxus::logger::tracing::debug!(%error, "closing an account connection failed");
    }
}

// --- Reading (export) --------------------------------------------------------------------------

/// Makes the caller's transaction a consistent, read-only snapshot. Must be its first statement.
pub async fn snapshot(tx: &mut PgConnection) -> Result<(), RepoError> {
    sqlx::query!("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(tx)
        .await?;
    Ok(())
}

/// The `users` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub created_at: OffsetDateTime,
    /// The `user_plan` enum value.
    pub plan: String,
    pub display_name: Option<String>,
}

/// The user's account row, `None` if the user does not exist (deleted meanwhile).
pub async fn account(tx: &mut PgConnection, user: UserId) -> Result<Option<Account>, RepoError> {
    Ok(sqlx::query_as!(
        Account,
        r#"SELECT created_at, plan::text AS "plan!", display_name FROM users WHERE id = $1"#,
        user.as_uuid()
    )
    .fetch_optional(tx)
    .await?)
}

/// A passkey's metadata: never the key material (`passkeys.passkey`) or the credential id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasskeyMetadata {
    pub nickname: String,
    pub created_at: OffsetDateTime,
    pub last_used_at: Option<OffsetDateTime>,
    pub backup_state: bool,
}

/// The user's passkeys, oldest first.
pub async fn passkeys(
    tx: &mut PgConnection,
    user: UserId,
) -> Result<Vec<PasskeyMetadata>, RepoError> {
    Ok(sqlx::query_as!(
        PasskeyMetadata,
        "SELECT nickname, created_at, last_used_at, backup_state FROM passkeys
         WHERE user_id = $1 ORDER BY created_at, id",
        user.as_uuid()
    )
    .fetch_all(tx)
    .await?)
}

/// A linked external account, without the provider's subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedAccount {
    pub provider: String,
    pub created_at: OffsetDateTime,
    pub last_used_at: Option<OffsetDateTime>,
}

/// The user's linked external accounts, oldest first.
pub async fn linked_accounts(
    tx: &mut PgConnection,
    user: UserId,
) -> Result<Vec<LinkedAccount>, RepoError> {
    Ok(sqlx::query_as!(
        LinkedAccount,
        r#"SELECT provider::text AS "provider!", created_at, last_used_at FROM oauth_identities
           WHERE user_id = $1 ORDER BY created_at, id"#,
        user.as_uuid()
    )
    .fetch_all(tx)
    .await?)
}

/// The user's saved settings and when they were saved, `None` if they never saved any.
pub async fn settings(
    tx: &mut PgConnection,
    user: UserId,
) -> Result<Option<(UserSettings, OffsetDateTime)>, RepoError> {
    let row = sqlx::query!(
        "SELECT unit, bar_weight_ng, plate_inventory, default_rest_s, sound_enabled,
                kg_weight_step_ng, lb_weight_step_ng, vibration_enabled, updated_at
         FROM user_settings WHERE user_id = $1",
        user.as_uuid()
    )
    .fetch_optional(tx)
    .await?;
    row.map(|row| {
        let settings = UserSettings {
            unit: super::settings::Unit::parse(&row.unit)?,
            bar_weight_ng: narrow(row.bar_weight_ng, "user_settings.bar_weight_ng")?,
            plate_inventory: row.plate_inventory,
            default_rest_s: narrow(row.default_rest_s, "user_settings.default_rest_s")?,
            sound_enabled: row.sound_enabled,
            kg_weight_step_ng: narrow(row.kg_weight_step_ng, "user_settings.kg_weight_step_ng")?,
            lb_weight_step_ng: narrow(row.lb_weight_step_ng, "user_settings.lb_weight_step_ng")?,
            vibration_enabled: row.vibration_enabled,
        };
        Ok((settings, row.updated_at))
    })
    .transpose()
}

/// A training max.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainingMax {
    pub exercise_id: String,
    pub weight_ng: i64,
    pub set_at: OffsetDateTime,
}

/// The user's training maxes, by exercise id.
pub async fn training_maxes(
    tx: &mut PgConnection,
    user: UserId,
) -> Result<Vec<TrainingMax>, RepoError> {
    Ok(sqlx::query_as!(
        TrainingMax,
        "SELECT exercise_id, weight_ng, set_at FROM training_maxes
         WHERE user_id = $1 ORDER BY exercise_id",
        user.as_uuid()
    )
    .fetch_all(tx)
    .await?)
}

/// One of the user's programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub id: Uuid,
    pub creation_id: Uuid,
    pub name: String,
    pub source_builtin_id: Option<String>,
    pub archived: bool,
    pub created_at: OffsetDateTime,
}

/// The user's programs, archived ones included, oldest first.
pub async fn programs(tx: &mut PgConnection, user: UserId) -> Result<Vec<Program>, RepoError> {
    Ok(sqlx::query_as!(
        Program,
        r#"SELECT id, creation_id AS "creation_id!", name, source_builtin_id, archived, created_at
           FROM programs WHERE user_id = $1 ORDER BY created_at, id"#,
        user.as_uuid()
    )
    .fetch_all(tx)
    .await?)
}

/// One version of one of the user's programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub program_id: Uuid,
    pub version: i32,
    pub created_at: OffsetDateTime,
    pub document: JsonValue,
}

/// Every version of the user's programs, by program and version number.
pub async fn versions(tx: &mut PgConnection, user: UserId) -> Result<Vec<Version>, RepoError> {
    Ok(sqlx::query_as!(
        Version,
        "SELECT program_id, version, created_at, document FROM program_versions
         WHERE user_id = $1 ORDER BY program_id, version",
        user.as_uuid()
    )
    .fetch_all(tx)
    .await?)
}

/// The user's active program, if any.
pub async fn active_program(
    tx: &mut PgConnection,
    user: UserId,
) -> Result<Option<Uuid>, RepoError> {
    Ok(sqlx::query_scalar!(
        "SELECT program_id FROM active_program WHERE user_id = $1",
        user.as_uuid()
    )
    .fetch_optional(tx)
    .await?)
}

/// A workout session, with the program and version number it was run from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: Uuid,
    pub program_id: Uuid,
    pub version: i32,
    pub day_id: String,
    pub status: String,
    pub started_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
}

/// The user's workout sessions, oldest first.
pub async fn sessions(tx: &mut PgConnection, user: UserId) -> Result<Vec<Session>, RepoError> {
    Ok(sqlx::query_as!(
        Session,
        "SELECT s.id, v.program_id, v.version, s.day_id, s.status, s.started_at, s.finished_at
         FROM workout_sessions s
         JOIN program_versions v ON v.id = s.program_version_id AND v.user_id = s.user_id
         WHERE s.user_id = $1
         ORDER BY s.started_at, s.id",
        user.as_uuid()
    )
    .fetch_all(tx)
    .await?)
}

/// A logged set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Set {
    pub session_id: Uuid,
    pub id: Uuid,
    pub exercise_id: String,
    pub set_index: i32,
    pub reps: i32,
    pub weight_ng: Option<i64>,
    pub duration_s: Option<i64>,
    pub warmup: bool,
    pub completed_at: OffsetDateTime,
    /// The prescribed target's load (#60); `None` for a body-weight target or no target.
    pub target_weight_ng: Option<i64>,
    /// The prescribed target's `SetGoal` JSON (#60); `None` when no target was recorded.
    pub target_goal: Option<JsonValue>,
}

/// The user's sets, by session and in the order they were completed.
pub async fn sets(tx: &mut PgConnection, user: UserId) -> Result<Vec<Set>, RepoError> {
    Ok(sqlx::query_as!(
        Set,
        "SELECT session_id, id, exercise_id, set_index, reps, weight_ng, duration_s, warmup,
                completed_at, target_weight_ng, target_goal
         FROM workout_sets WHERE user_id = $1
         ORDER BY session_id, completed_at, id",
        user.as_uuid()
    )
    .fetch_all(tx)
    .await?)
}

// --- Writing (import) --------------------------------------------------------------------------

/// Saves `settings` unless the user already has settings. Returns whether it did.
pub async fn insert_settings(
    tx: &mut PgConnection,
    user: UserId,
    settings: &UserSettings,
    updated_at: OffsetDateTime,
) -> Result<bool, RepoError> {
    let bar_weight_ng = i64::try_from(settings.bar_weight_ng).map_err(|_| RepoError::Invalid {
        constraint: Some("user_settings_bar_weight_ng_check".to_owned()),
    })?;
    let step = |value: u64, constraint: &str| {
        i64::try_from(value).map_err(|_| RepoError::Invalid {
            constraint: Some(constraint.to_owned()),
        })
    };
    let kg_weight_step_ng = step(
        settings.kg_weight_step_ng,
        "user_settings_kg_weight_step_ng_check",
    )?;
    let lb_weight_step_ng = step(
        settings.lb_weight_step_ng,
        "user_settings_lb_weight_step_ng_check",
    )?;
    let inserted = sqlx::query!(
        "INSERT INTO user_settings
             (user_id, unit, bar_weight_ng, plate_inventory, default_rest_s, sound_enabled,
              kg_weight_step_ng, lb_weight_step_ng, vibration_enabled, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (user_id) DO NOTHING",
        user.as_uuid(),
        settings.unit.as_str(),
        bar_weight_ng,
        settings.plate_inventory,
        i64::from(settings.default_rest_s),
        settings.sound_enabled,
        kg_weight_step_ng,
        lb_weight_step_ng,
        settings.vibration_enabled,
        updated_at,
    )
    .execute(tx)
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

/// Adds the training maxes of the exercises that have none yet. Returns how many it added.
pub async fn insert_training_maxes(
    tx: &mut PgConnection,
    user: UserId,
    maxes: &[TrainingMax],
) -> Result<u64, RepoError> {
    let exercise_ids: Vec<String> = maxes.iter().map(|m| m.exercise_id.clone()).collect();
    let weights: Vec<i64> = maxes.iter().map(|m| m.weight_ng).collect();
    let set_ats: Vec<OffsetDateTime> = maxes.iter().map(|m| m.set_at).collect();
    Ok(sqlx::query!(
        "INSERT INTO training_maxes (user_id, exercise_id, weight_ng, set_at)
         SELECT $1, u.exercise_id, u.weight_ng, u.set_at
         FROM UNNEST($2::text[], $3::bigint[], $4::timestamptz[])
             AS u (exercise_id, weight_ng, set_at)
         ON CONFLICT (user_id, exercise_id) DO NOTHING",
        user.as_uuid(),
        &exercise_ids,
        &weights,
        &set_ats,
    )
    .execute(tx)
    .await?
    .rows_affected())
}

/// The user's program with this creation id: its id and whether it is archived.
pub async fn program_by_creation(
    tx: &mut PgConnection,
    user: UserId,
    creation_id: Uuid,
) -> Result<Option<(Uuid, bool)>, RepoError> {
    let row = sqlx::query!(
        "SELECT id, archived FROM programs WHERE user_id = $1 AND creation_id = $2",
        user.as_uuid(),
        creation_id
    )
    .fetch_optional(tx)
    .await?;
    Ok(row.map(|row| (row.id, row.archived)))
}

/// A program to import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProgram<'a> {
    pub creation_id: Uuid,
    pub name: &'a str,
    pub source_builtin_id: Option<&'a str>,
    pub archived: bool,
    pub created_at: OffsetDateTime,
}

/// Inserts a program of the user's, with a new id. The caller checked that the user has no
/// program with this creation id, and took a quota slot if it is not archived.
pub async fn insert_program(
    tx: &mut PgConnection,
    user: UserId,
    program: &NewProgram<'_>,
) -> Result<Uuid, RepoError> {
    Ok(sqlx::query_scalar!(
        "INSERT INTO programs (user_id, creation_id, name, source_builtin_id, archived, created_at)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id",
        user.as_uuid(),
        program.creation_id,
        program.name,
        program.source_builtin_id,
        program.archived,
        program.created_at,
    )
    .fetch_one(tx)
    .await?)
}

/// The version numbers one of the user's programs already uses.
pub async fn version_numbers(
    tx: &mut PgConnection,
    user: UserId,
    program: Uuid,
) -> Result<Vec<i32>, RepoError> {
    Ok(sqlx::query_scalar!(
        "SELECT version FROM program_versions WHERE user_id = $1 AND program_id = $2",
        user.as_uuid(),
        program
    )
    .fetch_all(tx)
    .await?)
}

/// Locks one of the user's programs (`FOR UPDATE`) until the end of the transaction, so a
/// concurrent upload of a version (`programs::add_version`, which locks the same row) waits
/// instead of taking a number this import is about to use.
pub async fn lock_program(
    tx: &mut PgConnection,
    user: UserId,
    program: Uuid,
) -> Result<(), RepoError> {
    sqlx::query_scalar!(
        "SELECT id FROM programs WHERE id = $2 AND user_id = $1 FOR UPDATE",
        user.as_uuid(),
        program
    )
    .fetch_optional(tx)
    .await?
    .ok_or(RepoError::NotFound)?;
    Ok(())
}

/// Locks the user's programs `programs` (`FOR UPDATE`, in id order, so two transactions locking
/// overlapping sets never deadlock), until the end of the transaction. A concurrent upload of a
/// version (`programs::add_version`, which locks the program's row) then waits for the import.
pub async fn lock_programs(
    tx: &mut PgConnection,
    user: UserId,
    programs: &[Uuid],
) -> Result<(), RepoError> {
    sqlx::query_scalar!(
        "SELECT id FROM programs WHERE user_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE",
        user.as_uuid(),
        programs
    )
    .fetch_all(tx)
    .await?;
    Ok(())
}

/// The lowest-numbered version of one of the user's programs whose document equals `document`
/// (compared as `jsonb`: formatting and key order do not matter).
pub async fn version_with_document(
    tx: &mut PgConnection,
    user: UserId,
    program: Uuid,
    document: &JsonValue,
) -> Result<Option<Uuid>, RepoError> {
    Ok(sqlx::query_scalar!(
        "SELECT id FROM program_versions
         WHERE user_id = $1 AND program_id = $2 AND document = $3::jsonb
         ORDER BY version LIMIT 1",
        user.as_uuid(),
        program,
        document
    )
    .fetch_optional(tx)
    .await?)
}

/// Inserts version `version` of the user's program `program` (which must not have it yet).
/// Returns its new id.
pub async fn insert_version(
    tx: &mut PgConnection,
    user: UserId,
    program: Uuid,
    version: i32,
    document: &JsonValue,
    created_at: OffsetDateTime,
) -> Result<Uuid, RepoError> {
    // The owner is set by the `program_versions_set_owner` trigger from the program; the WHERE
    // clause makes sure that is the caller.
    sqlx::query_scalar!(
        "INSERT INTO program_versions (program_id, user_id, version, document, created_at)
         SELECT p.id, p.user_id, $3, $4, $5 FROM programs p WHERE p.id = $2 AND p.user_id = $1
         RETURNING id",
        user.as_uuid(),
        program,
        version,
        document,
        created_at,
    )
    .fetch_optional(tx)
    .await?
    .ok_or(RepoError::NotFound)
}

/// Makes `program` the user's active program unless they already have one, or it is archived.
/// Returns whether it did.
pub async fn insert_active_program(
    tx: &mut PgConnection,
    user: UserId,
    program: Uuid,
) -> Result<bool, RepoError> {
    let inserted = sqlx::query!(
        "INSERT INTO active_program (user_id, program_id)
         SELECT $1, p.id FROM programs p WHERE p.id = $2 AND p.user_id = $1 AND NOT p.archived
         ON CONFLICT (user_id) DO NOTHING",
        user.as_uuid(),
        program,
    )
    .execute(tx)
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

/// A session to import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub id: Uuid,
    /// One of the user's program versions.
    pub program_version_id: Uuid,
    pub day_id: String,
    pub status: &'static str,
    pub started_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
}

/// Inserts the sessions the user does not have yet. A session whose id the user already uses is
/// left as it is, and so is an in-progress session when the user already has another one in
/// progress (at most one at a time). Returns the ids of the sessions it inserted.
pub async fn insert_sessions(
    tx: &mut PgConnection,
    user: UserId,
    sessions: &[NewSession],
) -> Result<Vec<Uuid>, RepoError> {
    let ids: Vec<Uuid> = sessions.iter().map(|s| s.id).collect();
    let versions: Vec<Uuid> = sessions.iter().map(|s| s.program_version_id).collect();
    let days: Vec<String> = sessions.iter().map(|s| s.day_id.clone()).collect();
    let statuses: Vec<String> = sessions.iter().map(|s| s.status.to_owned()).collect();
    let started: Vec<OffsetDateTime> = sessions.iter().map(|s| s.started_at).collect();
    let finished: Vec<Option<OffsetDateTime>> = sessions.iter().map(|s| s.finished_at).collect();
    // No conflict target: skips both a taken id and the one-in-progress index.
    Ok(sqlx::query_scalar!(
        "INSERT INTO workout_sessions
             (id, user_id, program_version_id, day_id, status, started_at, finished_at)
         SELECT u.id, $1, u.program_version_id, u.day_id, u.status, u.started_at, u.finished_at
         FROM UNNEST($2::uuid[], $3::uuid[], $4::text[], $5::text[], $6::timestamptz[],
                     $7::timestamptz[])
             AS u (id, program_version_id, day_id, status, started_at, finished_at)
         ON CONFLICT DO NOTHING
         RETURNING id",
        user.as_uuid(),
        &ids,
        &versions,
        &days,
        &statuses,
        &started,
        &finished as &[Option<OffsetDateTime>],
    )
    .fetch_all(tx)
    .await?)
}

/// Inserts the sets whose id the user does not use yet. Every set's session must be one of the
/// user's. Returns how many it inserted.
pub async fn insert_sets(
    tx: &mut PgConnection,
    user: UserId,
    sets: &[Set],
) -> Result<u64, RepoError> {
    let ids: Vec<Uuid> = sets.iter().map(|s| s.id).collect();
    let sessions: Vec<Uuid> = sets.iter().map(|s| s.session_id).collect();
    let exercises: Vec<String> = sets.iter().map(|s| s.exercise_id.clone()).collect();
    let indexes: Vec<i32> = sets.iter().map(|s| s.set_index).collect();
    let reps: Vec<i32> = sets.iter().map(|s| s.reps).collect();
    let weights: Vec<Option<i64>> = sets.iter().map(|s| s.weight_ng).collect();
    let durations: Vec<Option<i64>> = sets.iter().map(|s| s.duration_s).collect();
    let warmups: Vec<bool> = sets.iter().map(|s| s.warmup).collect();
    let completed: Vec<OffsetDateTime> = sets.iter().map(|s| s.completed_at).collect();
    let target_weights: Vec<Option<i64>> = sets.iter().map(|s| s.target_weight_ng).collect();
    let target_goals: Vec<Option<JsonValue>> = sets.iter().map(|s| s.target_goal.clone()).collect();
    Ok(sqlx::query!(
        "INSERT INTO workout_sets
             (id, session_id, user_id, exercise_id, set_index, reps, weight_ng, duration_s,
              warmup, completed_at, target_weight_ng, target_goal)
         SELECT u.id, u.session_id, $1, u.exercise_id, u.set_index, u.reps, u.weight_ng,
                u.duration_s, u.warmup, u.completed_at, u.target_weight_ng, u.target_goal
         FROM UNNEST($2::uuid[], $3::uuid[], $4::text[], $5::int[], $6::int[], $7::bigint[],
                     $8::bigint[], $9::bool[], $10::timestamptz[], $11::bigint[], $12::jsonb[])
             AS u (id, session_id, exercise_id, set_index, reps, weight_ng, duration_s, warmup,
                   completed_at, target_weight_ng, target_goal)
         ON CONFLICT (user_id, id) DO NOTHING",
        user.as_uuid(),
        &ids,
        &sessions,
        &exercises,
        &indexes,
        &reps,
        &weights as &[Option<i64>],
        &durations as &[Option<i64>],
        &warmups,
        &completed,
        &target_weights as &[Option<i64>],
        &target_goals as &[Option<JsonValue>],
    )
    .execute(tx)
    .await?
    .rows_affected())
}

// --- Deleting ----------------------------------------------------------------------------------

/// Deletes the user and, through the `ON DELETE CASCADE` foreign keys, every row they own: the
/// training data, the sign-in methods and every sign-in session. Returns whether the user existed.
///
/// This is the only way programs and their versions are ever deleted: their
/// `forbid_direct_delete` triggers let only a cascade through.
pub async fn delete_user(tx: &mut PgConnection, user: UserId) -> Result<bool, RepoError> {
    let deleted = sqlx::query!("DELETE FROM users WHERE id = $1", user.as_uuid())
        .execute(tx)
        .await?
        .rows_affected();
    Ok(deleted == 1)
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;

    const SETTINGS: [&str; 3] = [
        "statement_timeout",
        "idle_in_transaction_session_timeout",
        "transaction_timeout",
    ];

    use super::*;
    use crate::server::db::tests::app_pool;

    async fn show(conn: &mut PgConnection, setting: &str) -> String {
        sqlx::query_scalar(&format!("SHOW {setting}"))
            .fetch_one(conn)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "needs Postgres"]
    async fn account_transactions_outlast_the_pools_deadlines_and_only_them(
        _: PgPoolOptions,
        options: sqlx::postgres::PgConnectOptions,
    ) {
        let pool = app_pool(options).await;
        let mut conn = long_connection(&pool).await.unwrap();
        for setting in SETTINGS {
            assert_eq!(show(&mut conn, setting).await, "1min", "{setting}");
        }
        // Longer than the pool's 5 s, for one statement and for the whole transaction.
        let mut tx = conn.begin().await.unwrap();
        sqlx::query("SELECT pg_sleep(5.5)")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        close(conn).await;
        // The pool's connections keep their 5 s.
        let mut pooled = pool.acquire().await.unwrap();
        for setting in SETTINGS {
            assert_eq!(show(&mut pooled, setting).await, "5s", "{setting}");
        }
        // And a plain pooled transaction is still cut at 5 s.
        let mut tx = pooled.begin().await.unwrap();
        let error = sqlx::query("SELECT pg_sleep(5.5)")
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert!(error.as_database_error().is_some(), "{error}");
    }
}
