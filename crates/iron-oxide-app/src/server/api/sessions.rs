//! Sessions (#18): the logic behind `crate::api::sessions`.
//!
//! Stored data is converted to the domain types (`Session`, `SessionLog`, `LoggedSet`, `Program`)
//! and the domain decides: the day rotation (`next_day`), the set and end rules (`SessionLog`), the
//! targets and progression changes (`next_targets`) and the records (`detect_prs`). The repository
//! stays the authority for idempotency and races.
//!
//! # History given to the domain
//!
//! As agreed on #18 and #12:
//!
//! - **Rotation:** every session of the active program (all versions, any status); `next_day`
//!   only counts the completed and skipped ones.
//! - **Progression:** for each exercise, the **completed** sessions of the session's program (all
//!   versions) that started before the planned session (for a summary: up to and including it),
//!   each judged against **its own prescription**: the exercise as defined on its day in the
//!   program version it was run from. For an exercise loaded as a percentage of the training max,
//!   only the sets completed after the training max was last entered (`set_at`). The training max
//!   the engine returns is shown, never stored.
//! - **Records:** the completed sessions of **any** program that started before the session.

use std::collections::{BTreeMap, HashMap};

use dioxus::logger::tracing;
use iron_oxide_domain::{
    DayId, E1rmFormula, ExerciseId, LoggedSet, PerformedSet, Reps, Seconds, Session, SessionId,
    SessionLog, SessionOutcome, SessionStatus, Unit, Weight, detect_prs, next_day,
    program::{Load, Program},
    progression::{
        NextTargets, Prescription, ProgressionSettings, SetTarget, exercise_history, next_targets,
    },
    session_volume,
    time::Timestamp,
};
use sqlx::PgPool;

use super::{ApiError, error::SESSION_IN_PROGRESS, offset_date_time, timestamp};
use crate::api::sessions::{
    NextSessionPlan, PlannedExercise, SessionPlan, SessionSummary, SessionView, SessionWithSets,
    StartChoice,
};
use crate::server::db::{
    self,
    error::RepoError,
    ids::{self, ProgramVersionId, UserId},
    sessions::{Cursor, WorkoutSession},
    training_maxes::TrainingMax,
};

/// `owner`'s session `id`.
pub async fn get(pool: &PgPool, owner: UserId, id: SessionId) -> Result<SessionView, ApiError> {
    view(db::sessions::get(pool, owner, id.into()).await?)
}

/// Starts session `id`: on the program version and day the device chose, or else on the active
/// program's next day. See `crate::api::sessions::start_session`.
pub async fn start(
    pool: &PgPool,
    owner: UserId,
    id: SessionId,
    started_at: Timestamp,
    choice: Option<StartChoice>,
) -> Result<SessionView, ApiError> {
    let started = offset_date_time(started_at)?;
    // A retry returns the session it created, whatever happened since (ended, a new program
    // version, another active program): the day is never computed again.
    match db::sessions::get(pool, owner, id.into()).await {
        Ok(existing) if existing.started_at == started => return view(existing),
        Ok(_) => return Err(ApiError::conflict(ID_REUSED)),
        Err(RepoError::NotFound) => {}
        Err(error) => return Err(error.into()),
    }
    // Two devices starting two different sessions at the same moment can both pass this check:
    // the database's one-in-progress index then refuses the second insert, with the same 409.
    if let Some(current) = db::sessions::get_in_progress(pool, owner).await?
        && current.id != id.into()
    {
        return Err(ApiError::conflict(SESSION_IN_PROGRESS));
    }
    let (version, day) = match choice {
        Some(choice) => chosen(pool, owner, &choice).await?,
        None => {
            let next = Next::load(pool, owner).await?;
            (next.version, next.day)
        }
    };
    let new = db::sessions::NewSession {
        id: id.into(),
        program_version_id: version,
        day_id: day.as_str().to_owned(),
        started_at: started,
    };
    db::sessions::start(pool, owner, &new).await?;
    get(pool, owner, id).await
}

/// Shown when a start names a program version that is not the user's, or not of that program.
pub const START_VERSION_GONE: &str =
    "This workout's program is not available any more. Discard it and start again.";

/// Shown when a start names a day its program version does not have.
pub const START_DAY_MISSING: &str = "This workout's day is not in its program.";

/// The version and day of a start that names them: checked, never re-picked.
async fn chosen(
    pool: &PgPool,
    owner: UserId,
    choice: &StartChoice,
) -> Result<(ProgramVersionId, DayId), ApiError> {
    let version =
        match db::programs::get_version(pool, owner, choice.program_version_id.into()).await {
            Ok(version) => version,
            Err(RepoError::NotFound) => return Err(ApiError::conflict(START_VERSION_GONE)),
            Err(error) => return Err(error.into()),
        };
    if version.program_id != choice.program_id.into() {
        return Err(ApiError::conflict(START_VERSION_GONE));
    }
    let program = parse_program(&version.document)?;
    if program.day(&choice.day).is_none() {
        return Err(ApiError::invalid(START_DAY_MISSING));
    }
    Ok((version.id, choice.day.clone()))
}

/// What the user trains next: the active program's latest version and the next day of its
/// rotation.
struct Next {
    program_id: ids::ProgramId,
    version: ProgramVersionId,
    program: Program,
    day: DayId,
}

impl Next {
    async fn load(pool: &PgPool, owner: UserId) -> Result<Self, ApiError> {
        let program_id = db::active_program::get(pool, owner)
            .await?
            .ok_or_else(|| ApiError::conflict("Choose a program first."))?;
        let version = db::programs::latest_version(pool, owner, program_id).await?;
        let program = parse_program(&version.document)?;
        let history = db::sessions::list_in_program(pool, owner, program_id)
            .await?
            .iter()
            .map(domain_session)
            .collect::<Result<Vec<_>, _>>()?;
        let day = next_day(&program.rotation, &history)
            .map_err(|error| {
                tracing::warn!(%error, "cannot pick the next day");
                ApiError::conflict(
                    "This program repeats a day in its rotation, which is not supported yet.",
                )
            })?
            .clone();
        Ok(Self {
            program_id,
            version: version.id,
            program,
            day,
        })
    }
}

/// The plan of the user's next session, before starting it: the next day of the active program
/// and its targets from every completed session so far.
pub async fn next_plan(pool: &PgPool, owner: UserId) -> Result<NextSessionPlan, ApiError> {
    let next = Next::load(pool, owner).await?;
    let day = next
        .program
        .day(&next.day)
        .ok_or_else(|| ApiError::internal("the rotation names a day the program lacks"))?
        .clone();
    let mut context = Context::load(pool, owner, next.program_id).await?;
    let mut exercises = Vec::with_capacity(day.exercises.len());
    for exercise in day.exercises {
        let targets = context.targets(pool, owner, &exercise, Bound::All).await?;
        exercises.push(PlannedExercise { exercise, targets });
    }
    Ok(NextSessionPlan {
        program_id: next.program_id.into(),
        program_version_id: next.version.into(),
        day: next.day,
        day_name: day.name,
        exercises,
    })
}

/// The public message when a client id comes back with other content.
const ID_REUSED: &str = "This was already saved with different values.";

/// `owner`'s most recently started session in progress, with its sets.
pub async fn in_progress(
    pool: &PgPool,
    owner: UserId,
) -> Result<Option<SessionWithSets>, ApiError> {
    let Some(session) = db::sessions::get_in_progress(pool, owner).await? else {
        return Ok(None);
    };
    let sets = db::sets::list_for_session(pool, owner, session.id)
        .await?
        .iter()
        .map(domain_set)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(SessionWithSets {
        session: view(session)?,
        sets,
    }))
}

/// Logs `set` in `owner`'s session `session_id`. See `crate::api::sessions::save_set`.
pub async fn save_set(
    pool: &PgPool,
    owner: UserId,
    session_id: SessionId,
    set: &LoggedSet<Timestamp>,
) -> Result<(), ApiError> {
    let row = repo_set(session_id, set)?;
    let (session, sets) = load_parts(pool, owner, session_id).await?;
    // A retry is recognised on the stored values as they are, before `lenient_log` clamps them.
    if let Some(stored) = sets.iter().find(|stored| stored.id == set.id) {
        return if stored == set {
            Ok(())
        } else {
            Err(ApiError::conflict(
                "This set was already saved with different values.",
            ))
        };
    }
    // The domain's rules (ended session, completed before the start), then the repository, which
    // settles races: a concurrent finish or duplicate, the id used in another session.
    lenient_log(session, sets)?.add_set(set.clone())?;
    db::sets::upsert_idempotent(pool, owner, &row).await?;
    Ok(())
}

/// Ends `owner`'s session `session_id` and returns its summary. See
/// `crate::api::sessions::finish_session`.
pub async fn finish(
    pool: &PgPool,
    owner: UserId,
    session_id: SessionId,
    outcome: SessionOutcome,
    finished_at: Timestamp,
) -> Result<SessionSummary, ApiError> {
    let finished = offset_date_time(finished_at)?;
    let mut log = load_log(pool, owner, session_id).await?;
    log.end(outcome, finished_at)?;
    db::sessions::finish(
        pool,
        owner,
        session_id.into(),
        repo_outcome(outcome),
        finished,
    )
    .await?;
    summary(pool, owner, session_id).await
}

