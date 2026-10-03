//! Workout sessions (#18): start a session, log its sets, finish it, and read what to do next.
//!
//! Every write is idempotent on a client-generated UUIDv7 id (`SessionId::new_v7()`,
//! `SetId::new_v7()`) and carries its own timestamps, so the retry queue (#30) can resend the exact
//! same request: the same id and content succeeds again without changing anything, the same id
//! with other content is a `409`. See `docs/api.md`.

use dioxus::prelude::*;
use iron_oxide_domain::{
    DayId, ExerciseId, LoggedSet, PrEvent, ProgramId, ProgramVersionId, SessionId, SessionOutcome,
    SessionStatus, Volume,
    program::Exercise,
    progression::{NextTargets, ProgressionChange},
    time::Timestamp,
};
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use {
    crate::server::{AppState, api::sessions, auth::AuthUser},
    dioxus::server::axum::Extension,
};

/// A workout session, as the client sees it.
#[cfg_attr(
    not(feature = "server"),
    allow(
        dead_code,
        reason = "the client only decodes it until the session screens (#29) land"
    )
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    pub id: SessionId,
    pub program_id: ProgramId,
    pub program_version_id: ProgramVersionId,
    pub day: DayId,
    pub status: SessionStatus,
    pub started_at: Timestamp,
    /// `None` while in progress.
    pub finished_at: Option<Timestamp>,
}

/// A session and the sets logged so far, in logging order: what the session screen restores.
#[cfg_attr(
    not(feature = "server"),
    allow(dead_code, reason = "decoded by the session screen (#29)")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWithSets {
    pub session: SessionView,
    pub sets: Vec<LoggedSet<Timestamp>>,
}

/// What a session asks for: its day, and for each exercise of the day its definition and the
/// targets computed by the progression engine from the history before the session.
#[cfg_attr(
    not(feature = "server"),
    allow(dead_code, reason = "decoded by the session screen (#28)")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPlan {
    pub session: SessionView,
    /// The day's name in the program.
    pub day_name: String,
    /// The day's exercises, in program order.
    pub exercises: Vec<PlannedExercise>,
}

/// The user's next session before it starts: the next day of the active program's rotation, in
/// its latest version, and the targets from every completed session so far. Starting the session
/// then gives the same day, and its [`SessionPlan`] the same targets.
#[cfg_attr(
    not(feature = "server"),
    allow(dead_code, reason = "decoded by the home screen (#28)")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextSessionPlan {
    pub program_id: ProgramId,
    pub program_version_id: ProgramVersionId,
    pub day: DayId,
    pub day_name: String,
    /// The day's exercises, in program order.
    pub exercises: Vec<PlannedExercise>,
}

/// One exercise of a [`SessionPlan`] or [`NextSessionPlan`].
#[cfg_attr(
    not(feature = "server"),
    allow(dead_code, reason = "decoded by the session screen (#28)")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedExercise {
    /// The exercise as the session's program version defines it on this day.
    pub exercise: Exercise,
    /// The sets to prefill, or a request for the training max.
    pub targets: NextTargets,
}

/// The end-of-session summary, computed from stored data up to this session: a retried
/// `finish_session` returns the same summary, unless the user changed a training max or their unit
/// in between (which `changes` and `needs_training_max` depend on).
#[cfg_attr(
    not(feature = "server"),
    allow(dead_code, reason = "decoded by the summary screen (#32)")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session: SessionView,
    /// Weight × reps of the working sets (warm-ups, body-weight and timed sets excluded).
    pub volume: Volume,
    /// Personal records set in this session, against every earlier completed session (any
    /// program). Empty unless the session was completed.
    pub prs: Vec<PrEvent>,
    /// What this session changed for each exercise of the day with a progression rule, in program
    /// order. Empty unless the session was completed.
    pub changes: Vec<ProgressionChange>,
    /// Exercises of the day loaded as a percentage of a training max the user has not entered.
    pub needs_training_max: Vec<ExerciseId>,
}

