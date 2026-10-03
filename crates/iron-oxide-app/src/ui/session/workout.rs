//! The active session screen (#28): one set at a time, with its steppers and Done.
//!
//! A set is saved when Done is tapped. If the save fails, the set is kept exactly as it was sent
//! (same id, values and time) and Done becomes "Retry save" with the steppers locked: resending
//! the identical set is idempotent, whereas a new one could log the same set twice.

use std::rc::Rc;

use dioxus::prelude::*;
use iron_oxide_domain::program::Exercise;
use iron_oxide_domain::progression::{NextTargets, SetGoal};
use iron_oxide_domain::time::Timestamp;
use iron_oxide_domain::timer::{HoldTimer, IntervalPhase, IntervalTimer};
use iron_oxide_domain::{LoggedSet, Reps, SessionOutcome, SetId, Weight};

use super::flow::{self, Entry, Step};
use super::rest::{self, Rest, RestScreen};
use super::summary::Finished;
use super::{Active, forget, note, platform, store_skipped, writes};
use crate::api::error::{ApiFailure, FailureKind};
use crate::auth::browser::sleep;
use crate::ui::components::icons::PlateIcon;
use crate::ui::components::{
    Button, ButtonVariant, Chip, IconButton, ProgressSegments, Sheet, Stepper, WeightStepper,
};
use crate::ui::errors::use_errors;
use crate::ui::plates::PlateCalculatorSheet;
use crate::ui::shell::Route;
use crate::ui::weight::use_unit;

/// A confirmation on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    Skip,
    Finish,
}

