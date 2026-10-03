//! The Programs screen (#35): the built-in programs and the user's own, the active one, a
//! program's details and versions, copying a built-in, activating, archiving, and uploading a
//! `program.json`.
//!
//! Everything shown from a program document, and every problem the server reports about an
//! uploaded one, is rendered as text nodes: those texts are the user's own and may contain markup.
//!
//! Programs can also be written by the user's own AI assistant ([`ai`], #108): the same upload,
//! with the document pasted from the assistant's answer.
//!
//! Copies and new-program uploads carry a client `creation_id` (UUIDv7). It is kept until the
//! request succeeds, so retrying after a lost answer returns the program the first attempt
//! created instead of a second one (which would also take a second slot of the plan's quota).

mod ai;

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use iron_oxide_domain::entitlements::{Entitlements, Feature, Limit, Quota};
use iron_oxide_domain::program::{
    BuiltinProgramId, Load, PROGRAM_SCHEMA_URL, Program, ProgressionRule, RepTarget, UnitWeight,
    Work, limits::MAX_DOCUMENT_BYTES,
};
use iron_oxide_domain::time::Timestamp;
use iron_oxide_domain::{CreationId, ProgramId, Unit};

use self::ai::{AiFlow, AiState};
use super::components::{Button, ButtonVariant, Card, Chip, EmptyState, LoadingState};
use super::errors::{BannerKind, Errors, use_errors};
use super::shell::Route;
use super::weight::{use_unit, weight_text};
use crate::api::billing::my_entitlements;
use crate::api::error::{ApiFailure, FailureKind};
use crate::api::programs::{
    BuiltinProgramView, ProgramDetail, ProgramProblem, ProgramProblems, ProgramView, UploadTarget,
    VersionView, copy_builtin_program, get_active_program, get_program, list_builtin_programs,
    list_program_versions, list_programs, set_active_program, set_program_archived, upload_program,
};

// --- View models ---------------------------------------------------------------------------------

/// `3 × 5`, `3 × 8–12`, `3 × 45 s hold`, `8 rounds: 30 s on, 90 s off`.
#[must_use]
pub fn work_text(work: Work) -> String {
    match work {
        Work::Reps { sets, reps } => match reps {
            RepTarget::Fixed(reps) => format!("{sets} × {reps}"),
            RepTarget::Range(range) => format!("{sets} × {}–{}", range.min, range.max),
        },
        Work::Hold { sets, seconds } => format!("{sets} × {} s hold", seconds.get()),
        Work::Intervals { work, rest, rounds } => {
            format!("{rounds} rounds: {} s on, {} s off", work.get(), rest.get())
        }
    }
}

/// A program weight in the user's unit, with the value as written when the program uses the other
/// unit: `60 kg`, `132.28 lb (60 kg)`.
#[must_use]
pub fn program_weight_text(weight: UnitWeight, unit: Unit) -> String {
    let shown = weight_text(weight.weight(), unit);
    if weight.unit() == unit {
        shown
    } else {
        format!("{shown} ({weight})")
    }
}

/// `60 kg` (in the user's unit), `75% of training max`, or nothing.
#[must_use]
pub fn load_text(load: Option<Load>, unit: Unit) -> Option<String> {
    match load? {
        Load::Weight(weight) => Some(program_weight_text(weight, unit)),
        Load::PercentOfTrainingMax(percent) => Some(format!("{percent} of training max")),
    }
}

/// How an exercise progresses, in a few words.
#[must_use]
pub fn progression_text(rule: &ProgressionRule, unit: Unit) -> String {
    let main = match rule {
        ProgressionRule::None => return "No automatic progression".to_owned(),
        ProgressionRule::AddWhenTopOfRange { increment, .. } => format!(
            "+{} when every set hits its reps",
            program_weight_text(*increment, unit)
        ),
        ProgressionRule::DoubleProgression { increment, .. } => format!(
            "Double progression: add reps, then +{}",
            program_weight_text(*increment, unit)
        ),
        ProgressionRule::TrainingMax { increment, .. } => format!(
            "Training max +{} when every set hits its reps",
            program_weight_text(*increment, unit)
        ),
    };
    match rule.deload() {
        Some(deload) => format!(
            "{main}; −{} after {} failed sessions",
            deload.percent, deload.failures
        ),
        None => main,
    }
}

/// `2026-10-03`: the UTC date of a timestamp.
#[must_use]
pub fn date_text(at: Timestamp) -> String {
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let days = at.epoch_millis().div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Where a problem is: its JSON path, its line and column, or the document itself.
#[must_use]
pub fn problem_place(problem: &ProgramProblem) -> String {
    let position = match (problem.line, problem.column) {
        (Some(line), Some(column)) => Some(format!("line {line}, column {column}")),
        (Some(line), None) => Some(format!("line {line}")),
        _ => None,
    };
    match (problem.path.is_empty(), position) {
        (false, Some(position)) => format!("{} ({position})", problem.path),
        (false, None) => problem.path.clone(),
        (true, Some(position)) => position,
        (true, None) => "Document".to_owned(),
    }
}

/// `256 KiB`.
#[must_use]
pub fn size_text(bytes: usize) -> String {
    format!("{} KiB", bytes / 1024)
}

/// The message for a file over [`MAX_DOCUMENT_BYTES`].
#[must_use]
pub fn too_large_message() -> String {
    format!(
        "This file is too large: a program.json can be at most {}.",
        size_text(MAX_DOCUMENT_BYTES)
    )
}

/// The text of a picked file: UTF-8 (as JSON must be), and at most [`MAX_DOCUMENT_BYTES`] once
/// decoded, the size the server checks.
///
/// # Errors
/// A message for the user.
pub fn decode_document(bytes: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        "This file is not UTF-8 text. Save the program.json as UTF-8 and try again.".to_owned()
    })?;
    // A byte-order mark is not part of the JSON.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(too_large_message());
    }
    Ok(text.to_owned())
}