/// The program version and day the device started, sent with [`start_session`] so the server
/// records exactly what the lifter trains (the device may have chosen it offline, before the
/// previous session's finish reached the server).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartChoice {
    pub program_id: ProgramId,
    pub program_version_id: ProgramVersionId,
    pub day: DayId,
}

/// Starts a session.
///
/// `session_id` is a new client UUIDv7 and `started_at` the client's clock. With `choice`, the
/// session is recorded on that program version and day, as long as the version is one of the
/// user's and belongs to that program (else `409`) and has that day (else `422`); the rotation is
/// never applied again. Without it (older clients), the active program's latest version, on the
/// next day of its rotation.
///
/// Retrying with the same id and time returns the same session, even after it has ended. `409`
/// when the id was used with another start time, when another session is still in progress
/// (finish or abandon it first), or, without `choice`, when no program is active.
#[post("/api/sessions/start", state: Extension<AppState>, user: AuthUser)]
pub async fn start_session(
    session_id: SessionId,
    started_at: Timestamp,
    choice: Option<StartChoice>,
) -> Result<SessionView, ServerFnError> {
    Ok(sessions::start(&state.db, user.owner(), session_id, started_at, choice).await?)
}

/// One of the user's sessions. `404` when it does not exist or is someone else's.
#[post("/api/sessions/get", state: Extension<AppState>, user: AuthUser)]
pub async fn get_session(session_id: SessionId) -> Result<SessionView, ServerFnError> {
    Ok(sessions::get(&state.db, user.owner(), session_id).await?)
}

/// The user's session in progress with its sets, if any (the most recently started one).
#[post("/api/sessions/in-progress", state: Extension<AppState>, user: AuthUser)]
pub async fn get_in_progress_session() -> Result<Option<SessionWithSets>, ServerFnError> {
    Ok(sessions::in_progress(&state.db, user.owner()).await?)
}

/// The plan of one of the user's sessions: the day's exercises and their targets.
#[post("/api/sessions/plan", state: Extension<AppState>, user: AuthUser)]
pub async fn get_session_plan(session_id: SessionId) -> Result<SessionPlan, ServerFnError> {
    Ok(sessions::plan(&state.db, user.owner(), session_id).await?)
}

/// The plan of the user's next session, before starting it (the "today" screen). `409` when no
/// program is active.
#[post("/api/sessions/next-plan", state: Extension<AppState>, user: AuthUser)]
pub async fn get_next_session_plan() -> Result<NextSessionPlan, ServerFnError> {
    Ok(sessions::next_plan(&state.db, user.owner()).await?)
}

/// Logs a set in one of the user's in-progress sessions. Idempotent on `set.id`: the same set again
/// succeeds (even after the session ended), the same id with other values is a `409`. `409` for a
/// new set in an ended session, `422` for a set completed before the session started, `404` for a
/// session that is not the user's.
///
/// `set_index` numbers the sets of one exercise and kind (warm-up or working) from 0: the
/// prescribed working sets are `0..n`, extras (a top single, back-off sets) `n` and up, and a
/// skipped working set leaves a gap (see the progression engine).
#[post("/api/sessions/save-set", state: Extension<AppState>, user: AuthUser)]
pub async fn save_set(
    session_id: SessionId,
    set: LoggedSet<Timestamp>,
) -> Result<(), ServerFnError> {
    Ok(sessions::save_set(&state.db, user.owner(), session_id, &set).await?)
}

/// Ends one of the user's sessions and returns its summary. Idempotent: the same outcome and time
/// again returns the same summary; another outcome or time is a `409`. `422` when `finished_at` is
/// before the start or before a logged set.
#[post("/api/sessions/finish", state: Extension<AppState>, user: AuthUser)]
pub async fn finish_session(
    session_id: SessionId,
    outcome: SessionOutcome,
    finished_at: Timestamp,
) -> Result<SessionSummary, ServerFnError> {
    Ok(sessions::finish(&state.db, user.owner(), session_id, outcome, finished_at).await?)
}
