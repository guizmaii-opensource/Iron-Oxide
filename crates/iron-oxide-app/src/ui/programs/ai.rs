//! Create with your AI (#108): the user's own AI assistant (ChatGPT, Claude, Gemini…) writes the
//! program, Iron Oxide runs it. The app contains no AI: it hands out the prompt
//! ([`AI_PROMPT`]), takes the assistant's answer back, previews it, saves it like an upload
//! (same server function, same limits, same plan rules, same creation key), and turns any problem
//! into a message to paste back into the chat.
//!
//! The JSON is taken out of the answer by the domain's [`extract_json`], then checked by
//! [`Program::from_json`] here for the preview; the server checks it again when it is saved.

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use iron_oxide_domain::ProgramId;
use iron_oxide_domain::program::{AI_PROMPT, ExtractError, Program, extract_json};

use super::{BusyGuard, Ended, ProgramDays, Programs, Screen, ended, problem_place, try_set};
use crate::api::programs::{
    ProgramProblems, ProgramView, UploadTarget, set_active_program, upload_program,
};
use crate::ui::components::{Button, ButtonVariant, Card};
use crate::ui::errors::BannerKind;
use crate::ui::shell::Route;

// --- View models ---------------------------------------------------------------------------------

/// Why a pasted answer can't be saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No single JSON object in it.
    Extract(ExtractError),
    /// The JSON is not a valid program: every problem, with its path.
    Problems(ProgramProblems),
}

/// A pasted answer, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    /// A valid program: the JSON document as the assistant wrote it (what is saved) and the
    /// program read from it (what is previewed).
    Ready {
        document: String,
        program: Program,
    },
    Refused(Refusal),
}

/// Takes the JSON out of the assistant's answer and validates it, as the server will.
#[must_use]
pub fn check_answer(answer: &str) -> Checked {
    let document = match extract_json(answer) {
        Ok(document) => document,
        Err(error) => return Checked::Refused(Refusal::Extract(error)),
    };
    match Program::from_json(document) {
        Ok(program) => Checked::Ready {
            document: document.to_owned(),
            program,
        },
        Err(error) => Checked::Refused(Refusal::Problems(error.into())),
    }
}

/// The prompt to copy. For a change to an existing program, the current document follows, with
/// the request to change it instead of starting from scratch.
#[must_use]
pub fn prompt_for(current: Option<&str>) -> String {
    match current {
        None => AI_PROMPT.to_owned(),
        Some(document) => format!(
            "{}\nMY CURRENT PROGRAM\n\nThis time I already have a program, below. Instead of the \
             STEP 1 interview, ask me what I want to change in it, then follow STEP 2: reply \
             with the whole updated program as one JSON document.\n\n{document}\n",
            AI_PROMPT.trim_end()
        ),
    }
}

/// The instruction that opens every "Copy for your AI" text.
pub const FIX_INSTRUCTION: &str = "Iron Oxide could not use this program. Fix the problems \
     below, keep everything else the same, and reply with only the whole corrected JSON \
     document, with no text before or after it.";

/// What "Copy for your AI" copies: a short instruction and the problems, one per line, as
/// `- path: message`. `None` when the problem is the user's to fix (nothing pasted, too long).
#[must_use]
pub fn fix_request(refusal: &Refusal) -> Option<String> {
    match refusal {
        Refusal::Extract(ExtractError::Empty | ExtractError::TooLong { .. }) => None,
        Refusal::Extract(ExtractError::NoJson) => Some(
            "I could not find the program in your answer. Reply with only the program, as one \
             JSON document, with no text before or after it."
                .to_owned(),
        ),
        Refusal::Extract(ExtractError::CutOff) => Some(
            "Your answer was cut off before the JSON ended. Send the complete program again as \
             one JSON document; if it is too long for one message, make the notes and the \
             description shorter."
                .to_owned(),
        ),
        Refusal::Extract(ExtractError::Several { .. }) => Some(
            "Your answer contained more than one program. Send only the new one, as one JSON \
             document, with no text before or after it."
                .to_owned(),
        ),
        Refusal::Problems(problems) => {
            let mut text = format!("{FIX_INSTRUCTION}\n\nProblems:\n");
            for problem in &problems.errors {
                let place = match problem_place(problem) {
                    place if place == "Document" => "(whole document)".to_owned(),
                    place => place,
                };
                text.push_str(&format!("- {place}: {}\n", problem.message));
            }
            if problems.omitted > 0 {
                text.push_str(&format!(
                    "- …and {} more problem(s) of the same kinds.\n",
                    problems.omitted
                ));
            }
            Some(text)
        }
    }
}