/// The plan of `owner`'s session `session_id`: its day's exercises and their targets, from the
/// history before it.
pub async fn plan(
    pool: &PgPool,
    owner: UserId,
    session_id: SessionId,
) -> Result<SessionPlan, ApiError> {
    let stored = db::sessions::get(pool, owner, session_id.into()).await?;
    let mut context = Context::load(pool, owner, stored.program_id).await?;
    let program = context
        .program(pool, owner, stored.program_version_id)
        .await?;
    let program = program.ok_or_else(|| ApiError::internal("the session's version is invalid"))?;
    let day_id = DayId::new(stored.day_id.clone()).map_err(ApiError::internal)?;
    let day = program
        .day(&day_id)
        .ok_or_else(|| ApiError::internal("the session's day is not in its version"))?
        .clone();
    let before = Bound::Before(stored.cursor());
    let mut exercises = Vec::with_capacity(day.exercises.len());
    for exercise in day.exercises {
        let targets = context.targets(pool, owner, &exercise, before).await?;
        exercises.push(PlannedExercise { exercise, targets });
    }
    Ok(SessionPlan {
        session: view(stored)?,
        day_name: day.name,
        exercises,
    })
}

/// The summary of `owner`'s session `session_id`, from stored data only (so a retry gets the
/// same one).
async fn summary(
    pool: &PgPool,
    owner: UserId,
    session_id: SessionId,
) -> Result<SessionSummary, ApiError> {
    let stored = db::sessions::get(pool, owner, session_id.into()).await?;
    let sets = db::sets::list_for_session(pool, owner, stored.id)
        .await?
        .iter()
        .map(domain_set)
        .collect::<Result<Vec<_>, _>>()?;
    let volume = session_volume(sets.iter().filter_map(performed));
    let mut summary = SessionSummary {
        volume,
        prs: Vec::new(),
        changes: Vec::new(),
        needs_training_max: Vec::new(),
        session: view(stored.clone())?,
    };
    if stored.status != db::sessions::SessionStatus::Completed {
        return Ok(summary);
    }

    // Records: each exercise of the session, in the order it was first logged.
    let mut logged: Vec<&ExerciseId> = Vec::new();
    for set in &sets {
        if !logged.contains(&&set.exercise) {
            logged.push(&set.exercise);
        }
    }
    let names: Vec<String> = logged.iter().map(|id| id.as_str().to_owned()).collect();
    let earlier = db::sets::completed_for_exercises_before(pool, owner, &names, stored.cursor())
        .await?
        .iter()
        .map(domain_set)
        .collect::<Result<Vec<_>, _>>()?;
    for exercise in logged {
        let history = earlier
            .iter()
            .filter(|set| &set.exercise == exercise)
            .filter_map(performed);
        summary.prs.extend(detect_prs(
            exercise,
            history,
            sets.iter()
                .filter(|set| &set.exercise == exercise)
                .filter_map(performed),
            E1rmFormula::STANDARD,
        ));
    }

    // Progression: each exercise of the day this session did working sets of.
    let mut context = Context::load(pool, owner, stored.program_id).await?;
    let Some(program) = context
        .program(pool, owner, stored.program_version_id)
        .await?
    else {
        return Ok(summary);
    };
    let day_id = DayId::new(stored.day_id.clone()).map_err(ApiError::internal)?;
    let Some(day) = program.day(&day_id).cloned() else {
        return Ok(summary);
    };
    let until = Bound::Through(stored.cursor());
    for exercise in &day.exercises {
        let worked = sets
            .iter()
            .any(|set| !set.warm_up && set.exercise == exercise.id);
        if !worked {
            continue;
        }
        match context.targets(pool, owner, exercise, until).await? {
            NextTargets::Ready(targets) => summary.changes.extend(targets.change),
            NextTargets::NeedsTrainingMax { exercise } => summary.needs_training_max.push(exercise),
        }
    }
    Ok(summary)
}

/// Which sessions a history includes, by (start, id) as the repository orders them.
#[derive(Debug, Clone, Copy)]
enum Bound {
    /// Strictly before this session: the targets it was planned with.
    Before(Cursor),
    /// Every completed session: the next session's targets.
    All,
    /// Up to and including it: what it changed.
    Through(Cursor),
}

impl Bound {
    fn includes(self, session: &WorkoutSession) -> bool {
        let key = (session.started_at, session.id);
        match self {
            Self::All => true,
            Self::Before(cursor) => key < (cursor.started_at, cursor.id),
            Self::Through(cursor) => key <= (cursor.started_at, cursor.id),
        }
    }
}

/// What the progression engine needs for one session's program: its completed sessions and
/// their sets, the parsed program versions, the training maxes and the settings.
struct Context {
    /// Completed sessions of the program, oldest first.
    sessions: Vec<WorkoutSession>,
    /// Their sets, by session.
    sets: HashMap<ids::SessionId, Vec<LoggedSet<Timestamp>>>,
    /// Parsed versions; `None` for one that no longer parses.
    programs: HashMap<ProgramVersionId, Option<Program>>,
    training_maxes: Vec<TrainingMax>,
    settings: ProgressionSettings,
}

impl Context {
    async fn load(pool: &PgPool, owner: UserId, program: ids::ProgramId) -> Result<Self, ApiError> {
        let sessions: Vec<WorkoutSession> = db::sessions::list_in_program(pool, owner, program)
            .await?
            .into_iter()
            .filter(|session| session.status == db::sessions::SessionStatus::Completed)
            .collect();
        let mut sets: HashMap<ids::SessionId, Vec<LoggedSet<Timestamp>>> = HashMap::new();
        for set in db::sets::completed_in_program(pool, owner, program).await? {
            sets.entry(set.session_id)
                .or_default()
                .push(domain_set(&set)?);
        }
        let settings = db::settings::find(pool, owner)
            .await?
            .unwrap_or_else(db::settings::UserSettings::defaults);
        let unit = match settings.unit {
            db::settings::Unit::Kg => Unit::Kg,
            db::settings::Unit::Lb => Unit::Lb,
        };
        Ok(Self {
            sessions,
            sets,
            programs: HashMap::new(),
            training_maxes: db::training_maxes::list(pool, owner).await?,
            settings: ProgressionSettings::for_unit(unit),
        })
    }

    /// Program version `id`, parsed once. `None` (logged) for a stored document that no longer
    /// parses: its sessions are then not judged.
    async fn program(
        &mut self,
        pool: &PgPool,
        owner: UserId,
        id: ProgramVersionId,
    ) -> Result<Option<Program>, ApiError> {
        if let Some(program) = self.programs.get(&id) {
            return Ok(program.clone());
        }
        let version = db::programs::get_version(pool, owner, id).await?;
        let program = match parse_program(&version.document) {
            Ok(program) => Some(program),
            Err(error) => {
                tracing::warn!(%error, ?id, "a stored program version does not parse");
                None
            }
        };
        self.programs.insert(id, program.clone());
        Ok(program)
    }

    /// The targets of `exercise` from the sessions in `bound`.
    async fn targets(
        &mut self,
        pool: &PgPool,
        owner: UserId,
        exercise: &iron_oxide_domain::program::Exercise,
        bound: Bound,
    ) -> Result<NextTargets, ApiError> {
        let training_max = self
            .training_maxes
            .iter()
            .find(|max| max.exercise_id == exercise.id.as_str())
            .cloned();
        let since = match (exercise.load, &training_max) {
            (Some(Load::PercentOfTrainingMax(_)), Some(max)) => Some(max.set_at),
            _ => None,
        };
        let sessions: Vec<WorkoutSession> = self
            .sessions
            .iter()
            .filter(|session| bound.includes(session))
            .cloned()
            .collect();
        let mut logs = Vec::with_capacity(sessions.len());
        let mut prescriptions = BTreeMap::new();
        for session in &sessions {
            let sets: Vec<LoggedSet<Timestamp>> = self
                .sets
                .get(&session.id)
                .into_iter()
                .flatten()
                .filter(|set| set.exercise == exercise.id)
                .filter(|set| since.is_none_or(|since| set.completed_at > timestamp_or_min(since)))
                .cloned()
                .collect();
            if sets.is_empty() {
                continue;
            }
            let program = self
                .program(pool, owner, session.program_version_id)
                .await?;
            let day = DayId::new(session.day_id.clone()).map_err(ApiError::internal)?;
            let prescription = program
                .as_ref()
                .and_then(|program| Prescription::in_program(program, &day, &exercise.id));
            prescriptions.insert(iron_oxide_domain::SessionId::from(session.id), prescription);
            logs.push(lenient_log(domain_session(session)?, sets)?);
        }
        let history = exercise_history(&exercise.id, &logs, |session: &Session<Timestamp>| {
            prescriptions.get(&session.id()).copied().flatten()
        });
        let training_max = training_max
            .map(|max| Weight::from_nanograms(max.weight_ng))
            .transpose()
            .map_err(ApiError::internal)?;
        Ok(next_targets(
            exercise,
            training_max,
            self.settings,
            &history,
        ))
    }
}

/// The set for the statistics (`None` for body-weight and timed sets).
fn performed(set: &LoggedSet<Timestamp>) -> Option<PerformedSet> {
    set.into()
}

/// `time` as a timestamp, or the earliest one when it does not fit (only a corrupt value would).
fn timestamp_or_min(time: sqlx::types::time::OffsetDateTime) -> Timestamp {
    timestamp(time).unwrap_or(Timestamp::from_epoch_millis(i64::MIN))
}