/// The session in progress. `on_reload` reloads it from the server (after a conflict);
/// `on_finished` receives the summary of a completed workout.
#[component]
pub fn Workout(
    initial: Active,
    on_reload: EventHandler<()>,
    on_finished: EventHandler<Finished>,
) -> Element {
    let mut active = use_signal(|| initial);
    let errors = use_errors();
    let unit = use_unit();
    let navigator = use_navigator();
    let mut busy = use_signal(|| false);
    // The set being saved, frozen until the server answers for good.
    let mut pending = use_signal(|| None::<LoggedSet<Timestamp>>);
    // The banner that reported the failed save, cleared once a retry succeeds.
    let mut failed_banner = use_signal(|| None::<u64>);
    // The finish being sent, resent unchanged after a failure.
    let mut finishing = use_signal(|| None::<(SessionOutcome, Timestamp)>);
    let mut ask = use_signal(|| None::<Ask>);
    // The lifter's changes to the current set. Tagged with their step, so a new step starts from
    // its own prefill.
    let mut edit = use_signal(|| None::<Edit>);
    // The rest after the last set, resumed after a reload.
    let mut rest = use_signal(|| {
        let state = active.peek();
        rest::restore(state.plan.session.id, &state.sets, platform::now())
    });
    // The screen stays on for the whole workout.
    use_hook(|| Rc::new(platform::ScreenAwake::keep()));
    // Any tap, or the page coming back, resumes a suspended or interrupted audio context.
    use_hook(|| Rc::new(platform::KeepAudioReady::new()));

    let state = active.read();
    let plan = &state.plan;
    let session_id = plan.session.id;
    let started_at = plan.session.started_at;
    let steps = flow::steps(plan, state.settings.bar_weight);
    let current = flow::current_step(&steps, plan, &state.sets, &state.skipped);
    let remaining = flow::remaining(&steps, plan, &state.sets, &state.skipped);

    let mut finish = move |outcome: SessionOutcome| {
        // A set waiting for "Retry save" would be lost: it must be saved first.
        if *busy.peek() || pending.peek().is_some() {
            return;
        }
        let (outcome, at) = match *finishing.peek() {
            Some((sent, at)) if sent == outcome => (sent, at),
            _ => (
                outcome,
                flow::finish_time(platform::now(), started_at, &active.peek().sets),
            ),
        };
        finishing.set(Some((outcome, at)));
        busy.set(true);
        spawn(async move {
            let result = writes::finish_session(session_id, outcome, at).await;
            busy.set(false);
            match result {
                Ok(summary) => {
                    forget(session_id);
                    if outcome == SessionOutcome::Abandoned {
                        note(errors, "Workout discarded.");
                        navigator.push(Route::Home {});
                    } else {
                        let state = active.peek();
                        on_finished.call(Finished {
                            summary,
                            plan: state.plan.clone(),
                            sets: state.sets.clone(),
                        });
                    }
                }
                Err(error) => {
                    let failure = ApiFailure::classify(&error);
                    errors.report(&error);
                    if !failure.kind.is_retryable() {
                        finishing.set(None);
                    }
                    if failure.kind == FailureKind::Conflict {
                        on_reload.call(());
                    }
                }
            }
        });
    };

    let current_exercise =
        current.map(|index| plan.exercises[steps[index].exercise].exercise.id.clone());
    let sheet = ask().map(|asked| {
        let name = current
            .map(|index| plan.exercises[steps[index].exercise].exercise.name.clone())
            .unwrap_or_default();
        let current_exercise = current_exercise.clone();
        let close = move |()| ask.set(None);
        match asked {
            Ask::Skip => rsx! {
                Sheet { title: "Skip {name}?", on_close: close,
                    p { class: "io-muted", "Its remaining sets won't be logged today." }
                    div { class: "io-actions",
                        Button {
                            block: true,
                            onclick: move |_| {
                                ask.set(None);
                                if let Some(exercise) = current_exercise.clone() {
                                    let mut state = active.write();
                                    state.skipped.insert(exercise);
                                    store_skipped(session_id, &state.skipped);
                                }
                            },
                            "Skip exercise"
                        }
                        Button { variant: ButtonVariant::Ghost, block: true, onclick: move |_| ask.set(None), "Cancel" }
                    }
                }
            },
            Ask::Finish => rsx! {
                Sheet { title: "Finish workout?", on_close: close,
                    p { class: "io-muted",
                        if remaining == 0 {
                            "Every set is done."
                        } else {
                            "{flow::plural(u32::try_from(remaining).unwrap_or(u32::MAX), \"set\")} not done yet: they won't be logged."
                        }
                    }
                    div { class: "io-actions",
                        Button {
                            block: true,
                            busy: busy(),
                            onclick: move |_| {
                                ask.set(None);
                                finish(SessionOutcome::Completed);
                            },
                            "Finish workout"
                        }
                        Button {
                            variant: ButtonVariant::Danger,
                            block: true,
                            busy: busy(),
                            onclick: move |_| {
                                ask.set(None);
                                finish(SessionOutcome::Abandoned);
                            },
                            "Discard workout"
                        }
                        Button { variant: ButtonVariant::Ghost, block: true, onclick: move |_| ask.set(None), "Keep training" }
                    }
                }
            },
        }
    });

    let Some(index) = current else {
        return rsx! {
            div { class: "io-session",
                span { class: "io-label", "{plan.day_name.to_uppercase()} · DONE" }
                h1 { class: "io-session-title", "All sets done" }
                p { class: "io-muted", "Finish the workout to save it and see what changes next time." }
                div { class: "io-session-actions",
                    Button {
                        xl: true,
                        block: true,
                        busy: busy(),
                        onclick: move |_| finish(SessionOutcome::Completed),
                        "Finish"
                    }
                }
            }
        };
    };

    if rest.read().is_some() {
        let last = state.sets.last().cloned();
        let (title, logged) = last
            .as_ref()
            .map(|set| flow::rest_header(plan, set))
            .unwrap_or_default();
        let after = last
            .and_then(|set| {
                plan.exercises
                    .iter()
                    .position(|planned| planned.exercise.id == set.exercise)
            })
            .unwrap_or(steps[index].exercise);
        return rsx! {
            RestScreen {
                session: session_id,
                rest,
                title,
                logged,
                up_next: Some(flow::up_next(&steps, index, after, plan, unit)),
                sound: state.settings.sound_enabled,
                vibration: state.settings.vibration_enabled,
            }
            {sheet}
        };
    }

    let step = steps[index];
    let planned = &plan.exercises[step.exercise];
    let exercise = planned.exercise.clone();
    let next = flow::next_step(&steps, index, plan, &state.sets, &state.skipped);
    let frozen = pending
        .read()
        .clone()
        .filter(|set| step.is_logged_by(&exercise.id, set));
    let header = flow::header_label(&plan.day_name, &step);
    let tag = flow::superset_tag(plan, step.exercise);
    let target = flow::target_line(&step, &exercise, unit);
    let next = flow::next_line(&steps, index, next, plan, unit);
    let needs_training_max = matches!(planned.targets, NextTargets::NeedsTrainingMax { .. });
    let prefill = flow::prefill(&steps, &step, &exercise.id, &state.sets);
    let weight_step = state.settings.weight_step(unit);
    let current_edit = edit().filter(|edit| edit.step == step).unwrap_or(Edit {
        step,
        reps: i64::from(prefill.reps.get()),
        weight: prefill.weight,
        started: None,
    });
    let on_done = move |entry: Entry| {
        // Inside the tap, before any await: lets iOS play the rest timer's beeps later.
        platform::unlock_audio();
        if *busy.peek() {
            return;
        }
        let set = pending.peek().clone().unwrap_or_else(|| {
            flow::logged_set(
                SetId::new_v7(),
                &step,
                &exercise_of(&active.peek(), step.exercise),
                entry,
                platform::now(),
                started_at,
            )
        });
        pending.set(Some(set.clone()));
        busy.set(true);
        spawn(async move {
            let result = writes::save_set(session_id, set.clone()).await;
            busy.set(false);
            match result {
                Ok(()) => {
                    pending.set(None);
                    let after = set.id;
                    active.write().sets.push(set);
                    if let Some(length) = rest_length(&active.peek(), step) {
                        let started = Rest::start(after, platform::now(), length);
                        rest::store(session_id, &started);
                        rest.set(Some(started));
                    }
                    if let Some(id) = failed_banner.take() {
                        // The failure it reported is over (unless a newer banner replaced it).
                        errors.dismiss_if(id);
                    }
                }
                Err(error) => {
                    let failure = ApiFailure::classify(&error);
                    errors.report(&error);
                    failed_banner.set(errors.banner().map(|banner| banner.id));
                    match failure.kind {
                        // Refused for good: let the lifter change it and send a new set.
                        FailureKind::Invalid | FailureKind::NotFound | FailureKind::Forbidden => {
                            pending.set(None);
                        }
                        // The session changed elsewhere (ended, or this set id was used).
                        FailureKind::Conflict => {
                            pending.set(None);
                            on_reload.call(());
                        }
                        // It may have been saved: resend the same set.
                        _ => {}
                    }
                }
            }
        });
    };
    rsx! {
        SetCard {
            step,
            exercise,
            header,
            tag,
            target,
            next,
            needs_training_max,
            weight_step,
            frozen,
            edit: current_edit,
            on_edit: move |changed| edit.set(Some(changed)),
            busy: busy(),
            on_done,
            on_skip: move |()| ask.set(Some(Ask::Skip)),
            on_finish: move |()| ask.set(Some(Ask::Finish)),
        }
        {sheet}
    }
}