/// `4 days · 20 exercises`.
#[must_use]
pub fn program_summary(program: &Program) -> String {
    let days = program.days.len();
    let exercises = program.exercises().count();
    format!(
        "{days} {} · {exercises} {}",
        if days == 1 { "day" } else { "days" },
        if exercises == 1 {
            "exercise"
        } else {
            "exercises"
        }
    )
}

// --- State ---------------------------------------------------------------------------------------

/// A program saved from an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub program: ProgramView,
    pub version: u32,
}

/// The pasted answer, its check and what was saved from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flow {
    pub answer: String,
    pub checked: Option<Checked>,
    pub saved: Option<Saved>,
}

impl Flow {
    /// A new answer, typed (checked when asked) or pasted (checked now). Whatever was saved
    /// before is done with: the new answer is what the screen shows.
    pub fn take_answer(&mut self, answer: String, check_now: bool) {
        self.checked = check_now.then(|| check_answer(&answer));
        self.answer = answer;
        self.saved = None;
    }

    /// Checks the answer as it is.
    pub fn check(&mut self) {
        self.checked = Some(check_answer(&self.answer));
        self.saved = None;
    }

    /// The answer was saved: the screen offers to make it active.
    pub fn saved(&mut self, saved: Saved) {
        self.answer.clear();
        self.checked = None;
        self.saved = Some(saved);
    }

    /// The server refused the answer's program.
    pub fn refused(&mut self, problems: ProgramProblems) {
        self.checked = Some(Checked::Refused(Refusal::Problems(problems)));
    }
}

/// Whether `program` is the same as the program's current version (`current`, its document).
/// The `$schema` link is not part of the program: with or without it, they are the same.
#[must_use]
pub fn is_current_version(current: Option<&str>, program: &Program) -> bool {
    let without_schema = |program: &Program| Program {
        schema: None,
        ..program.clone()
    };
    current
        .and_then(|document| Program::from_json(document).ok())
        .is_some_and(|current| without_schema(&current) == without_schema(program))
}

/// The flow's state, held by the Programs screen so it survives the lists reloading. Reset when
/// the screen changes.
#[derive(Clone, Copy, PartialEq)]
pub struct AiState {
    flow: Signal<Flow>,
    /// A clipboard notice (copied, or blocked: what to do instead).
    notice: Signal<Option<String>>,
    show_prompt: Signal<bool>,
    busy: Signal<bool>,
}

impl AiState {
    pub fn new() -> Self {
        Self {
            flow: Signal::new(Flow::default()),
            notice: Signal::new(None),
            show_prompt: Signal::new(false),
            busy: Signal::new(false),
        }
    }

    /// Changes the flow, if the screen is still there.
    fn update(self, change: impl FnOnce(&mut Flow)) {
        let mut flow = self.flow;
        if let Ok(mut flow) = flow.try_write() {
            change(&mut flow);
        }
    }

    pub fn reset(self) {
        try_set(self.flow, Flow::default());
        try_set(self.notice, None);
        try_set(self.show_prompt, false);
    }
}

// --- Clipboard -----------------------------------------------------------------------------------

/// `navigator.clipboard`, which only secure pages (https, localhost) have.
#[cfg(feature = "web")]
fn clipboard() -> Option<web_sys::Clipboard> {
    use wasm_bindgen::JsCast;
    let navigator = web_sys::window()?.navigator();
    let clipboard = js_sys::Reflect::get(&navigator, &"clipboard".into()).ok()?;
    (!clipboard.is_undefined() && !clipboard.is_null()).then(|| clipboard.unchecked_into())
}

/// Starts copying `text`. Call it in the click handler itself, before any `await`: browsers only
/// allow the clipboard during the user's gesture. The future says whether it worked.
fn start_copy(text: &str) -> impl Future<Output = bool> + 'static {
    #[cfg(feature = "web")]
    let promise = clipboard().map(|clipboard| clipboard.write_text(text));
    #[cfg(not(feature = "web"))]
    let _ = text;
    async move {
        #[cfg(feature = "web")]
        if let Some(promise) = promise {
            return wasm_bindgen_futures::JsFuture::from(promise).await.is_ok();
        }
        false
    }
}

