//! The user's rights over their data (#22, GDPR): export everything they own, import such an
//! export back, and delete the account.
//!
//! - [`export_account_data`] returns an [`ExportDocument`]: a versioned JSON document of the user's
//!   training data and account, never anyone else's and never a secret (no session, no passkey
//!   key material). The format is documented in `docs/export-format.md`.
//! - [`import_account_data`] takes such a document back, as the JSON text of the file. It is
//!   validated in full before anything is written, then written in one transaction. Importing the
//!   same export twice changes nothing the second time: rows are matched by their ids (programs
//!   by their `creation_id`) and **what the account already has wins**.
//! - [`delete_account`] deletes the account and everything it owns, signs out every session, and
//!   needs a sign-in from the last [`DELETE_REAUTH_WINDOW_SECS`] seconds.
//!
//! The logic is in `crate::server::api::account`; conventions in `docs/api.md`.

// The browser build only calls these functions; the Settings screen that does is still to come.
#![cfg_attr(
    not(feature = "server"),
    allow(dead_code, reason = "used by the Settings/account screen (#34)")
)]

use dioxus::prelude::*;
use iron_oxide_domain::{
    CreationId, DayId, LoggedSet, SessionId, SessionStatus, UserId, entitlements::Plan,
    program::BuiltinProgramId, time::Timestamp,
};
use serde::{Deserialize, Serialize};

use crate::api::settings::{Settings, TrainingMax};

#[cfg(feature = "server")]
use {
    crate::server::{
        AppState,
        api::account,
        auth::{AuthContext, AuthUser},
    },
    dioxus::server::axum::{Extension, extract::DefaultBodyLimit},
};

/// The `format` of every export: what tells an Iron Oxide export apart from any other JSON file.
pub const EXPORT_FORMAT: &str = "iron-oxide-export";

/// The version of the export format this server writes. A change that an older reader would
/// misread gets a new version; see `docs/export-format.md`.
///
/// - 1: #22.
/// - 2: #60, each set may carry the `target` it was prescribed. An older reader would import a
///   version 2 file without them, which changes how its training max sessions are judged.
pub const EXPORT_FORMAT_VERSION: u32 = 2;

/// The oldest version this server still imports. Version 1 is version 2 without set targets: its
/// sets load as logged before #60.
pub const OLDEST_IMPORTED_FORMAT_VERSION: u32 = 1;

/// The largest export, in bytes of compact JSON. An account whose export would be larger gets a
/// `413` from [`export_account_data`] (and support), so that every export this server writes can
/// be imported back: [`import_account_data`] accepts documents up to the same size.
///
/// About 40,000 logged sets, ten years of training three times a week.
pub const MAX_EXPORT_BYTES: usize = 8 * 1024 * 1024;

/// Largest request body [`import_account_data`] reads, in bytes, checked before anything parses
/// it; a larger body is refused with `413`.
///
/// The document travels as a JSON string, where each `"` and `\` takes two bytes, so a document
/// of [`MAX_EXPORT_BYTES`] can take up to twice as many bytes in the body.
pub const IMPORT_BODY_LIMIT: usize = 2 * MAX_EXPORT_BYTES + 64 * 1024;

/// The route of [`import_account_data`]: it reads its own body (see `server::limits`).
pub const IMPORT_PATH: &str = "/api/account/import";

/// How long an import body may take to arrive (the default for other requests is 10 s).
///
/// Sized from the largest import: a real export of [`MAX_EXPORT_BYTES`] travels as about 9.4 MB
/// (its quotes escaped), which takes 60 s at 1.25 Mbit/s, a slow mobile uplink. A slower body
/// gets `408` and frees its import slot.
pub const IMPORT_BODY_READ_TIMEOUT_SECS: u64 = 60;

/// How recent the sign-in must be for [`delete_account`] (and for adding a passkey or linking
/// Google): 10 minutes. Signing in again (with a passkey or Google) restarts it.
pub const DELETE_REAUTH_WINDOW_SECS: u64 = 10 * 60;

/// Everything a user owns, as exported. See `docs/export-format.md` for every field.
///
/// Times are [`Timestamp`]s (milliseconds since the Unix epoch, UTC), like everywhere in the API:
/// sub-millisecond digits of server-side times (`created_at`, `set_at`) are dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportDocument {
    /// Always [`EXPORT_FORMAT`].
    pub format: String,
    /// [`EXPORT_FORMAT_VERSION`] when written by this server.
    pub format_version: u32,
    pub exported_at: Timestamp,
    /// For information only: an import never changes the account.
    pub account: ExportAccount,
    /// For information only: sign-in methods are never imported.
    pub sign_in: ExportSignIn,
    /// `None` when the user never saved settings (the app then uses its defaults).
    pub settings: Option<ExportSettings>,
    /// By exercise id.
    pub training_maxes: Vec<TrainingMax>,
    /// The user's own programs (copies of built-ins included), oldest first, archived ones too.
    pub programs: Vec<ExportProgram>,
    /// The `creation_id` of the active program, if any.
    pub active_program: Option<CreationId>,
    /// Every workout session, oldest first, with its sets.
    pub sessions: Vec<ExportSession>,
}

