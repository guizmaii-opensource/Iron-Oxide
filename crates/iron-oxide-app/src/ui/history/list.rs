//! The history screen: the exercises with a progress chart, then the sessions, most recent first,
//! a page at a time.

use dioxus::prelude::*;

use super::progress::LockedCharts;
use super::view::{append_page, day_name, duration_text, status_label, volume_text};
use super::{ChartAccess, HistoryContext, local_date, use_history};
use crate::api::history::{HistoryCursor, SessionSummary, history_page, logged_exercises};
use crate::ui::components::{Button, ButtonVariant, Card, EmptyState, LoadingState};
use crate::ui::errors::{Errors, use_errors};
use crate::ui::shell::Route;
use crate::ui::weight::use_unit;

/// Where the list of sessions is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loading {
    /// A page is on its way.
    Busy,
    /// Shows what it has.
    Idle,
    /// The last page failed (the error is in the banner); it can be tried again.
    Failed,
}

/// The sessions loaded so far and where the next page starts.
#[derive(Clone, Copy, PartialEq)]
struct Pages {
    sessions: Signal<Vec<SessionSummary>>,
    next: Signal<Option<HistoryCursor>>,
    /// Whether the first page has arrived.
    started: Signal<bool>,
    loading: Signal<Loading>,
    history: HistoryContext,
}

impl Pages {
    /// Loads the page after the last one shown (the first page if none is).
    fn load_next(mut self, errors: Errors) {
        if *self.loading.peek() == Loading::Busy {
            return;
        }
        self.loading.set(Loading::Busy);
        let cursor = *self.next.peek();
        let history = self.history;
        spawn(async move {
            match history_page(cursor, None).await {
                Ok(page) => {
                    history.learn_programs(page.sessions.iter().map(|s| s.program_id), errors);
                    append_page(&mut self.sessions.write(), page.sessions);
                    self.next.set(page.next);
                    self.started.set(true);
                    self.loading.set(Loading::Idle);
                }
                Err(error) => {
                    errors.report(&error);
                    self.loading.set(Loading::Failed);
                }
            }
        });
    }
}

/// The history screen.
#[component]
pub fn History() -> Element {
    let errors = use_errors();
    let pages = Pages {
        sessions: use_signal(Vec::new),
        next: use_signal(|| None),
        started: use_signal(|| false),
        loading: use_signal(|| Loading::Idle),
        history: use_history(),
    };
    // Fresh on every visit, so a session finished meanwhile shows.
    use_hook(move || {
        if cfg!(feature = "web") {
            pages.load_next(errors);
        }
    });

    rsx! {
        div { class: "io-page-header",
            h1 { class: "io-title", "History" }
        }
        ProgressSection {}
        Sessions { pages }
    }
}

/// The sessions, or the state of the list.
#[component]
fn Sessions(pages: Pages) -> Element {
    let errors = use_errors();
    let loading = *pages.loading.read();
    let sessions = pages.sessions.read();

    if !*pages.started.read() {
        return match loading {
            Loading::Failed => rsx! {
                EmptyState {
                    title: "Couldn't load",
                    message: "Your workouts could not be loaded. Check your connection and try again.",
                    Button {
                        variant: ButtonVariant::Secondary,
                        onclick: move |_| pages.load_next(errors),
                        "Try again"
                    }
                }
            },
            Loading::Busy | Loading::Idle => rsx! {
                LoadingState { message: "Loading your workouts…" }
            },
        };
    }
    if sessions.is_empty() {
        return rsx! {
            EmptyState {
                title: "No workouts yet",
                message: "Finished workouts show up here, with their sets and your progress.",
                Link { class: "io-button io-button-secondary", to: Route::Home {}, "Start a workout" }
            }
        };
    }

    let more = pages.next.read().is_some();
    rsx! {
        section { class: "io-history", aria_labelledby: "io-sessions-title",
            h2 { id: "io-sessions-title", class: "io-label", "Sessions" }
            ul { class: "io-history-list",
                for session in sessions.iter() {
                    SessionRow { key: "{session.id}", session: session.clone() }
                }
            }
            if more || loading == Loading::Failed {
                Button {
                    variant: ButtonVariant::Secondary,
                    block: true,
                    busy: loading == Loading::Busy,
                    onclick: move |_| pages.load_next(errors),
                    if loading == Loading::Failed { "Try again" } else { "Load more" }
                }
            }
        }
    }
}