/// `owner`'s session `id` with its sets, as the domain aggregate.
async fn load_log(
    pool: &PgPool,
    owner: UserId,
    id: SessionId,
) -> Result<SessionLog<Timestamp>, ApiError> {
    let (session, sets) = load_parts(pool, owner, id).await?;
    lenient_log(session, sets)
}

/// `owner`'s session `id` and its sets, as stored.
async fn load_parts(
    pool: &PgPool,
    owner: UserId,
    id: SessionId,
) -> Result<(Session<Timestamp>, Vec<LoggedSet<Timestamp>>), ApiError> {
    let stored = db::sessions::get(pool, owner, id.into()).await?;
    let sets = db::sets::list_for_session(pool, owner, stored.id)
        .await?
        .iter()
        .map(domain_set)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((domain_session(&stored)?, sets))
}

/// The stored session and sets as a [`SessionLog`].
///
/// Stored data went through the same rules, but the clocks are the clients' and two requests can
/// race (a set saved while its session is being finished), so a set may fall outside its
/// session's span. Such a set's time is clamped into the span for the domain: the time plays no
/// part in the rules applied to stored data (the sets' order is kept), and refusing would block
/// the session for good.
fn lenient_log(
    session: Session<Timestamp>,
    mut sets: Vec<LoggedSet<Timestamp>>,
) -> Result<SessionLog<Timestamp>, ApiError> {
    let start = session.started_at();
    let end = session
        .finished_at()
        .unwrap_or(Timestamp::from_epoch_millis(i64::MAX))
        .max(start);
    for set in &mut sets {
        set.completed_at = set.completed_at.clamp(start, end);
    }
    // Only duplicate set ids are left, which the primary key rules out.
    SessionLog::from_parts(session, sets).map_err(ApiError::internal)
}

/// A stored session as the client sees it.
pub fn view(session: WorkoutSession) -> Result<SessionView, ApiError> {
    Ok(SessionView {
        id: session.id.into(),
        program_id: session.program_id.into(),
        program_version_id: session.program_version_id.into(),
        day: DayId::new(session.day_id).map_err(ApiError::internal)?,
        status: status(session.status),
        started_at: timestamp(session.started_at)?,
        finished_at: session.finished_at.map(timestamp).transpose()?,
    })
}

/// A stored session as the domain's [`Session`], checking its invariants.
fn domain_session(session: &WorkoutSession) -> Result<Session<Timestamp>, ApiError> {
    Session::from_parts(
        session.id.into(),
        session.program_version_id.into(),
        DayId::new(session.day_id.clone()).map_err(ApiError::internal)?,
        timestamp(session.started_at)?,
        status(session.status),
        session.finished_at.map(timestamp).transpose()?,
    )
    .map_err(ApiError::internal)
}

pub(super) const fn status(status: db::sessions::SessionStatus) -> SessionStatus {
    match status {
        db::sessions::SessionStatus::InProgress => SessionStatus::InProgress,
        db::sessions::SessionStatus::Completed => SessionStatus::Completed,
        db::sessions::SessionStatus::Skipped => SessionStatus::Skipped,
        db::sessions::SessionStatus::Abandoned => SessionStatus::Abandoned,
    }
}

const fn repo_outcome(outcome: SessionOutcome) -> db::sessions::SessionOutcome {
    match outcome {
        SessionOutcome::Completed => db::sessions::SessionOutcome::Completed,
        SessionOutcome::Skipped => db::sessions::SessionOutcome::Skipped,
        SessionOutcome::Abandoned => db::sessions::SessionOutcome::Abandoned,
    }
}

/// A stored set as the domain's [`LoggedSet`].
fn domain_set(set: &db::sets::LoggedSet) -> Result<LoggedSet<Timestamp>, ApiError> {
    Ok(LoggedSet {
        id: set.id.into(),
        exercise: ExerciseId::new(set.exercise_id.clone()).map_err(ApiError::internal)?,
        set_index: set.set_index,
        reps: Reps::new(set.reps),
        weight: set
            .weight_ng
            .map(Weight::from_nanograms)
            .transpose()
            .map_err(ApiError::internal)?,
        duration: set.duration_s.map(Seconds::new),
        warm_up: set.warmup,
        completed_at: timestamp(set.completed_at)?,
        target: set.target.as_ref().map(domain_target).transpose()?,
    })
}

/// A stored target (#60) as the domain's [`SetTarget`]. It was checked when it was saved, so a
/// failure means the stored data is wrong.
pub(super) fn domain_target(target: &db::sets::Target) -> Result<SetTarget, ApiError> {
    Ok(SetTarget {
        weight: target
            .weight_ng
            .map(Weight::from_nanograms)
            .transpose()
            .map_err(ApiError::internal)?,
        goal: serde_json::from_value(target.goal.clone()).map_err(ApiError::internal)?,
    })
}

/// A target sent by the client, for the repository. It is what the client says it showed: the
/// server checks its types (when the set is decoded), not its values, which only ever move the
/// sender's own progression.
pub(super) fn stored_target(target: &SetTarget) -> Result<db::sets::Target, ApiError> {
    Ok(db::sets::Target {
        weight_ng: target.weight.map(Weight::as_nanograms),
        goal: serde_json::to_value(target.goal).map_err(ApiError::internal)?,
    })
}

/// A set sent by the client, for the repository.
fn repo_set(
    session: SessionId,
    set: &LoggedSet<Timestamp>,
) -> Result<db::sets::LoggedSet, ApiError> {
    Ok(db::sets::LoggedSet {
        id: set.id.into(),
        session_id: session.into(),
        exercise_id: set.exercise.as_str().to_owned(),
        set_index: set.set_index,
        reps: set.reps.get(),
        weight_ng: set.weight.map(Weight::as_nanograms),
        duration_s: set.duration.map(Seconds::get),
        warmup: set.warm_up,
        completed_at: offset_date_time(set.completed_at)?,
        target: set.target.as_ref().map(stored_target).transpose()?,
    })
}

/// A stored program document as the domain's [`Program`]. It was validated when it was saved;
/// failing now means the rules changed since, which is a bug to fix, not the user's doing.
fn parse_program(document: &serde_json::Value) -> Result<Program, ApiError> {
    Program::from_json(&document.to_string()).map_err(ApiError::internal)
}