/// Why an action failed, as the screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionFailure {
    /// The document breaks rules: listed one by one.
    Problems(ProgramProblems),
    /// A plan limit (403): its message, with a link to the plan.
    Plan(String),
    /// Anything else: the shared banner.
    Other(String),
}

/// Classifies a failed copy, upload, activation or archive.
#[must_use]
pub fn action_failure(error: &ServerFnError) -> ActionFailure {
    if let Some(problems) = ProgramProblems::from_error(error) {
        return ActionFailure::Problems(problems);
    }
    let failure = ApiFailure::classify(error);
    let code = match error {
        ServerFnError::ServerError { code, .. }
        | ServerFnError::Request(dioxus::fullstack::RequestError::Status(_, code)) => Some(*code),
        _ => None,
    };
    match (code, failure.kind) {
        // A 413 may come from the transport without our message.
        (Some(413), _) => ActionFailure::Other(too_large_message()),
        (_, FailureKind::Forbidden) => ActionFailure::Plan(failure.message),
        _ => ActionFailure::Other(failure.message),
    }
}

/// One create the user asked for (a copy of a built-in, a new-program upload) and the
/// `creation_id` it is sent with, kept until it succeeds or is refused for good. Retrying it, or
/// tapping again after leaving and coming back, sends the same id: the server then returns what
/// the first request created instead of creating a second one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent<K> {
    pub key: K,
    pub creation_id: CreationId,
    /// Whether its request is in flight.
    pub in_flight: bool,
}

/// How a create's request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// Created, or refused for good: the next create is a new one.
    Done,
    /// May succeed if sent again (network, 503, 429): keep its id.
    Retryable,
}

/// The creates the user asked for, by key. App-level, so they outlive the Programs screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intents<K> {
    intents: Vec<Intent<K>>,
}

impl<K> Default for Intents<K> {
    fn default() -> Self {
        Self {
            intents: Vec::new(),
        }
    }
}

impl<K: PartialEq + Clone> Intents<K> {
    /// Starts a create for `key`: the id to send, the pending one if there is one; `None` while
    /// a request for it is already in flight.
    pub fn begin(&mut self, key: &K) -> Option<CreationId> {
        match self.intents.iter_mut().find(|intent| intent.key == *key) {
            Some(intent) if intent.in_flight => None,
            Some(intent) => {
                intent.in_flight = true;
                Some(intent.creation_id)
            }
            None => {
                let creation_id = CreationId::new_v7();
                self.intents.push(Intent {
                    key: key.clone(),
                    creation_id,
                    in_flight: true,
                });
                Some(creation_id)
            }
        }
    }

    /// Records how the request for `key` ended.
    pub fn end(&mut self, key: &K, ended: Ended) {
        match ended {
            Ended::Done => self.intents.retain(|intent| intent.key != *key),
            Ended::Retryable => {
                if let Some(intent) = self.intents.iter_mut().find(|intent| intent.key == *key) {
                    intent.in_flight = false;
                }
            }
        }
    }

    /// Whether a request for `key` is in flight.
    #[must_use]
    pub fn in_flight(&self, key: &K) -> bool {
        self.intents
            .iter()
            .any(|intent| intent.key == *key && intent.in_flight)
    }

    /// Whether any request is in flight.
    #[must_use]
    pub fn any_in_flight(&self) -> bool {
        self.intents.iter().any(|intent| intent.in_flight)
    }
}

/// How a create's failure ends it.
fn ended(error: &ServerFnError) -> Ended {
    if ApiFailure::classify(error).is_retryable() {
        Ended::Retryable
    } else {
        Ended::Done
    }
}

/// The creates in progress, provided by the app root so they outlive the Programs screen.
#[derive(Clone, Copy, PartialEq)]
pub struct ProgramIntents {
    copies: Signal<Intents<BuiltinProgramId>>,
    /// New-program uploads, keyed by the document.
    uploads: Signal<Intents<String>>,
}

/// Provides the creates in progress. Called once, by the app root.
pub fn use_program_intents_provider() {
    let intents = ProgramIntents {
        copies: use_signal(Intents::default),
        uploads: use_signal(Intents::default),
    };
    use_context_provider(|| intents);
}

/// What the upload card says about the plan: whether uploading is included, and the slots left.
#[must_use]
pub fn upload_allowance(entitlements: &Entitlements, unarchived: usize) -> (bool, Option<String>) {
    let allowed = entitlements
        .features
        .iter()
        .any(|access| access.feature == Feature::UploadPrograms && access.allowed);
    let slots = entitlements
        .limits
        .iter()
        .find(|limit| limit.quota == Quota::CustomPrograms)
        .and_then(|limit| match limit.limit {
            Limit::AtMost { max } => Some(format!(
                "{unarchived} of {max} active programs used (archived ones don't count)."
            )),
            Limit::Unlimited => None,
        });
    (allowed, slots)
}

/// The user's programs, unarchived ones first, each group oldest first (the server's order).
#[must_use]
pub fn split_programs(programs: &[ProgramView]) -> (Vec<ProgramView>, Vec<ProgramView>) {
    programs
        .iter()
        .cloned()
        .partition(|program| !program.archived)
}

// --- State ---------------------------------------------------------------------------------------

/// What the screen shows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Screen {
    List,
    Builtin(BuiltinProgramId),
    Mine(ProgramId),
}

/// Sets a signal that may belong to a component that is gone (the user navigated away while an
/// action ran); `false` if it is gone.
fn try_set<T: 'static>(mut signal: Signal<T>, value: T) -> bool {
    match signal.try_write() {
        Ok(mut slot) => {
            *slot = value;
            true
        }
        Err(_) => false,
    }
}

/// Marks an action as running for as long as it lives: dropping it, however the action ends,
/// clears the component's busy flag (if the component is still there).
struct BusyGuard(Signal<bool>);

