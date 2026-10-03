//! Export, import and account deletion (#22): the logic behind `crate::api::account`. The export
//! format is documented in `docs/export-format.md`.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use dioxus::logger::tracing;
use dioxus::prelude::ServerFnError;
use dioxus::server::axum::{
    body::Body,
    extract::{FromRequestParts, Request},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use iron_oxide_domain::{
    CreationId, ExerciseId, LoggedSet, Reps, Seconds, SessionId, SessionStatus, SetId,
    UserId as DomainUserId, Weight,
    entitlements::{Plan, Quota},
    program::{BuiltinProgramId, Program},
    time::Timestamp,
};
use serde::Deserialize;
use sqlx::{
    Connection, PgConnection, PgPool,
    types::{Uuid, time::OffsetDateTime},
};
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;

use super::{ApiError, error::INTERNAL, errors_layer, offset_date_time, settings, timestamp};
use crate::api::{
    account::{
        EXPORT_FORMAT, EXPORT_FORMAT_VERSION, ExportAccount, ExportDocument, ExportLinkedAccount,
        ExportPasskey, ExportProgram, ExportSession, ExportSettings, ExportSignIn, ExportVersion,
        IMPORT_BODY_LIMIT, ImportSummary, MAX_EXPORT_BYTES, OLDEST_IMPORTED_FORMAT_VERSION,
    },
    programs::{ProgramProblem, ProgramProblems},
    settings::{SettingsUpdate, TrainingMax},
};
use crate::server::{
    AppState,
    auth::{AuthContext, AuthError, AuthUser},
    billing,
    db::{self, account as repo, ids::UserId, settings::UserSettings},
    entitlements, limits,
};

/// The `403` of a deletion without a recent sign-in.
pub const REAUTHENTICATE: &str = "To delete your account, sign in again first.";

/// The longest part of a parser message shown to the user: serde echoes the offending value,
/// which can be as long as the file.
const MAX_MESSAGE_CHARS: usize = 200;

// --- Export ------------------------------------------------------------------------------------

/// Everything `owner` owns, read from one consistent snapshot.
pub async fn export(pool: &PgPool, owner: UserId) -> Result<ExportDocument, ApiError> {
    let mut tx = pool.begin().await.map_err(db::error::RepoError::from)?;
    repo::snapshot(&mut tx).await?;
    let document = read_export(&mut tx, owner).await?;
    tx.commit().await.map_err(db::error::RepoError::from)?;
    let size = serde_json::to_vec(&document)
        .map_err(ApiError::internal)?
        .len();
    if size > MAX_EXPORT_BYTES {
        tracing::warn!(size, "export larger than the import limit");
        return Err(ApiError::TooLarge(Cow::Borrowed(
            "Your data is too large to export in one file. Please contact support.",
        )));
    }
    Ok(document)
}

/// Stored data that does not fit its type: a 500, with the column in the log.
fn corrupt(column: &'static str) -> impl FnOnce(String) -> ApiError {
    move |error| ApiError::internal(format!("corrupt {column}: {error}"))
}

fn stored<T, E: std::fmt::Display>(
    result: Result<T, E>,
    column: &'static str,
) -> Result<T, ApiError> {
    result.map_err(|error| corrupt(column)(error.to_string()))
}

async fn read_export(tx: &mut PgConnection, owner: UserId) -> Result<ExportDocument, ApiError> {
    let account = repo::account(tx, owner)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let account = ExportAccount {
        user_id: DomainUserId::from_uuid(owner.as_uuid()),
        created_at: timestamp(account.created_at)?,
        plan: stored(account.plan.parse::<Plan>(), "users.plan")?,
        display_name: account.display_name,
    };
    let passkeys = repo::passkeys(tx, owner)
        .await?
        .into_iter()
        .map(|passkey| {
            Ok(ExportPasskey {
                nickname: passkey.nickname,
                created_at: timestamp(passkey.created_at)?,
                last_used_at: passkey.last_used_at.map(timestamp).transpose()?,
                backed_up: passkey.backup_state,
            })
        })
        .collect::<Result<_, ApiError>>()?;
    let linked_accounts = repo::linked_accounts(tx, owner)
        .await?
        .into_iter()
        .map(|linked| {
            Ok(ExportLinkedAccount {
                provider: linked.provider,
                created_at: timestamp(linked.created_at)?,
                last_used_at: linked.last_used_at.map(timestamp).transpose()?,
            })
        })
        .collect::<Result<_, ApiError>>()?;
    let settings = match repo::settings(tx, owner).await? {
        Some((saved, updated_at)) => Some(ExportSettings {
            settings: settings::from_stored(saved)?,
            updated_at: timestamp(updated_at)?,
        }),
        None => None,
    };
    let training_maxes = repo::training_maxes(tx, owner)
        .await?
        .into_iter()
        .map(|max| {
            let weight = u64::try_from(max.weight_ng).map_err(|e| e.to_string());
            Ok(TrainingMax {
                exercise_id: stored(
                    ExerciseId::new(max.exercise_id),
                    "training_maxes.exercise_id",
                )?,
                weight: stored(
                    weight.and_then(|ng| Weight::from_nanograms(ng).map_err(|e| e.to_string())),
                    "training_maxes.weight_ng",
                )?,
                set_at: timestamp(max.set_at)?,
            })
        })
        .collect::<Result<_, ApiError>>()?;

    let programs = repo::programs(tx, owner).await?;
    let creation_of: HashMap<Uuid, CreationId> = programs
        .iter()
        .map(|program| (program.id, CreationId::from_uuid(program.creation_id)))
        .collect();
    let mut versions_of: HashMap<Uuid, Vec<ExportVersion>> = HashMap::new();
    for version in repo::versions(tx, owner).await? {
        versions_of
            .entry(version.program_id)
            .or_default()
            .push(ExportVersion {
                version: stored(u32::try_from(version.version), "program_versions.version")?,
                created_at: timestamp(version.created_at)?,
                document: version.document,
            });
    }
    let programs = programs
        .into_iter()
        .map(|program| {
            Ok(ExportProgram {
                creation_id: CreationId::from_uuid(program.creation_id),
                name: program.name,
                source_builtin_id: program
                    .source_builtin_id
                    .map(BuiltinProgramId::new)
                    .transpose()
                    .map_err(|e| corrupt("programs.source_builtin_id")(e.to_string()))?,
                archived: program.archived,
                created_at: timestamp(program.created_at)?,
                versions: versions_of.remove(&program.id).unwrap_or_default(),
            })
        })
        .collect::<Result<_, ApiError>>()?;
    let active_program = match repo::active_program(tx, owner).await? {
        Some(id) => Some(
            *creation_of
                .get(&id)
                .ok_or_else(|| ApiError::internal("active program not among the user's"))?,
        ),
        None => None,
    };

    let mut sets_of: HashMap<Uuid, Vec<LoggedSet<Timestamp>>> = HashMap::new();
    for set in repo::sets(tx, owner).await? {
        let session = set.session_id;
        sets_of.entry(session).or_default().push(export_set(set)?);
    }
    let sessions = repo::sessions(tx, owner)
        .await?
        .into_iter()
        .map(|session| {
            Ok(ExportSession {
                id: SessionId::from_uuid(session.id),
                program: *creation_of
                    .get(&session.program_id)
                    .ok_or_else(|| ApiError::internal("session of a program not the user's"))?,
                version: stored(u32::try_from(session.version), "program_versions.version")?,
                day: stored(
                    iron_oxide_domain::DayId::new(session.day_id),
                    "workout_sessions.day_id",
                )?,
                status: stored(parse_status(&session.status), "workout_sessions.status")?,
                started_at: timestamp(session.started_at)?,
                finished_at: session.finished_at.map(timestamp).transpose()?,
                sets: sets_of.remove(&session.id).unwrap_or_default(),
            })
        })
        .collect::<Result<_, ApiError>>()?;

    Ok(ExportDocument {
        format: EXPORT_FORMAT.to_owned(),
        format_version: EXPORT_FORMAT_VERSION,
        exported_at: timestamp(OffsetDateTime::now_utc())?,
        account,
        sign_in: ExportSignIn {
            passkeys,
            linked_accounts,
        },
        settings,
        training_maxes,
        programs,
        active_program,
        sessions,
    })
}

fn export_set(set: repo::Set) -> Result<LoggedSet<Timestamp>, ApiError> {
    let weight = set
        .weight_ng
        .map(|ng| {
            u64::try_from(ng)
                .map_err(|e| e.to_string())
                .and_then(|ng| Weight::from_nanograms(ng).map_err(|e| e.to_string()))
        })
        .transpose();
    Ok(LoggedSet {
        id: SetId::from_uuid(set.id),
        exercise: stored(ExerciseId::new(set.exercise_id), "workout_sets.exercise_id")?,
        set_index: stored(u16::try_from(set.set_index), "workout_sets.set_index")?,
        reps: Reps::new(stored(u16::try_from(set.reps), "workout_sets.reps")?),
        weight: stored(weight, "workout_sets.weight_ng")?,
        duration: set
            .duration_s
            .map(|s| stored(u32::try_from(s), "workout_sets.duration_s").map(Seconds::new))
            .transpose()?,
        warm_up: set.warmup,
        completed_at: timestamp(set.completed_at)?,
        target: set
            .target_goal
            .map(|goal| {
                let weight_ng = set
                    .target_weight_ng
                    .map(|ng| stored(u64::try_from(ng), "workout_sets.target_weight_ng"))
                    .transpose()?;
                super::sessions::domain_target(&db::sets::Target { weight_ng, goal })
            })
            .transpose()?,
    })
}

/// The stored text of a session status (the domain's serde name).
const fn status_name(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::InProgress => "in_progress",
        SessionStatus::Completed => "completed",
        SessionStatus::Skipped => "skipped",
        SessionStatus::Abandoned => "abandoned",
    }
}

