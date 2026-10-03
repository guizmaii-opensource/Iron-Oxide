//! The workout session (#28): start the next workout, log it one set at a time, finish it and
//! see its summary.
//!
//! - `flow`: the pure view model (order of the sets, prefill, labels), unit-tested.
//! - `writes`: every session write (start, save a set, finish), queued in the offline outbox (#107).
//! - `local`: the session in progress kept on the device, and how it is reconciled on load.
//! - `workout`: the active session screen.
//! - `rest`: the rest timer between sets (#29).
//! - `summary`: the end-of-session summary (#32).
//! - `platform`: the browser clock, sound, vibration and the screen wake lock.
//!
//! The page loads on the client only (like the shell's sign-in check), so the server render shows
//! the loading state and hydration matches. It waits until the outbox knows the user, then
//! restores the session kept on the device (offline too) or loads it from the server, see
//! [`local::reconcile`].

mod flow;
pub(crate) mod local;
pub(crate) mod platform;
mod rest;
mod summary;
mod workout;
pub(crate) mod writes;

use std::collections::BTreeSet;

use dioxus::prelude::*;
use iron_oxide_domain::time::Timestamp;
use iron_oxide_domain::{ExerciseId, LoggedSet};

use crate::api::error::{ApiFailure, FailureKind};
use crate::api::sessions::{
    NextSessionPlan, SessionPlan, SessionWithSets, get_in_progress_session, get_next_session_plan,
    get_session_plan,
};
use crate::api::settings::{Settings, get_settings};
use crate::auth::api::me;
use crate::offline::{Outbox, use_outbox};
use crate::ui::components::{Button, Card, EmptyState, LoadingState};
use crate::ui::errors::{BannerKind, Errors, use_errors};
use crate::ui::shell::Route;
use crate::ui::weight::{UnitSetting, use_unit};
use local::{Draft, Restore};
use rest::Rest;
use summary::{Finished, SummaryScreen};
use workout::Workout;

/// Shown when a write cannot be queued because nobody is signed in on this device.
pub const NOT_SIGNED_IN: &str = "Sign in again to save this workout.";

/// A session in progress, as the screen works on it.
#[derive(Debug, Clone, PartialEq)]
pub struct Active {
    pub plan: SessionPlan,
    pub settings: Settings,
    /// The sets logged so far, in logging order (queued or delivered).
    pub sets: Vec<LoggedSet<Timestamp>>,
    /// The exercises skipped on this device.
    pub skipped: BTreeSet<ExerciseId>,
    /// The program's name, when known (for Home's Resume card).
    pub program_name: Option<String>,
}

/// What the page shows.
#[derive(Debug, Clone, PartialEq)]
enum Page {
    Loading,
    /// Loading failed (the banner says why).
    Failed,
    /// No session in progress: the next one, ready to start.
    Start(Box<NextSessionPlan>, Box<Settings>),
    /// No session can start (no active program): the server's message.
    Blocked(String),
    /// The session in progress, with the values being entered and the rest kept with it.
    Active(Box<Active>, Option<Draft>, Option<Rest>),
    /// The workout just ended: its summary, once its finish is delivered.
    Summary(Box<Finished>),
}

/// The `/session` page.
#[component]
pub fn SessionPage() -> Element {
    let page = use_signal(|| Page::Loading);
    let errors = use_errors();
    let unit = use_context::<UnitSetting>();
    let outbox = use_outbox();
    let mut loaded = use_signal(|| false);
    let mut asked_who = use_signal(|| false);

    // Client only, once the outbox knows whose writes it holds (it reads the remembered user
    // after its first render): only then is an empty queue "delivered" rather than "not loaded".
    use_effect(move || {
        if !cfg!(feature = "web") || *loaded.peek() {
            return;
        }
        if outbox.user().is_none() {
            // Signed in before the outbox remembered users: ask the server once who it is.
            if !*asked_who.peek() {
                asked_who.set(true);
                spawn(async move {
                    if let Ok(me) = me().await {
                        outbox.signed_in(me.user_id);
                    }
                });
            }
            return;
        }
        loaded.set(true);
        spawn(load(page, errors, unit, outbox));
    });

    let current = page.read().clone();
    match current {
        Page::Loading => rsx! { LoadingState { message: "Loading your workout…" } },
        Page::Failed => rsx! {
            EmptyState {
                title: "Workout not loaded",
                message: "Check your connection and try again.",
                Button { onclick: move |_| { spawn(load(page, errors, unit, outbox)); }, "Try again" }
            }
        },
        Page::Blocked(message) => rsx! {
            EmptyState { title: "No workout to start", message,
                Link { class: "io-button io-button-secondary", to: Route::Programs {}, "Choose a program" }
            }
        },
        Page::Start(next, settings) => rsx! {
            StartCard {
                next: *next,
                settings: *settings,
                on_started: move |active: Active| {
                    let mut page = page;
                    page.set(Page::Active(Box::new(active), None, None));
                },
            }
        },
        Page::Active(active, draft, rest) => rsx! {
            Workout {
                key: "{active.plan.session.id}",
                initial: *active,
                initial_draft: draft,
                initial_rest: rest,
                on_finished: move |finished: Finished| {
                    let mut page = page;
                    page.set(Page::Summary(Box::new(finished)));
                },
            }
        },
        Page::Summary(finished) => rsx! {
            SummaryScreen { key: "{finished.plan.session.id}", finished: *finished }
        },
    }
}