impl BusyGuard {
    /// Starts an action, unless one is already running in this component.
    fn start(busy: Signal<bool>) -> Option<Self> {
        if busy.try_peek().is_ok_and(|busy| *busy) {
            return None;
        }
        try_set(busy, true);
        Some(Self(busy))
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        try_set(self.0, false);
    }
}

/// The screen's shared state. `Copy`, so every handler can take one.
///
/// Actions run with `spawn_forever`, so they finish even if the user leaves the screen: their
/// outcome then goes to the app-wide banner instead of the screen.
#[derive(Clone, Copy, PartialEq)]
struct Programs {
    screen: Signal<Screen>,
    /// Bumped after every change, so the lists and details reload.
    generation: Signal<u64>,
    /// A plan limit (403) to show with a link to the plan, on the screen of the action it refused.
    plan_notice: Signal<Option<(Screen, String)>>,
    /// The problems of the last refused upload, for the program it was for (`None`: a new one).
    problems: Signal<Option<(Option<ProgramId>, ProgramProblems)>>,
    intents: ProgramIntents,
    /// Create with your AI: the pasted answer, its check and what was saved.
    ai: AiState,
    errors: Errors,
}

impl Programs {
    fn open(self, screen: Screen) {
        self.clear_notices();
        self.ai.reset();
        if try_set(self.screen, screen) {
            scroll_to_top();
        }
    }

    /// Opens `screen` if the user is still on `from` (they may have moved on meanwhile).
    fn open_from(self, from: &Screen, screen: Screen) {
        if self
            .screen
            .try_peek()
            .is_ok_and(|current| *current == *from)
        {
            self.open(screen);
        }
    }

    fn changed(self) {
        if let Ok(generation) = self.generation.try_peek().map(|generation| *generation) {
            try_set(self.generation, generation + 1);
        }
    }

    fn clear_notices(self) {
        try_set(self.plan_notice, None);
        try_set(self.problems, None);
    }

    /// Whether the user is on `screen`.
    fn is_on(self, screen: &Screen) -> bool {
        self.screen
            .try_peek()
            .is_ok_and(|current| *current == *screen)
    }

    /// Shows why an action started on `origin` failed: on that screen if the user is still there,
    /// else in the banner.
    fn fail(self, error: &ServerFnError, origin: &Screen) {
        let here = self.is_on(origin);
        match action_failure(error) {
            ActionFailure::Problems(problems) => {
                let count = problems.errors.len() + problems.omitted;
                let target = match origin {
                    Screen::Mine(id) => Some(*id),
                    Screen::List | Screen::Builtin(_) => None,
                };
                if !(here && try_set(self.problems, Some((target, problems)))) {
                    self.errors.show(
                        BannerKind::Error,
                        format!(
                            "The program was not uploaded: it has {count} problem(s). Upload it \
                             again to see them."
                        ),
                    );
                }
            }
            ActionFailure::Plan(message) => {
                if !(here && try_set(self.plan_notice, Some((origin.clone(), message.clone())))) {
                    self.errors.show(BannerKind::Error, message);
                }
            }
            ActionFailure::Other(message) if error_is_413(error) => {
                self.errors.show(BannerKind::Error, message);
            }
            ActionFailure::Other(_) => self.errors.report(error),
        }
    }

    /// Starts a copy of `builtin`: its creation id, or `None` while one is in flight.
    fn begin_copy(self, builtin: &BuiltinProgramId) -> Option<CreationId> {
        let mut copies = self.intents.copies;
        copies.try_write().ok()?.begin(builtin)
    }

    fn end_copy(self, builtin: &BuiltinProgramId, ended: Ended) {
        let mut copies = self.intents.copies;
        if let Ok(mut copies) = copies.try_write() {
            copies.end(builtin, ended);
        }
    }

    fn begin_upload(self, document: &String) -> Option<CreationId> {
        let mut uploads = self.intents.uploads;
        uploads.try_write().ok()?.begin(document)
    }

    fn end_upload(self, document: &String, ended: Ended) {
        let mut uploads = self.intents.uploads;
        if let Ok(mut uploads) = uploads.try_write() {
            uploads.end(document, ended);
        }
    }
}

fn error_is_413(error: &ServerFnError) -> bool {
    matches!(
        error,
        ServerFnError::ServerError { code: 413, .. }
            | ServerFnError::Request(dioxus::fullstack::RequestError::Status(_, 413))
    )
}

fn scroll_to_top() {
    #[cfg(feature = "web")]
    if let Some(window) = web_sys::window() {
        window.scroll_to_with_x_and_y(0.0, 0.0);
    }
}

// --- Screen --------------------------------------------------------------------------------------

/// The Programs page.
#[component]
pub fn ProgramsPage() -> Element {
    let state = Programs {
        screen: use_signal(|| Screen::List),
        generation: use_signal(|| 0),
        plan_notice: use_signal(|| None),
        problems: use_signal(|| None),
        intents: use_context::<ProgramIntents>(),
        ai: use_hook(AiState::new),
        errors: use_errors(),
    };
    let screen = state.screen.read().clone();
    match screen {
        Screen::List => rsx! { ProgramList { state } },
        Screen::Builtin(id) => rsx! { BuiltinDetail { state, id } },
        Screen::Mine(id) => rsx! { MineDetail { state, id } },
    }
}

/// A 403 from a plan limit, with a link to the plan.
#[component]
fn PlanNotice(state: Programs) -> Element {
    let current = state.screen.read().clone();
    let Some(message) = state
        .plan_notice
        .read()
        .clone()
        .filter(|(screen, _)| *screen == current)
        .map(|(_, message)| message)
    else {
        return rsx! {};
    };
    rsx! {
        div { class: "io-notice io-notice-error io-plan-notice", role: "alert",
            p { "{message}" }
            Link { to: Route::Settings {}, "See your plan in Settings" }
        }
    }
}