/// Starts reading the clipboard's text, like [`start_copy`]; `None` if the browser refuses.
#[cfg_attr(
    not(feature = "web"),
    allow(
        clippy::manual_async_fn,
        reason = "the browser build starts the read before returning the future"
    )
)]
fn start_read() -> impl Future<Output = Option<String>> + 'static {
    #[cfg(feature = "web")]
    let promise = clipboard().map(|clipboard| clipboard.read_text());
    async move {
        #[cfg(feature = "web")]
        if let Some(promise) = promise {
            return wasm_bindgen_futures::JsFuture::from(promise)
                .await
                .ok()
                .and_then(|text| text.as_string());
        }
        None
    }
}

// --- Components ----------------------------------------------------------------------------------

/// The flow, for a new program (`target: None`, on the list) or a new version of one (on its
/// details, with its `current` document). `allowed`: whether the plan includes uploads (`None`
/// while unknown).
#[component]
pub fn AiFlow(
    state: Programs,
    allowed: Option<bool>,
    target: Option<ProgramId>,
    current: Option<String>,
    active: bool,
) -> Element {
    let ai = state.ai;
    let prompt = prompt_for(current.as_deref());
    let Flow {
        answer,
        checked,
        saved,
    } = ai.flow.read().clone();
    let busy =
        *ai.busy.read() || (target.is_none() && state.intents.uploads.read().any_in_flight());
    let allowed = allowed.unwrap_or(true);
    let (title, intro) = match target {
        None => (
            "Create with your AI",
            "Your AI assistant (ChatGPT, Claude, Gemini…) asks about your goals, schedule, \
             equipment and injuries, then writes a program made for you. Iron Oxide runs it.",
        ),
        Some(_) => (
            "Change it with your AI",
            "Your AI assistant (ChatGPT, Claude, Gemini…) gets this program with the prompt, asks \
             what to change, and writes the next version.",
        ),
    };

    let copy_prompt = {
        let prompt = prompt.clone();
        move |_| {
            let copied = start_copy(&prompt);
            spawn(async move {
                let message = if copied.await {
                    "Prompt copied. Paste it into a chat with your AI assistant."
                } else {
                    try_set(ai.show_prompt, true);
                    "Copying is blocked here: select the prompt below and copy it."
                };
                try_set(ai.notice, Some(message.to_owned()));
            });
        }
    };
    let paste = move |_| {
        let read = start_read();
        spawn(async move {
            match read.await {
                Some(text) if !text.trim().is_empty() => {
                    ai.update(|flow| flow.take_answer(text, true));
                    try_set(ai.notice, None);
                }
                Some(_) => {
                    try_set(
                        ai.notice,
                        Some("The clipboard is empty: copy your AI's answer first.".to_owned()),
                    );
                }
                None => {
                    try_set(
                        ai.notice,
                        Some(
                            "Reading the clipboard is blocked here: long-press the box and \
                             choose Paste."
                                .to_owned(),
                        ),
                    );
                }
            }
        });
    };
    let check = move |_| {
        ai.update(Flow::check);
        try_set(ai.notice, None);
    };
    let field_id = if target.is_some() {
        "ai-answer-version"
    } else {
        "ai-answer"
    };

    rsx! {
        section { class: "io-card io-ai", aria_labelledby: "ai-title",
            h2 { id: "ai-title", "{title}" }
            p { class: "io-muted", "{intro}" }
            if !allowed {
                p { class: "io-notice io-notice-info",
                    "Saving your own programs is part of Iron Oxide Pro. "
                    Link { to: Route::Settings {}, "See your plan" }
                }
            } else {
                div { class: "io-ai-step",
                    span { class: "io-label", "1 · Copy the prompt" }
                    p { class: "io-muted io-hint",
                        "Paste it into a chat with your AI assistant and answer its questions."
                    }
                    Button { block: true, onclick: copy_prompt, "Copy the prompt" }
                    details {
                        class: "io-ai-prompt",
                        open: *ai.show_prompt.read(),
                        summary { "Show the prompt" }
                        textarea {
                            class: "io-input io-textarea",
                            readonly: true,
                            rows: 10,
                            "aria-label": "The prompt",
                            value: "{prompt}",
                        }
                    }
                }
                div { class: "io-ai-step",
                    label { class: "io-label", r#for: field_id, "2 · Paste its answer" }
                    textarea {
                        id: field_id,
                        class: "io-input io-textarea",
                        rows: 6,
                        placeholder: "Paste your AI's whole answer here",
                        spellcheck: false,
                        autocomplete: "off",
                        value: "{answer}",
                        oninput: move |event| {
                            ai.update(|flow| flow.take_answer(event.value(), false));
                        },
                    }
                    div { class: "io-ai-buttons",
                        Button { variant: ButtonVariant::Secondary, onclick: paste, "Paste from clipboard" }
                        Button { onclick: check, "Check the program" }
                    }
                }
                if let Some(notice) = ai.notice.read().clone() {
                    p { class: "io-notice io-notice-info", role: "status", "{notice}" }
                }
                if let Some(Checked::Refused(refusal)) = &checked {
                    RefusalNotice { state, refusal: refusal.clone() }
                }
            }
        }
        if allowed {
            if let Some(saved) = saved {
                SavedCard { state, saved, target, active }
            } else if let Some(Checked::Ready { document, program }) = checked {
                Preview {
                    state,
                    unchanged: is_current_version(current.as_deref(), &program),
                    document,
                    program,
                    target,
                    busy,
                }
            }
        }
    }
}

/// Why the answer was refused, and what to send back to the assistant.
#[component]
fn RefusalNotice(state: Programs, refusal: Refusal) -> Element {
    let ai = state.ai;
    let request = fix_request(&refusal);
    let copy_fix = move |_| {
        let Some(request) = request.clone() else {
            return;
        };
        let copied = start_copy(&request);
        spawn(async move {
            let message = if copied.await {
                "Copied. Paste it into the same chat, then paste the new answer here."
            } else {
                "Copying is blocked here: tell your AI the problems listed above."
            };
            try_set(ai.notice, Some(message.to_owned()));
        });
    };
    let can_copy = fix_request(&refusal).is_some();
    rsx! {
        div { class: "io-notice io-notice-error io-problems", role: "alert",
            match &refusal {
                Refusal::Extract(error) => rsx! { p { "{error}" } },
                Refusal::Problems(problems) => rsx! {
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
                },
            }
            if can_copy {
                Button { variant: ButtonVariant::Secondary, block: true, onclick: copy_fix, "Copy for your AI" }
                p { class: "io-hint", "Paste it into the same chat: your AI fixes the program and sends it again." }
            }
        }
    }
}

/// The program as it will run, and saving it.
#[component]
fn Preview(
    state: Programs,
    document: String,
    program: Program,
    target: Option<ProgramId>,
    busy: bool,
    /// The program equals the current version: nothing to save.
    unchanged: bool,
) -> Element {
    let save_document = document.clone();
    let save = move |_| {
        let Some(guard) = BusyGuard::start(state.ai.busy) else {
            return;
        };
        let document = save_document.clone();
        state.clear_notices();
        spawn_forever(async move {
            let _guard = guard;
            save(state, target, document).await;
        });
    };
    let label = if busy {
        "Saving…"
    } else if target.is_some() {
        "Save as a new version"
    } else {
        "Save as a new program"
    };
    rsx! {
        section { class: "io-card io-ai-preview", aria_labelledby: "ai-preview-title",
            span { class: "io-label", "Preview" }
            h2 { id: "ai-preview-title", "{program.name}" }
            p { class: "io-muted", "{program_summary(&program)}" }
        }
        ProgramDays { document: program.clone() }
        div { class: "io-actions",
            if unchanged {
                p { class: "io-notice io-notice-info", role: "status",
                    "This is the same as the current version: your AI changed nothing. Tell it \
                     what you want to change, then paste its new answer."
                }
            } else {
                Button { block: true, busy, onclick: save, "{label}" }
            }
            Button {
                variant: ButtonVariant::Ghost,
                block: true,
                disabled: busy,
                onclick: move |_| state.ai.reset(),
                "Discard"
            }
        }
    }
}

/// Saves the checked document, as an upload.
async fn save(state: Programs, target: Option<ProgramId>, document: String) {
    let origin = target.map_or(Screen::List, Screen::Mine);
    let upload_target = match target {
        Some(program_id) => UploadTarget::NewVersion { program_id },
        None => {
            let Some(creation_id) = state.begin_upload(&document) else {
                // The same document is already being saved.
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
                (false, false) => {
                    format!("\u{201c}{}\u{201d} is already saved.", outcome.program.name)
                }
                (true, true) => format!("Version {} saved.", outcome.version.version),
                (true, false) => format!("\u{201c}{}\u{201d} saved.", outcome.program.name),
            };
            state.errors.show(BannerKind::Info, message);
            // A new version equal to the latest one saved nothing: the preview stays.
            let saved_something = outcome.saved || target.is_none();
            if saved_something && state.is_on(&origin) {
                state.ai.update(|flow| {
                    flow.saved(Saved {
                        program: outcome.program,
                        version: outcome.version.version,
                    });
                });
            }
            state.changed();
        }
        Err(error) => match ProgramProblems::from_error(&error) {
            // The server's checks are the same as the preview's, but they decide.
            Some(problems) if state.is_on(&origin) => {
                state.ai.update(|flow| flow.refused(problems));
            }
            _ => state.fail(&error, &origin),
        },
    }
}

/// After saving: make it the program to train with.
#[component]
fn SavedCard(state: Programs, saved: Saved, target: Option<ProgramId>, active: bool) -> Element {
    let busy = *state.ai.busy.read();
    let id = saved.program.id;
    let activate = move |_| {
        let Some(guard) = BusyGuard::start(state.ai.busy) else {
            return;
        };
        state.clear_notices();
        spawn_forever(async move {
            let _guard = guard;
            let origin = target.map_or(Screen::List, Screen::Mine);
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
                    state.open_from(&origin, Screen::Mine(id));
                }
                Err(error) => state.fail(&error, &origin),
            }
        });
    };
    let title = if target.is_some() {
        format!("Version {} saved", saved.version)
    } else {
        format!("\u{201c}{}\u{201d} saved", saved.program.name)
    };
    rsx! {
        Card { title,
            if active {
                p { class: "io-muted", "You train with this program: the next session uses this version." }
            } else {
                p { class: "io-muted", "Make it active to train with it." }
                Button { block: true, busy, onclick: activate, "Make active" }
            }
            if target.is_none() {
                Button {
                    variant: ButtonVariant::Secondary,
                    block: true,
                    disabled: busy,
                    onclick: move |_| state.open(Screen::Mine(id)),
                    "Open the program"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use iron_oxide_domain::program::AI_PROMPT_SCHEMA_URL;

    use super::*;
    use crate::api::programs::ProgramProblem;

    const VALID: &str = r#"{"schema_version": 1, "name": "Mine", "days": [{"id": "a", "name": "A",
        "exercises": [{"id": "squat", "name": "Squat", "work": {"reps": {"sets": 3, "reps": 5}},
        "rest": 120}]}], "rotation": ["a"]}"#;

    #[test]
    fn a_fenced_answer_is_checked_and_kept_as_written() {
        let answer = format!("Here you go!\n```json\n{VALID}\n```\nEnjoy.");
        let Checked::Ready { document, program } = check_answer(&answer) else {
            panic!("refused");
        };
        assert_eq!(document, VALID);
        assert_eq!(program.name, "Mine");
        assert_eq!(program_summary(&program), "1 day · 1 exercise");
    }

    #[test]
    fn answers_that_are_not_one_valid_program_are_refused() {
        assert_eq!(
            check_answer("  "),
            Checked::Refused(Refusal::Extract(ExtractError::Empty))
        );
        assert_eq!(
            check_answer(&format!("{VALID}\n{VALID}")),
            Checked::Refused(Refusal::Extract(ExtractError::Several { count: 2 }))
        );
        // Review of #111: a valid snippet beside a broken program is not what gets reported.
        let Checked::Refused(Refusal::Problems(problems)) = check_answer(&format!(
            "Increments look like {{\"kg\": 2.5}}.\n```json\n{}\n```",
            VALID.replace("\"rotation\"", "\"x\": 1,, \"rotation\"")
        )) else {
            panic!("not refused with problems");
        };
        assert!(
            problems
                .errors
                .iter()
                .all(|problem| !problem.message.contains("kg")),
            "{problems:?}"
        );
        // The validator is not relaxed: a program that breaks a rule is refused with its path.
        let broken = VALID.replace("\"sets\": 3", "\"sets\": 0");
        let Checked::Refused(Refusal::Problems(problems)) = check_answer(&broken) else {
            panic!("accepted");
        };
        assert_eq!(
            problems.errors[0].path,
            "days[0].exercises[0].work.reps.sets"
        );
        // A syntax error carries its position in the JSON.
        let Checked::Refused(Refusal::Problems(problems)) =
            check_answer("```json\n{\"name\": \"x\", \"days\": [],}\n```")
        else {
            panic!("accepted");
        };
        assert_eq!(problems.errors[0].line, Some(1));
    }

    fn problem(path: &str, message: &str, line: Option<usize>) -> ProgramProblem {
        ProgramProblem {
            path: path.to_owned(),
            message: message.to_owned(),
            line,
            column: line.map(|_| 5),
        }
    }

    #[test]
    fn the_fix_request_lists_every_problem_with_its_path() {
        let refusal = Refusal::Problems(ProgramProblems {
            errors: vec![
                problem(
                    "days[1].exercises[2].work.reps.reps",
                    "min 12 is greater than max 8",
                    None,
                ),
                problem("", "expected `,` or `}`", Some(3)),
                problem("days", "must contain at least one day", None),
                problem("", "must be a JSON object", None),
            ],
            omitted: 2,
        });
        assert_eq!(
            fix_request(&refusal).unwrap(),
            "Iron Oxide could not use this program. Fix the problems below, keep everything else \
             the same, and reply with only the whole corrected JSON document, with no text before \
             or after it.\n\
             \n\
             Problems:\n\
             - days[1].exercises[2].work.reps.reps: min 12 is greater than max 8\n\
             - line 3, column 5: expected `,` or `}`\n\
             - days: must contain at least one day\n\
             - (whole document): must be a JSON object\n\
             - …and 2 more problem(s) of the same kinds.\n"
        );
    }

    #[test]
    fn extraction_failures_have_their_own_requests() {
        let request = |error| fix_request(&Refusal::Extract(error));
        assert_eq!(request(ExtractError::Empty), None);
        assert_eq!(request(ExtractError::TooLong { bytes: 1 }), None);
        assert!(
            request(ExtractError::NoJson)
                .unwrap()
                .contains("only the program")
        );
        assert!(request(ExtractError::CutOff).unwrap().contains("cut off"));
        assert!(
            request(ExtractError::Several { count: 3 })
                .unwrap()
                .contains("more than one program")
        );
    }

    fn saved() -> Saved {
        Saved {
            program: ProgramView {
                id: ProgramId::from_uuid(uuid::Uuid::from_u128(1)),
                name: "Old".to_owned(),
                source_builtin_id: None,
                archived: false,
                created_at: iron_oxide_domain::time::Timestamp::from_epoch_millis(0),
            },
            version: 1,
        }
    }

    /// Review of #111: after a save, a new answer (pasted or typed) replaces the saved card.
    #[test]
    fn a_new_answer_after_a_save_shows_its_own_preview() {
        let mut flow = Flow::default();
        flow.take_answer(VALID.to_owned(), true);
        flow.saved(saved());
        assert_eq!(flow.answer, "");
        let other = VALID.replace("\"Mine\"", "\"Other\"");
        flow.take_answer(other.clone(), true);
        assert_eq!(flow.saved, None);
        assert!(matches!(
            &flow.checked,
            Some(Checked::Ready { program, .. }) if program.name == "Other"
        ));
        // Typing: the saved card goes too, and the answer waits for "Check the program".
        flow.saved(saved());
        flow.take_answer(other, false);
        assert_eq!(
            (flow.saved.is_some(), flow.checked.is_some()),
            (false, false)
        );
        flow.check();
        assert!(matches!(flow.checked, Some(Checked::Ready { .. })));
    }

    #[test]
    fn an_answer_equal_to_the_current_version_is_recognised() {
        let Checked::Ready { program, .. } = check_answer(VALID) else {
            panic!("refused");
        };
        let current = program.to_json_pretty().unwrap();
        assert!(is_current_version(Some(&current), &program));
        let other = Program::from_json(&VALID.replace("\"Mine\"", "\"Other\"")).unwrap();
        assert!(!is_current_version(Some(&current), &other));
        assert!(!is_current_version(None, &program));
        // Fix round of #111: the stored version carries `$schema`, the AI's answer doesn't.
        assert_eq!(program.schema, None);
        let with_schema = VALID.replacen(
            '{',
            &format!(
                "{{\"$schema\": \"{}\",",
                iron_oxide_domain::program::PROGRAM_SCHEMA_URL
            ),
            1,
        );
        assert!(is_current_version(Some(&with_schema), &program));
    }

    #[test]
    fn the_prompt_names_the_schema_and_can_carry_the_current_program() {
        let fresh = prompt_for(None);
        assert_eq!(fresh, AI_PROMPT);
        assert!(fresh.contains(AI_PROMPT_SCHEMA_URL));
        let change = prompt_for(Some(VALID));
        assert!(change.starts_with(AI_PROMPT.trim_end()));
        assert!(change.contains("MY CURRENT PROGRAM"));
        assert!(change.trim_end().ends_with(VALID));
    }
}