/// Restores the session kept on the device, or loads the session in progress (or the next one)
/// from the server. Offline, the record is enough.
async fn load(mut page: Signal<Page>, errors: Errors, unit: UnitSetting, outbox: Outbox) {
    page.set(Page::Loading);
    let Some(user) = outbox.user() else {
        page.set(Page::Failed);
        return;
    };
    let record = local::load(user);
    // Only worth asking when there is something to reconcile: offline it fails fast.
    let server = get_in_progress_session().await;
    let queued = outbox.queued();
    let decision = local::reconcile(record, server.as_ref().ok().map(Option::as_ref), &queued);
    let next = match decision {
        Restore::Resume(record) => match local::active_of(&record) {
            Some((active, draft, rest)) => {
                show_unit(unit, &active.settings);
                // Keep what the server added (sets logged elsewhere).
                let _ = local::save(user, &record);
                Page::Active(Box::new(active), draft, rest)
            }
            None => from_server(server, errors, unit, outbox).await,
        },
        Restore::Ended(record) => match (local::screen_of(&record), record.finished) {
            (Some(screen), Some(finish)) => Page::Summary(Box::new(Finished {
                plan: screen.plan,
                sets: record.sets,
                outcome: finish.outcome,
                finished_at: finish.finished_at,
            })),
            _ => from_server(server, errors, unit, outbox).await,
        },
        Restore::Drop => {
            local::clear(user);
            from_server(server, errors, unit, outbox).await
        }
        Restore::Server => from_server(server, errors, unit, outbox).await,
    };
    page.set(next);
}

/// The page from the server: the session in progress (kept on the device from now on), or the
/// next one to start.
async fn from_server(
    server: Result<Option<SessionWithSets>, ServerFnError>,
    errors: Errors,
    unit: UnitSetting,
    outbox: Outbox,
) -> Page {
    let running = match server {
        Ok(running) => running,
        Err(error) => {
            errors.report(&error);
            return Page::Failed;
        }
    };
    let settings = match get_settings().await {
        Ok(settings) => settings,
        Err(error) => {
            errors.report(&error);
            return Page::Failed;
        }
    };
    show_unit(unit, &settings);
    match running {
        Some(running) => match get_session_plan(running.session.id).await {
            Ok(plan) => {
                let active = Active {
                    plan,
                    settings,
                    sets: running.sets,
                    skipped: BTreeSet::new(),
                    program_name: None,
                };
                // From now on the device keeps it, so an offline reload still finds it.
                if let Some(user) = outbox.user() {
                    let _ = local::save(user, &local::record(&active, None, None));
                }
                Page::Active(Box::new(active), None, None)
            }
            Err(error) => {
                errors.report(&error);
                Page::Failed
            }
        },
        None => match get_next_session_plan().await {
            Ok(next) => Page::Start(Box::new(next), Box::new(settings)),
            Err(error) => {
                let failure = ApiFailure::classify(&error);
                if failure.kind == FailureKind::Conflict {
                    // No active program: the page itself says so, with the way out.
                    Page::Blocked(failure.message)
                } else {
                    errors.report(&error);
                    Page::Failed
                }
            }
        },
    }
}

/// Shows weights in the session's unit.
fn show_unit(mut unit: UnitSetting, settings: &Settings) {
    if *unit.0.peek() != settings.unit {
        unit.0.set(settings.unit);
    }
}

/// The next workout, with its exercises, and the button that starts it.
#[component]
fn StartCard(
    next: NextSessionPlan,
    settings: Settings,
    on_started: EventHandler<Active>,
) -> Element {
    let errors = use_errors();
    let unit = use_unit();
    let outbox = use_outbox();
    let bar_weight = settings.bar_weight;
    let next_plan = next.clone();

    let start = move |_| {
        // Inside the tap: lets iOS play the rest timer's beeps later.
        platform::unlock_audio();
        match local::start(outbox, &next_plan, settings.clone(), None) {
            Ok(active) => on_started.call(active),
            Err(_) => {
                errors.show(BannerKind::Error, NOT_SIGNED_IN);
            }
        }
    };

    let lines: Vec<(String, String)> = next
        .exercises
        .iter()
        .map(|planned| {
            (
                planned.exercise.name.clone(),
                flow::exercise_summary(planned, bar_weight, unit),
            )
        })
        .collect();
    rsx! {
        div { class: "io-page-header",
            span { class: "io-label", "Next workout" }
            h1 { class: "io-title", "{next.day_name}" }
        }
        Card {
            ul { class: "io-list io-session-plan",
                for (index, (name, summary)) in lines.into_iter().enumerate() {
                    li { key: "{index}", class: "io-row",
                        div { class: "io-row-main",
                            span { class: "io-row-title", "{name}" }
                            span { class: "io-row-meta io-muted", "{summary}" }
                        }
                    }
                }
            }
        }
        Button { xl: true, block: true, onclick: start, "Start workout" }
    }
}

/// Reports `message` as a note in the banner.
fn note(errors: Errors, message: &str) {
    errors.show(BannerKind::Info, message);
}
