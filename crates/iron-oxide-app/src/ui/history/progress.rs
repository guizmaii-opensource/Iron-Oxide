//! An exercise's progress: its estimated 1RM and its top set over time, as inline SVG charts with
//! a data table. A Pro feature (`Feature::ExerciseCharts`): without it, the locked card.

use dioxus::prelude::*;
use iron_oxide_domain::ExerciseId;

use super::chart::{self, ChartLayout, VIEW_HEIGHT, VIEW_WIDTH, WeightPoint};
use super::view::{
    Measure, chart_series, chart_summary, charts_locked_after, latest_number, latest_volume,
    series_rows, volume_summary,
};
use super::{BackToHistory, ChartAccess, local_date, use_history};
use crate::api::billing::my_entitlements;
use crate::api::error::{ApiFailure, FailureKind};
use crate::api::history::{ExerciseSeries, exercise_series};
use crate::ui::components::{Card, EmptyState, LoadingState};
use crate::ui::errors::use_errors;
use crate::ui::weight::use_unit;

/// The progress screen of `exercise`.
#[component]
pub fn ExerciseProgress(exercise: ExerciseId) -> Element {
    let history = use_history();
    let errors = use_errors();
    let name = history.names.read().exercise(&exercise);
    let content = match *history.charts.read() {
        ChartAccess::Checking => rsx! { LoadingState {} },
        ChartAccess::Locked => rsx! { LockedCharts {} },
        ChartAccess::Unknown => rsx! {
            EmptyState {
                title: "Couldn't load",
                message: "Your plan could not be checked. Check your connection and try again.",
                button {
                    r#type: "button",
                    class: "io-button io-button-secondary",
                    onclick: move |_| history.check_plan(errors),
                    "Try again"
                }
            }
        },
        ChartAccess::Included => rsx! { Charts { exercise: exercise.clone() } },
    };

    rsx! {
        BackToHistory {}
        div { class: "io-page-header",
            span { class: "io-label", "Progress" }
            h1 { class: "io-title", "{name}" }
        }
        {content}
    }
}

/// The card shown instead of the charts when the plan does not include them.
#[component]
pub fn LockedCharts() -> Element {
    rsx! {
        section { class: "io-card io-locked", aria_labelledby: "io-locked-title",
            div { class: "io-locked-head",
                LockIcon {}
                h2 { id: "io-locked-title", "Progress charts" }
                span { class: "io-chip", "Pro" }
            }
            p {
                "Charts of your top set and estimated 1RM over time, for every exercise, are part of Iron Oxide Pro."
            }
            p { class: "io-muted",
                "Your workouts and their sets stay in your history on every plan."
            }
        }
    }
}

#[component]
fn LockIcon() -> Element {
    rsx! {
        svg {
            class: "io-locked-icon",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            rect { x: "5", y: "11", width: "14", height: "10", rx: "2" }
            path { d: "M8 11V7a4 4 0 0 1 8 0v4" }
        }
    }
}

/// Loads the series and shows the charts.
#[component]
fn Charts(exercise: ExerciseId) -> Element {
    let errors = use_errors();
    let history = use_history();
    let mut series = use_resource(use_reactive!(|exercise| async move {
        let result = exercise_series(exercise.as_str().to_owned()).await;
        if let Err(error) = &result {
            let kind = ApiFailure::classify(error).kind;
            if kind == FailureKind::Forbidden {
                // Maybe the plan changed since it was checked: ask again before deciding.
                let refreshed = my_entitlements().await.ok();
                if charts_locked_after(kind, refreshed.as_ref()) {
                    let mut charts = history.charts;
                    charts.set(ChartAccess::Locked);
                    return result;
                }
            }
            errors.report(error);
        }
        result
    }));

    match &*series.read() {
        None => rsx! { LoadingState { message: "Loading your progress…" } },
        Some(Err(_)) => rsx! {
            EmptyState {
                title: "Couldn't load",
                message: "Your progress could not be loaded. Check your connection and try again.",
                button {
                    r#type: "button",
                    class: "io-button io-button-secondary",
                    onclick: move |_| series.restart(),
                    "Try again"
                }
            }
        },
        Some(Ok(loaded)) if loaded.points.is_empty() => rsx! {
            EmptyState {
                title: "No data yet",
                message: "Charts start once a workout with a working set of this exercise, with a weight and reps (not a timed hold), is finished.",
            }
        },
        Some(Ok(loaded)) => rsx! { Loaded { series: loaded.clone() } },
    }
}