/// What the lifter set on the current step: the steppers, and when its timer started.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Edit {
    step: Step,
    reps: i64,
    weight: Option<Weight>,
    started: Option<Timestamp>,
}

/// The rest after `done`, just logged, given the sets left (see [`flow::rest_after`]).
fn rest_length(state: &Active, done: Step) -> Option<iron_oxide_domain::Seconds> {
    let steps = flow::steps(&state.plan, state.settings.bar_weight);
    let index = steps.iter().position(|step| *step == done)?;
    let next = flow::current_step(&steps, &state.plan, &state.sets, &state.skipped);
    flow::rest_after(
        &state.plan,
        &steps,
        index,
        next,
        state.settings.default_rest,
    )
}

/// The exercise at `position` in the plan.
fn exercise_of(state: &Active, position: usize) -> Exercise {
    state.plan.exercises[position].exercise.clone()
}

/// One set: the header, the exercise, the steppers (or the timer) and Done.
#[component]
fn SetCard(
    step: Step,
    exercise: Exercise,
    header: String,
    tag: Option<String>,
    target: String,
    next: String,
    needs_training_max: bool,
    weight_step: Weight,
    /// The set whose save failed, shown as it was sent.
    frozen: Option<LoggedSet<Timestamp>>,
    edit: Edit,
    on_edit: EventHandler<Edit>,
    busy: bool,
    on_done: EventHandler<Entry>,
    on_skip: EventHandler<()>,
    on_finish: EventHandler<()>,
) -> Element {
    let mut plates = use_signal(|| false);

    let locked = frozen.is_some();
    let shown_reps = frozen
        .as_ref()
        .map_or(edit.reps, |set| i64::from(set.reps.get()));
    let shown_weight = frozen.as_ref().map_or(edit.weight, |set| set.weight);
    let goal = step.target.goal;
    let timed = !matches!(goal, SetGoal::Reps { .. });
    let done_label = if locked { "Retry save" } else { "Done" };

    let done = move |_| {
        let entry = Entry {
            reps: Reps::new(u16::try_from(edit.reps).unwrap_or(u16::MAX)),
            weight: edit.weight,
            duration: flow::timed_duration(goal, edit.started, platform::now()),
        };
        on_done.call(entry);
    };

    rsx! {
        div { class: "io-session",
            div { class: "io-session-header",
                span { class: "io-label", "{header}" }
                if let Some(value) = shown_weight {
                    IconButton { label: "Plate calculator", onclick: move |_| plates.set(true), PlateIcon {} }
                    if plates() {
                        PlateCalculatorSheet { weight: value, on_close: move |()| plates.set(false) }
                    }
                }
            }
            div { class: "io-session-heading",
                if let Some(tag) = tag {
                    span { class: "io-chip io-session-tag", "Superset {tag}" }
                }
                h1 { class: "io-session-title", "{exercise.name}" }
                p { class: "io-session-target", "{target}" }
            }
            ProgressSegments {
                done: u32::from(step.set_index) + 1,
                total: u32::from(step.of),
                label: if step.warm_up { "Warm-up sets" } else { "Working sets" },
            }
            ExerciseNotes { exercise: exercise.clone(), warm_up: step.warm_up }
            if needs_training_max {
                p { class: "io-notice io-notice-info",
                    "No training max for {exercise.name} yet: log the weight you lift, and set one in Settings."
                }
            }
            div { class: "io-session-inputs",
                if timed {
                    TimedPanel {
                        goal,
                        started: edit.started,
                        locked: locked || busy,
                        on_start: move |()| on_edit.call(Edit { started: Some(platform::now()), ..edit }),
                    }
                } else {
                    Stepper {
                        label: "Reps",
                        value: shown_reps,
                        max: i64::from(u16::MAX),
                        less_label: "One rep less",
                        more_label: "One rep more",
                        disabled: locked,
                        on_change: move |value| on_edit.call(Edit { reps: value, ..edit }),
                    }
                }
                if let Some(value) = shown_weight {
                    WeightStepper {
                        value,
                        step: weight_step,
                        disabled: locked,
                        on_change: move |value| on_edit.call(Edit { weight: Some(value), ..edit }),
                    }
                }
            }
            div { class: "io-session-actions",
                Button { xl: true, block: true, busy, onclick: done, "{done_label}" }
                p { class: "io-session-next", "{next}" }
                div { class: "io-session-more",
                    Button { variant: ButtonVariant::Ghost, disabled: locked || busy, onclick: move |_| on_skip.call(()), "Skip exercise" }
                    Button { variant: ButtonVariant::Ghost, disabled: locked || busy, onclick: move |_| on_finish.call(()), "Finish workout" }
                }
                if locked {
                    p { class: "io-session-next", role: "status",
                        "This set isn't saved yet: retry the save before finishing, or it would be lost."
                    }
                }
            }
        }
    }
}

