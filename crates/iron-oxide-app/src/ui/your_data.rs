//! The "Your data" card of Settings (#118): export everything, import an export back, delete the
//! account. The server side is #22 (`crate::api::account`, `docs/export-format.md`).
//!
//! - **Export** downloads the export as `iron-oxide-export-<date>.json`.
//! - **Import** takes a file or pasted text, checks it is an export this server reads, shows what it
//!   holds, and imports it on confirmation. What the account already has is kept, so the preview
//!   says what the file holds and the result says what was added.
//! - **Delete** says what goes, asks to type `DELETE`, and needs a sign-in from the last
//!   10 minutes: when the server answers `403`, the card asks to sign in again (a passkey) and
//!   retries. Then the app is signed out.
//!
//! Actions run to completion even if the user leaves Settings (`spawn_forever`); their outcome
//! always reaches the app-wide banner, and the card too while it is shown.

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use iron_oxide_domain::time::Timestamp;

use super::components::{Button, ButtonVariant};
use super::errors::{BannerKind, use_errors, wait_text};
use super::programs::date_text;
use super::shell::{SessionStatus, set_session, use_session};
use super::user_settings::use_user_settings;
use crate::api::account::{
    EXPORT_FORMAT, EXPORT_FORMAT_VERSION, ExportDocument, ImportSummary, MAX_EXPORT_BYTES,
    OLDEST_IMPORTED_FORMAT_VERSION, delete_account, export_account_data, import_account_data,
};
use crate::api::error::{ApiFailure, FailureKind};
use crate::api::programs::{ProgramProblems, list_programs};
use crate::auth::api::{me, passkey_sign_in_begin, passkey_sign_in_finish};
use crate::auth::browser;
use crate::offline::use_outbox;

/// What the user types to confirm the deletion.
pub const DELETE_CONFIRMATION: &str = "DELETE";

// --- View models ---------------------------------------------------------------------------------

/// The export's file name: `iron-oxide-export-2026-10-04.json` (the export's UTC date).
#[must_use]
pub fn export_file_name(exported_at: Timestamp) -> String {
    format!("iron-oxide-export-{}.json", date_text(exported_at))
}

/// What an export file holds, shown before importing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPreview {
    pub exported_on: String,
    pub settings: bool,
    pub training_maxes: usize,
    pub programs: usize,
    pub versions: usize,
    pub active_program: bool,
    pub sessions: usize,
    pub sets: usize,
}

impl ImportPreview {
    /// Reads an export's text: it must be an export, of a version this server imports, and fit
    /// the size the server accepts. The server checks everything again.
    ///
    /// # Errors
    /// A message for the user.
    pub fn read(text: &str) -> Result<Self, String> {
        let text = text.trim_start_matches('\u{feff}').trim();
        if text.is_empty() {
            return Err("Choose an export file or paste its contents.".to_owned());
        }
        if text.len() > MAX_EXPORT_BYTES {
            return Err(too_large_message());
        }
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|_| "This is not an Iron Oxide export (it is not JSON).".to_owned())?;
        if value.get("format").and_then(serde_json::Value::as_str) != Some(EXPORT_FORMAT) {
            return Err("This is not an Iron Oxide export.".to_owned());
        }
        let version = value
            .get("format_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if version > u64::from(EXPORT_FORMAT_VERSION) {
            return Err(
                "This export was made by a newer version of Iron Oxide. Reload the app and try \
                 again."
                    .to_owned(),
            );
        }
        if version < u64::from(OLDEST_IMPORTED_FORMAT_VERSION) {
            return Err("This export's format is not supported.".to_owned());
        }
        let document: ExportDocument = serde_json::from_value(value).map_err(|_| {
            "This export is damaged: some of its content cannot be read.".to_owned()
        })?;
        Ok(Self::of(&document))
    }

    /// What `document` holds.
    #[must_use]
    pub fn of(document: &ExportDocument) -> Self {
        Self {
            exported_on: date_text(document.exported_at),
            settings: document.settings.is_some(),
            training_maxes: document.training_maxes.len(),
            programs: document.programs.len(),
            versions: document
                .programs
                .iter()
                .map(|program| program.versions.len())
                .sum(),
            active_program: document.active_program.is_some(),
            sessions: document.sessions.len(),
            sets: document
                .sessions
                .iter()
                .map(|session| session.sets.len())
                .sum(),
        }
    }

    /// One line per kind of data the export holds.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.programs > 0 {
            lines.push(format!(
                "{} ({} {})",
                count(self.programs, "program", "programs"),
                self.versions,
                if self.versions == 1 {
                    "version"
                } else {
                    "versions"
                }
            ));
        }
        if self.sessions > 0 {
            lines.push(format!(
                "{} with {}",
                count(self.sessions, "workout", "workouts"),
                count(self.sets, "set", "sets")
            ));
        }
        if self.training_maxes > 0 {
            lines.push(count(self.training_maxes, "training max", "training maxes"));
        }
        if self.settings {
            lines.push("Settings".to_owned());
        }
        if self.active_program {
            lines.push("Which program is active".to_owned());
        }
        lines
    }

    /// Whether the export holds anything that can be added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines().is_empty()
    }
}

