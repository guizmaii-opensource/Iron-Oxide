//! History server functions (#20): the list of past sessions, one session's sets, and the
//! per-exercise series behind the charts (#33).
//!
//! The history is the user's ended sessions (completed, skipped or abandoned), most recently
//! finished first. Weights travel as the domain [`Weight`] (kg numbers on the wire, exact
//! nanograms inside); the UI converts them to the user's unit for display.

use dioxus::prelude::*;
use iron_oxide_domain::{
    DayId, ExerciseId, Lift, LoggedSet, ProgramId, ProgramVersionId, SessionId, SessionStatus,
    Volume, Weight, time::Timestamp,
};
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use {
    crate::server::{AppState, api::history, auth::AuthUser},
    dioxus::server::axum::Extension,
};

/// The default number of sessions per page.
pub const DEFAULT_PAGE_SIZE: u32 = 20;
/// The largest page [`history_page`] accepts.
pub const MAX_PAGE_SIZE: u32 = 100;

/// Where the next page of [`history_page`] starts. Opaque to the client: pass back the one a
/// page returned.
///
/// It keeps the session's finish time to the microsecond (the database's precision), so no
/// session is skipped or repeated between pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HistoryCursor {
    finished_at_us: i64,
    id: SessionId,
}

#[cfg(feature = "server")]
impl HistoryCursor {
    /// A cursor after the session `id`, finished `finished_at_us` microseconds after the epoch.
    pub(crate) const fn new(finished_at_us: i64, id: SessionId) -> Self {
        Self { finished_at_us, id }
    }

    /// The finish time of the last session of the page, in microseconds since the epoch.
    pub(crate) const fn finished_at_us(self) -> i64 {
        self.finished_at_us
    }

    /// The id of the last session of the page.
    pub(crate) const fn id(self) -> SessionId {
        self.id
    }
}

/// One page of the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    /// Most recently finished first.
    pub sessions: Vec<SessionSummary>,
    /// The cursor of the next page, `None` on the last page.
    pub next: Option<HistoryCursor>,
}

/// A session as the history list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub program_id: ProgramId,
    /// The program's current name.
    pub program_name: String,
    pub program_version_id: ProgramVersionId,
    /// The version number the session was run from (1, 2, ...).
    pub program_version: u32,
    pub day_id: DayId,
    /// The day's name in the session's own program version, as it was when the session ran (a
    /// later version renaming the day does not rename it here). `None` if that version has no
    /// such day.
    pub day_name: Option<String>,
    pub status: SessionStatus,
    pub started_at: Timestamp,
    /// `None` only for a session still in progress (never in the history list).
    pub finished_at: Option<Timestamp>,
    /// Sets logged that are not warm-ups.
    pub working_sets: u32,
    /// Weight × reps of the working sets (warm-ups, body-weight and timed sets add nothing), as
    /// the end-of-session summary counts it.
    pub volume: Volume,
    /// Whether the session set a personal record (heaviest weight, best e1RM or most reps at a
    /// weight), exactly as its end-of-session summary reports them: only completed sessions,
    /// against the completed sessions started before it.
    pub set_pr: bool,
}

/// One session with its sets, grouped by exercise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDetails {
    pub session: SessionSummary,
    /// In the order each exercise was first logged.
    pub exercises: Vec<ExerciseLog>,
}

/// The sets of one exercise in a session, with the statistics the details screen shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExerciseLog {
    pub exercise_id: ExerciseId,
    /// In the order they were completed.
    pub sets: Vec<LoggedSet<Timestamp>>,
    /// The heaviest working set (then the most reps); `None` without a weighted working set.
    pub top_set: Option<Lift>,
    /// The best estimated one-rep max over the working sets (Epley).
    pub best_e1rm: Option<Weight>,
    /// Weight × reps of the working sets.
    pub volume: Volume,
}

/// Where a chart point sits: the session's start, then its id (so two sessions started in the same
/// millisecond still have distinct, ordered keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SeriesKey {
    pub started_at: Timestamp,
    pub session_id: SessionId,
}

/// One session of an exercise's chart series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExercisePoint {
    pub key: SeriesKey,
    /// The heaviest working set (then the most reps).
    pub top_set: Lift,
    /// The best estimated one-rep max (Epley); `None` when every set has too many reps.
    pub best_e1rm: Option<Weight>,
    /// Weight × reps of the exercise's working sets in the session.
    pub volume: Volume,
}

/// The chart series of one exercise: one point per ended session with a weighted working set,
/// oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExerciseSeries {
    pub exercise_id: ExerciseId,
    pub points: Vec<ExercisePoint>,
}

/// An exercise the user has logged in an ended session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggedExercise {
    pub exercise_id: ExerciseId,
    /// In how many ended sessions.
    pub sessions: u32,
    /// The start of the most recent one.
    pub last_session_at: Timestamp,
}

/// A page of the signed-in user's history. `cursor` is the previous page's `next` (`None` for the
/// first page); `limit` defaults to [`DEFAULT_PAGE_SIZE`] and must be 1 to [`MAX_PAGE_SIZE`].
///
/// # Errors
/// 422 for a limit out of range or a cursor that is not a valid time.
#[post("/api/history/page", state: Extension<AppState>, user: AuthUser)]
pub async fn history_page(
    cursor: Option<HistoryCursor>,
    limit: Option<u32>,
) -> Result<HistoryPage, ServerFnError> {
    Ok(history::page(&state.db, user.owner(), cursor, limit).await?)
}

/// One of the signed-in user's sessions (ended or in progress) with its sets by exercise.
///
/// # Errors
/// 404 when the user has no session with that id (whether it does not exist or is someone
/// else's).
#[post("/api/history/session", state: Extension<AppState>, user: AuthUser)]
pub async fn session_details(session_id: SessionId) -> Result<SessionDetails, ServerFnError> {
    Ok(history::details(&state.db, user.owner(), session_id).await?)
}

/// The chart series of one exercise: per ended session, the top set, the best e1RM and the volume.
/// Empty when the user never logged a weighted working set of it.
///
/// # Errors
/// 422 when `exercise_id` is not a valid exercise id (a slug).
#[post("/api/history/exercise-series", state: Extension<AppState>, user: AuthUser)]
pub async fn exercise_series(exercise_id: String) -> Result<ExerciseSeries, ServerFnError> {
    Ok(history::series(&state.db, user.owner(), &exercise_id).await?)
}

/// The exercises the signed-in user has logged in ended sessions, most recently trained first.
#[post("/api/history/exercises", state: Extension<AppState>, user: AuthUser)]
pub async fn logged_exercises() -> Result<Vec<LoggedExercise>, ServerFnError> {
    Ok(history::exercises(&state.db, user.owner()).await?)
}