/// The problems of a refused upload, one per line, as plain text.
#[component]
fn ProblemList(state: Programs, target: Option<ProgramId>) -> Element {
    let Some(problems) = state
        .problems
        .read()
        .clone()
        .filter(|(key, _)| *key == target)
        .map(|(_, problems)| problems)
    else {
        return rsx! {};
    };
    rsx! {
        div { class: "io-notice io-notice-error io-problems", role: "alert",
            p { "This program is not valid:" }
            ul {
                for (index, problem) in problems.errors.iter().enumerate() {
                    li { key: "{index}",
                        span { class: "io-problem-place", "{problem_place(problem)}" }
                        " "
                        span { "{problem.message}" }
                    }
                }
            }
            if problems.omitted > 0 {
                p { "…and {problems.omitted} more." }
            }
        }
    }
}

#[component]
fn ProgramList(state: Programs) -> Element {
    let errors = use_errors();
    let mut show_archived = use_signal(|| false);
    let data = use_resource(move || async move {
        let _ = state.generation.read();
        let loaded = async {
            let mine = list_programs(true).await?;
            let active = get_active_program().await?;
            let builtins = list_builtin_programs().await?;
            Ok::<_, ServerFnError>((mine, active, builtins))
        }
        .await;
        if let Err(error) = &loaded {
            errors.report(error);
        }
        loaded.ok()
    });
    let entitlements = use_entitlements(state);

    let header = rsx! {
        div { class: "io-page-header",
            h1 { class: "io-title", "Programs" }
        }
    };
    let loaded = data.read().clone();
    let Some(loaded) = loaded else {
        return rsx! { {header} LoadingState { message: "Loading your programs…" } };
    };
    let Some((mine, active, builtins)) = loaded else {
        let mut data = data;
        return rsx! {
            {header}
            EmptyState { title: "Not loaded", message: "Your programs could not be loaded.",
                Button { variant: ButtonVariant::Secondary, onclick: move |_| data.restart(), "Try again" }
            }
        };
    };
    let active_id = active.as_ref().map(|detail| detail.program.id);
    let (current, archived) = split_programs(&mine);
    let allowance = entitlements
        .read()
        .clone()
        .flatten()
        .map(|entitlements| upload_allowance(&entitlements, current.len()));

    rsx! {
        {header}
        PlanNotice { state }
        match &active {
            Some(detail) => rsx! {
                section { class: "io-card io-active", aria_labelledby: "active-title",
                    span { class: "io-label", "Training with" }
                    h2 { id: "active-title", "{detail.program.name}" }
                    p { class: "io-muted",
                        "{detail.document.days.len()} days · version {detail.version.version}"
                    }
                    Button {
                        variant: ButtonVariant::Secondary,
                        onclick: {
                            let id = detail.program.id;
                            move |_| state.open(Screen::Mine(id))
                        },
                        "Details"
                    }
                }
            },
            None => rsx! {
                Card { title: "No active program",
                    p { class: "io-muted",
                        "Create one with your AI, copy a built-in program below, or upload your \
                         own, then make it active."
                    }
                }
            },
        }

        AiFlow {
            state,
            allowed: allowance.as_ref().map(|(allowed, _)| *allowed),
            target: None,
            current: None,
            active: false,
        }

        Card { title: "Your programs",
            if current.is_empty() {
                p { class: "io-muted", "None yet." }
            }
            ul { class: "io-list",
                for program in current {
                    ProgramRow { key: "{program.id}", state, program: program.clone(), active: Some(program.id) == active_id }
                }
            }
            if !archived.is_empty() {
                Chip {
                    selected: *show_archived.read(),
                    onclick: move |_| {
                        let shown = *show_archived.peek();
                        show_archived.set(!shown);
                    },
                    "Archived ({archived.len()})"
                }
                if *show_archived.read() {
                    ul { class: "io-list",
                        for program in archived {
                            ProgramRow { key: "{program.id}", state, program: program.clone(), active: false }
                        }
                    }
                }
            }
        }

        UploadCard { state, allowance, target: None }

        Card { title: "Built-in programs",
            ul { class: "io-list",
                for builtin in builtins {
                    BuiltinRow { key: "{builtin.builtin_id}", state, builtin: builtin.clone() }
                }
            }
        }
    }
}

#[component]
fn ProgramRow(state: Programs, program: ProgramView, active: bool) -> Element {
    let id = program.id;
    let origin = if program.source_builtin_id.is_some() {
        "Copied from a built-in"
    } else {
        "Uploaded"
    };
    rsx! {
        li { class: "io-row",
            div { class: "io-row-main",
                span { class: "io-row-title",
                    "{program.name}"
                    if active {
                        span { class: "io-badge", "active" }
                    }
                    if program.archived {
                        span { class: "io-badge io-badge-muted", "archived" }
                    }
                }
                span { class: "io-muted io-row-meta", "{origin} · {date_text(program.created_at)}" }
            }
            Button {
                variant: ButtonVariant::Secondary,
                onclick: move |_| state.open(Screen::Mine(id)),
                "Open"
            }
        }
    }
}

#[component]
fn BuiltinRow(state: Programs, builtin: BuiltinProgramView) -> Element {
    let id = builtin.builtin_id.clone();
    rsx! {
        li { class: "io-row",
            div { class: "io-row-main",
                span { class: "io-row-title", "{builtin.name}" }
                span { class: "io-muted io-row-meta",
                    "{builtin.document.days.len()} days · version {builtin.version}"
                }
            }
            Button {
                variant: ButtonVariant::Secondary,
                onclick: move |_| state.open(Screen::Builtin(id.clone())),
                "Open"
            }
        }
    }
}