/// One session of the list: date, day, program, duration and working sets; a link to its details.
#[component]
fn SessionRow(session: SessionSummary) -> Element {
    let history = use_history();
    let unit = use_unit();
    let date = local_date(session.started_at.epoch_millis()).long();
    let day = day_name(&session, &history.names.read());
    let mut meta = vec![session.program_name.clone()];
    if let Some(duration) = duration_text(session.started_at, session.finished_at) {
        meta.push(duration);
    }
    meta.push(match session.working_sets {
        1 => "1 set".to_owned(),
        count => format!("{count} sets"),
    });
    if !session.volume.is_zero() {
        meta.push(volume_text(session.volume, unit));
    }
    let meta = meta.join(" · ");
    let status = status_label(session.status);

    rsx! {
        li {
            Link {
                class: "io-history-row",
                to: Route::HistorySession { id: session.id },
                span { class: "io-history-date", "{date}" }
                span { class: "io-history-day",
                    "{day}"
                    if session.set_pr {
                        PrBadge {}
                    }
                    if let Some(status) = status {
                        span { class: "io-chip io-status-chip", "{status}" }
                    }
                }
                span { class: "io-history-meta", "{meta}" }
            }
        }
    }
}

/// The badge of a session that set a personal record.
#[component]
pub fn PrBadge() -> Element {
    rsx! {
        span { class: "io-pr-badge",
            span { "aria-hidden": "true", "PR" }
            span { class: "io-sr-only", "Personal record" }
        }
    }
}

/// The exercises with a progress chart, or the locked card when the plan has no charts.
#[component]
fn ProgressSection() -> Element {
    let history = use_history();
    match *history.charts.read() {
        ChartAccess::Included => rsx! { ExerciseLinks {} },
        ChartAccess::Locked => rsx! { LockedCharts {} },
        // Nothing yet, or the plan is unknown (said in the banner): the sessions still show.
        ChartAccess::Checking | ChartAccess::Unknown => rsx! {},
    }
}

/// Links to the charts of every exercise the user has logged, most recently trained first.
#[component]
fn ExerciseLinks() -> Element {
    let errors = use_errors();
    let history = use_history();
    let mut exercises = use_resource(move || async move {
        let result = logged_exercises().await;
        if let Err(error) = &result {
            errors.report(error);
        }
        result
    });

    let content = match &*exercises.read() {
        None => rsx! {
            p { class: "io-muted", role: "status", "Loading your exercises…" }
        },
        Some(Err(_)) => rsx! {
            p { class: "io-muted", "Your exercises could not be loaded." }
            button {
                r#type: "button",
                class: "io-button io-button-secondary",
                onclick: move |_| exercises.restart(),
                "Try again"
            }
        },
        Some(Ok(list)) if list.is_empty() => rsx! {
            p { class: "io-muted", "Charts appear once you have finished a workout." }
        },
        Some(Ok(list)) => {
            let names = history.names.read();
            rsx! {
                ul { class: "io-chips io-exercise-links",
                    for exercise in list.iter() {
                        li { key: "{exercise.exercise_id}",
                            Link {
                                class: "io-chip",
                                to: Route::ExerciseProgress { exercise: exercise.exercise_id.clone() },
                                {names.exercise(&exercise.exercise_id)}
                            }
                        }
                    }
                }
            }
        }
    };
    rsx! {
        Card { title: "Progress",
            {content}
        }
    }
}