/// The two charts and the table of a non-empty series.
#[component]
fn Loaded(series: ExerciseSeries) -> Element {
    let unit = use_unit();
    let lines = chart_series(&series);
    let rows = series_rows(&series, unit);
    let date = |ms: i64| local_date(ms).short();
    let weight_line = |title: &str, points: &[WeightPoint], measure: Measure| {
        (
            chart::layout(points, unit, date),
            latest_number(points, measure, unit),
            chart_summary(title, points, measure, unit, date),
        )
    };
    let (e1rm_layout, e1rm_latest, e1rm_summary) =
        weight_line("Estimated 1RM", &lines.e1rm, Measure::Estimate);
    let (top_layout, top_latest, top_summary) =
        weight_line("Top set", &lines.top_set, Measure::Load);

    rsx! {
        ChartCard {
            id: "e1rm",
            title: "Estimated 1RM",
            layout: e1rm_layout,
            latest: e1rm_latest,
            summary: e1rm_summary,
            unit: unit.symbol(),
            empty: "No estimate yet: estimates need sets of 10 reps or fewer.",
        }
        ChartCard {
            id: "top-set",
            title: "Top set",
            layout: top_layout,
            latest: top_latest,
            summary: top_summary,
            unit: unit.symbol(),
            empty: "No top set yet.",
        }
        ChartCard {
            id: "volume",
            title: "Volume",
            layout: chart::volume_layout(&lines.volume, unit, date),
            latest: latest_volume(&lines.volume, unit),
            summary: volume_summary("Volume", &lines.volume, unit, date),
            unit: unit.symbol(),
            empty: "No volume yet.",
        }
        Card { title: "Sessions",
            table { class: "io-table",
                caption { class: "io-sr-only",
                    "Top set, estimated 1RM and volume per session, newest first"
                }
                thead {
                    tr {
                        th { scope: "col", "Date" }
                        th { scope: "col", "Top set" }
                        th { scope: "col", "e1RM" }
                        th { scope: "col", "Volume" }
                    }
                }
                tbody {
                    for row in rows {
                        tr { key: "{row.session_id}",
                            th { scope: "row", {local_date(row.at_ms).short()} }
                            td { "{row.top_set}" }
                            td { "{row.e1rm}" }
                            td { "{row.volume}" }
                        }
                    }
                }
            }
        }
    }
}

/// One line chart in a card: the latest value big, the chart, and a sentence for screen readers.
#[component]
fn ChartCard(
    id: &'static str,
    title: &'static str,
    layout: Option<ChartLayout>,
    latest: Option<String>,
    summary: String,
    unit: &'static str,
    empty: &'static str,
) -> Element {
    let title_id = format!("io-chart-{id}");

    rsx! {
        section { class: "io-card io-chart-card", aria_labelledby: "{title_id}",
            div { class: "io-chart-head",
                h2 { id: "{title_id}", "{title}" }
                if let Some(latest) = latest {
                    p { class: "io-chart-latest",
                        span { class: "io-sr-only", "Latest: " }
                        span { class: "io-chart-number", "{latest}" }
                        span { class: "io-chart-unit", "{unit}" }
                    }
                }
            }
            match layout {
                None => rsx! { p { class: "io-muted", "{empty}" } },
                Some(layout) => rsx! {
                    svg {
                        class: "io-chart",
                        view_box: "0 0 {VIEW_WIDTH} {VIEW_HEIGHT}",
                        role: "img",
                        "aria-label": "{summary}",
                        g { class: "io-chart-grid", "aria-hidden": "true",
                            for tick in layout.y_ticks.iter() {
                                g { key: "{tick.label}",
                                line {
                                    x1: "{layout.plot.0}",
                                    x2: "{layout.plot.2}",
                                    y1: "{tick.at}",
                                    y2: "{tick.at}",
                                }
                                text {
                                    x: "{layout.plot.0 - 8.0}",
                                    y: "{tick.at}",
                                    text_anchor: "end",
                                    dominant_baseline: "middle",
                                    "{tick.label}"
                                }
                                }
                            }
                            for (index, tick) in layout.x_ticks.iter().enumerate() {
                                text {
                                    key: "x-{index}",
                                    x: "{tick.at}",
                                    y: "{layout.plot.3 + 20.0}",
                                    text_anchor: if index == 0 && layout.x_ticks.len() > 1 { "start" } else if index == 0 { "middle" } else { "end" },
                                    "{tick.label}"
                                }
                            }
                        }
                        g { class: "io-chart-series", "aria-hidden": "true",
                            path { class: "io-chart-line", d: "{layout.path}" }
                            for (index, dot) in layout.dots.iter().enumerate() {
                                circle {
                                    key: "{index}",
                                    class: "io-chart-dot",
                                    cx: "{dot.x}",
                                    cy: "{dot.y}",
                                    r: "3.5",
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}