/// `1 program`, `3 programs`.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The note about archived "(imported)" copies (decision log #41), when it can apply: the export
/// has programs and so does the account.
#[must_use]
pub fn companion_note(preview: &ImportPreview, account_has_programs: bool) -> Option<&'static str> {
    (preview.programs > 0 && account_has_programs).then_some(
        "If a program in this export is already in your account with other versions, your \
         program stays as it is: the export's other versions go to an archived copy named \
         \u{201c}<name> (imported)\u{201d}, with the workouts done with them.",
    )
}

/// What an import added.
#[must_use]
pub fn summary_text(summary: &ImportSummary) -> String {
    let mut added = Vec::new();
    let (programs, versions) = (summary.programs as usize, summary.versions as usize);
    match (programs, versions) {
        (0, 0) => {}
        (0, versions) => added.push(count(versions, "program version", "program versions")),
        (programs, versions) if versions == programs => {
            added.push(count(programs, "program", "programs"));
        }
        (programs, versions) => added.push(format!(
            "{} ({})",
            count(programs, "program", "programs"),
            count(versions, "version", "versions")
        )),
    }
    if summary.sessions > 0 {
        added.push(count(summary.sessions as usize, "workout", "workouts"));
    }
    if summary.sets > 0 {
        added.push(count(summary.sets as usize, "set", "sets"));
    }
    if summary.training_maxes > 0 {
        added.push(count(
            summary.training_maxes as usize,
            "training max",
            "training maxes",
        ));
    }
    if summary.settings {
        added.push("your settings".to_owned());
    }
    if summary.active_program {
        added.push("the active program".to_owned());
    }
    match added.as_slice() {
        [] => "Nothing new: your account already had everything in this export.".to_owned(),
        [one] => format!("Imported: {one}."),
        [rest @ .., last] => format!("Imported: {} and {last}.", rest.join(", ")),
    }
}

/// The message for an export file over the size the server imports.
#[must_use]
pub fn too_large_message() -> String {
    format!(
        "This file is too large to be an export (the limit is {} MiB).",
        MAX_EXPORT_BYTES / (1024 * 1024)
    )
}

/// The message for a failed export, import or deletion: the server's own (`docs/api.md`), with
/// how long to wait for a `503` that says (`Busy`, both slots taken) or a `429`.
#[must_use]
pub fn failure_text(error: &ServerFnError) -> String {
    let failure = ApiFailure::classify(error);
    let wait = failure
        .details
        .as_ref()
        .and_then(|details| details.get("retry_after_secs"))
        .and_then(serde_json::Value::as_u64)
        .filter(|_| {
            matches!(
                failure.kind,
                FailureKind::Transient | FailureKind::RateLimited
            )
        });
    match wait {
        Some(secs) => format!("{} Try again in {}.", failure.message, wait_text(secs)),
        None => failure.message,
    }
}

/// Whether a refused deletion asks for a fresh sign-in (its only `403`).
#[must_use]
pub fn needs_fresh_sign_in(error: &ServerFnError) -> bool {
    ApiFailure::classify(error).kind == FailureKind::Forbidden
}

/// Whether the typed confirmation allows the deletion.
#[must_use]
pub fn confirms_deletion(typed: &str) -> bool {
    typed.trim() == DELETE_CONFIRMATION
}