fn parse_status(name: &str) -> Result<SessionStatus, String> {
    [
        SessionStatus::InProgress,
        SessionStatus::Completed,
        SessionStatus::Skipped,
        SessionStatus::Abandoned,
    ]
    .into_iter()
    .find(|status| status_name(*status) == name)
    .ok_or_else(|| format!("unknown status {name:?}"))
}

// --- Import ------------------------------------------------------------------------------------

/// How many account imports and deletions one server process runs at once, together. Each runs on
/// a dedicated connection with 60 s deadlines (`repo::long_connection`), and an import also holds
/// its body (up to [`IMPORT_BODY_LIMIT`]), the decoded document and its parsed form: about 80 MB
/// at the largest, on a 512 MB machine. One more gets a retryable `503` with `Retry-After`
/// ([`BUSY_RETRY_AFTER_SECS`]): an import before its body is read.
pub const MAX_ACCOUNT_OPERATIONS: usize = 2;

/// How long a client waits before retrying an import or a deletion refused for lack of a slot.
pub const BUSY_RETRY_AFTER_SECS: u64 = 5;

/// One of the process's [`MAX_ACCOUNT_OPERATIONS`] slots, held by an import or a deletion until its
/// work has finished. An import's is taken by its middleware and handed to the server function
/// through the request's extensions: Dioxus runs the function in a task of its own that goes on
/// after the client disconnects, and the slot goes with it, so abandoned imports never hold more
/// dedicated connections than the slots.
#[derive(Debug, Clone)]
pub struct AccountSlot(#[allow(dead_code, reason = "held, never read")] Arc<OwnedSemaphorePermit>);

/// Takes one of the process's account slots, or a retryable `503`.
fn take_slot(state: &AppState) -> Result<AccountSlot, ApiError> {
    match state.account_slots.clone().try_acquire_owned() {
        Ok(permit) => Ok(AccountSlot(Arc::new(permit))),
        Err(_) => {
            tracing::warn!("account import or deletion refused: every slot is taken");
            Err(ApiError::Busy(BUSY_RETRY_AFTER_SECS))
        }
    }
}

/// Middleware of `import_account_data`, in order:
/// 1. the session: a signed-out client gets its `401` without the body being read;
/// 2. an [`AccountSlot`] (none free: `503` with `Retry-After`), put in the request's extensions for
///    the server function, which holds it until the import has finished;
/// 3. the body, with [`limits::read_body`]: `413` past [`IMPORT_BODY_LIMIT`] (announced or
///    actually sent), `408` when it does not arrive within the import's read timeout, which then
///    frees the slot. The route is in `limits::OWN_BODY_LIMIT`, so the default cap leaves it alone.
pub async fn limit_import_body(request: Request, next: Next) -> Response {
    let (mut parts, body) = request.into_parts();
    if let Err(rejection) = AuthUser::from_request_parts(&mut parts, &()).await {
        return rejection;
    }
    let Some(state) = parts.extensions.get::<AppState>().cloned() else {
        tracing::error!("AppState missing on the import route: check server::router");
        return errors_layer::error_response(StatusCode::INTERNAL_SERVER_ERROR, INTERNAL, None);
    };
    let slot = match take_slot(&state) {
        Ok(slot) => slot,
        Err(error) => return ServerFnError::from(error).into_response(),
    };
    parts.extensions.insert(slot);
    let timeout = state.config.request_limits.import_body_read_timeout;
    match limits::read_body(&parts.headers, body, IMPORT_BODY_LIMIT, timeout).await {
        Ok(bytes) => {
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
        Err(error) => limits::refuse_body(&error, true, too_large().public().1),
    }
}

fn too_large() -> ApiError {
    ApiError::TooLarge(Cow::Owned(format!(
        "The export file is too large (the limit is {} MiB).",
        MAX_EXPORT_BYTES / (1024 * 1024)
    )))
}

/// Imports `document` (an export's JSON text) into `user`'s account: validates all of it, then
/// writes it in one transaction. See the module documentation of `crate::api::account` and
/// `docs/export-format.md` for the rules.
pub async fn import(
    pool: &PgPool,
    user: AuthUser,
    document: &str,
    // Held until the import has finished, client gone or not.
    _slot: AccountSlot,
) -> Result<ImportSummary, ApiError> {
    if document.len() > MAX_EXPORT_BYTES {
        return Err(too_large());
    }
    let import = Import::parse(document)?;
    // A whole account in one transaction: longer database deadlines than the pool's.
    let mut conn = repo::long_connection(pool).await?;
    let result = async {
        let mut tx = conn.begin().await.map_err(db::error::RepoError::from)?;
        let summary = import.write(&mut tx, user).await?;
        tx.commit().await.map_err(db::error::RepoError::from)?;
        Ok::<_, ApiError>(summary)
    }
    .await;
    repo::close(conn).await;
    let summary = result?;
    tracing::info!(?summary, "account data imported");
    Ok(summary)
}

/// A validated import, in the repository's types.
#[derive(Debug)]
struct Import {
    settings: Option<(UserSettings, OffsetDateTime)>,
    training_maxes: Vec<repo::TrainingMax>,
    programs: Vec<ImportProgram>,
    active_program: Option<Uuid>,
    sessions: Vec<ImportSession>,
}

#[derive(Debug)]
struct ImportProgram {
    creation_id: Uuid,
    name: String,
    source_builtin_id: Option<String>,
    archived: bool,
    created_at: OffsetDateTime,
    versions: Vec<(i32, serde_json::Value, OffsetDateTime)>,
}

#[derive(Debug)]
struct ImportSession {
    id: Uuid,
    program: Uuid,
    version: i32,
    day_id: String,
    status: &'static str,
    started_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    sets: Vec<repo::Set>,
}

/// Just enough of a document to check what it is before parsing all of it.
#[derive(Deserialize)]
struct Header {
    format: Option<String>,
    format_version: Option<serde_json::Value>,
}

/// A `422` whose message ends with a (shortened) parser message.
fn invalid_with(prefix: &str, detail: impl std::fmt::Display) -> ApiError {
    let detail = detail.to_string();
    let mut shown: String = detail.chars().take(MAX_MESSAGE_CHARS).collect();
    if shown.len() < detail.len() {
        shown.push('…');
    }
    ApiError::invalid(format!("{prefix}: {shown}."))
}

/// A time of the document, for the database.
fn time(value: Timestamp, at: impl FnOnce() -> String) -> Result<OffsetDateTime, ApiError> {
    offset_date_time(value).map_err(|_| ApiError::invalid(format!("{}: invalid time.", at())))
}

impl Import {
    /// Checks what the document is, parses it and validates everything, before any write.
    fn parse(text: &str) -> Result<Self, ApiError> {
        let header: Header = serde_json::from_str(text)
            .map_err(|error| invalid_with("This file is not an Iron Oxide export", error))?;
        if header.format.as_deref() != Some(EXPORT_FORMAT) {
            return Err(ApiError::invalid("This file is not an Iron Oxide export."));
        }
        match header
            .format_version
            .as_ref()
            .and_then(serde_json::Value::as_u64)
        {
            Some(version)
                if (u64::from(OLDEST_IMPORTED_FORMAT_VERSION)
                    ..=u64::from(EXPORT_FORMAT_VERSION))
                    .contains(&version) => {}
            Some(version) => {
                return Err(ApiError::invalid(format!(
                    "This export has format version {version}, which this version of Iron Oxide \
                     cannot read (it reads versions {OLDEST_IMPORTED_FORMAT_VERSION} to \
                     {EXPORT_FORMAT_VERSION})."
                )));
            }
            None => {
                return Err(ApiError::invalid(
                    "This export has no valid format version.",
                ));
            }
        }
        let document: ExportDocument = serde_json::from_str(text)
            .map_err(|error| invalid_with("This export is not valid", error))?;
        Self::validate(document)
    }

    fn validate(document: ExportDocument) -> Result<Self, ApiError> {
        let settings = document
            .settings
            .map(|saved| {
                let settings = settings::validate(SettingsUpdate::from(saved.settings))?;
                let updated_at = time(saved.updated_at, || "settings.updated_at".to_owned())?;
                Ok::<_, ApiError>((settings::to_stored(&settings)?, updated_at))
            })
            .transpose()?;

        let mut exercises = HashSet::new();
        let training_maxes = document
            .training_maxes
            .iter()
            .enumerate()
            .map(|(i, max)| {
                if !exercises.insert(max.exercise_id.as_str()) {
                    return Err(ApiError::invalid(format!(
                        "training_maxes[{i}]: a second training max for the same exercise."
                    )));
                }
                if max.weight.is_zero() {
                    return Err(ApiError::invalid(format!(
                        "training_maxes[{i}]: a training max must be more than zero."
                    )));
                }
                Ok(repo::TrainingMax {
                    exercise_id: max.exercise_id.as_str().to_owned(),
                    weight_ng: nanograms(max.weight),
                    set_at: time(max.set_at, || format!("training_maxes[{i}].set_at"))?,
                })
            })
            .collect::<Result<_, _>>()?;

        // The parsed program of each (program, version), to check the sessions' days.
        let mut parsed: HashMap<(Uuid, u32), Program> = HashMap::new();
        let mut archived: HashMap<Uuid, bool> = HashMap::new();
        let mut programs = Vec::with_capacity(document.programs.len());
        for (i, program) in document.programs.into_iter().enumerate() {
            let creation = program.creation_id.as_uuid();
            if archived.insert(creation, program.archived).is_some() {
                return Err(ApiError::invalid(format!(
                    "programs[{i}]: a second program with the same creation_id."
                )));
            }
            let length = program.name.chars().count();
            if !(1..=100).contains(&length) || program.name.chars().any(char::is_control) {
                return Err(ApiError::invalid(format!(
                    "programs[{i}].name: must be 1 to 100 characters, without control characters."
                )));
            }
            if program.versions.is_empty() {
                return Err(ApiError::invalid(format!(
                    "programs[{i}].versions: a program has at least one version."
                )));
            }
            let mut versions = Vec::with_capacity(program.versions.len());
            for (j, version) in program.versions.into_iter().enumerate() {
                let at = || format!("programs[{i}].versions[{j}]");
                let number = i32::try_from(version.version)
                    .ok()
                    .filter(|n| *n >= 1)
                    .ok_or_else(|| {
                        ApiError::invalid(format!("{}.version: must be 1 or more.", at()))
                    })?;
                let program_document =
                    Program::from_json(&version.document.to_string()).map_err(|error| {
                        let at = format!("{}.document", at());
                        invalid_program(&at, &program.name, version.version, error.into())
                    })?;
                if parsed
                    .insert((creation, version.version), program_document)
                    .is_some()
                {
                    return Err(ApiError::invalid(format!(
                        "{}.version: a second version with the same number.",
                        at()
                    )));
                }
                let created_at = time(version.created_at, || format!("{}.created_at", at()))?;
                versions.push((number, version.document, created_at));
            }
            programs.push(ImportProgram {
                creation_id: creation,
                name: program.name,
                source_builtin_id: program.source_builtin_id.map(|id| id.as_str().to_owned()),
                archived: program.archived,
                created_at: time(program.created_at, || format!("programs[{i}].created_at"))?,
                versions,
            });
        }

        let active_program = document
            .active_program
            .map(|creation| match archived.get(&creation.as_uuid()) {
                Some(false) => Ok(creation.as_uuid()),
                Some(true) => Err(ApiError::invalid(
                    "active_program: an archived program cannot be the active program.",
                )),
                None => Err(ApiError::invalid(
                    "active_program: not one of the export's programs.",
                )),
            })
            .transpose()?;

        let mut session_ids = HashSet::new();
        let mut set_ids = HashSet::new();
        let mut in_progress = false;
        let mut sessions = Vec::with_capacity(document.sessions.len());
        for (i, session) in document.sessions.into_iter().enumerate() {
            let at = |field: &str| format!("sessions[{i}].{field}");
            if !session_ids.insert(session.id) {
                return Err(ApiError::invalid(format!(
                    "{}: a second session with the same id.",
                    at("id")
                )));
            }
            let program = parsed
                .get(&(session.program.as_uuid(), session.version))
                .ok_or_else(|| {
                    ApiError::invalid(format!(
                        "{}: not one of the export's program versions.",
                        at("version")
                    ))
                })?;
            if program.day(&session.day).is_none() {
                return Err(ApiError::invalid(format!(
                    "{}: not a day of its program version.",
                    at("day")
                )));
            }
            if session.status.is_ended() != session.finished_at.is_some() {
                return Err(ApiError::invalid(format!(
                    "{}: an ended session has an end time, and only an ended one.",
                    at("finished_at")
                )));
            }
            if session.status == SessionStatus::InProgress {
                if in_progress {
                    return Err(ApiError::invalid(format!(
                        "{}: a second session in progress.",
                        at("status")
                    )));
                }
                in_progress = true;
            }
            let started_at = time(session.started_at, || at("started_at"))?;
            let finished_at = session
                .finished_at
                .map(|finished| time(finished, || at("finished_at")))
                .transpose()?;
            if finished_at.is_some_and(|finished| finished < started_at) {
                return Err(ApiError::invalid(format!(
                    "{}: before the session started.",
                    at("finished_at")
                )));
            }
            // Sets are not checked against the session's time span: stored sets may fall outside
            // it (docs/api.md), and an export must import back.
            let sets = session
                .sets
                .iter()
                .enumerate()
                .map(|(j, set)| {
                    if !set_ids.insert(set.id) {
                        return Err(ApiError::invalid(format!(
                            "sessions[{i}].sets[{j}].id: a second set with the same id."
                        )));
                    }
                    Ok(repo::Set {
                        session_id: session.id.as_uuid(),
                        id: set.id.as_uuid(),
                        exercise_id: set.exercise.as_str().to_owned(),
                        set_index: i32::from(set.set_index),
                        reps: i32::from(set.reps.get()),
                        weight_ng: set.weight.map(nanograms),
                        duration_s: set.duration.map(|d| i64::from(d.get())),
                        warmup: set.warm_up,
                        completed_at: time(set.completed_at, || {
                            format!("sessions[{i}].sets[{j}].completed_at")
                        })?,
                        target_weight_ng: set.target.and_then(|t| t.weight).map(nanograms),
                        target_goal: set
                            .target
                            .map(|target| serde_json::to_value(target.goal))
                            .transpose()
                            .map_err(ApiError::internal)?,
                    })
                })
                .collect::<Result<_, _>>()?;
            sessions.push(ImportSession {
                id: session.id.as_uuid(),
                program: session.program.as_uuid(),
                version: i32::try_from(session.version).map_err(ApiError::internal)?,
                day_id: session.day.as_str().to_owned(),
                status: status_name(session.status),
                started_at,
                finished_at,
                sets,
            });
        }

        Ok(Self {
            settings,
            training_maxes,
            programs,
            active_program,
            sessions,
        })
    }

    /// Writes what the account does not have yet, in the caller's transaction.
    async fn write(self, tx: &mut PgConnection, user: AuthUser) -> Result<ImportSummary, ApiError> {
        let owner = user.owner();
        // The user's row first, as every quota write does (docs/billing.md): concurrent imports
        // and program writes of the same user run one after the other.
        let plan = db::users::lock_plan(tx, owner)
            .await?
            .ok_or(ApiError::Unauthorized)?;
        let mut summary = ImportSummary::default();

        // Programs the account has keep theirs (matched by creation id); the new unarchived ones
        // must all fit in the plan, or nothing is written.
        let mut existing = HashMap::new();
        for program in &self.programs {
            if let Some((id, _)) = repo::program_by_creation(tx, owner, program.creation_id).await?
            {
                existing.insert(program.creation_id, id);
            }
        }
        // The account's programs this import touches, locked now (after the user's row, as
        // everywhere), so what the account has cannot change under the import: a concurrent
        // version upload waits for it instead of taking a number the import has read.
        let mut locked: Vec<Uuid> = existing.values().copied().collect();
        locked.sort_unstable();
        repo::lock_programs(tx, owner, &locked).await?;
        let new_unarchived = self
            .programs
            .iter()
            .filter(|p| !p.archived && !existing.contains_key(&p.creation_id))
            .count();
        if new_unarchived > 0 {
            let used = db::users::unarchived_programs(&mut *tx, owner).await?;
            let new = u32::try_from(new_unarchived).unwrap_or(u32::MAX);
            entitlements::check_quota(plan, Quota::CustomPrograms, used.saturating_add(new - 1))?;
        }

        if let Some((settings, updated_at)) = &self.settings {
            summary.settings = repo::insert_settings(tx, owner, settings, *updated_at).await?;
        }
        summary.training_maxes =
            count(repo::insert_training_maxes(tx, owner, &self.training_maxes).await?);

        // Every program of this import, by creation id: the account's (looked up above), and those
        // this import creates (the export's programs and the companions), so that no creation id
        // is ever inserted twice, whatever the order (an export may hold a program and its
        // companion, which an earlier step of the same import may have just created).
        let mut resolved = existing;
        let mut program_ids = HashMap::new();
        let mut version_ids = HashMap::new();
        for program in &self.programs {
            let account_has = resolved.get(&program.creation_id).copied();
            let id = match account_has {
                Some(id) => id,
                None => {
                    if !program.archived {
                        entitlements::reserve_quota(tx, user, Quota::CustomPrograms).await?;
                    }
                    let new = repo::NewProgram {
                        creation_id: program.creation_id,
                        name: &program.name,
                        source_builtin_id: program.source_builtin_id.as_deref(),
                        archived: program.archived,
                        created_at: program.created_at,
                    };
                    summary.programs += 1;
                    let id = repo::insert_program(tx, owner, &new).await?;
                    resolved.insert(program.creation_id, id);
                    id
                }
            };
            program_ids.insert(program.creation_id, id);
            if account_has.is_some() {
                // The account's program keeps its versions, its current version and its place
                // as the active program. Versions are matched by content, never by number
                // alone; the export's versions it does not have go to the program's archived
                // companion (see `companion_creation_id`), and their sessions with them.
                repo::lock_program(tx, owner, id).await?;
                let mut foreign = Vec::new();
                for version in &program.versions {
                    match repo::version_with_document(tx, owner, id, &version.1).await? {
                        Some(found) => {
                            version_ids.insert((program.creation_id, version.0), found);
                        }
                        None => foreign.push(version),
                    }
                }
                if foreign.is_empty() {
                    continue;
                }
                let creation = companion_creation_id(program.creation_id);
                let known = match resolved.get(&creation) {
                    Some(companion) => Some(*companion),
                    None => repo::program_by_creation(tx, owner, creation)
                        .await?
                        .map(|(companion, _)| companion),
                };
                let companion = match known {
                    Some(companion) => companion,
                    None => {
                        summary.programs += 1;
                        let name = companion_name(&program.name);
                        let new = repo::NewProgram {
                            creation_id: creation,
                            name: &name,
                            source_builtin_id: program.source_builtin_id.as_deref(),
                            // Archived: takes no quota slot and is never the active program.
                            archived: true,
                            // Now: listed after the program it accompanies.
                            created_at: OffsetDateTime::now_utc(),
                        };
                        repo::insert_program(tx, owner, &new).await?
                    }
                };
                resolved.insert(creation, companion);
                for (number, version) in
                    add_versions(tx, owner, companion, foreign, &mut summary).await?
                {
                    version_ids.insert((program.creation_id, number), version);
                }
            } else {
                // A program this import created takes all the export's versions, so its current
                // version is the export's.
                for (number, version) in add_versions(
                    tx,
                    owner,
                    id,
                    program.versions.iter().collect(),
                    &mut summary,
                )
                .await?
                {
                    version_ids.insert((program.creation_id, number), version);
                }
            }
        }

        if let Some(creation) = self.active_program {
            let program = program_ids
                .get(&creation)
                .ok_or_else(|| ApiError::internal("active program not imported"))?;
            summary.active_program = repo::insert_active_program(tx, owner, *program).await?;
        }

        let mut new_sessions = Vec::with_capacity(self.sessions.len());
        for session in &self.sessions {
            let version = version_ids
                .get(&(session.program, session.version))
                .ok_or_else(|| ApiError::internal("session version not imported"))?;
            new_sessions.push(repo::NewSession {
                id: session.id,
                program_version_id: *version,
                day_id: session.day_id.clone(),
                status: session.status,
                started_at: session.started_at,
                finished_at: session.finished_at,
            });
        }
        let inserted: HashSet<Uuid> = repo::insert_sessions(tx, owner, &new_sessions)
            .await?
            .into_iter()
            .collect();
        summary.sessions = count(inserted.len() as u64);
        // A session the account already has keeps its own sets.
        let sets: Vec<repo::Set> = self
            .sessions
            .into_iter()
            .filter(|session| inserted.contains(&session.id))
            .flat_map(|session| session.sets)
            .collect();
        summary.sets = count(repo::insert_sets(tx, owner, &sets).await?);
        Ok(summary)
    }
}

/// Adds `versions` (the export's number, document and time) to the user's program `program`,
/// each unless the program already has the same document. A version keeps its number if the
/// program does not use it, else gets the next free one. Returns the export's number of each
/// version with the id of the program's version holding its document.
async fn add_versions(
    tx: &mut PgConnection,
    owner: UserId,
    program: Uuid,
    versions: Vec<&(i32, serde_json::Value, OffsetDateTime)>,
    summary: &mut ImportSummary,
) -> Result<Vec<(i32, Uuid)>, ApiError> {
    // Locked first, as `add_version` does: a concurrent upload waits for the import.
    repo::lock_program(tx, owner, program).await?;
    let mut numbers: HashSet<i32> = repo::version_numbers(tx, owner, program)
        .await?
        .into_iter()
        .collect();
    let mut ids = Vec::with_capacity(versions.len());
    for (number, document, created_at) in versions {
        let id = match repo::version_with_document(tx, owner, program, document).await? {
            Some(id) => id,
            None => {
                let free = if numbers.contains(number) {
                    numbers.iter().max().map_or(1, |max| max.saturating_add(1))
                } else {
                    *number
                };
                numbers.insert(free);
                summary.versions += 1;
                repo::insert_version(tx, owner, program, free, document, *created_at).await?
            }
        };
        ids.push((*number, id));
    }
    Ok(ids)
}

/// The namespace of [`companion_creation_id`] (a fixed, random UUID).
const COMPANION_NAMESPACE: Uuid = Uuid::from_u128(0x7c1e_6a43_2f0b_4d5e_9a61_3b8c_d04f_e215);

/// The `creation_id` of the archived companion of the program `creation_id`: where an import puts
/// the export's versions of that program that the account's program does not have. Derived
/// (UUIDv5), so importing the same export again finds the same companion.
fn companion_creation_id(creation_id: Uuid) -> Uuid {
    Uuid::new_v5(&COMPANION_NAMESPACE, creation_id.as_bytes())
}

/// `"<name> (imported)"`, within the 100 characters of a program name.
fn companion_name(name: &str) -> String {
    const SUFFIX: &str = " (imported)";
    let kept: String = name.chars().take(100 - SUFFIX.chars().count()).collect();
    format!("{kept}{SUFFIX}")
}

fn count(rows: u64) -> u32 {
    u32::try_from(rows).unwrap_or(u32::MAX)
}

/// A weight as the database stores it. Every `Weight` fits: at most 2000 kg.
fn nanograms(weight: Weight) -> i64 {
    i64::try_from(weight.as_nanograms()).unwrap_or(i64::MAX)
}

/// A `422` listing a program document's problems, with paths from the export's root. The message
/// names the program, the version and the first broken rule, so that a support case can be
/// diagnosed from it alone: a stored version must always pass today's rules (a rule that is
/// tightened ships with a migration that fixes the stored documents, decision of 2026-10-03 on
/// #41), so this means a hand-edited file or a missing migration.
fn invalid_program(at: &str, name: &str, version: u32, problems: ProgramProblems) -> ApiError {
    let first = problems.errors.first().map_or_else(String::new, |problem| {
        let path = if problem.path.is_empty() {
            "the document".to_owned()
        } else {
            problem.path.clone()
        };
        format!(": {path}: {}", problem.message)
    });
    let name: String = name.chars().take(MAX_MESSAGE_CHARS).collect();
    let message = format!(
        "Program \"{name}\", version {version} ({at}), is not valid under this version of Iron \
         Oxide's rules{first}."
    );
    ApiError::InvalidProgramIn(
        Cow::Owned(message),
        ProgramProblems {
            omitted: problems.omitted,
            errors: problems
                .errors
                .into_iter()
                .map(|problem| ProgramProblem {
                    path: match problem.path.as_str() {
                        "" => at.to_owned(),
                        path if path.starts_with('[') => format!("{at}{path}"),
                        path => format!("{at}.{path}"),
                    },
                    message: problem.message,
                    // Positions in the re-serialized document would not match the file.
                    line: None,
                    column: None,
                })
                .collect(),
        },
    )
}

// --- Deletion ----------------------------------------------------------------------------------

/// Deletes `user`'s account after checking that they signed in recently: cancels billing (a
/// documented stub until Stripe is implemented), deletes the user and everything they own in one
/// transaction (every sign-in session included), then clears this session's cookie.
pub async fn delete(ctx: &AuthContext, user: AuthUser) -> Result<(), ApiError> {
    // The step-up shared with adding a sign-in method (see `require_recent_sign_in`).
    match ctx.require_recent_sign_in().await {
        Ok(()) => {}
        Err(AuthError::ReauthenticationRequired) => {
            return Err(ApiError::Forbidden(Cow::Borrowed(REAUTHENTICATE)));
        }
        Err(error) => return Err(error.into()),
    }
    let owner = user.owner();
    // One of the slots shared with the imports, held until the deletion has finished.
    let _slot = take_slot(&ctx.app)?;
    // Before anything is deleted: a failure keeps the account, so the user is never left billed
    // for an account that no longer exists.
    billing::cancel_before_account_deletion(ctx.db(), owner).await?;
    // A large account's cascade may take longer than the pool's deadlines.
    let mut conn = repo::long_connection(ctx.db()).await?;
    let result = async {
        let mut tx = conn.begin().await.map_err(db::error::RepoError::from)?;
        let deleted = repo::delete_user(&mut tx, owner).await?;
        tx.commit().await.map_err(db::error::RepoError::from)?;
        Ok::<_, ApiError>(deleted)
    }
    .await;
    repo::close(conn).await;
    let deleted = result?;
    if !deleted {
        // Deleted by a concurrent request: same outcome.
        tracing::info!("account already deleted");
    }
    tracing::info!("account deleted");
    ctx.sign_out().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
