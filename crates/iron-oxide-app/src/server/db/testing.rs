//! Test helpers for the Postgres tests (#24).
//!
//! Each test is a `#[sqlx::test(migrator = "MIGRATOR")]` marked `#[ignore = "needs Postgres"]`:
//! sqlx creates a fresh database for the test on the server named by `DATABASE_URL`, applies every
//! migration and drops the database afterwards, so tests never see each other's rows and can run
//! in parallel. Run them with
//! `cargo test -p iron-oxide-app --features server -- --ignored` (see README).
//!
//! The helpers below create users and the rows a test needs, going through the repository where
//! it has a function for it.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use sqlx::{
    PgConnection, PgPool,
    types::{JsonValue, Uuid, time::OffsetDateTime},
};

use super::{
    error::RepoError,
    ids::{CreationId, ProgramId, ProgramVersionId, SessionId, SetId, UserId},
    programs, sessions,
    sets::{self, LoggedSet},
};

/// Creates a user and returns their id.
pub async fn user(pool: &PgPool) -> UserId {
    let id = sqlx::query_scalar!("INSERT INTO users DEFAULT VALUES RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap();
    UserId::from_uuid(id)
}

/// Creates two users: A, whose data the tests try to reach, and B, the one trying.
pub async fn users_a_and_b(pool: &PgPool) -> (UserId, UserId) {
    (user(pool).await, user(pool).await)
}

/// A minimal valid program document.
pub fn document(name: &str) -> JsonValue {
    json!({ "schema_version": 1, "name": name, "days": [] })
}

/// A fixed point in time, plus `seconds`.
pub fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000 + seconds).unwrap()
}

/// A fresh id that no row uses yet (unique within the test process).
pub fn random_uuid() -> Uuid {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    Uuid::from_u128(0x5eed_0000_0000_4000_8000_0000_0000_0000 | u128::from(n))
}

/// The `reserve` step of the program writes that take a quota slot, for repository tests that do
/// not test the quota: it takes the slot without checking anything.
pub fn unlimited(_: &mut PgConnection) -> programs::Reserve<'_, RepoError> {
    Box::pin(async { Ok(()) })
}

/// A fresh creation id (idempotency key) for `programs::create` and `copy_builtin`.
pub fn creation() -> CreationId {
    CreationId::from_uuid(random_uuid())
}

/// Creates a program owned by `user`, with one version.
pub async fn program(pool: &PgPool, user: UserId) -> (ProgramId, ProgramVersionId) {
    let (_, program, version) = programs::create(
        pool,
        user,
        creation(),
        "Program",
        &document("Program"),
        unlimited,
    )
    .await
    .unwrap();
    (program.id, version.id)
}

/// The start of a session of `version` on day `a`.
pub fn new_session(version: ProgramVersionId) -> sessions::NewSession {
    sessions::NewSession {
        id: SessionId::from_uuid(random_uuid()),
        program_version_id: version,
        day_id: "a".to_owned(),
        started_at: at(0),
    }
}

/// Creates an in-progress session owned by `user`, with its own program.
pub async fn session(pool: &PgPool, user: UserId) -> SessionId {
    let (_, version) = program(pool, user).await;
    let session = new_session(version);
    sessions::start(pool, user, &session).await.unwrap();
    session.id
}

/// A work set of 5 × 100 kg of back squat in `session`.
pub fn new_set(session: SessionId) -> LoggedSet {
    LoggedSet {
        id: SetId::from_uuid(random_uuid()),
        session_id: session,
        exercise_id: "back-squat".to_owned(),
        set_index: 0,
        reps: 5,
        weight_ng: Some(100_000_000_000_000),
        duration_s: None,
        warmup: false,
        completed_at: at(60),
        target: None,
    }
}

/// Creates a set in `session` (which must be `user`'s and in progress).
pub async fn set(pool: &PgPool, user: UserId, session: SessionId) -> LoggedSet {
    let set = new_set(session);
    sets::upsert_idempotent(pool, user, &set).await.unwrap();
    set
}

/// Gives `user` one row in every user-owned table: settings, a training max, a program with two
/// versions, the active program, a session and a set.
pub async fn populate(pool: &PgPool, user: UserId) {
    super::settings::save(pool, user, &super::settings::UserSettings::defaults())
        .await
        .unwrap();
    super::training_maxes::set(
        pool,
        user,
        &super::training_maxes::TrainingMax {
            exercise_id: "back-squat".to_owned(),
            weight_ng: 1,
            set_at: at(0),
        },
    )
    .await
    .unwrap();
    let (program, version) = program(pool, user).await;
    programs::add_version(pool, user, program, &document("Second"))
        .await
        .unwrap();
    super::active_program::set(pool, user, program)
        .await
        .unwrap();
    let session = new_session(version);
    sessions::start(pool, user, &session).await.unwrap();
    set(pool, user, session.id).await;
    populate_auth(pool, user).await;
}

/// Sign-in rows (#5) for `user`, one in each auth table, with raw SQL (the auth code writes them
/// through WebAuthn and OIDC ceremonies).
async fn populate_auth(pool: &PgPool, user: UserId) {
    let id = user.as_uuid();
    for sql in [
        // A user signed up through the API already has one.
        "INSERT INTO webauthn_user_handles (user_id, user_handle) VALUES ($1, gen_random_uuid())
         ON CONFLICT (user_id) DO NOTHING",
        "INSERT INTO passkeys (user_id, credential_id, passkey, backup_eligible, backup_state,
                               nickname)
         VALUES ($1, uuid_send($1), '{}', false, false, 'test')",
        "INSERT INTO oauth_identities (user_id, provider, subject) VALUES ($1, 'google', $1::text)",
        "INSERT INTO sessions (id_hash, user_id, data, expires_at)
         VALUES (decode(md5($1::text) || md5(reverse($1::text)), 'hex'), $1, '{}',
                 now() + interval '1 day')",
        "INSERT INTO auth_ceremonies (id, kind, user_id, state, expires_at)
         VALUES (uuidv7(), 'passkey_add', $1, '{}', now() + interval '5 minutes')",
    ] {
        sqlx::query(sql).bind(id).execute(pool).await.unwrap();
    }
}