/// The exercise's tempo, demo link and notes, when the program gives them. Warm-ups show the
/// tempo and demo only.
#[component]
fn ExerciseNotes(exercise: Exercise, warm_up: bool) -> Element {
    let tempo = exercise.tempo.map(|tempo| tempo.to_string());
    let demo = exercise
        .demo_url
        .as_ref()
        .map(|url| url.as_str().to_owned());
    let notes = exercise.notes.clone().filter(|_| !warm_up);
    if tempo.is_none() && demo.is_none() && notes.is_none() {
        return rsx! {};
    }
    rsx! {
        div { class: "io-session-notes",
            if tempo.is_some() || demo.is_some() {
                div { class: "io-chips",
                    if let Some(tempo) = tempo {
                        Chip { "Tempo {tempo}" }
                    }
                    if let Some(href) = demo {
                        a {
                            class: "io-session-demo",
                            href,
                            target: "_blank",
                            rel: "noopener noreferrer",
                            "Watch demo"
                            span { class: "io-sr-only", " (opens in a new tab)" }
                        }
                    }
                }
            }
            if let Some(notes) = notes {
                p { class: "io-session-note", "{notes}" }
            }
        }
    }
}

/// The timer of a hold or of intervals: started by the lifter, derived from its start time.
#[component]
fn TimedPanel(
    goal: SetGoal,
    started: Option<Timestamp>,
    locked: bool,
    on_start: EventHandler<()>,
) -> Element {
    match started {
        None => {
            let (label, time) = match goal {
                SetGoal::Hold { seconds } => ("HOLD".to_owned(), seconds.to_string()),
                SetGoal::Intervals { work, rounds, .. } => {
                    (format!("WORK · ROUND 1 / {rounds}"), work.to_string())
                }
                SetGoal::Reps { .. } => (String::new(), String::new()),
            };
            rsx! {
                div { class: "io-card io-timer",
                    span { class: "io-stepper-label", "{label}" }
                    span { class: "io-timer-number", "{time}" }
                    Button {
                        variant: ButtonVariant::Secondary,
                        block: true,
                        disabled: locked,
                        onclick: move |_| on_start.call(()),
                        "Start timer"
                    }
                }
            }
        }
        Some(at) => rsx! { RunningTimer { goal, started_at: at } },
    }
}

