//! The active session screen (#28): one set at a time, with its steppers and Done.
//!
//! Done queues the set in the offline outbox (#107, through `super::writes`) and moves on at once:
//! the outbox delivers it, in order, retrying the same set (same id, values and time). The screen
//! shows which sets are still saving or were refused. Every change (a set, a skip, the values
//! being entered, the rest) is saved in the session record on the device (`super::local`), so a
//! reload, even offline, lands on the same set with the same values.

use std::rc::Rc;

use dioxus::prelude::*;
use iron_oxide_domain::program::Exercise;
use iron_oxide_domain::progression::{NextTargets, SetGoal};
use iron_oxide_domain::time::Timestamp;
use iron_oxide_domain::timer::{HoldTimer, IntervalPhase, IntervalTimer};
use iron_oxide_domain::{Reps, SessionOutcome, SetId, Weight};

use super::flow::{self, Entry, Step};
use super::local::{self, Draft};
use super::rest::{self, Rest, RestScreen};
use super::summary::Finished;
use super::{Active, NOT_SIGNED_IN, note, platform, writes};
use crate::auth::browser::sleep;
use crate::offline::{LocalFinish, use_outbox};
use crate::ui::components::icons::PlateIcon;
use crate::ui::components::{
    Button, ButtonVariant, Chip, IconButton, ProgressSegments, Sheet, Stepper, WeightStepper,
};
use crate::ui::errors::{BannerKind, use_errors};
use crate::ui::plates::PlateCalculatorSheet;
use crate::ui::shell::Route;
use crate::ui::weight::use_unit;

/// A confirmation on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    Skip,
    Finish,
}