#[cfg(test)]
mod tests {
    use dioxus::server::axum::http::StatusCode;
    use iron_oxide_domain::{
        PrKind, ProgramId, SetId,
        progression::{ChangeKind, TargetSource},
    };
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};
    use sqlx::types::Uuid;

    use super::*;
    use crate::server::{
        api::testing::{self, CallError, TestApi, TestUser},
        db::{testing as db_testing, training_maxes},
    };

    /// Builds a request body around an id.
    type BodyOf = Box<dyn Fn(Uuid) -> Value>;

    const START: &str = "/api/sessions/start";
    const GET: &str = "/api/sessions/get";
    const IN_PROGRESS: &str = "/api/sessions/in-progress";
    const PLAN: &str = "/api/sessions/plan";
    const NEXT_PLAN: &str = "/api/sessions/next-plan";
    const SAVE_SET: &str = "/api/sessions/save-set";
    const FINISH: &str = "/api/sessions/finish";

    /// A point in time, `minutes` after a fixed start.
    fn t(minutes: i64) -> Timestamp {
        Timestamp::from_epoch_millis(1_790_000_000_000 + minutes * 60_000)
    }

    fn kg(value: f64) -> Weight {
        Weight::from_kg(value).unwrap()
    }

    fn squat() -> ExerciseId {
        ExerciseId::new("back-squat").unwrap()
    }

    fn bench() -> ExerciseId {
        ExerciseId::new("bench-press").unwrap()
    }

    /// Day `a`: back squat 3 × 5 at 100 kg (+2.5 kg on success) and a plank. Day `b`: back squat
    /// as on `a`, and bench press 3 × 5 at 80 % of its training max. Rotation `a`, `b`.
    /// `squat_reps` changes the squat's reps on day `a` (a new version of the program).
    fn document(squat_reps: u16) -> Value {
        let squat = |reps: u16| {
            json!({
                "id": "back-squat",
                "name": "Back squat",
                "work": { "reps": { "sets": 3, "reps": reps } },
                "load": { "kg": 100 },
                "rest": 180,
                "progression": { "add_when_top_of_range": { "increment": { "kg": 2.5 } } }
            })
        };
        json!({
            "schema_version": 1,
            "name": "Test program",
            "days": [
                {
                    "id": "a",
                    "name": "Day A",
                    "exercises": [
                        squat(squat_reps),
                        {
                            "id": "plank",
                            "name": "Plank",
                            "work": { "hold": { "sets": 3, "seconds": 30 } },
                            "rest": 60
                        }
                    ]
                },
                {
                    "id": "b",
                    "name": "Day B",
                    "exercises": [
                        squat(5),
                        {
                            "id": "bench-press",
                            "name": "Bench press",
                            "work": { "reps": { "sets": 3, "reps": 5 } },
                            "load": { "percent_of_training_max": 80 },
                            "rest": 180,
                            "progression": { "training_max": { "increment": { "kg": 2.5 } } }
                        }
                    ]
                }
            ],
            "rotation": ["a", "b"]
        })
    }

    /// Gives `user` the test program and makes it their active one.
    async fn active_program(api: &TestApi, user: &TestUser) -> ProgramId {
        let (_, program, _) = db::programs::create(
            &api.db,
            user.id,
            db_testing::creation(),
            "Test program",
            &document(5),
            db_testing::unlimited,
        )
        .await
        .unwrap();
        db::active_program::set(&api.db, user.id, program.id)
            .await
            .unwrap();
        program.id.into()
    }

    async fn call<T: DeserializeOwned>(
        user: &mut TestUser,
        path: &str,
        body: Value,
    ) -> Result<T, CallError> {
        user.call(path, body).await
    }

    async fn start(
        user: &mut TestUser,
        id: SessionId,
        at: Timestamp,
    ) -> Result<SessionView, CallError> {
        call(user, START, json!({ "session_id": id, "started_at": at })).await
    }

    async fn save(
        user: &mut TestUser,
        session: SessionId,
        set: &LoggedSet<Timestamp>,
    ) -> Result<(), CallError> {
        call(user, SAVE_SET, json!({ "session_id": session, "set": set })).await
    }

    async fn finish(
        user: &mut TestUser,
        session: SessionId,
        outcome: SessionOutcome,
        at: Timestamp,
    ) -> Result<SessionSummary, CallError> {
        call(
            user,
            FINISH,
            json!({ "session_id": session, "outcome": outcome, "finished_at": at }),
        )
        .await
    }

    async fn plan(user: &mut TestUser, session: SessionId) -> Result<SessionPlan, CallError> {
        call(user, PLAN, json!({ "session_id": session })).await
    }

    /// A working set of `exercise`: `reps` at `weight` kg, set number `index`, at `at`.
    fn set(
        exercise: ExerciseId,
        index: u16,
        reps: u16,
        weight: f64,
        at: Timestamp,
    ) -> LoggedSet<Timestamp> {
        LoggedSet {
            id: SetId::new_v7(),
            exercise,
            set_index: index,
            reps: Reps::new(reps),
            weight: Some(kg(weight)),
            duration: None,
            warm_up: false,
            completed_at: at,
            target: None,
        }
    }

    /// Starts a session at `minute`, logs 3 × `reps` of squat at `weight`, and ends it.
    async fn squat_session(
        user: &mut TestUser,
        minute: i64,
        reps: u16,
        weight: f64,
        outcome: SessionOutcome,
    ) -> (SessionView, SessionSummary) {
        let id = SessionId::new_v7();
        let view = start(user, id, t(minute)).await.unwrap();
        for index in 0..3 {
            let at = t(minute + 1 + i64::from(index));
            save(user, id, &set(squat(), index, reps, weight, at))
                .await
                .unwrap();
        }
        let summary = finish(user, id, outcome, t(minute + 10)).await.unwrap();
        (view, summary)
    }

    async fn stored_sets(
        api: &TestApi,
        user: &TestUser,
        session: SessionId,
    ) -> Vec<db::sets::LoggedSet> {
        db::sets::list_for_session(&api.db, user.id, session.into())
            .await
            .unwrap()
    }

    fn assert_status<T: std::fmt::Debug>(
        result: Result<T, CallError>,
        status: StatusCode,
    ) -> String {
        match result {
            Err(error) if error.status == status => error.message,
            other => panic!("expected {status}, got {other:?}"),
        }
    }

    fn squat_targets(plan: &SessionPlan) -> &iron_oxide_domain::progression::ExerciseTargets {
        plan.exercises
            .iter()
            .find(|planned| planned.exercise.id == squat())
            .and_then(|planned| planned.targets.ready())
            .unwrap()
    }

    // --- Unit tests -----------------------------------------------------------------------------

    fn stored(day: &str, status: db::sessions::SessionStatus) -> WorkoutSession {
        WorkoutSession {
            id: ids::SessionId::from_uuid(Uuid::from_u128(1)),
            program_version_id: ProgramVersionId::from_uuid(Uuid::from_u128(2)),
            program_id: ids::ProgramId::from_uuid(Uuid::from_u128(3)),
            day_id: day.to_owned(),
            status,
            started_at: db_testing::at(0),
            finished_at: None,
        }
    }

    #[test]
    fn view_converts_every_status() {
        for (stored_status, expected) in [
            (
                db::sessions::SessionStatus::InProgress,
                SessionStatus::InProgress,
            ),
            (
                db::sessions::SessionStatus::Completed,
                SessionStatus::Completed,
            ),
            (db::sessions::SessionStatus::Skipped, SessionStatus::Skipped),
            (
                db::sessions::SessionStatus::Abandoned,
                SessionStatus::Abandoned,
            ),
        ] {
            assert_eq!(view(stored("a", stored_status)).unwrap().status, expected);
        }
        for outcome in [
            SessionOutcome::Completed,
            SessionOutcome::Skipped,
            SessionOutcome::Abandoned,
        ] {
            assert_eq!(
                status(db::sessions::SessionStatus::from(repo_outcome(outcome))),
                SessionStatus::from(outcome)
            );
        }
    }

    #[test]
    fn a_stored_day_that_is_not_a_slug_is_an_internal_error() {
        let error = view(stored("Day A", db::sessions::SessionStatus::InProgress)).unwrap_err();
        assert_eq!(error.public().0, 500);
        let error =
            domain_session(&stored("Day A", db::sessions::SessionStatus::InProgress)).unwrap_err();
        assert_eq!(error.public().0, 500);
    }

    #[test]
    fn a_stored_ended_session_without_an_end_is_an_internal_error() {
        let error =
            domain_session(&stored("a", db::sessions::SessionStatus::Completed)).unwrap_err();
        assert_eq!(error.public().0, 500);
    }

    #[test]
    fn sets_round_trip_between_the_client_and_the_repository() {
        let session = SessionId::new_v7();
        for set in [
            set(squat(), 3, 5, 102.5, t(1)),
            LoggedSet {
                weight: None,
                duration: Some(Seconds::new(45)),
                warm_up: true,
                ..set(ExerciseId::new("plank").unwrap(), 0, 1, 0.0, t(2))
            },
        ] {
            let row = repo_set(session, &set).unwrap();
            assert_eq!(row.session_id.as_uuid(), session.as_uuid());
            assert_eq!(domain_set(&row).unwrap(), set);
        }
        let far = LoggedSet {
            completed_at: Timestamp::from_epoch_millis(i64::MAX),
            ..set(squat(), 0, 5, 100.0, t(0))
        };
        assert_eq!(repo_set(session, &far).unwrap_err().public().0, 422);
    }

    #[test]
    fn bounds_order_sessions_by_start_then_id() {
        let mut session = stored("a", db::sessions::SessionStatus::Completed);
        let cursor = session.cursor();
        assert!(!Bound::Before(cursor).includes(&session));
        assert!(Bound::Through(cursor).includes(&session));
        session.id = ids::SessionId::from_uuid(Uuid::from_u128(0));
        assert!(Bound::Before(cursor).includes(&session));
        session.started_at = db_testing::at(1);
        assert!(!Bound::Through(cursor).includes(&session));
    }

    #[test]
    fn stored_sets_outside_their_session_are_clamped_into_it() {
        let session = Session::from_parts(
            SessionId::new_v7(),
            iron_oxide_domain::ProgramVersionId::new_v7(),
            DayId::new("a").unwrap(),
            t(10),
            SessionStatus::Completed,
            Some(t(20)),
        )
        .unwrap();
        let early = set(squat(), 0, 5, 100.0, t(5));
        let late = set(squat(), 1, 5, 100.0, t(25));
        let log = lenient_log(session, vec![early, late]).unwrap();
        let times: Vec<Timestamp> = log.sets().iter().map(|set| set.completed_at).collect();
        assert_eq!(times, [t(10), t(20)]);
        assert_eq!(log.sets()[0].set_index, 0, "order kept");
    }

    // --- Behaviour ------------------------------------------------------------------------------

    /// The outbox's expected-user header (#30): a write queued for another account is refused
    /// with `409` and changes nothing; a matching header or none at all is unaffected.
    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn expected_user_header_refuses_writes_for_another_account(db: PgPool) {
        use crate::auth::types::{ACCOUNT_CHANGED_MESSAGE, EXPECTED_USER_HEADER};
        use dioxus::server::axum::body::Body;

        let api = TestApi::new(db).await;
        let (mut a, b) = api.users_a_and_b().await;
        active_program(&api, &a).await;
        let session_id = SessionId::new_v7();
        let body = json!({ "session_id": session_id, "started_at": t(0) }).to_string();
        let request = |user: &TestUser, expected: Option<Uuid>| {
            let builder = user.post(START);
            let builder = match expected {
                Some(user) => builder.header(EXPECTED_USER_HEADER, user.to_string()),
                None => builder,
            };
            builder.body(Body::from(body.clone())).unwrap()
        };

        // Queued for B, sent with A's cookie: refused, nothing created.
        let (status, error) = a.send(request(&a, Some(b.id.as_uuid()))).await;
        assert_eq!(status, StatusCode::CONFLICT, "{error}");
        assert_eq!(error["message"], ACCOUNT_CHANGED_MESSAGE);
        let missing = a
            .call_err(GET, json!({ "session_id": session_id.as_uuid() }))
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);

        // A's own id, or no header: unaffected.
        let (status, view) = a.send(request(&a, Some(a.id.as_uuid()))).await;
        assert_eq!(status, StatusCode::OK, "{view}");
        let (status, replay) = a.send(request(&a, None)).await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(view, replay);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn get_session_returns_the_users_own_session(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let session = db_testing::session(&api.db, a.id).await;
        db::sessions::finish(
            &api.db,
            a.id,
            session,
            db::sessions::SessionOutcome::Completed,
            db_testing::at(90),
        )
        .await
        .unwrap();
        let view: SessionView = call(&mut a, GET, json!({ "session_id": session.as_uuid() }))
            .await
            .unwrap();
        assert_eq!(view.id.as_uuid(), session.as_uuid());
        assert_eq!(view.day.as_str(), "a");
        assert_eq!(view.status, SessionStatus::Completed);
        assert_eq!(view.started_at.epoch_millis(), 1_790_000_000_000);
        assert_eq!(
            view.finished_at.map(Timestamp::epoch_millis),
            Some(1_790_000_090_000)
        );
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn arguments_that_do_not_decode_are_422(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        for (path, body) in [
            (GET, json!({ "session_id": "not-a-uuid" })),
            (GET, json!({ "session_id": 5 })),
            (GET, json!({})),
            (
                START,
                json!({ "session_id": SessionId::new_v7(), "started_at": "noon" }),
            ),
            (
                FINISH,
                json!({ "session_id": SessionId::new_v7(), "outcome": "won", "finished_at": 1 }),
            ),
            (
                SAVE_SET,
                json!({ "session_id": SessionId::new_v7(), "set": { "id": "x" } }),
            ),
        ] {
            let error = a.call_err(path, body).await;
            assert_eq!(
                error,
                CallError {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    message: "Invalid request.".to_owned(),
                },
                "{path}"
            );
        }
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn a_database_outage_is_a_retryable_503(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let session = db_testing::session(&api.db, a.id).await;
        // The session load in `AuthUser` is the first query to fail.
        api.db.close().await;
        let error = a
            .call_err(GET, json!({ "session_id": session.as_uuid() }))
            .await;
        assert_eq!(
            error,
            CallError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: crate::server::api::error::TRANSIENT.to_owned(),
            }
        );
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn every_endpoint_needs_a_signed_in_user(db: PgPool) {
        let api = TestApi::new(db).await;
        let a = api.user("A").await;
        let session = db_testing::session(&api.db, a.id).await.as_uuid();
        let set = set(squat(), 0, 5, 100.0, t(1));
        for (path, body) in [
            (START, json!({ "session_id": session, "started_at": t(0) })),
            (GET, json!({ "session_id": session })),
            (IN_PROGRESS, json!({})),
            (NEXT_PLAN, json!({})),
            (PLAN, json!({ "session_id": session })),
            (SAVE_SET, json!({ "session_id": session, "set": set })),
            (
                FINISH,
                json!({ "session_id": session, "outcome": "completed", "finished_at": t(9) }),
            ),
        ] {
            testing::assert_unauthorized_when_signed_out(&api, path, body).await;
        }
        assert!(
            stored_sets(&api, &a, SessionId::from_uuid(session))
                .await
                .is_empty()
        );
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn start_session_follows_the_rotation_of_the_active_program(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let program = active_program(&api, &a).await;

        let (first, _) = squat_session(&mut a, 0, 5, 100.0, SessionOutcome::Completed).await;
        assert_eq!(first.day.as_str(), "a");
        assert_eq!(first.program_id, program);
        assert_eq!(first.status, SessionStatus::InProgress);
        assert_eq!(first.started_at, t(0));

        // Abandoned: day b comes up again. Skipped: the rotation moves on.
        let (second, _) = squat_session(&mut a, 60, 5, 100.0, SessionOutcome::Abandoned).await;
        assert_eq!(second.day.as_str(), "b");
        let third = start(&mut a, SessionId::new_v7(), t(120)).await.unwrap();
        assert_eq!(third.day.as_str(), "b");
        finish(&mut a, third.id, SessionOutcome::Skipped, t(121))
            .await
            .unwrap();
        let fourth = start(&mut a, SessionId::new_v7(), t(180)).await.unwrap();
        assert_eq!(fourth.day.as_str(), "a");
        finish(&mut a, fourth.id, SessionOutcome::Abandoned, t(181))
            .await
            .unwrap();

        // Another program starts at its first day: the history is filtered to the active program.
        let (_, other, _) = db::programs::create(
            &api.db,
            a.id,
            db_testing::creation(),
            "Other",
            &document(5),
            db_testing::unlimited,
        )
        .await
        .unwrap();
        db::active_program::set(&api.db, a.id, other.id)
            .await
            .unwrap();
        let fifth = start(&mut a, SessionId::new_v7(), t(240)).await.unwrap();
        assert_eq!(fifth.day.as_str(), "a");
        assert_eq!(fifth.program_id, other.id.into());
    }

    /// Review of #113: a start queued behind the previous finish is recorded on the day the
    /// device chose, not on the next day of the rotation once that finish arrived.
    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn start_session_records_the_chosen_day(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let program = active_program(&api, &a).await;
        let version = db::programs::latest_version(&api.db, a.id, program.into())
            .await
            .unwrap();
        // Day a done: the rotation now says b, but the device started a (again) offline.
        squat_session(&mut a, 0, 5, 100.0, SessionOutcome::Completed).await;
        let choice = |day: &str| {
            json!({
                "program_id": program,
                "program_version_id": iron_oxide_domain::ProgramVersionId::from(version.id),
                "day": day,
            })
        };
        let id = SessionId::new_v7();
        let body = json!({ "session_id": id, "started_at": t(60), "choice": choice("a") });
        let view: SessionView = call(&mut a, START, body.clone()).await.unwrap();
        assert_eq!(view.day.as_str(), "a");
        assert_eq!(view.program_version_id, version.id.into());
        // A replay returns it unchanged.
        let again: SessionView = call(&mut a, START, body).await.unwrap();
        assert_eq!(again, view);
        finish(&mut a, id, SessionOutcome::Abandoned, t(61))
            .await
            .unwrap();

        // A day the version lacks: 422. Another user's version or a wrong program: 409.
        let missing = json!({ "session_id": SessionId::new_v7(), "started_at": t(70), "choice": choice("z") });
        let message = assert_status(
            call::<SessionView>(&mut a, START, missing).await,
            StatusCode::UNPROCESSABLE_ENTITY,
        );
        assert_eq!(message, START_DAY_MISSING);
        let mut b = api.user("B").await;
        active_program(&api, &b).await;
        let foreign = json!({ "session_id": SessionId::new_v7(), "started_at": t(70), "choice": choice("a") });
        let message = assert_status(
            call::<SessionView>(&mut b, START, foreign).await,
            StatusCode::CONFLICT,
        );
        assert_eq!(message, START_VERSION_GONE);
        let mut wrong = choice("a");
        wrong["program_id"] = json!(ProgramId::new_v7());
        let mismatch =
            json!({ "session_id": SessionId::new_v7(), "started_at": t(70), "choice": wrong });
        let message = assert_status(
            call::<SessionView>(&mut a, START, mismatch).await,
            StatusCode::CONFLICT,
        );
        assert_eq!(message, START_VERSION_GONE);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn start_session_uses_the_latest_version(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let program = active_program(&api, &a).await;
        let (_, version) = db::programs::add_version(&api.db, a.id, program.into(), &document(8))
            .await
            .unwrap();
        let view = start(&mut a, SessionId::new_v7(), t(0)).await.unwrap();
        assert_eq!(view.program_version_id, version.id.into());
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn start_session_needs_an_active_program_and_no_other_session_in_progress(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let message = assert_status(
            start(&mut a, SessionId::new_v7(), t(0)).await,
            StatusCode::CONFLICT,
        );
        assert_eq!(message, "Choose a program first.");

        active_program(&api, &a).await;
        let current = start(&mut a, SessionId::new_v7(), t(0)).await.unwrap();
        let message = assert_status(
            start(&mut a, SessionId::new_v7(), t(1)).await,
            StatusCode::CONFLICT,
        );
        assert_eq!(
            message,
            "Another session is in progress. Finish or abandon it first."
        );
        finish(&mut a, current.id, SessionOutcome::Abandoned, t(2))
            .await
            .unwrap();
        start(&mut a, SessionId::new_v7(), t(3)).await.unwrap();
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn get_in_progress_session_returns_it_with_its_sets(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        active_program(&api, &a).await;
        let none: Option<SessionWithSets> = call(&mut a, IN_PROGRESS, json!({})).await.unwrap();
        assert_eq!(none, None);

        let id = SessionId::new_v7();
        let view = start(&mut a, id, t(0)).await.unwrap();
        let sets = [
            set(squat(), 0, 5, 100.0, t(2)),
            set(squat(), 1, 5, 100.0, t(1)),
        ];
        for set in &sets {
            save(&mut a, id, set).await.unwrap();
        }
        let current: Option<SessionWithSets> = call(&mut a, IN_PROGRESS, json!({})).await.unwrap();
        // Sets in the order they were completed.
        assert_eq!(
            current,
            Some(SessionWithSets {
                session: view,
                sets: vec![sets[1].clone(), sets[0].clone()],
            })
        );

        finish(&mut a, id, SessionOutcome::Completed, t(3))
            .await
            .unwrap();
        let none: Option<SessionWithSets> = call(&mut a, IN_PROGRESS, json!({})).await.unwrap();
        assert_eq!(none, None);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn save_set_validates_against_the_session(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        active_program(&api, &a).await;
        let id = SessionId::new_v7();
        start(&mut a, id, t(10)).await.unwrap();

        let early = set(squat(), 0, 5, 100.0, t(9));
        let message = assert_status(
            save(&mut a, id, &early).await,
            StatusCode::UNPROCESSABLE_ENTITY,
        );
        assert_eq!(
            message,
            "A set cannot be completed before its session started."
        );
        // A value the database refuses: a slug is fine, a weight above the maximum is not.
        let heavy = json!({
            "session_id": id,
            "set": { "id": SetId::new_v7(), "exercise": "back-squat", "set_index": 0, "reps": 5,
                     "weight": 1e9, "duration": null, "warm_up": false, "completed_at": t(11) }
        });
        assert!(a.call_err(SAVE_SET, heavy).await.status.is_client_error());

        finish(&mut a, id, SessionOutcome::Completed, t(20))
            .await
            .unwrap();
        let after = set(squat(), 0, 5, 100.0, t(15));
        let message = assert_status(save(&mut a, id, &after).await, StatusCode::CONFLICT);
        assert_eq!(message, "This session has already ended.");
        assert!(stored_sets(&api, &a, id).await.is_empty());
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn finish_session_validates_and_is_idempotent(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        active_program(&api, &a).await;
        let id = SessionId::new_v7();
        start(&mut a, id, t(10)).await.unwrap();
        save(&mut a, id, &set(squat(), 0, 5, 100.0, t(15)))
            .await
            .unwrap();

        let message = assert_status(
            finish(&mut a, id, SessionOutcome::Completed, t(9)).await,
            StatusCode::UNPROCESSABLE_ENTITY,
        );
        assert_eq!(
            message,
            "A session cannot end before its sets were completed."
        );
        assert_status(
            finish(&mut a, id, SessionOutcome::Completed, t(14)).await,
            StatusCode::UNPROCESSABLE_ENTITY,
        );

        let summary = finish(&mut a, id, SessionOutcome::Completed, t(20))
            .await
            .unwrap();
        assert_eq!(summary.session.status, SessionStatus::Completed);
        assert_eq!(summary.session.finished_at, Some(t(20)));
        // A retry, even much later, gets the same summary.
        squat_session(&mut a, 60, 5, 150.0, SessionOutcome::Completed).await;
        assert_eq!(
            finish(&mut a, id, SessionOutcome::Completed, t(20))
                .await
                .unwrap(),
            summary
        );
        // Another outcome or time is a conflict.
        for (outcome, at) in [
            (SessionOutcome::Abandoned, t(20)),
            (SessionOutcome::Completed, t(21)),
        ] {
            let message =
                assert_status(finish(&mut a, id, outcome, at).await, StatusCode::CONFLICT);
            assert_eq!(message, "This session has already ended.");
        }
        let stored = db::sessions::get(&api.db, a.id, id.into()).await.unwrap();
        assert_eq!(stored.status, db::sessions::SessionStatus::Completed);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn finish_session_summarises_volume_records_and_progression(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        active_program(&api, &a).await;

        // First session: 3 × 5 at 100 kg, plus warm-ups, a plank and an extra failed single at
        // index 3 (an extra: it changes neither the verdict nor the base).
        let id = SessionId::new_v7();
        start(&mut a, id, t(0)).await.unwrap();
        let warm = LoggedSet {
            warm_up: true,
            ..set(squat(), 0, 5, 60.0, t(1))
        };
        save(&mut a, id, &warm).await.unwrap();
        for index in 0..3 {
            save(
                &mut a,
                id,
                &set(squat(), index, 5, 100.0, t(2 + i64::from(index))),
            )
            .await
            .unwrap();
        }
        save(&mut a, id, &set(squat(), 3, 0, 120.0, t(6)))
            .await
            .unwrap();
        let plank = LoggedSet {
            weight: None,
            duration: Some(Seconds::new(30)),
            ..set(ExerciseId::new("plank").unwrap(), 0, 1, 0.0, t(7))
        };
        save(&mut a, id, &plank).await.unwrap();
        let summary = finish(&mut a, id, SessionOutcome::Completed, t(10))
            .await
            .unwrap();
        // 3 × 5 × 100 kg; the warm-up, the failed single and the plank add nothing.
        assert_eq!(
            summary.volume,
            iron_oxide_domain::Volume::of(kg(100.0), Reps::new(15))
        );
        assert!(
            summary.prs.is_empty(),
            "nothing to beat yet: {:?}",
            summary.prs
        );
        assert_eq!(summary.changes.len(), 1);
        assert_eq!(summary.changes[0].exercise, squat());
        assert_eq!(
            summary.changes[0].kind,
            ChangeKind::WeightIncrease {
                from: kg(100.0),
                to: kg(102.5)
            }
        );
        assert!(summary.needs_training_max.is_empty());

        // Second session (day b): heavier squats set records; the bench needs a training max.
        let id = SessionId::new_v7();
        start(&mut a, id, t(60)).await.unwrap();
        for index in 0..3 {
            save(
                &mut a,
                id,
                &set(squat(), index, 5, 102.5, t(61 + i64::from(index))),
            )
            .await
            .unwrap();
        }
        save(&mut a, id, &set(bench(), 0, 5, 60.0, t(65)))
            .await
            .unwrap();
        let summary = finish(&mut a, id, SessionOutcome::Completed, t(70))
            .await
            .unwrap();
        let squat_prs: Vec<&PrKind> = summary
            .prs
            .iter()
            .filter(|pr| pr.exercise == squat())
            .map(|pr| &pr.kind)
            .collect();
        assert!(
            squat_prs.iter().any(|kind| matches!(kind, PrKind::HeaviestWeight { previous, .. } if *previous == kg(100.0))),
            "{squat_prs:?}"
        );
        assert!(
            summary.prs.iter().all(|pr| pr.exercise == squat()),
            "no bench history, no bench PR"
        );
        assert_eq!(summary.needs_training_max, vec![bench()]);
        assert_eq!(
            summary.changes[0].kind,
            ChangeKind::WeightIncrease {
                from: kg(102.5),
                to: kg(105.0)
            }
        );

        // An abandoned session has a volume but no records or changes.
        let (_, abandoned) = squat_session(&mut a, 120, 5, 200.0, SessionOutcome::Abandoned).await;
        assert_eq!(
            abandoned.volume,
            iron_oxide_domain::Volume::of(kg(200.0), Reps::new(15))
        );
        assert!(abandoned.prs.is_empty() && abandoned.changes.is_empty());
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn plan_targets_come_from_the_history_before_the_session(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        active_program(&api, &a).await;

        let id = SessionId::new_v7();
        start(&mut a, id, t(0)).await.unwrap();
        let first = plan(&mut a, id).await.unwrap();
        assert_eq!(first.day_name, "Day A");
        assert_eq!(
            first
                .exercises
                .iter()
                .map(|planned| planned.exercise.id.as_str())
                .collect::<Vec<_>>(),
            ["back-squat", "plank"]
        );
        let targets = squat_targets(&first);
        assert_eq!(targets.source, TargetSource::ProgramDefault);
        assert_eq!(targets.working.len(), 3);
        assert!(
            targets
                .working
                .iter()
                .all(|target| target.weight == Some(kg(100.0)))
        );
        for index in 0..3 {
            save(
                &mut a,
                id,
                &set(squat(), index, 5, 100.0, t(1 + i64::from(index))),
            )
            .await
            .unwrap();
        }
        finish(&mut a, id, SessionOutcome::Completed, t(10))
            .await
            .unwrap();
        // The plan of a session never changes: it only looks at what came before it.
        let again = plan(&mut a, id).await.unwrap();
        assert_eq!(again.exercises, first.exercises);
        assert_eq!(again.session.status, SessionStatus::Completed);

        let id = SessionId::new_v7();
        start(&mut a, id, t(60)).await.unwrap();
        let second = plan(&mut a, id).await.unwrap();
        assert_eq!(second.day_name, "Day B");
        let targets = squat_targets(&second);
        assert_eq!(targets.source, TargetSource::Progression);
        assert!(
            targets
                .working
                .iter()
                .all(|target| target.weight == Some(kg(102.5)))
        );
        let bench_targets = &second.exercises[1].targets;
        assert_eq!(
            bench_targets,
            &NextTargets::NeedsTrainingMax { exercise: bench() }
        );
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn next_plan_previews_the_next_session(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let result: Result<NextSessionPlan, _> = call(&mut a, NEXT_PLAN, json!({})).await;
        assert_eq!(
            assert_status(result, StatusCode::CONFLICT),
            "Choose a program first."
        );

        let program = active_program(&api, &a).await;
        let first: NextSessionPlan = call(&mut a, NEXT_PLAN, json!({})).await.unwrap();
        assert_eq!(
            (first.day.as_str(), first.day_name.as_str()),
            ("a", "Day A")
        );
        assert_eq!(first.program_id, program);
        squat_session(&mut a, 0, 5, 100.0, SessionOutcome::Completed).await;

        let next: NextSessionPlan = call(&mut a, NEXT_PLAN, json!({})).await.unwrap();
        assert_eq!(next.day.as_str(), "b");
        let squat_next = next.exercises[0].targets.ready().unwrap();
        assert_eq!(squat_next.source, TargetSource::Progression);
        assert!(
            squat_next
                .working
                .iter()
                .all(|target| target.weight == Some(kg(102.5)))
        );
        // Starting it gives the same day, version and targets.
        let id = SessionId::new_v7();
        let started = start(&mut a, id, t(60)).await.unwrap();
        assert_eq!(
            (started.day.clone(), started.program_version_id),
            (next.day.clone(), next.program_version_id)
        );
        assert_eq!(plan(&mut a, id).await.unwrap().exercises, next.exercises);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn past_sessions_are_judged_against_their_own_prescription(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let program = active_program(&api, &a).await;
        // Done as prescribed by version 1 (3 × 5): a success.
        squat_session(&mut a, 0, 5, 100.0, SessionOutcome::Completed).await;
        // Version 2 asks for 3 × 8 on day a. Judged against it, 3 × 5 would be a failure.
        db::programs::add_version(&api.db, a.id, program.into(), &document(8))
            .await
            .unwrap();
        let id = SessionId::new_v7();
        let view = start(&mut a, id, t(60)).await.unwrap();
        assert_eq!(view.day.as_str(), "b");
        let targets = squat_targets(&plan(&mut a, id).await.unwrap()).clone();
        assert!(
            targets
                .working
                .iter()
                .all(|target| target.weight == Some(kg(102.5))),
            "{targets:?}"
        );
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn training_max_history_starts_when_the_training_max_was_entered(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        active_program(&api, &a).await;
        // Day a, then day b with a bench session before the training max exists.
        squat_session(&mut a, 0, 5, 100.0, SessionOutcome::Completed).await;
        let early = SessionId::new_v7();
        start(&mut a, early, t(60)).await.unwrap();
        for index in 0..3 {
            save(
                &mut a,
                early,
                &set(bench(), index, 5, 70.0, t(61 + i64::from(index))),
            )
            .await
            .unwrap();
        }
        finish(&mut a, early, SessionOutcome::Completed, t(70))
            .await
            .unwrap();

        training_maxes::set(
            &api.db,
            a.id,
            &training_maxes::TrainingMax {
                exercise_id: "bench-press".to_owned(),
                weight_ng: kg(100.0).as_nanograms(),
                set_at: offset_date_time(t(100)).unwrap(),
            },
        )
        .await
        .unwrap();
        // Next day b: the bench session before the training max is not history.
        squat_session(&mut a, 120, 5, 102.5, SessionOutcome::Completed).await;
        let id = SessionId::new_v7();
        let view = start(&mut a, id, t(180)).await.unwrap();
        assert_eq!(view.day.as_str(), "b");
        let plan = plan(&mut a, id).await.unwrap();
        let bench_targets = plan.exercises[1].targets.ready().unwrap();
        assert_eq!(bench_targets.source, TargetSource::ProgramDefault);
        assert!(
            bench_targets
                .working
                .iter()
                .all(|target| target.weight == Some(kg(80.0)))
        );
        assert_eq!(bench_targets.training_max, Some(kg(100.0)));

        // After a successful session, the effective training max goes up (display only).
        for index in 0..3 {
            save(
                &mut a,
                id,
                &set(bench(), index, 5, 80.0, t(181 + i64::from(index))),
            )
            .await
            .unwrap();
        }
        let summary = finish(&mut a, id, SessionOutcome::Completed, t(190))
            .await
            .unwrap();
        let change = summary
            .changes
            .iter()
            .find(|change| change.exercise == bench())
            .unwrap();
        assert_eq!(
            change.kind,
            ChangeKind::TrainingMaxIncrease {
                from: kg(100.0),
                to: kg(102.5)
            }
        );
        let stored = training_maxes::list(&api.db, a.id).await.unwrap();
        assert_eq!(
            stored[0].weight_ng,
            kg(100.0).as_nanograms(),
            "never stored back"
        );
    }

    /// #60: a set is saved with the target it was shown, read back with it, and a training max
    /// session is judged against that target exactly. 77.5 kg lifted against an exact 80 kg is
    /// outside the legacy 1.25 kg tolerance: logged with a 77.5 kg target, the session is a
    /// success; logged without a target, a failure. Same id with another target: `409`.
    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn sets_keep_their_target_and_are_judged_against_it(db: PgPool) {
        let api = TestApi::new(db).await;
        let (mut a, mut b) = (api.user("A").await, api.user("B").await);
        let shown = iron_oxide_domain::progression::SetTarget {
            weight: Some(kg(77.5)),
            goal: iron_oxide_domain::progression::SetGoal::Reps {
                reps: Reps::new(5),
                range: None,
            },
        };
        let mut changes = Vec::new();
        for (user, target) in [(&mut a, Some(shown)), (&mut b, None)] {
            active_program(&api, user).await;
            training_maxes::set(
                &api.db,
                user.id,
                &training_maxes::TrainingMax {
                    exercise_id: "bench-press".to_owned(),
                    weight_ng: kg(100.0).as_nanograms(),
                    set_at: offset_date_time(t(-10)).unwrap(),
                },
            )
            .await
            .unwrap();
            squat_session(user, 0, 5, 100.0, SessionOutcome::Completed).await;
            let id = SessionId::new_v7();
            start(user, id, t(60)).await.unwrap();
            for index in 0..3 {
                let logged = LoggedSet {
                    target,
                    ..set(bench(), index, 5, 77.5, t(61 + i64::from(index)))
                };
                // Retried: one row, unchanged.
                save(user, id, &logged).await.unwrap();
                save(user, id, &logged).await.unwrap();
                if target.is_some() {
                    let other = LoggedSet {
                        target: None,
                        ..logged.clone()
                    };
                    assert_status(save(user, id, &other).await, StatusCode::CONFLICT);
                }
            }
            let current: Option<SessionWithSets> =
                call(user, IN_PROGRESS, json!({})).await.unwrap();
            let bench_sets: Vec<_> = current
                .unwrap()
                .sets
                .into_iter()
                .filter(|set| set.exercise == bench())
                .collect();
            assert_eq!(bench_sets.len(), 3);
            assert!(bench_sets.iter().all(|set| set.target == target));
            let summary = finish(user, id, SessionOutcome::Completed, t(70))
                .await
                .unwrap();
            changes.push(
                summary
                    .changes
                    .into_iter()
                    .find(|change| change.exercise == bench())
                    .unwrap()
                    .kind,
            );
        }
        assert_eq!(
            changes,
            [
                ChangeKind::TrainingMaxIncrease {
                    from: kg(100.0),
                    to: kg(102.5)
                },
                ChangeKind::TrainingMaxUnchanged {
                    training_max: kg(100.0),
                    failed_sessions: 1
                },
            ]
        );
    }

    // --- Idempotency (#25) ----------------------------------------------------------------------

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn retried_starts_create_one_session(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        active_program(&api, &a).await;
        let id = SessionId::new_v7();
        let first = start(&mut a, id, t(0)).await.unwrap();
        assert_eq!(start(&mut a, id, t(0)).await.unwrap(), first);
        let message = assert_status(start(&mut a, id, t(1)).await, StatusCode::CONFLICT);
        assert_eq!(message, ID_REUSED);

        // After the session ended (and the rotation moved on), a retry still returns it.
        save(&mut a, id, &set(squat(), 0, 5, 100.0, t(1)))
            .await
            .unwrap();
        finish(&mut a, id, SessionOutcome::Completed, t(5))
            .await
            .unwrap();
        let retried = start(&mut a, id, t(0)).await.unwrap();
        assert_eq!(retried.id, id);
        assert_eq!(retried.day.as_str(), "a");
        assert_eq!(retried.status, SessionStatus::Completed);
        let all = db::sessions::list(&api.db, a.id, None, None, 100)
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn concurrent_duplicate_starts_create_one_session(db: PgPool) {
        let api = TestApi::new(db).await;
        let a = api.user("A").await;
        active_program(&api, &a).await;
        let id = SessionId::new_v7();
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let mut user = a.clone();
                tokio::spawn(async move { start(&mut user, id, t(0)).await })
            })
            .collect();
        let mut views = Vec::new();
        for task in tasks {
            match task.await.unwrap() {
                Ok(view) => views.push(view),
                // A lost race is a retryable 503, never a duplicate or a conflict.
                Err(error) => {
                    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE, "{error:?}")
                }
            }
        }
        assert!(!views.is_empty());
        assert!(views.iter().all(|view| view == &views[0]));
        let all = db::sessions::list(&api.db, a.id, None, None, 100)
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn concurrent_starts_of_different_sessions_let_exactly_one_through(db: PgPool) {
        let api = TestApi::new(db).await;
        let a = api.user("A").await;
        active_program(&api, &a).await;
        for round in 0..5 {
            let tasks: Vec<_> = (0..2)
                .map(|_| {
                    let mut user = a.clone();
                    tokio::spawn(
                        async move { start(&mut user, SessionId::new_v7(), t(round)).await },
                    )
                })
                .collect();
            let mut started = Vec::new();
            for task in tasks {
                match task.await.unwrap() {
                    Ok(view) => started.push(view),
                    Err(error) => assert_eq!(
                        error,
                        CallError {
                            status: StatusCode::CONFLICT,
                            message: SESSION_IN_PROGRESS.to_owned(),
                        }
                    ),
                }
            }
            assert_eq!(started.len(), 1, "round {round}");
            let in_progress: Vec<_> = db::sessions::list(&api.db, a.id, None, None, 100)
                .await
                .unwrap()
                .into_iter()
                .filter(|session| session.status == db::sessions::SessionStatus::InProgress)
                .collect();
            assert_eq!(in_progress.len(), 1);
            let mut user = a.clone();
            finish(
                &mut user,
                started[0].id,
                SessionOutcome::Abandoned,
                t(round + 1),
            )
            .await
            .unwrap();
        }
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn retried_save_set_is_one_row_and_a_conflicting_retry_is_409(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        active_program(&api, &a).await;
        let id = SessionId::new_v7();
        start(&mut a, id, t(0)).await.unwrap();
        let logged = set(squat(), 0, 5, 100.0, t(1));
        for _ in 0..5 {
            save(&mut a, id, &logged).await.unwrap();
        }
        assert_eq!(
            stored_sets(&api, &a, id).await,
            vec![repo_set(id, &logged).unwrap()]
        );

        let changed = LoggedSet {
            reps: Reps::new(4),
            ..logged.clone()
        };
        let message = assert_status(save(&mut a, id, &changed).await, StatusCode::CONFLICT);
        assert_eq!(message, "This set was already saved with different values.");
        // The same set id in another session of the user is a conflict too.
        finish(&mut a, id, SessionOutcome::Completed, t(5))
            .await
            .unwrap();
        let other = SessionId::new_v7();
        start(&mut a, other, t(10)).await.unwrap();
        let moved = LoggedSet {
            completed_at: t(11),
            ..logged.clone()
        };
        assert_status(save(&mut a, other, &moved).await, StatusCode::CONFLICT);

        // A retry after the session ended still succeeds, and changes nothing.
        save(&mut a, id, &logged).await.unwrap();
        assert_eq!(
            stored_sets(&api, &a, id).await,
            vec![repo_set(id, &logged).unwrap()]
        );
        assert!(stored_sets(&api, &a, other).await.is_empty());
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn a_set_stored_outside_its_session_still_retries_and_summarises(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        active_program(&api, &a).await;
        let id = SessionId::new_v7();
        start(&mut a, id, t(10)).await.unwrap();
        // Written straight to the repository (a client clock behind the server's session start).
        let early = set(squat(), 0, 5, 100.0, t(5));
        db::sets::upsert_idempotent(&api.db, a.id, &repo_set(id, &early).unwrap())
            .await
            .unwrap();
        save(&mut a, id, &early).await.unwrap();
        let summary = finish(&mut a, id, SessionOutcome::Completed, t(20))
            .await
            .unwrap();
        assert_eq!(
            summary.volume,
            iron_oxide_domain::Volume::of(kg(100.0), Reps::new(5))
        );
        save(&mut a, id, &early).await.unwrap();
        assert!(plan(&mut a, id).await.is_ok());
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn concurrent_duplicate_save_sets_create_one_row(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        active_program(&api, &a).await;
        let id = SessionId::new_v7();
        start(&mut a, id, t(0)).await.unwrap();
        let logged = set(squat(), 0, 5, 100.0, t(1));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (mut user, logged) = (a.clone(), logged.clone());
                tokio::spawn(async move { save(&mut user, id, &logged).await })
            })
            .collect();
        for task in tasks {
            if let Err(error) = task.await.unwrap() {
                assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE, "{error:?}");
            }
        }
        assert_eq!(
            stored_sets(&api, &a, id).await,
            vec![repo_set(id, &logged).unwrap()]
        );
    }

    // --- Isolation ------------------------------------------------------------------------------

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_session_is_not_found_by_any_endpoint(db: PgPool) {
        let api = TestApi::new(db).await;
        let (mut a, mut b) = api.users_a_and_b().await;
        active_program(&api, &a).await;
        active_program(&api, &b).await;
        let id = SessionId::new_v7();
        start(&mut a, id, t(0)).await.unwrap();
        let logged = set(squat(), 0, 5, 100.0, t(1));
        save(&mut a, id, &logged).await.unwrap();
        let before = (
            db::sessions::get(&api.db, a.id, id.into()).await.unwrap(),
            stored_sets(&api, &a, id).await,
        );

        let new_set = set(squat(), 1, 5, 100.0, t(2));
        let bodies: [(&str, BodyOf); 4] = [
            (GET, Box::new(|id| json!({ "session_id": id }))),
            (PLAN, Box::new(|id| json!({ "session_id": id }))),
            (
                SAVE_SET,
                Box::new(move |id| json!({ "session_id": id, "set": new_set })),
            ),
            (
                FINISH,
                Box::new(
                    |id| json!({ "session_id": id, "outcome": "abandoned", "finished_at": t(3) }),
                ),
            ),
        ];
        for (path, body) in &bodies {
            testing::assert_not_found_for_other_user(&mut b, path, id.as_uuid(), body).await;
        }
        // Even A's own set, resent by B into A's session.
        testing::assert_not_found_for_other_user(
            &mut b,
            SAVE_SET,
            id.as_uuid(),
            |id| json!({ "session_id": id, "set": logged }),
        )
        .await;
        let none: Option<SessionWithSets> = call(&mut b, IN_PROGRESS, json!({})).await.unwrap();
        assert_eq!(none, None);

        // A's session and sets are untouched, and A still reaches them.
        let after = (
            db::sessions::get(&api.db, a.id, id.into()).await.unwrap(),
            stored_sets(&api, &a, id).await,
        );
        assert_eq!(after, before);
        assert!(plan(&mut a, id).await.is_ok());
        let current: Option<SessionWithSets> = call(&mut a, IN_PROGRESS, json!({})).await.unwrap();
        assert_eq!(current.map(|current| current.sets), Some(vec![logged]));
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_ids_start_separate_sessions_and_sets(db: PgPool) {
        let api = TestApi::new(db).await;
        let (mut a, mut b) = api.users_a_and_b().await;
        active_program(&api, &a).await;
        let program_b = active_program(&api, &b).await;
        // A has a completed day a, so A's next day is b; B has no history.
        let (a_first, _) = squat_session(&mut a, 0, 5, 100.0, SessionOutcome::Completed).await;
        let a_sets = stored_sets(&api, &a, a_first.id).await;

        // B reuses A's session id and set id: B's own, separate session and set.
        let b_view = start(&mut b, a_first.id, t(60)).await.unwrap();
        assert_eq!(b_view.program_id, program_b);
        assert_eq!(b_view.day.as_str(), "a", "B's rotation, not A's");
        let reused = LoggedSet {
            completed_at: t(61),
            ..domain_set(&a_sets[0]).unwrap()
        };
        save(&mut b, a_first.id, &reused).await.unwrap();
        let b_plan = plan(&mut b, a_first.id).await.unwrap();
        assert_eq!(
            squat_targets(&b_plan).source,
            TargetSource::ProgramDefault,
            "A's history is not B's"
        );
        let b_summary = finish(&mut b, a_first.id, SessionOutcome::Completed, t(70))
            .await
            .unwrap();
        assert!(b_summary.prs.is_empty(), "A's records are not B's");

        // A's session and sets did not move.
        let a_after = db::sessions::get(&api.db, a.id, a_first.id.into())
            .await
            .unwrap();
        assert_eq!(a_after.started_at, offset_date_time(t(0)).unwrap());
        assert_eq!(stored_sets(&api, &a, a_first.id).await, a_sets);
        let a_next = start(&mut a, SessionId::new_v7(), t(120)).await.unwrap();
        assert_eq!(
            a_next.day.as_str(),
            "b",
            "B's sessions do not move A's rotation"
        );
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_history_never_reaches_the_next_plan(db: PgPool) {
        let api = TestApi::new(db).await;
        let (mut a, mut b) = api.users_a_and_b().await;
        active_program(&api, &a).await;
        squat_session(&mut a, 0, 5, 100.0, SessionOutcome::Completed).await;
        let program_b = active_program(&api, &b).await;
        let next: NextSessionPlan = call(&mut b, NEXT_PLAN, json!({})).await.unwrap();
        assert_eq!(next.program_id, program_b);
        assert_eq!(next.day.as_str(), "a", "B's rotation, not A's");
        let squat_next = next.exercises[0].targets.ready().unwrap();
        assert_eq!(squat_next.source, TargetSource::ProgramDefault);
        assert!(
            squat_next
                .working
                .iter()
                .all(|target| target.weight == Some(kg(100.0)))
        );
        // And A's next plan is A's.
        let a_next: NextSessionPlan = call(&mut a, NEXT_PLAN, json!({})).await.unwrap();
        assert_eq!(a_next.day.as_str(), "b");
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_active_program_is_never_used(db: PgPool) {
        let api = TestApi::new(db).await;
        let (mut a, mut b) = api.users_a_and_b().await;
        active_program(&api, &a).await;
        start(&mut a, SessionId::new_v7(), t(0)).await.unwrap();
        // B has no program and no session in progress of their own.
        let message = assert_status(
            start(&mut b, SessionId::new_v7(), t(1)).await,
            StatusCode::CONFLICT,
        );
        assert_eq!(message, "Choose a program first.");
    }
}
