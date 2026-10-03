//! The rest timer screen (#29): a countdown to an end timestamp, with ±15 s, skip, a preview of
//! the next set, and a beep and a vibration 10 seconds before the end and at the end.
//!
//! The domain [`RestTimer`] stores the instants and what it already announced; the screen only
//! asks it for the time left at `now`. It re-reads the clock every 250 ms and at once when the page
//! becomes visible again, so the countdown is right after a lock screen or a tab switch, and an
//! alert missed meanwhile collapses to the latest one. The timer is kept in `localStorage` so a
//! reload resumes the rest without announcing anything twice.

use std::rc::Rc;

use dioxus::prelude::*;
use iron_oxide_domain::time::Timestamp;
use iron_oxide_domain::timer::{ADJUSTMENT_STEP, RestTimer, TimerAlert};
use iron_oxide_domain::{LoggedSet, Seconds, SessionId, SetId};
use serde::{Deserialize, Serialize};

use super::flow;
use super::platform::{self, Cue, OnVisible};
use crate::auth::browser::sleep;
use crate::ui::components::Button;

/// A rest in progress, after the set `after`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rest {
    pub after: SetId,
    pub timer: RestTimer,
}

impl Rest {
    /// A rest of `length` from `now`, after the set `after`.
    #[must_use]
    pub fn start(after: SetId, now: Timestamp, length: Seconds) -> Self {
        Self {
            after,
            timer: RestTimer::start(now, length.as_duration()),
        }
    }
}

/// The `localStorage` key of a session's rest.
fn key(session: SessionId) -> String {
    format!("io.session.{}.rest", session.as_uuid())
}

/// The rest to resume after a reload: the one stored for `session`, if it follows the last logged
/// set and is not over yet.
#[must_use]
pub fn restore(session: SessionId, sets: &[LoggedSet<Timestamp>], now: Timestamp) -> Option<Rest> {
    let rest: Rest = serde_json::from_str(&platform::load(&key(session))?).ok()?;
    let last = sets.last()?;
    (last.id == rest.after && !rest.timer.is_finished(now)).then_some(rest)
}

/// Keeps `rest` for a reload.
pub fn store(session: SessionId, rest: &Rest) {
    if let Ok(json) = serde_json::to_string(rest) {
        platform::store(&key(session), &json);
    }
}

/// Forgets the session's rest (skipped, over, or the session ended).
pub fn clear(session: SessionId) {
    platform::remove(&key(session));
}

/// The rest screen. `rest` is the workout's rest: this screen counts it down, adjusts it, and
/// clears it when the lifter moves on.
#[component]
pub fn RestScreen(
    session: SessionId,
    rest: Signal<Option<Rest>>,
    title: String,
    logged: String,
    up_next: Option<(String, String)>,
    sound: bool,
    vibration: bool,
) -> Element {
    let mut rest = rest;
    let mut now = use_signal(platform::now);
    let mut alert = use_signal(|| None::<TimerAlert>);

    // Re-reads the clock and raises the alert reached, if any. Never counts ticks.
    let mut tick = move || {
        let at = platform::now();
        now.set(at);
        let stored = *rest.peek();
        let Some(current) = stored else {
            return;
        };
        let mut observed = current;
        if let Some(fired) = observed.timer.observe(at) {
            let cue = match fired {
                TimerAlert::Warning => Cue::Warning,
                TimerAlert::Finished => Cue::Finished,
            };
            platform::announce(cue, sound, vibration);
            alert.set(Some(fired));
        }
        if observed != current {
            store(session, &observed);
            rest.set(Some(observed));
        }
    };
    use_future(move || async move {
        loop {
            tick();
            sleep(250).await;
        }
    });
    use_hook(move || Rc::new(OnVisible::new(tick)));

    let Some(current) = rest() else {
        return rsx! {};
    };
    let at = now();
    let left = current.timer.remaining(at);
    let total = current.timer.total();
    let over = current.timer.is_finished(at);
    let percent = flow::rest_left_percent(left, total);
    let total_text = flow::clock_text(total);
    let mut adjust = move |change: fn(&mut RestTimer, Timestamp)| {
        platform::unlock_audio();
        let current = *rest.peek();
        if let Some(mut adjusted) = current {
            let at = platform::now();
            change(&mut adjusted.timer, at);
            store(session, &adjusted);
            rest.set(Some(adjusted));
            now.set(at);
            alert.set(None);
        }
    };
    let mut done = move || {
        platform::unlock_audio();
        clear(session);
        rest.set(None);
    };
    let message = match alert() {
        _ if over => "Rest over: time for the next set.",
        Some(TimerAlert::Warning) => "10 seconds left.",
        _ => "",
    };

    rsx! {
        div { class: "io-session io-rest", "data-over": over,
            div { class: "io-session-header",
                span { class: "io-label", "{title}" }
                span { class: "io-rest-logged", "{logged}" }
            }
            div { class: "io-rest-clock",
                span { class: "io-rest-number", role: "timer", aria_label: "Rest left", "{flow::clock_text(left)}" }
                div {
                    class: "io-rest-bar",
                    role: "progressbar",
                    aria_label: "Rest left",
                    aria_valuemin: "0",
                    aria_valuemax: "100",
                    aria_valuenow: "{percent}",
                    div { class: "io-rest-fill", style: "width: {percent}%" }
                }
                span { class: "io-muted", "of {total_text} rest" }
                p { class: "io-rest-alert", role: "status", aria_live: "assertive", "{message}" }
            }
            div { class: "io-rest-adjust",
                button {
                    r#type: "button",
                    class: "io-rest-step",
                    aria_label: "15 seconds less",
                    disabled: over,
                    onclick: move |_| adjust(|timer, at| timer.subtract(at, ADJUSTMENT_STEP)),
                    "−15 s"
                }
                button {
                    r#type: "button",
                    class: "io-rest-step",
                    aria_label: "15 seconds more",
                    onclick: move |_| adjust(|timer, at| timer.add(at, ADJUSTMENT_STEP)),
                    "+15 s"
                }
            }
            div { class: "io-card io-rest-next",
                if let Some((label, value)) = up_next {
                    div { class: "io-rest-next-text",
                        span { class: "io-stepper-label", "{label}" }
                        span { class: "io-rest-next-value", "{value}" }
                    }
                }
                Button {
                    onclick: move |_| done(),
                    if over { "Next set" } else { "Skip rest" }
                }
            }
        }
    }
}