/// Uploading a `program.json`: as a new program (`target: None`) or as a new version of one.
#[component]
fn UploadCard(
    state: Programs,
    allowance: Option<(bool, Option<String>)>,
    target: Option<ProgramId>,
) -> Element {
    // Re-keys the file input after each pick, so picking the same file again works.
    let mut picks = use_signal(|| 0_u32);
    let allowed = allowance.as_ref().is_none_or(|(allowed, _)| *allowed);
    let slots = allowance.and_then(|(_, slots)| slots);
    let busy_flag = use_signal(|| false);
    // A new-program upload stays in flight after leaving the screen: it is app-level.
    let busy =
        *busy_flag.read() || (target.is_none() && state.intents.uploads.read().any_in_flight());
    let input_id = if target.is_some() {
        "upload-version"
    } else {
        "upload-program"
    };

    let on_pick = move |event: FormEvent| {
        let Some(file) = event.files().into_iter().next() else {
            return;
        };
        let next = *picks.peek() + 1;
        picks.set(next);
        // An early guard on the raw size; the decoded text is checked again below.
        if usize::try_from(file.size()).map_or(true, |size| size > MAX_DOCUMENT_BYTES) {
            state.errors.show(BannerKind::Error, too_large_message());
            return;
        }
        let Some(guard) = BusyGuard::start(busy_flag) else {
            return;
        };
        state.clear_notices();
        spawn_forever(async move {
            let _guard = guard;
            let document = match file.read_bytes().await {
                Ok(bytes) => decode_document(&bytes),
                Err(_) => Err("This file could not be read.".to_owned()),
            };
            match document {
                Ok(document) => upload(state, target, document).await,
                Err(message) => {
                    state.errors.show(BannerKind::Error, message);
                }
            }
        });
    };

    let (title, intro) = match target {
        None => (
            "Upload a program",
            "Write your own program as a program.json file and upload it.",
        ),
        Some(_) => (
            "Upload a new version",
            "Upload a changed program.json: it becomes this program's next version.",
        ),
    };
    rsx! {
        Card { title,
            p { class: "io-muted", "{intro}" }
            p { class: "io-muted io-hint",
                "The format: "
                a { href: PROGRAM_SCHEMA_URL, target: "_blank", rel: "noopener noreferrer", "program.schema.json" }
                " (at most {size_text(MAX_DOCUMENT_BYTES)})."
            }
            if allowed {
                FilePicker {
                    key: "{picks}",
                    id: input_id,
                    busy,
                    on_pick,
                }
            } else {
                p { class: "io-notice io-notice-info",
                    "Uploading programs is part of Iron Oxide Pro. "
                    Link { to: Route::Settings {}, "See your plan" }
                }
            }
            if let Some(slots) = slots {
                if target.is_none() {
                    p { class: "io-muted io-hint", "{slots}" }
                }
            }
            ProblemList { state, target }
        }
    }
}

/// The button that opens the file picker: a label for a visually hidden file input, so it looks
/// like every other button. Re-created (by its key) after each pick.
#[component]
fn FilePicker(id: &'static str, busy: bool, on_pick: EventHandler<FormEvent>) -> Element {
    let class = if busy {
        "io-button io-button-primary io-file-button io-file-button-busy"
    } else {
        "io-button io-button-primary io-file-button"
    };
    rsx! {
        label { class, r#for: id, "aria-busy": busy,
            if busy { "Uploading…" } else { "Choose a program.json" }
        }
        input {
            id,
            class: "io-sr-only",
            r#type: "file",
            accept: ".json,application/json",
            disabled: busy,
            onchange: move |event| on_pick.call(event),
        }
    }
}

async fn upload(state: Programs, target: Option<ProgramId>, document: String) {
    let origin = target.map_or(Screen::List, Screen::Mine);
    let upload_target = match target {
        Some(program_id) => UploadTarget::NewVersion { program_id },
        None => {
            let Some(creation_id) = state.begin_upload(&document) else {
                // The same document is already being uploaded.
                return;
            };
            UploadTarget::NewProgram { creation_id }
        }
    };
    let result = upload_program(upload_target, document.clone()).await;
    if target.is_none() {
        let how = result.as_ref().map_or_else(ended, |_| Ended::Done);
        state.end_upload(&document, how);
    }
    match result {
        Ok(outcome) => {
            let message = match (outcome.saved, target.is_some()) {
                (false, true) => "No change: this is the same as the latest version.".to_owned(),
                (false, false) => format!(
                    "\u{201c}{}\u{201d} is already uploaded.",
                    outcome.program.name
                ),
                (true, true) => format!("Version {} uploaded.", outcome.version.version),
                (true, false) => format!("\u{201c}{}\u{201d} uploaded.", outcome.program.name),
            };
            state.errors.show(BannerKind::Info, message);
            state.changed();
            if target.is_none() {
                state.open_from(&Screen::List, Screen::Mine(outcome.program.id));
            }
        }
        Err(error) => state.fail(&error, &origin),
    }
}

/// The user's entitlements, reloaded after every change; a failure is reported.
fn use_entitlements(state: Programs) -> Resource<Option<Entitlements>> {
    let errors = use_errors();
    use_resource(move || async move {
        let _ = state.generation.read();
        let result = my_entitlements().await;
        if let Err(error) = &result {
            errors.report(error);
        }
        result.ok()
    })
}