/// The account itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportAccount {
    pub user_id: UserId,
    pub created_at: Timestamp,
    pub plan: Plan,
    pub display_name: Option<String>,
}

/// How the user signs in: what the account screen shows, without any key material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSignIn {
    pub passkeys: Vec<ExportPasskey>,
    /// Linked external accounts (Google).
    pub linked_accounts: Vec<ExportLinkedAccount>,
}

/// A passkey's metadata. Neither its public key nor its credential id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportPasskey {
    pub nickname: String,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
    /// Whether the passkey is synced by its provider.
    pub backed_up: bool,
}

/// A linked external account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportLinkedAccount {
    /// `google`.
    pub provider: String,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
}

/// The saved settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSettings {
    #[serde(flatten)]
    pub settings: Settings,
    pub updated_at: Timestamp,
}

/// One of the user's programs with all its versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportProgram {
    /// The program's key in the export (and in the account: unique per user). Sessions and the
    /// active program refer to the program by it.
    pub creation_id: CreationId,
    pub name: String,
    /// The built-in it was copied from, if any.
    pub source_builtin_id: Option<BuiltinProgramId>,
    pub archived: bool,
    pub created_at: Timestamp,
    /// Oldest first. At least one.
    pub versions: Vec<ExportVersion>,
}

/// One immutable version of a program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportVersion {
    /// 1, 2, … within its program.
    pub version: u32,
    pub created_at: Timestamp,
    /// The `program.json` document, as stored.
    pub document: serde_json::Value,
}

/// A workout session and its sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportSession {
    pub id: SessionId,
    /// The `creation_id` of the session's program.
    pub program: CreationId,
    /// The program version it was run from.
    pub version: u32,
    pub day: DayId,
    pub status: SessionStatus,
    pub started_at: Timestamp,
    /// `None` while in progress.
    pub finished_at: Option<Timestamp>,
    /// In the order they were completed.
    pub sets: Vec<LoggedSet<Timestamp>>,
}

/// What an import added. All zero when the account already had everything (a repeated import).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSummary {
    /// Whether the settings were added (an account that already has settings keeps them).
    pub settings: bool,
    pub training_maxes: u32,
    pub programs: u32,
    /// Versions added, those of new programs included.
    pub versions: u32,
    /// Whether the active program was set (an account that already has one keeps it).
    pub active_program: bool,
    pub sessions: u32,
    pub sets: u32,
}

/// Everything the signed-in user owns, as an [`ExportDocument`]. `413` in the unlikely case that
/// it would be larger than [`MAX_EXPORT_BYTES`].
#[post("/api/account/export", state: Extension<AppState>, user: AuthUser)]
pub async fn export_account_data() -> Result<ExportDocument, ServerFnError> {
    Ok(account::export(&state.db, user.owner()).await?)
}

/// Imports an export (the JSON text of the file) into the signed-in user's account and returns
/// what it added. Rows the account already has are kept as they are, so importing the same export
/// twice adds nothing the second time.
///
/// # Errors
/// - `413` for a body over [`IMPORT_BODY_LIMIT`] or a document over [`MAX_EXPORT_BYTES`];
/// - `422` for a file that is not an export, an unsupported `format_version`, or invalid content
///   (a program version that is not a valid program carries its problems, as for an upload);
/// - `403` when the new programs would take the account over its plan's program limit. Nothing is
///   written then.
#[post("/api/account/import", state: Extension<AppState>, user: AuthUser, slot: Extension<account::AccountSlot>)]
#[middleware(DefaultBodyLimit::max(IMPORT_BODY_LIMIT))]
#[middleware(dioxus::server::axum::middleware::from_fn(account::limit_import_body))]
pub async fn import_account_data(document: String) -> Result<ImportSummary, ServerFnError> {
    Ok(account::import(&state.db, user, &document, slot.0).await?)
}

/// Deletes the signed-in user's account and everything it owns, signs out all of its sessions on
/// every device, and clears this browser's cookie.
///
/// The user must have signed in within the last [`DELETE_REAUTH_WINDOW_SECS`] seconds: otherwise
/// `403`, and the UI asks them to sign in again (passkey or Google) and retries.
#[post("/api/account/delete", ctx: AuthContext, user: AuthUser)]
pub async fn delete_account() -> Result<(), ServerFnError> {
    Ok(account::delete(&ctx, user).await?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_transport_limit_leaves_room_for_an_escaped_document() {
        let document = "\"\\".repeat(MAX_EXPORT_BYTES / 2);
        let body = json!({ "document": document }).to_string();
        assert!(body.len() <= IMPORT_BODY_LIMIT, "{}", body.len());
    }

    #[test]
    fn settings_are_flattened_next_to_their_time() {
        let settings = ExportSettings {
            settings: Settings::defaults(),
            updated_at: Timestamp::from_epoch_millis(5),
        };
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["unit"], "kg");
        assert_eq!(value["bar_weight"], 20.0);
        assert_eq!(value["updated_at"], 5);
        assert_eq!(
            serde_json::from_value::<ExportSettings>(value).unwrap(),
            settings
        );
    }
}