/// Reads a picked file as an export's text: UTF-8 (as JSON must be) and within the size limit.
///
/// # Errors
/// A message for the user.
pub fn decode_file(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > MAX_EXPORT_BYTES + 3 {
        return Err(too_large_message());
    }
    std::str::from_utf8(bytes)
        .map(|text| text.trim_start_matches('\u{feff}').to_owned())
        .map_err(|_| "This file is not an export: it is not UTF-8 text.".to_owned())
}

// --- Browser -------------------------------------------------------------------------------------

/// Saves `text` as a file named `name` (a download), where the browser can.
fn download(name: &str, text: &str) -> Result<(), String> {
    #[cfg(feature = "web")]
    {
        use wasm_bindgen::JsCast;

        let failed = |_| "The export could not be saved.".to_owned();
        let window = web_sys::window().ok_or("No browser window.")?;
        let document = window.document().ok_or("No document.")?;
        let parts = js_sys::Array::of1(&wasm_bindgen::JsValue::from_str(text));
        let options = web_sys::BlobPropertyBag::new();
        options.set_type("application/json");
        let blob =
            web_sys::Blob::new_with_str_sequence_and_options(&parts, &options).map_err(failed)?;
        let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(failed)?;
        let link: web_sys::HtmlAnchorElement = document
            .create_element("a")
            .map_err(failed)?
            .dyn_into()
            .map_err(|_| "The export could not be saved.".to_owned())?;
        link.set_href(&url);
        link.set_download(name);
        link.click();
        let _ = web_sys::Url::revoke_object_url(&url);
        Ok(())
    }
    #[cfg(not(feature = "web"))]
    {
        let _ = (name, text);
        Err("Downloads need a browser.".to_owned())
    }
}

/// Reloads the page, so the app starts again from the server's session.
fn reload_page() {
    #[cfg(feature = "web")]
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}

// --- Card ----------------------------------------------------------------------------------------

/// A message in the card.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Note {
    Info(String),
    Error(String),
}

/// Sets a signal of a component that may be gone by the time an action ends.
fn try_set<T: 'static>(mut signal: Signal<T>, value: T) {
    if let Ok(mut slot) = signal.try_write() {
        *slot = value;
    }
}

#[component]
fn NoteView(note: Option<Note>) -> Element {
    match note {
        Some(Note::Info(text)) => {
            rsx! { p { class: "io-notice io-notice-info", role: "status", "{text}" } }
        }
        Some(Note::Error(text)) => {
            rsx! { p { class: "io-notice io-notice-error", role: "alert", "{text}" } }
        }
        None => rsx! {},
    }
}

/// The "Your data" card.
#[component]
pub fn YourDataCard() -> Element {
    rsx! {
        section { class: "io-card io-your-data", aria_labelledby: "your-data-title",
            h2 { id: "your-data-title", "Your data" }
            ExportPart {}
            ImportPart {}
            DeletePart {}
        }
    }
}

#[component]
fn ExportPart() -> Element {
    let errors = use_errors();
    let busy = use_signal(|| false);
    let note = use_signal(|| None::<Note>);
    let export = move |_| {
        if *busy.peek() {
            return;
        }
        try_set(busy, true);
        try_set(note, None);
        spawn_forever(async move {
            match export_account_data().await {
                Ok(document) => {
                    let name = export_file_name(document.exported_at);
                    let saved = serde_json::to_string(&document)
                        .map_err(|_| "The export could not be saved.".to_owned())
                        .and_then(|text| download(&name, &text));
                    match saved {
                        Ok(()) => try_set(note, Some(Note::Info(format!("Saved as {name}.")))),
                        Err(message) => {
                            errors.show(BannerKind::Error, message.clone());
                            try_set(note, Some(Note::Error(message)));
                        }
                    }
                }
                Err(error) => {
                    let message = failure_text(&error);
                    errors.show(BannerKind::Error, message.clone());
                    try_set(note, Some(Note::Error(message)));
                }
            }
            try_set(busy, false);
        });
    };
    let busy_now = *busy.read();
    rsx! {
        h3 { "Export" }
        p { class: "io-muted",
            "Download everything you have in Iron Oxide (programs, workouts, settings and account details) as one JSON file."
        }
        Button {
            id: "export-data",
            variant: ButtonVariant::Secondary,
            block: true,
            busy: busy_now,
            onclick: export,
            if busy_now { "Preparing your export…" } else { "Download my data" }
        }
        NoteView { note: note.read().clone() }
    }
}

