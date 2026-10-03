//! The end-of-session summary (#32): duration, volume and sets, the records set, what changes
//! next time, and a preview of the next workout.
//!
//! Everything comes from the server's [`SessionSummary`] (computed by the domain: volume, PRs,
//! progression changes) and the sets logged; nothing is recomputed here.
//!
//! The finish is queued in the offline outbox (#107): the summary waits until it is delivered,
//! then asks for it again with the same arguments (a replay). Until then, or offline, it says the
//! workout is saved on the device and the summary follows.

use dioxus::prelude::*;
use iron_oxide_domain::time::Timestamp;
use iron_oxide_domain::{ExerciseId, LoggedSet, PrKind, Seconds, SessionOutcome, Unit, Volume};

use super::{flow, local, writes};
use crate::api::sessions::{SessionPlan, SessionSummary, get_next_session_plan};
use crate::offline::{WriteKey, use_outbox};
use crate::ui::components::{Button, Card, LoadingState};
use crate::ui::errors::use_errors;
use crate::ui::shell::Route;
use crate::ui::weight::{WEIGHT_DECIMALS, use_unit, weight_text};

/// A workout ended on this device: what the summary needs, and the finish that was queued.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub plan: SessionPlan,
    /// The sets logged, warm-ups included.
    pub sets: Vec<LoggedSet<Timestamp>>,
    pub outcome: SessionOutcome,
    pub finished_at: Timestamp,
}

/// Where the queued finish stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishState {
    /// Delivered: the summary can be asked for.
    Delivered,
    /// Still queued (offline, or waiting its turn; the unsaved indicator says why).
    Queued,
    /// Refused by the server: its message.
    Refused(String),
}

/// The state of the finish of `session`, from the outbox's queued writes. A refused start
/// refuses the whole workout: its finish waits behind it.
#[must_use]
pub fn finish_state(
    session: iron_oxide_domain::SessionId,
    queued: &[(WriteKey, Option<String>)],
) -> FinishState {
    if let Some(message) = flow::start_refused(session, queued) {
        return FinishState::Refused(message);
    }
    match queued
        .iter()
        .find(|(key, _)| *key == WriteKey::FinishSession(session))
    {
        None => FinishState::Delivered,
        Some((_, Some(message))) => FinishState::Refused(message.clone()),
        Some((_, None)) => FinishState::Queued,
    }
}

/// The summary of a workout just ended: waits for its finish to be delivered, then shows it.
#[component]
pub fn SummaryScreen(finished: Finished) -> Element {
    let outbox = use_outbox();
    let errors = use_errors();
    let navigator = use_navigator();
    let session_id = finished.plan.session.id;
    let (outcome, finished_at) = (finished.outcome, finished.finished_at);
    let state = finish_state(session_id, &outbox.queued());
    let delivered = state == FinishState::Delivered && outbox.user().is_some();
    let mut summary = use_resource(move || async move {
        // Reactive: runs again when the outbox delivers the finish.
        let ready =
            outbox.user().is_some() && !outbox.is_pending(WriteKey::FinishSession(session_id));
        if !ready || outcome == SessionOutcome::Abandoned {
            return None;
        }
        let replay = writes::finished_summary(session_id, outcome, finished_at).await;
        match &replay {
            Ok(_) => {
                // Delivered and summed up: the record on the device has done its job.
                if let Some(user) = outbox.user()
                    && local::load(user).is_some_and(|record| record.session_id == session_id)
                {
                    local::clear(user);
                }
            }
            Err(error) => errors.report(error),
        }
        Some(replay)
    });

    let home = move |_| {
        navigator.push(Route::Home {});
    };
    if outcome == SessionOutcome::Abandoned {
        let message = if delivered {
            "The workout was discarded."
        } else {
            "The workout is discarded on this device; the server hears of it once you're back online."
        };
        return rsx! {
            div { class: "io-session io-summary",
                h1 { class: "io-session-title", "Discarded" }
                p { class: "io-muted", "{message}" }
                Button { xl: true, block: true, onclick: home, "Back to Home" }
            }
        };
    }
    let waiting = |title: &str, message: String| {
        rsx! {
            div { class: "io-session io-summary",
                div { class: "io-session-heading",
                    span { class: "io-label", "{finished.plan.day_name.to_uppercase()} · DONE" }
                    h1 { class: "io-session-title", "{title}" }
                }
                p { class: "io-summary-waiting", role: "status", "{message}" }
                Button { xl: true, block: true, onclick: home, "Back to Home" }
            }
        }
    };
    match state {
        FinishState::Refused(message) => {
            return waiting(
                "Not saved",
                format!(
                    "The server refused this workout: {message} Retry or discard it from the banner at the top."
                ),
            );
        }
        FinishState::Queued => {
            return waiting(
                "Workout done",
                "Saved on this device: the summary appears once it reaches the server.".to_owned(),
            );
        }
        FinishState::Delivered => {}
    }
    match summary.read().clone() {
        Some(Some(Ok(loaded))) => rsx! {
            SummaryView { summary: loaded, plan: finished.plan, sets: finished.sets }
        },
        Some(Some(Err(_))) => rsx! {
            div { class: "io-session io-summary",
                h1 { class: "io-session-title", "Workout done" }
                p { class: "io-muted", "It is saved, but its summary could not be loaded." }
                Button { onclick: move |_| summary.restart(), "Try again" }
                Button { xl: true, block: true, onclick: home, "Back to Home" }
            }
        },
        _ => rsx! { LoadingState { message: "Loading the summary…" } },
    }
}