/// The session in progress, from the record or the server, with the values being entered and the
/// rest that were saved with it. `on_finished` receives a completed workout, for its summary.
#[component]
pub fn Workout(
    initial: Active,
    initial_draft: Option<Draft>,
    initial_rest: Option<Rest>,
    on_finished: EventHandler<Finished>,
) -> Element {
    let mut active = use_signal(|| initial);
    let errors = use_errors();
    let outbox = use_outbox();
    let unit = use_unit();
    let navigator = use_navigator();
    let mut ask = use_signal(|| None::<Ask>);
    // The values being entered on the current set, saved with the record.
    let mut edit = use_signal(|| initial_draft);
    // The rest after the last set, resumed after a reload.
    let mut rest = use_signal(|| rest::resume(initial_rest, &active.peek().sets, platform::now()));
    // Set once the workout is finished or discarded: the record then belongs to the finish.
    let mut ended = use_signal(|| false);
    // When Done last logged a set: a double tap logs one.
    let mut last_done = use_signal(|| None::<Timestamp>);
    // Whether "not saved on this device" was already said.
    let mut storage_warned = use_signal(|| false);
    // The screen stays on for the whole workout.
    use_hook(|| Rc::new(platform::ScreenAwake::keep()));
    // Any tap, or the page coming back, resumes a suspended or interrupted audio context.
    use_hook(|| Rc::new(platform::KeepAudioReady::new()));

    // Saves the record after every change: the sets, skips, the values being entered, the rest.
    use_effect(move || {
        let record = local::record(&active.read(), edit(), rest());
        if *ended.peek() {
            return;
        }
        let Some(user) = outbox.user() else {
            return;
        };
        if let Err(error) = local::save(user, &record)
            && !*storage_warned.peek()
        {
            storage_warned.set(true);
            errors.show(
                BannerKind::Warning,
                format!("Not saved on this device: {}", error.0),
            );
        }
    });

    let queued = outbox.queued();
    let state = active.read();
    let plan = &state.plan;
    let session_id = plan.session.id;
    let started_at = plan.session.started_at;
    let steps = flow::steps(plan, state.settings.bar_weight);
    let current = flow::current_step(&steps, plan, &state.sets, &state.skipped);
    let remaining = flow::remaining(&steps, plan, &state.sets, &state.skipped);
    let unsaved = flow::unsaved_line(plan.session.id, &state.sets, &queued);

    // Queues the finish (after every set, in order), marks the record, and moves on: the summary
    // waits for the finish to be delivered.
    let mut finish = move |outcome: SessionOutcome| {
        if *ended.peek() {
            return;
        }
        let at = flow::finish_time(platform::now(), started_at, &active.peek().sets);
        if writes::finish_session(outbox, session_id, outcome, at).is_err() {
            errors.show(BannerKind::Error, NOT_SIGNED_IN);
            return;
        }
        ended.set(true);
        if let Some(user) = outbox.user() {
            let mut record = local::record(&active.peek(), None, None);
            record.finished = Some(LocalFinish {
                outcome,
                finished_at: at,
            });
            let _ = local::save(user, &record);
        }
        if outcome == SessionOutcome::Abandoned {
            note(errors, "Workout discarded.");
            navigator.push(Route::Home {});
        } else {
            let state = active.peek();
            on_finished.call(Finished {
                plan: state.plan.clone(),
                sets: state.sets.clone(),
                outcome,
                finished_at: at,
            });
        }
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
                                    active.write().skipped.insert(exercise);
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
                            onclick: move |_| {
                                ask.set(None);
                                finish(SessionOutcome::Completed);
                            },
                            "Finish workout"
                        }
                        Button {
                            variant: ButtonVariant::Danger,
                            block: true,
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
                if let Some(line) = unsaved.clone() {
                    p { class: "io-session-saving", role: "status", "{line}" }
                }
                div { class: "io-session-actions",
                    Button {
                        xl: true,
                        block: true,
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
            .map(|set| flow::rest_header(plan, set, &flow::save_state(set.id, &queued)))
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
    let header = flow::header_label(&plan.day_name, &step);
    let tag = flow::superset_tag(plan, step.exercise);
    let target = flow::target_line(&step, &exercise, unit);
    let next = flow::next_line(&steps, index, next, plan, unit);
    let needs_training_max = matches!(planned.targets, NextTargets::NeedsTrainingMax { .. });
    let prefill = flow::prefill(&steps, &step, &exercise.id, &state.sets);
    let weight_step = state.settings.weight_step(unit);
    let current_edit = edit().filter(|edit| edit.step == step).unwrap_or(Draft {
        step,
        reps: i64::from(prefill.reps.get()),
        weight: prefill.weight,
        started: None,
    });
    let on_done = move |entry: Entry| {
        // Inside the tap: lets iOS play the rest timer's beeps later.
        platform::unlock_audio();
        if *ended.peek() {
            return;
        }
        let now = platform::now();
        let current = {
            let state = active.peek();
            let steps = flow::steps(&state.plan, state.settings.bar_weight);
            flow::current_step(&steps, &state.plan, &state.sets, &state.skipped)
                .map(|index| steps[index])
        };
        if !flow::accepts_done(&step, current.as_ref(), *last_done.peek(), now) {
            return;
        }
        last_done.set(Some(now));
        let set = flow::logged_set(
            SetId::new_v7(),
            &step,
            &exercise_of(&active.peek(), step.exercise),
            entry,
            now,
            started_at,
        );
        if writes::save_set(outbox, session_id, set.clone()).is_err() {
            errors.show(BannerKind::Error, NOT_SIGNED_IN);
            return;
        }
        let after = set.id;
        active.write().sets.push(set);
        edit.set(None);
        let length = rest_length(&active.peek(), step);
        rest.set(length.map(|length| Rest::start(after, platform::now(), length)));
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
            unsaved,
            edit: current_edit,
            on_edit: move |changed| edit.set(Some(changed)),
            on_done,
            on_skip: move |()| ask.set(Some(Ask::Skip)),
            on_finish: move |()| ask.set(Some(Ask::Finish)),
        }
        {sheet}
    }
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
    /// Which logged sets are still saving, or were refused.
    unsaved: Option<String>,
    edit: Draft,
    on_edit: EventHandler<Draft>,
    on_done: EventHandler<Entry>,
    on_skip: EventHandler<()>,
    on_finish: EventHandler<()>,
) -> Element {
    let mut plates = use_signal(|| false);

    let shown_reps = edit.reps;
    let shown_weight = edit.weight;
    let goal = step.target.goal;
    let timed = !matches!(goal, SetGoal::Reps { .. });

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
                        on_start: move |()| on_edit.call(Draft { started: Some(platform::now()), ..edit }),
                    }
                } else {
                    Stepper {
                        label: "Reps",
                        value: shown_reps,
                        max: i64::from(u16::MAX),
                        less_label: "One rep less",
                        more_label: "One rep more",
                        on_change: move |value| on_edit.call(Draft { reps: value, ..edit }),
                    }
                }
                if let Some(value) = shown_weight {
                    WeightStepper {
                        value,
                        step: weight_step,
                        on_change: move |value| on_edit.call(Draft { weight: Some(value), ..edit }),
                    }
                }
            }
            div { class: "io-session-actions",
                Button { xl: true, block: true, onclick: done, "Done" }
                p { class: "io-session-next", "{next}" }
                if let Some(line) = unsaved {
                    p { class: "io-session-saving", role: "status", "{line}" }
                }
                div { class: "io-session-more",
                    Button { variant: ButtonVariant::Ghost, onclick: move |_| on_skip.call(()), "Skip exercise" }
                    Button { variant: ButtonVariant::Ghost, onclick: move |_| on_finish.call(()), "Finish workout" }
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
fn TimedPanel(goal: SetGoal, started: Option<Timestamp>, on_start: EventHandler<()>) -> Element {
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