/// What the import part shows.
#[derive(Debug, Clone, PartialEq)]
enum ImportStage {
    /// Waiting for a file or pasted text.
    Choose,
    /// An export was read: confirm to import it.
    Preview {
        text: String,
        preview: ImportPreview,
    },
}

#[component]
fn ImportPart() -> Element {
    let errors = use_errors();
    let settings = use_user_settings();
    let mut stage = use_signal(|| ImportStage::Choose);
    let mut pasted = use_signal(String::new);
    let mut picks = use_signal(|| 0_u32);
    let busy = use_signal(|| false);
    let note = use_signal(|| None::<Note>);
    let mut problems = use_signal(|| None::<ProgramProblems>);
    // Whether the account already has programs: the "(imported)" note can only apply then.
    let programs = use_resource(move || async move { list_programs(true).await.ok() });
    let account_has_programs = programs
        .read()
        .clone()
        .flatten()
        .is_some_and(|list| !list.is_empty());

    let mut preview_text = move |text: String| {
        try_set(note, None);
        problems.set(None);
        match ImportPreview::read(&text) {
            Ok(preview) => stage.set(ImportStage::Preview { text, preview }),
            Err(message) => try_set(note, Some(Note::Error(message))),
        }
    };
    let on_pick = move |event: FormEvent| {
        let Some(file) = event.files().into_iter().next() else {
            return;
        };
        let next = *picks.peek() + 1;
        picks.set(next);
        if usize::try_from(file.size()).map_or(true, |size| size > MAX_EXPORT_BYTES + 3) {
            try_set(note, Some(Note::Error(too_large_message())));
            return;
        }
        spawn(async move {
            match file.read_bytes().await {
                Ok(bytes) => match decode_file(&bytes) {
                    Ok(text) => preview_text(text),
                    Err(message) => try_set(note, Some(Note::Error(message))),
                },
                Err(_) => try_set(
                    note,
                    Some(Note::Error("This file could not be read.".to_owned())),
                ),
            }
        });
    };
    let import = move |_| {
        let ImportStage::Preview { text, .. } = stage.peek().clone() else {
            return;
        };
        if *busy.peek() {
            return;
        }
        try_set(busy, true);
        try_set(note, None);
        spawn_forever(async move {
            match import_account_data(text).await {
                Ok(summary) => {
                    let message = summary_text(&summary);
                    errors.show(BannerKind::Info, message.clone());
                    try_set(note, Some(Note::Info(message)));
                    try_set(stage, ImportStage::Choose);
                    try_set(pasted, String::new());
                    // Settings may have been added: show them.
                    if summary.settings {
                        settings.reload();
                    }
                }
                Err(error) => {
                    let message = failure_text(&error);
                    errors.show(BannerKind::Error, message.clone());
                    try_set(note, Some(Note::Error(message)));
                    try_set(problems, ProgramProblems::from_error(&error));
                }
            }
            try_set(busy, false);
        });
    };

    let busy_now = *busy.read();
    let current = stage.read().clone();
    rsx! {
        h3 { "Import" }
        match current {
            ImportStage::Choose => rsx! {
                p { class: "io-muted",
                    "Bring back an Iron Oxide export. Only what your account does not have yet is added; nothing you have is changed."
                }
                div { class: "io-file-picker", ImportFilePicker { key: "{picks}", on_pick } }
                div { class: "io-field",
                    label { r#for: "import-text", "Or paste an export" }
                    textarea {
                        id: "import-text",
                        class: "io-input io-textarea",
                        rows: "4",
                        spellcheck: "false",
                        value: "{pasted}",
                        oninput: move |event| pasted.set(event.value()),
                    }
                    Button {
                        variant: ButtonVariant::Secondary,
                        disabled: pasted.read().trim().is_empty(),
                        onclick: move |_| preview_text(pasted.peek().clone()),
                        "Check this export"
                    }
                }
            },
            ImportStage::Preview { preview, .. } => rsx! {
                div { class: "io-import-preview",
                    p { "This export (from {preview.exported_on}) holds:" }
                    if preview.is_empty() {
                        p { class: "io-muted", "Nothing to import." }
                    } else {
                        ul {
                            for line in preview.lines() {
                                li { key: "{line}", "{line}" }
                            }
                        }
                    }
                    p { class: "io-muted io-hint",
                        "What your account already has is kept as it is. The result says what was added."
                    }
                    if let Some(text) = companion_note(&preview, account_has_programs) {
                        p { class: "io-muted io-hint", "{text}" }
                    }
                    div { class: "io-actions",
                        Button {
                            id: "import-confirm",
                            busy: busy_now,
                            disabled: preview.is_empty(),
                            onclick: import,
                            if busy_now { "Importing…" } else { "Import" }
                        }
                        Button {
                            variant: ButtonVariant::Ghost,
                            disabled: busy_now,
                            onclick: move |_| stage.set(ImportStage::Choose),
                            "Cancel"
                        }
                    }
                }
            },
        }
        NoteView { note: note.read().clone() }
        if let Some(problems) = problems.read().clone() {
            ul { class: "io-notice io-notice-error io-problems",
                for (index, problem) in problems.errors.iter().enumerate() {
                    li { key: "{index}", "{problem.path}: {problem.message}" }
                }
            }
        }
    }
}

/// The button that opens the file picker (a label for a hidden file input). Re-created by its
/// key after each pick, so picking the same file again works.
#[component]
fn ImportFilePicker(on_pick: EventHandler<FormEvent>) -> Element {
    rsx! {
        label { class: "io-button io-button-secondary io-file-button", r#for: "import-file",
            "Choose an export file"
        }
        input {
            id: "import-file",
            class: "io-sr-only",
            r#type: "file",
            accept: ".json,application/json",
            onchange: move |event| on_pick.call(event),
        }
    }
}

/// Where the deletion is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteStage {
    Idle,
    /// The server asked for a fresh sign-in.
    NeedsSignIn,
}