/// A running hold or interval timer. Its display is recomputed from the clock, never counted.
#[component]
fn RunningTimer(goal: SetGoal, started_at: Timestamp) -> Element {
    let mut now = use_signal(platform::now);
    use_future(move || async move {
        loop {
            sleep(250).await;
            now.set(platform::now());
        }
    });
    let now = now();
    let (label, time, over) = match goal {
        SetGoal::Hold { seconds } => {
            let timer = HoldTimer::start(started_at, seconds.as_duration());
            let over = timer.is_finished(now);
            let label = if over { "HOLD DONE" } else { "HOLD" };
            (
                label.to_owned(),
                flow::clock_text(timer.remaining(now)),
                over,
            )
        }
        SetGoal::Intervals { .. } => match flow::interval_plan(goal) {
            Some(plan) => {
                let status = IntervalTimer::start(started_at, plan).status(now);
                let label = match status.phase {
                    IntervalPhase::Work => {
                        format!("WORK · ROUND {} / {}", status.round, plan.rounds())
                    }
                    IntervalPhase::Rest => {
                        format!("REST · ROUND {} / {}", status.round, plan.rounds())
                    }
                    IntervalPhase::Done => "INTERVALS DONE".to_owned(),
                };
                let over = status.phase == IntervalPhase::Done;
                (label, flow::clock_text(status.remaining_in_phase), over)
            }
            None => (String::new(), String::new(), true),
        },
        SetGoal::Reps { .. } => (String::new(), String::new(), true),
    };
    rsx! {
        div { class: "io-card io-timer", "data-over": over,
            span { class: "io-stepper-label", role: "status", "{label}" }
            span { class: "io-timer-number", "{time}" }
            p { class: "io-muted", if over { "Tap Done to log it." } else { "Tap Done when you stop." } }
        }
    }
}