/// How long the workout lasted: `52:10`, `1:02:05`.
#[must_use]
pub fn duration_text(started_at: Timestamp, finished_at: Option<Timestamp>) -> String {
    let millis = finished_at.map_or(0, |end| {
        u64::try_from(end.epoch_millis().saturating_sub(started_at.epoch_millis())).unwrap_or(0)
    });
    Seconds::new(u32::try_from(millis / 1_000).unwrap_or(u32::MAX)).to_string()
}

/// A volume as a number in `unit`, formatted like every weight of the app (up to two decimals,
/// no trailing zeros): `1937.5`, `3500`.
#[must_use]
pub fn volume_number(volume: Volume, unit: Unit) -> String {
    volume.format_value(unit, WEIGHT_DECIMALS)
}

/// The size of a stat's number, in px: smaller for long values so they fit a third of the screen.
#[must_use]
pub fn stat_size(text: &str) -> u32 {
    match text.chars().count() {
        0..=5 => 32,
        6 | 7 => 26,
        _ => 22,
    }
}

/// The working sets logged (warm-ups left out, as in the volume).
#[must_use]
pub fn working_sets(sets: &[LoggedSet<Timestamp>]) -> usize {
    sets.iter().filter(|set| !set.warm_up).count()
}

/// An exercise's name in the plan, or its id when the plan does not have it.
#[must_use]
pub fn exercise_name(plan: &SessionPlan, id: &ExerciseId) -> String {
    plan.exercises
        .iter()
        .find(|planned| planned.exercise.id == *id)
        .map_or_else(|| id.to_string(), |planned| planned.exercise.name.clone())
}

/// One record, in words: `Heaviest: 102.5 kg × 5 (was 100 kg)`.
#[must_use]
pub fn record_text(kind: &PrKind, unit: Unit) -> String {
    let weight = |weight| weight_text(weight, unit);
    match *kind {
        PrKind::HeaviestWeight { lift, previous } => format!(
            "Heaviest: {} × {} (was {})",
            weight(lift.weight),
            lift.reps.get(),
            weight(previous)
        ),
        PrKind::BestE1rm {
            e1rm,
            lift,
            previous,
        } => format!(
            "Estimated 1RM: {} from {} × {} (was {})",
            weight(e1rm),
            weight(lift.weight),
            lift.reps.get(),
            weight(previous)
        ),
        PrKind::RepsAtWeight { lift, previous } => format!(
            "{} at {} (was {})",
            flow::plural(u32::from(lift.reps.get()), "rep"),
            weight(lift.weight),
            previous.get()
        ),
    }
}