#[component]
fn DeletePart() -> Element {
    let errors = use_errors();
    let session = use_session();
    let outbox = use_outbox();
    let mut typed = use_signal(String::new);
    let busy = use_signal(|| false);
    let stage = use_signal(|| DeleteStage::Idle);
    let note = use_signal(|| None::<Note>);

    // Deletes, then signs out; a 403 asks for a fresh sign-in.
    let run_delete = move || {
        spawn_forever(async move {
            let user = me().await.map(|me| me.user_id);
            match delete_account().await {
                Ok(()) => {
                    if let Ok(user) = user {
                        outbox.signed_out(user);
                    }
                    errors.show(
                        BannerKind::Info,
                        "Your account and everything in it were deleted.",
                    );
                    set_session(session, SessionStatus::SignedOut);
                }
                Err(error) if needs_fresh_sign_in(&error) => {
                    try_set(stage, DeleteStage::NeedsSignIn);
                    try_set(note, Some(Note::Info(failure_text(&error))));
                }
                Err(error) => {
                    let message = failure_text(&error);
                    errors.show(BannerKind::Error, message.clone());
                    try_set(note, Some(Note::Error(message)));
                }
            }
            try_set(busy, false);
        });
    };
    let delete = move |_| {
        if !confirms_deletion(&typed.peek()) || *busy.peek() {
            return;
        }
        if !browser::confirm("Delete your account and everything in it? This cannot be undone.") {
            return;
        }
        try_set(busy, true);
        try_set(note, None);
        run_delete();
    };
    // Signs in again with a passkey (the same account), then deletes.
    let sign_in_again = move |_| {
        if *busy.peek() {
            return;
        }
        try_set(busy, true);
        try_set(note, None);
        spawn_forever(async move {
            let before = me().await.map(|me| me.user_id);
            let result = async {
                let options = passkey_sign_in_begin()
                    .await
                    .map_err(|error| failure_text(&error))?;
                let credential = browser::get_passkey(options)
                    .await
                    .map_err(|error| error.to_string())?;
                passkey_sign_in_finish(credential)
                    .await
                    .map_err(|error| failure_text(&error))
            }
            .await;
            match (result, before) {
                (Ok(now), Ok(before)) if now.user_id == before => {
                    try_set(stage, DeleteStage::Idle);
                    run_delete();
                }
                (Ok(_), _) => {
                    // Another account's passkey: this browser is now signed in to it. Start the
                    // app again from the server's session, and delete nothing.
                    errors.show(
                        BannerKind::Error,
                        "That passkey belongs to another account. Nothing was deleted.",
                    );
                    try_set(busy, false);
                    reload_page();
                }
                (Err(message), _) => {
                    try_set(note, Some(Note::Error(message)));
                    try_set(busy, false);
                }
            }
        });
    };

    let busy_now = *busy.read();
    let confirmed = confirms_deletion(&typed.read());
    rsx! {
        h3 { "Delete your account" }
        p { class: "io-muted",
            "Deletes your account and everything in it, for good: programs, workouts, records, settings, passkeys and the Google link. Every device is signed out. Download your data first if you may want it back."
        }
        if *stage.read() == DeleteStage::NeedsSignIn {
            div { class: "io-waiting", role: "status",
                p { "For your safety, deleting your account needs a sign-in from the last 10 minutes." }
                Button {
                    id: "delete-sign-in-again",
                    busy: busy_now,
                    onclick: sign_in_again,
                    "Sign in again with a passkey, then delete"
                }
                p { class: "io-muted io-hint",
                    "Signed in with Google? Sign out, sign in again with Google, and come back here within 10 minutes."
                }
            }
        } else {
            div { class: "io-field",
                label { r#for: "delete-confirm", "Type {DELETE_CONFIRMATION} to confirm" }
                input {
                    id: "delete-confirm",
                    class: "io-input",
                    r#type: "text",
                    autocomplete: "off",
                    autocapitalize: "characters",
                    spellcheck: "false",
                    value: "{typed}",
                    disabled: busy_now,
                    oninput: move |event| typed.set(event.value()),
                }
            }
            Button {
                id: "delete-account",
                variant: ButtonVariant::Danger,
                block: true,
                busy: busy_now,
                disabled: !confirmed,
                onclick: delete,
                if busy_now { "Deleting…" } else { "Delete my account" }
            }
        }
        NoteView { note: note.read().clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::account::{ExportAccount, ExportSignIn};
    use iron_oxide_domain::{UserId, entitlements::Plan};
    use serde_json::json;

    fn document() -> ExportDocument {
        ExportDocument {
            format: EXPORT_FORMAT.to_owned(),
            format_version: EXPORT_FORMAT_VERSION,
            exported_at: Timestamp::from_epoch_millis(1_791_000_000_000),
            account: ExportAccount {
                user_id: UserId::from_uuid(uuid::Uuid::nil()),
                created_at: Timestamp::from_epoch_millis(0),
                plan: Plan::Free,
                display_name: None,
            },
            sign_in: ExportSignIn {
                passkeys: vec![],
                linked_accounts: vec![],
            },
            settings: None,
            training_maxes: vec![],
            programs: vec![],
            active_program: None,
            sessions: vec![],
        }
    }

    #[test]
    fn the_file_name_has_the_export_date() {
        assert_eq!(
            export_file_name(Timestamp::from_epoch_millis(1_791_000_000_000)),
            "iron-oxide-export-2026-10-03.json"
        );
    }

    #[test]
    fn an_export_is_read_and_previewed() {
        let text = serde_json::to_string(&document()).unwrap();
        let preview = ImportPreview::read(&text).unwrap();
        assert_eq!(preview.exported_on, "2026-10-03");
        assert!(preview.is_empty());
        // A byte-order mark and surrounding blanks are fine.
        assert_eq!(
            ImportPreview::read(&format!("\u{feff}  {text}\n")),
            Ok(preview)
        );
        // Version 1 is still read.
        let mut v1 = serde_json::to_value(document()).unwrap();
        v1["format_version"] = json!(1);
        assert!(ImportPreview::read(&v1.to_string()).is_ok());
    }

    #[test]
    fn other_files_are_refused_with_a_reason() {
        let read = |text: &str| ImportPreview::read(text).unwrap_err();
        assert!(read("").contains("Choose"));
        assert!(read("not json").contains("not JSON"));
        assert_eq!(
            read(r#"{"format": "something-else"}"#),
            "This is not an Iron Oxide export."
        );
        let mut newer = serde_json::to_value(document()).unwrap();
        newer["format_version"] = json!(EXPORT_FORMAT_VERSION + 1);
        assert!(read(&newer.to_string()).contains("newer version"));
        let mut old = serde_json::to_value(document()).unwrap();
        old["format_version"] = json!(0);
        assert!(read(&old.to_string()).contains("not supported"));
        let mut damaged = serde_json::to_value(document()).unwrap();
        damaged["sessions"] = json!("nope");
        assert!(read(&damaged.to_string()).contains("damaged"));
        assert_eq!(
            read(&" ".repeat(MAX_EXPORT_BYTES + 1).replace(' ', "x")),
            too_large_message()
        );
    }

    #[test]
    fn the_preview_counts_what_the_export_holds() {
        let preview = ImportPreview {
            exported_on: "2026-10-03".to_owned(),
            settings: true,
            training_maxes: 1,
            programs: 2,
            versions: 3,
            active_program: true,
            sessions: 1,
            sets: 5,
        };
        assert_eq!(
            preview.lines(),
            [
                "2 programs (3 versions)",
                "1 workout with 5 sets",
                "1 training max",
                "Settings",
                "Which program is active",
            ]
        );
        assert!(
            companion_note(&preview, true)
                .unwrap()
                .contains("(imported)")
        );
        assert_eq!(companion_note(&preview, false), None);
        let no_programs = ImportPreview {
            programs: 0,
            versions: 0,
            ..preview
        };
        assert_eq!(companion_note(&no_programs, true), None);
    }

    #[test]
    fn the_summary_says_what_was_added() {
        assert_eq!(
            summary_text(&ImportSummary::default()),
            "Nothing new: your account already had everything in this export."
        );
        assert_eq!(
            summary_text(&ImportSummary {
                sessions: 2,
                sets: 10,
                ..ImportSummary::default()
            }),
            "Imported: 2 workouts and 10 sets."
        );
        assert_eq!(
            summary_text(&ImportSummary {
                settings: true,
                training_maxes: 1,
                programs: 1,
                versions: 1,
                active_program: true,
                sessions: 1,
                sets: 1,
            }),
            "Imported: 1 program, 1 workout, 1 set, 1 training max, your settings and the active program."
        );
        // Versions added to existing programs (companions) are counted on their own.
        assert_eq!(
            summary_text(&ImportSummary {
                versions: 2,
                ..ImportSummary::default()
            }),
            "Imported: 2 program versions."
        );
    }

    fn server(code: u16, message: &str, details: Option<serde_json::Value>) -> ServerFnError {
        ServerFnError::ServerError {
            message: message.to_owned(),
            code,
            details,
        }
    }

    #[test]
    fn failures_use_the_servers_message_and_wait() {
        assert_eq!(
            failure_text(&server(
                503,
                "The server is busy. Please try again.",
                Some(json!({ "retry_after_secs": 30 }))
            )),
            "The server is busy. Please try again. Try again in 30 s."
        );
        assert_eq!(
            failure_text(&server(
                413,
                "The export file is too large (the limit is 8 MiB).",
                None
            )),
            "The export file is too large (the limit is 8 MiB)."
        );
        assert_eq!(
            failure_text(&server(422, "This is not an Iron Oxide export.", None)),
            "This is not an Iron Oxide export."
        );
        // A 500's text is never shown.
        assert_eq!(
            failure_text(&server(500, "db.rs:12", None)),
            crate::api::error::GENERIC_MESSAGE
        );
    }

    #[test]
    fn a_403_on_deletion_asks_for_a_fresh_sign_in() {
        assert!(needs_fresh_sign_in(&server(
            403,
            "To delete your account, sign in again first.",
            None
        )));
        assert!(!needs_fresh_sign_in(&server(503, "Busy.", None)));
    }

    #[test]
    fn deletion_needs_the_word_typed_exactly() {
        assert!(confirms_deletion("DELETE"));
        assert!(confirms_deletion(" DELETE "));
        assert!(!confirms_deletion("delete"));
        assert!(!confirms_deletion("DELET"));
        assert!(!confirms_deletion(""));
    }

    #[test]
    fn picked_files_must_be_utf8() {
        assert_eq!(decode_file(b"\xef\xbb\xbf{}"), Ok("{}".to_owned()));
        assert!(decode_file(b"{\xff}").unwrap_err().contains("UTF-8"));
    }
}
