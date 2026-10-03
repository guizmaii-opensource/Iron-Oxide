//! One session's details: when, how long, how much, and every set of every exercise.

use dioxus::prelude::*;
use iron_oxide_domain::SessionId;

use super::list::PrBadge;
use super::view::{day_name, duration_text, set_rows, status_label, volume_text};
use super::{BackToHistory, ChartAccess, local_date, use_history};
use crate::api::error::{ApiFailure, FailureKind};
use crate::api::history::{ExerciseLog, SessionDetails, session_details};
use crate::ui::components::{Card, EmptyState, LoadingState};
use crate::ui::errors::use_errors;
use crate::ui::shell::Route;
use crate::ui::weight::{estimate_text, use_unit, weight_text};

/// The details screen of the session `id`.
#[component]
pub fn HistorySession(id: SessionId) -> Element {
    let errors = use_errors();
    let history = use_history();
    let mut details = use_resource(use_reactive!(|id| async move {
        let result = session_details(id).await;
        match &result {
            Ok(details) => history.learn_programs([details.session.program_id], errors),
            Err(error) => errors.report(error),
        }
        result
    }));

    let content = match &*details.read() {
        None => rsx! { LoadingState { message: "Loading the workout…" } },
        Some(Err(error)) => {
            let missing = ApiFailure::classify(error).kind == FailureKind::NotFound;
            rsx! {
                EmptyState {
                    title: if missing { "Not found" } else { "Couldn't load" },
                    message: if missing {
                        "This workout is not in your history."
                    } else {
                        "This workout could not be loaded. Check your connection and try again."
                    },
                    if !missing {
                        button {
                            r#type: "button",
                            class: "io-button io-button-secondary",
                            onclick: move |_| details.restart(),
                            "Try again"
                        }
                    }
                }
            }
        }
        Some(Ok(loaded)) => rsx! { Session { details: loaded.clone() } },
    };

    rsx! {
        BackToHistory {}
        {content}
    }
}

/// A loaded session.
#[component]
fn Session(details: SessionDetails) -> Element {
    let history = use_history();
    let unit = use_unit();
    let session = &details.session;
    let date = local_date(session.started_at.epoch_millis()).long();
    let day = day_name(session, &history.names.read());
    let duration =
        duration_text(session.started_at, session.finished_at).unwrap_or_else(|| "—".to_owned());
    let volume = volume_text(session.volume, unit);
    let sets = session.working_sets;
    let status = status_label(session.status);
    let program = format!(
        "{} · version {}",
        session.program_name, session.program_version
    );

    rsx! {
        div { class: "io-page-header",
            span { class: "io-label", "{date}" }
            h1 { class: "io-title", "{day}" }
            p { class: "io-muted io-session-meta",
                "{program}"
                if session.set_pr {
                    " "
                    PrBadge {}
                }
                if let Some(status) = status {
                    " "
                    span { class: "io-chip io-status-chip", "{status}" }
                }
            }
        }
        dl { class: "io-stats",
            div { class: "io-stat",
                dt { "Duration" }
                dd { "{duration}" }
            }
            div { class: "io-stat",
                dt { "Volume" }
                dd { "{volume}" }
            }
            div { class: "io-stat",
                dt { "Sets" }
                dd { "{sets}" }
            }
        }
        if details.exercises.is_empty() {
            Card {
                p { class: "io-muted", "No sets were logged in this workout." }
            }
        }
        for log in details.exercises.iter() {
            ExerciseCard { key: "{log.exercise_id}", log: log.clone() }
        }
    }
}

/// The sets of one exercise, with its best set, estimate and volume.
#[component]
fn ExerciseCard(log: ExerciseLog) -> Element {
    let history = use_history();
    let unit = use_unit();
    let name = history.names.read().exercise(&log.exercise_id);
    let rows = set_rows(&log.sets, unit);
    let mut facts = Vec::new();
    if let Some(top) = log.top_set {
        facts.push((
            "Top set",
            format!("{} × {}", weight_text(top.weight, unit), top.reps.get()),
        ));
    }
    if let Some(e1rm) = log.best_e1rm {
        facts.push(("e1RM", estimate_text(e1rm, unit)));
    }
    if !log.volume.is_zero() {
        facts.push(("Volume", volume_text(log.volume, unit)));
    }
    let charts = *history.charts.read();

    rsx! {
        Card { title: name.clone(),
            ol { class: "io-sets", aria_label: "Sets of {name}",
                for (index, row) in rows.iter().enumerate() {
                    li {
                        key: "{index}",
                        class: "io-set",
                        "data-warmup": row.warm_up,
                        "data-failed": row.failed,
                        span { class: "io-set-label",
                            if row.warm_up {
                                span { class: "io-sr-only", "Warm-up " }
                            } else {
                                span { class: "io-sr-only", "Set " }
                            }
                            "{row.label}"
                        }
                        span { class: "io-set-text", "{row.text}" }
                        if row.failed {
                            span { class: "io-muted io-set-note", "missed" }
                        }
                    }
                }
            }
            if !facts.is_empty() {
                dl { class: "io-facts",
                    for (label, value) in facts {
                        div { key: "{label}",
                            dt { "{label}" }
                            dd { "{value}" }
                        }
                    }
                }
            }
            if log.top_set.is_some() && charts != ChartAccess::Unknown {
                Link {
                    class: "io-button io-button-ghost",
                    to: Route::ExerciseProgress { exercise: log.exercise_id.clone() },
                    if charts == ChartAccess::Locked { "Progress chart · Pro" } else { "Progress chart" }
                }
            }
        }
    }
}