/// The days of a program: each day's exercises, their work, load and progression.
#[component]
fn ProgramDays(document: Program) -> Element {
    let unit = use_unit();
    let rotation: Vec<String> = document
        .rotation
        .iter()
        .filter_map(|id| document.days.iter().find(|day| &day.id == id))
        .map(|day| day.name.clone())
        .collect();
    rsx! {
        if let Some(description) = &document.description {
            p { class: "io-program-description", "{description}" }
        }
        p { class: "io-muted io-hint", "Rotation: {rotation.join(\" → \")}" }
        for day in document.days.iter() {
            section { key: "{day.id}", class: "io-card io-day",
                h2 { "{day.name}" }
                ul { class: "io-list",
                    for (index, exercise) in day.exercises.iter().enumerate() {
                        li { key: "{index}", class: "io-row io-exercise",
                            div { class: "io-row-main",
                                span { class: "io-row-title", "{exercise.name}" }
                                span { class: "io-exercise-work",
                                    "{work_text(exercise.work)}"
                                    if let Some(load) = load_text(exercise.load, unit) {
                                        " · {load}"
                                    }
                                    " · rest {exercise.rest.get()} s"
                                }
                                span { class: "io-muted io-row-meta", "{progression_text(&exercise.progression, unit)}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A back button to the list.
#[component]
fn BackButton(state: Programs) -> Element {
    rsx! {
        div { class: "io-back",
            Button { variant: ButtonVariant::Ghost, onclick: move |_| state.open(Screen::List), "← Programs" }
        }
    }
}

#[component]
fn BuiltinDetail(state: Programs, id: BuiltinProgramId) -> Element {
    let errors = use_errors();
    let builtins = use_resource(move || async move {
        let result = list_builtin_programs().await;
        if let Err(error) = &result {
            errors.report(error);
        }
        result.ok()
    });
    let found = builtins
        .read()
        .clone()
        .map(|list| list.and_then(|list| list.into_iter().find(|b| b.builtin_id == id)));
    let busy = state.intents.copies.read().in_flight(&id);
    let builtin = match found {
        None => return rsx! { BackButton { state } LoadingState {} },
        Some(None) => {
            return rsx! {
                BackButton { state }
                EmptyState { title: "Not found", message: "This built-in program could not be loaded." }
            };
        }
        Some(Some(builtin)) => builtin,
    };
    let copy_id = builtin.builtin_id.clone();
    let copy = move |_| {
        let builtin_id = copy_id.clone();
        let Some(creation_id) = state.begin_copy(&builtin_id) else {
            // Already copying it.
            return;
        };
        state.clear_notices();
        spawn_forever(async move {
            let origin = Screen::Builtin(builtin_id.clone());
            let result = copy_builtin_program(builtin_id.as_str().to_owned(), creation_id).await;
            state.end_copy(
                &builtin_id,
                result.as_ref().map_or_else(ended, |_| Ended::Done),
            );
            match result {
                Ok(detail) => {
                    state.errors.show(
                        BannerKind::Info,
                        format!(
                            "\u{201c}{}\u{201d} is now one of your programs.",
                            detail.program.name
                        ),
                    );
                    state.changed();
                    state.open_from(&origin, Screen::Mine(detail.program.id));
                }
                Err(error) => state.fail(&error, &origin),
            }
        });
    };
    rsx! {
        BackButton { state }
        div { class: "io-page-header",
            span { class: "io-label", "Built-in · version {builtin.version}" }
            h1 { class: "io-title", "{builtin.name}" }
        }
        PlanNotice { state }
        div { class: "io-actions",
            Button { busy, block: true, onclick: copy, if busy { "Copying…" } else { "Copy to my programs" } }
            p { class: "io-muted io-hint",
                "You train with your own copy, which you can make active and change."
            }
        }
        ProgramDays { document: builtin.document.clone() }
    }
}

/// What the details of one of the user's programs need.
#[derive(Debug, Clone, PartialEq)]
struct MineData {
    detail: ProgramDetail,
    versions: Vec<VersionView>,
    active: bool,
}

#[component]
fn MineDetail(state: Programs, id: ProgramId) -> Element {
    let errors = use_errors();
    let data = use_resource(move || async move {
        let _ = state.generation.read();
        let loaded = async {
            let detail = get_program(id).await?;
            let versions = list_program_versions(id).await?;
            let active = get_active_program()
                .await?
                .is_some_and(|active| active.program.id == id);
            Ok::<_, ServerFnError>(MineData {
                detail,
                versions,
                active,
            })
        }
        .await;
        if let Err(error) = &loaded {
            errors.report(error);
        }
        loaded.ok()
    });
    let entitlements = use_entitlements(state);
    let busy_flag = use_signal(|| false);
    let busy = *busy_flag.read();
    let loaded = data.read().clone();
    let data_ = match loaded {
        None => return rsx! { BackButton { state } LoadingState {} },
        Some(None) => {
            return rsx! {
                BackButton { state }
                EmptyState { title: "Not found", message: "This program could not be loaded." }
            };
        }
        Some(Some(data_)) => data_,
    };
    let allowance = entitlements
        .read()
        .clone()
        .flatten()
        .map(|entitlements| upload_allowance(&entitlements, 0));
    let program = data_.detail.program.clone();
    let archived = program.archived;
    let active = data_.active;

    let activate = move |_| {
        let Some(guard) = BusyGuard::start(busy_flag) else {
            return;
        };
        state.clear_notices();
        spawn_forever(async move {
            let _guard = guard;
            match set_active_program(id).await {
                Ok(detail) => {
                    state.errors.show(
                        BannerKind::Info,
                        format!(
                            "You now train with \u{201c}{}\u{201d}.",
                            detail.program.name
                        ),
                    );
                    state.changed();
                }
                Err(error) => state.fail(&error, &Screen::Mine(id)),
            }
        });
    };
    let archive = move |_| {
        let Some(guard) = BusyGuard::start(busy_flag) else {
            return;
        };
        state.clear_notices();
        spawn_forever(async move {
            let _guard = guard;
            match set_program_archived(id, !archived).await {
                Ok(()) => {
                    let message = if archived {
                        "Program restored."
                    } else {
                        "Program archived. Its history is kept."
                    };
                    state.errors.show(BannerKind::Info, message);
                    state.changed();
                }
                Err(error) => state.fail(&error, &Screen::Mine(id)),
            }
        });
    };

    let label = if active {
        "Active"
    } else if archived {
        "Archived"
    } else {
        "Your program"
    };
    rsx! {
        BackButton { state }
        div { class: "io-page-header",
            span { class: "io-label", "{label} · version {data_.detail.version.version}" }
            h1 { class: "io-title", "{program.name}" }
        }
        PlanNotice { state }
        div { class: "io-actions",
            if !active {
                Button {
                    block: true,
                    busy,
                    disabled: archived,
                    onclick: activate,
                    "Make active"
                }
                if archived {
                    p { class: "io-muted io-hint", "Restore this program to make it active." }
                }
            }
            Button {
                variant: ButtonVariant::Ghost,
                block: true,
                disabled: busy || active,
                onclick: archive,
                if archived { "Restore" } else { "Archive" }
            }
            if active {
                p { class: "io-muted io-hint",
                    "This is the program you train with: make another one active to archive it."
                }
            }
        }
        ProgramDays { document: data_.detail.document.clone() }
        Card { title: "Versions",
            ul { class: "io-list",
                for version in data_.versions.iter().rev() {
                    li { key: "{version.id}", class: "io-row",
                        div { class: "io-row-main",
                            span { class: "io-row-title",
                                "Version {version.version}"
                                if version.id == data_.detail.version.id {
                                    span { class: "io-badge", "latest" }
                                }
                            }
                            span { class: "io-muted io-row-meta", "{date_text(version.created_at)}" }
                        }
                    }
                }
            }
        }
        AiFlow {
            state,
            allowed: allowance.as_ref().map(|(allowed, _)| *allowed),
            target: Some(id),
            current: data_.detail.document.to_json_pretty().ok(),
            active,
        }
        UploadCard { state, allowance, target: Some(id) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iron_oxide_domain::entitlements::Plan;
    use serde_json::json;

    fn program(json: &str) -> Program {
        Program::from_json(json).unwrap()
    }

    fn sample() -> Program {
        program(
            r#"{
              "schema_version": 1,
              "name": "Sample",
              "days": [
                { "id": "a", "name": "A", "exercises": [
                  { "id": "squat", "name": "Squat", "work": { "reps": { "sets": 3, "reps": 5 } },
                    "load": { "kg": 100 }, "rest": 180,
                    "progression": { "add_when_top_of_range": { "increment": { "kg": 2.5 },
                      "deload_after_failures": { "failures": 3, "percent": 10 } } } },
                  { "id": "curl", "name": "Curl", "work": { "reps": { "sets": 3, "reps": { "min": 8, "max": 12 } } },
                    "load": { "lb": 30 }, "rest": 60,
                    "progression": { "double_progression": { "increment": { "lb": 5 } } } },
                  { "id": "bench", "name": "Bench", "work": { "reps": { "sets": 3, "reps": 5 } },
                    "load": { "percent_of_training_max": 72.5 }, "rest": 120,
                    "progression": { "training_max": { "increment": { "kg": 2.5 } } } },
                  { "id": "plank", "name": "Plank", "work": { "hold": { "sets": 3, "seconds": 45 } }, "rest": 60 },
                  { "id": "sprint", "name": "Sprint", "work": { "intervals": { "work": 30, "rest": 90, "rounds": 8 } }, "rest": 120 }
                ] }
              ],
              "rotation": ["a"]
            }"#,
        )
    }

    #[test]
    fn work_reads_at_a_glance() {
        let document = sample();
        let texts: Vec<String> = document.days[0]
            .exercises
            .iter()
            .map(|exercise| work_text(exercise.work))
            .collect();
        assert_eq!(
            texts,
            [
                "3 × 5",
                "3 × 8–12",
                "3 × 5",
                "3 × 45 s hold",
                "8 rounds: 30 s on, 90 s off"
            ]
        );
    }

    #[test]
    fn loads_follow_the_users_unit() {
        let document = sample();
        let loads: Vec<Option<String>> = document.days[0]
            .exercises
            .iter()
            .map(|exercise| load_text(exercise.load, Unit::Kg))
            .collect();
        assert_eq!(
            loads,
            [
                Some("100 kg".to_owned()),
                Some("13.61 kg (30 lb)".to_owned()),
                Some("72.5% of training max".to_owned()),
                None,
                None
            ]
        );
        assert_eq!(
            load_text(document.days[0].exercises[0].load, Unit::Lb),
            Some("220.46 lb (100 kg)".to_owned())
        );
    }

    #[test]
    fn progressions_are_summarised() {
        let document = sample();
        let rules: Vec<String> = document.days[0]
            .exercises
            .iter()
            .map(|exercise| progression_text(&exercise.progression, Unit::Kg))
            .collect();
        assert_eq!(
            rules,
            [
                "+2.5 kg when every set hits its reps; −10% after 3 failed sessions",
                "Double progression: add reps, then +2.27 kg (5 lb)",
                "Training max +2.5 kg when every set hits its reps",
                "No automatic progression",
                "No automatic progression",
            ]
        );
    }

    #[test]
    fn dates_are_utc_calendar_days() {
        assert_eq!(date_text(Timestamp::from_epoch_millis(0)), "1970-01-01");
        // 2026-10-03T02:00:00Z.
        assert_eq!(
            date_text(Timestamp::from_epoch_millis(1_791_000_000_000)),
            "2026-10-03"
        );
        // 2024-02-29 (leap day), 23:59:59.999.
        assert_eq!(
            date_text(Timestamp::from_epoch_millis(1_709_251_199_999)),
            "2024-02-29"
        );
        assert_eq!(date_text(Timestamp::from_epoch_millis(-1)), "1969-12-31");
    }

    fn problem(path: &str, line: Option<usize>, column: Option<usize>) -> ProgramProblem {
        ProgramProblem {
            path: path.to_owned(),
            message: "m".to_owned(),
            line,
            column,
        }
    }

    #[test]
    fn problems_say_where() {
        assert_eq!(
            problem_place(&problem("days[1].exercises[2].reps", None, None)),
            "days[1].exercises[2].reps"
        );
        assert_eq!(
            problem_place(&problem("", Some(2), Some(5))),
            "line 2, column 5"
        );
        assert_eq!(
            problem_place(&problem("days[0]", Some(4), Some(1))),
            "days[0] (line 4, column 1)"
        );
        assert_eq!(problem_place(&problem("", None, None)), "Document");
    }

    fn server(code: u16, message: &str, details: Option<serde_json::Value>) -> ServerFnError {
        ServerFnError::ServerError {
            message: message.to_owned(),
            code,
            details,
        }
    }

    #[test]
    fn failures_are_classified_for_the_screen() {
        let problems = ProgramProblems {
            errors: vec![problem("name", None, None)],
            omitted: 0,
        };
        let details = serde_json::to_value(&problems).unwrap();
        assert_eq!(
            action_failure(&server(422, "This program is not valid.", Some(details))),
            ActionFailure::Problems(problems)
        );
        assert_eq!(
            action_failure(&server(
                403,
                "Your plan keeps up to 10 programs. Archive one, or upgrade to Pro.",
                None
            )),
            ActionFailure::Plan(
                "Your plan keeps up to 10 programs. Archive one, or upgrade to Pro.".to_owned()
            )
        );
        // A 413, with or without our body, says how large a file may be.
        for error in [
            server(413, "Too large.", None),
            ServerFnError::Request(dioxus::fullstack::RequestError::Status(
                "Payload Too Large".to_owned(),
                413,
            )),
        ] {
            assert_eq!(
                action_failure(&error),
                ActionFailure::Other(too_large_message())
            );
            assert!(error_is_413(&error));
        }
        assert_eq!(
            action_failure(&server(409, "This program is active.", None)),
            ActionFailure::Other("This program is active.".to_owned())
        );
        // A 422 without problems is a plain message.
        assert_eq!(
            action_failure(&server(422, "Bad.", Some(json!("x")))),
            ActionFailure::Other("Bad.".to_owned())
        );
    }

    #[test]
    fn a_create_keeps_its_id_until_it_is_done() {
        let mut copies = Intents::<String>::default();
        let first = copies.begin(&"a".to_owned()).unwrap();
        assert!(copies.in_flight(&"a".to_owned()));
        assert!(copies.any_in_flight());
        // Tapping again (after leaving and coming back) while in flight sends nothing.
        assert_eq!(copies.begin(&"a".to_owned()), None);
        // Another built-in is its own create.
        let other = copies.begin(&"b".to_owned()).unwrap();
        assert_ne!(other, first);
        // A lost answer: the retry reuses the id.
        copies.end(&"a".to_owned(), Ended::Retryable);
        assert!(!copies.in_flight(&"a".to_owned()));
        assert_eq!(copies.begin(&"a".to_owned()), Some(first));
        // Done: a deliberate second copy is a new create.
        copies.end(&"a".to_owned(), Ended::Done);
        let second = copies.begin(&"a".to_owned()).unwrap();
        assert_ne!(second, first);
    }

    #[test]
    fn failures_end_creates_unless_retryable() {
        assert_eq!(ended(&server(422, "Bad.", None)), Ended::Done);
        assert_eq!(ended(&server(403, "Plan.", None)), Ended::Done);
        assert_eq!(ended(&server(503, "Busy.", None)), Ended::Retryable);
        assert_eq!(
            ended(&ServerFnError::Request(
                dioxus::fullstack::RequestError::Connect("x".into())
            )),
            Ended::Retryable
        );
    }

    #[test]
    fn the_upload_card_follows_the_plan() {
        let (allowed, slots) = upload_allowance(&Entitlements::of(Plan::Free), 3);
        assert!(allowed);
        assert_eq!(
            slots.as_deref(),
            Some("3 of 10 active programs used (archived ones don't count).")
        );
        let (allowed, slots) = upload_allowance(&Entitlements::of(Plan::Pro), 30);
        assert!(allowed);
        assert_eq!(slots, None);
        // A plan without the feature locks the upload.
        let mut locked = Entitlements::of(Plan::Free);
        for access in &mut locked.features {
            access.allowed = false;
        }
        assert!(!upload_allowance(&locked, 0).0);
    }

    #[test]
    fn archived_programs_are_listed_apart() {
        let view = |n: u128, archived: bool| ProgramView {
            id: ProgramId::from_uuid(uuid::Uuid::from_u128(n)),
            name: format!("P{n}"),
            source_builtin_id: None,
            archived,
            created_at: Timestamp::from_epoch_millis(0),
        };
        let (current, archived) = split_programs(&[view(1, false), view(2, true), view(3, false)]);
        let names = |list: &[ProgramView]| list.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&current), ["P1", "P3"]);
        assert_eq!(names(&archived), ["P2"]);
    }

    #[test]
    fn picked_files_must_be_utf8_and_fit_once_decoded() {
        assert_eq!(decode_document(b"{}"), Ok("{}".to_owned()));
        assert_eq!(decode_document(b"\xef\xbb\xbf{}"), Ok("{}".to_owned()));
        assert!(
            decode_document(b"{\"name\": \"\xff\"}")
                .unwrap_err()
                .contains("UTF-8")
        );
        let exactly = "a".repeat(MAX_DOCUMENT_BYTES);
        assert_eq!(decode_document(exactly.as_bytes()), Ok(exactly.clone()));
        let over = format!("{exactly}a");
        assert_eq!(decode_document(over.as_bytes()), Err(too_large_message()));
    }

    #[test]
    fn the_size_limit_is_said_in_kib() {
        assert_eq!(size_text(MAX_DOCUMENT_BYTES), "256 KiB");
        assert!(too_large_message().contains("256 KiB"));
    }
}