/// The summary screen, with the way back to Home.
#[component]
fn SummaryView(
    summary: SessionSummary,
    plan: SessionPlan,
    sets: Vec<LoggedSet<Timestamp>>,
) -> Element {
    let unit = use_unit();
    let errors = use_errors();
    let navigator = use_navigator();
    let next = use_resource(move || async move {
        let next = get_next_session_plan().await;
        if let Err(error) = &next {
            errors.report(error);
        }
        next.ok()
    });

    let duration = duration_text(summary.session.started_at, summary.session.finished_at);
    // The number alone fits a third of the screen; the unit goes in the label.
    let volume = volume_number(summary.volume, unit);
    let volume_size = stat_size(&volume);
    let volume_label = format!("{} volume", unit.symbol());
    let set_count = working_sets(&sets);
    let records: Vec<(String, String)> = summary
        .prs
        .iter()
        .map(|pr| {
            (
                exercise_name(&plan, &pr.exercise),
                record_text(&pr.kind, unit),
            )
        })
        .collect();
    let changes: Vec<String> = summary
        .changes
        .iter()
        .map(|change| change.display_in(unit).to_string())
        .collect();
    let missing: Vec<String> = summary
        .needs_training_max
        .iter()
        .map(|id| exercise_name(&plan, id))
        .collect();
    let next_plan = next.read().clone().flatten();
    let preview: Option<(String, Vec<(String, String)>)> = next_plan.map(|next| {
        let lines = next
            .exercises
            .iter()
            .map(|planned| {
                (
                    planned.exercise.name.clone(),
                    flow::exercise_summary(planned, iron_oxide_domain::Weight::ZERO, unit),
                )
            })
            .collect();
        (next.day_name, lines)
    });

    rsx! {
        div { class: "io-session io-summary",
            div { class: "io-session-heading",
                span { class: "io-label", "{plan.day_name.to_uppercase()} · DONE" }
                h1 { class: "io-session-title", "Workout done" }
            }
            dl { class: "io-summary-stats",
                div { class: "io-summary-stat",
                    dt { "Duration" }
                    dd { "{duration}" }
                }
                div { class: "io-summary-stat",
                    dt { "{volume_label}" }
                    dd { style: "font-size: {volume_size}px", "{volume}" }
                }
                div { class: "io-summary-stat",
                    dt { "Sets" }
                    dd { "{set_count}" }
                }
            }
            Card { title: "Records",
                if records.is_empty() {
                    p { class: "io-muted", "No new records this time." }
                } else {
                    ul { class: "io-list io-session-plan",
                        for (index, (name, text)) in records.into_iter().enumerate() {
                            li { key: "{index}", class: "io-row",
                                div { class: "io-row-main",
                                    span { class: "io-row-title", "{name}" }
                                    span { class: "io-row-meta io-muted", "{text}" }
                                }
                                span { class: "io-badge", "PR" }
                            }
                        }
                    }
                }
            }
            if !changes.is_empty() || !missing.is_empty() {
                Card { title: "Next time",
                    ul { class: "io-list io-session-plan",
                        for (index, change) in changes.into_iter().enumerate() {
                            li { key: "c{index}", class: "io-row", span { "{change}" } }
                        }
                        for (index, name) in missing.into_iter().enumerate() {
                            li { key: "m{index}", class: "io-row",
                                span { "{name}: set a training max in Settings." }
                            }
                        }
                    }
                }
            }
            if let Some((day, lines)) = preview {
                Card { title: "Next workout: {day}",
                    ul { class: "io-list io-session-plan",
                        for (index, (name, line)) in lines.into_iter().enumerate() {
                            li { key: "{index}", class: "io-row",
                                div { class: "io-row-main",
                                    span { class: "io-row-title", "{name}" }
                                    span { class: "io-row-meta io-muted", "{line}" }
                                }
                            }
                        }
                    }
                }
            }
            Button {
                xl: true,
                block: true,
                onclick: move |_| {
                    navigator.push(Route::Home {});
                },
                "Back to Home"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iron_oxide_domain::{Lift, Reps, SetId, Weight};
    use uuid::Uuid;

    fn kg(value: f64) -> Weight {
        Weight::from_kg(value).unwrap()
    }

    fn lift(weight: f64, reps: u16) -> Lift {
        Lift {
            weight: kg(weight),
            reps: Reps::new(reps),
        }
    }

    #[test]
    fn durations_read_like_a_clock() {
        let start = Timestamp::from_epoch_millis(1_000_000);
        let at = |ms: i64| Some(Timestamp::from_epoch_millis(1_000_000 + ms));
        assert_eq!(duration_text(start, at(3_130_900)), "52:10");
        assert_eq!(duration_text(start, at(3_725_000)), "1:02:05");
        assert_eq!(duration_text(start, None), "0:00");
        assert_eq!(duration_text(start, at(-5_000)), "0:00");
    }

    /// Review of #102: the volume keeps its decimals, like every weight.
    #[test]
    fn volumes_are_formatted_like_weights() {
        let volume = Volume::of(kg(193.75), Reps::new(10));
        assert_eq!(volume_number(volume, Unit::Kg), "1937.5");
        assert_eq!(
            volume_number(Volume::of(kg(100.0), Reps::new(35)), Unit::Kg),
            "3500"
        );
        assert_eq!(volume_number(Volume::ZERO, Unit::Kg), "0");
        assert_eq!(stat_size("1937.5"), 26);
        assert_eq!(stat_size("3500"), 32);
        assert_eq!(stat_size("12345.75"), 22);
    }

    #[test]
    fn the_summary_waits_for_its_finish() {
        let id = iron_oxide_domain::SessionId::from_uuid(Uuid::from_u128(1));
        let other = iron_oxide_domain::SessionId::from_uuid(Uuid::from_u128(2));
        assert_eq!(finish_state(id, &[]), FinishState::Delivered);
        let queued = vec![
            (
                WriteKey::SaveSet(SetId::from_uuid(Uuid::from_u128(9))),
                None,
            ),
            (WriteKey::FinishSession(id), None),
        ];
        assert_eq!(finish_state(id, &queued), FinishState::Queued);
        assert_eq!(finish_state(other, &queued), FinishState::Delivered);
        let start = vec![
            (WriteKey::StartSession(id), Some("In progress.".to_owned())),
            (WriteKey::FinishSession(id), None),
        ];
        assert_eq!(
            finish_state(id, &start),
            FinishState::Refused("In progress.".to_owned())
        );
        let refused = vec![(WriteKey::FinishSession(id), Some("Ended.".to_owned()))];
        assert_eq!(
            finish_state(id, &refused),
            FinishState::Refused("Ended.".to_owned())
        );
    }

    #[test]
    fn only_working_sets_count() {
        let set = |warm_up| LoggedSet {
            id: SetId::from_uuid(Uuid::from_u128(1)),
            exercise: ExerciseId::new("squat").unwrap(),
            set_index: 0,
            reps: Reps::new(5),
            weight: Some(kg(100.0)),
            duration: None,
            warm_up,
            completed_at: Timestamp::EPOCH,
            target: None,
        };
        assert_eq!(working_sets(&[set(true), set(false), set(false)]), 2);
    }

    #[test]
    fn records_say_what_was_beaten() {
        assert_eq!(
            record_text(
                &PrKind::HeaviestWeight {
                    lift: lift(102.5, 5),
                    previous: kg(100.0)
                },
                Unit::Kg
            ),
            "Heaviest: 102.5 kg × 5 (was 100 kg)"
        );
        assert_eq!(
            record_text(
                &PrKind::BestE1rm {
                    e1rm: kg(119.58),
                    lift: lift(102.5, 5),
                    previous: kg(116.67)
                },
                Unit::Kg
            ),
            "Estimated 1RM: 119.58 kg from 102.5 kg × 5 (was 116.67 kg)"
        );
        assert_eq!(
            record_text(
                &PrKind::RepsAtWeight {
                    lift: lift(100.0, 8),
                    previous: Reps::new(6)
                },
                Unit::Kg
            ),
            "8 reps at 100 kg (was 6)"
        );
    }
}
