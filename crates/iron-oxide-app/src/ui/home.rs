//! The home screen (#27): the active program, the next day of its rotation with a preview of its
//! exercises, a big Start (or Resume) button, and the last session.
//!
//! The next day comes from the server (`get_next_session_plan`, which applies the domain's
//! rotation). Starting a session queues it in the offline outbox and opens it at once
//! ([`super::session::local::start`]). The session kept on the device decides Resume: it is
//! offered while its start is still queued or the server cannot be reached, and not once the
//! session ended on this device (even before its finish is delivered).

use dioxus::prelude::*;
use iron_oxide_domain::program::{Day, Program};
use iron_oxide_domain::progression::{NextTargets, SetGoal};
use iron_oxide_domain::{
    DayId, ProgramId, ProgramVersionId, Session, SessionStatus, Unit, Weight, next_day,
    time::Timestamp,
};

use super::components::{Button, Card, EmptyState, LoadingState};
use super::errors::{BannerKind, Errors, use_errors};
use super::session::local::{self, Restore};
use super::session::{NOT_SIGNED_IN, platform};
use super::shell::Route;
use super::weight::{use_unit, weight_number, weight_text};
use crate::api::history::{SessionSummary, history_page};
use crate::api::programs::{get_active_program, get_program};
use crate::api::sessions::{
    NextSessionPlan, PlannedExercise, SessionPlan, get_in_progress_session, get_next_session_plan,
    get_session_plan,
};
use crate::api::settings::Settings;
use crate::offline::{LocalSession, Outbox, WriteKey, use_outbox};
use crate::ui::user_settings::use_user_settings;

/// What the home screen shows once loaded.
#[derive(Debug, Clone, PartialEq)]
enum HomeData {
    /// No active program: point to Programs.
    NoProgram,
    Ready(Box<Today>),
    /// A workout ended on this device and its finish has not reached the server yet: no Start
    /// until it has (the server would offer that same day again).
    Finishing(Finishing),
}

/// The workout waiting for its finish to be delivered, and the day that follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finishing {
    /// The finished workout's day.
    pub day_name: String,
    /// The next day by the program's rotation, when the program is known (online).
    pub next_day: Option<String>,
    /// The server refused one of its writes (the start, a set or the finish): its message. It
    /// then waits for the lifter (Retry or Discard in the unsaved indicator), not for the network.
    pub refused: Option<String>,
}

/// The title and text of Home while a finished workout waits: on the network, or, once the server
/// refused one of its writes, on the lifter.
#[must_use]
pub fn finishing_text(finishing: &Finishing) -> (String, String) {
    let day = &finishing.day_name;
    match &finishing.refused {
        Some(reason) => (
            "Last workout not saved".to_owned(),
            format!(
                "The server refused {day}: {reason} Use Retry or Discard in the unsaved changes \
                 at the top. You can start the next workout once it's settled."
            ),
        ),
        None => (
            "Finishing your last workout…".to_owned(),
            format!(
                "{day} is saved on this device and goes to the server as soon as it can. You can \
                 start the next workout once it's there."
            ),
        ),
    }
}

/// Home for a workout ended on this device whose finish is still queued (or refused). The next
/// day comes from the domain's rotation applied to that workout, when `program` (the active one)
/// is the workout's program.
#[must_use]
pub fn finishing(
    record: &LocalSession,
    program: Option<(ProgramId, &Program)>,
    queued: &[(WriteKey, Option<String>)],
) -> Finishing {
    let ours = |key: &WriteKey| match key {
        WriteKey::StartSession(id) | WriteKey::FinishSession(id) => *id == record.session_id,
        WriteKey::SaveSet(id) => record.sets.iter().any(|set| set.id == *id),
    };
    let refused = queued
        .iter()
        .filter(|(key, _)| ours(key))
        .find_map(|(_, refusal)| refusal.clone());
    let day_name = local::screen_of(record)
        .map_or_else(|| record.day.to_string(), |screen| screen.plan.day_name);
    let next_day = program
        .filter(|(id, _)| *id == record.program_id)
        .and_then(|(_, program)| {
            let finish = record.finished?;
            let ended = Session::from_parts(
                record.session_id,
                record.program_version_id,
                record.day.clone(),
                record.started_at,
                finish.outcome.into(),
                Some(finish.finished_at),
            )
            .ok()?;
            let day = next_day(&program.rotation, &[ended]).ok()?;
            program.day(day).map(|day| day.name.clone())
        });
    Finishing {
        day_name,
        next_day,
        refused,
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Today {
    shown: Shown,
    last: LastSession,
    /// The next day, to start; `None` when only the session kept on the device is known.
    next: Option<NextSessionPlan>,
}

/// The session in progress, read from its own program version (never from the active program,
/// which may have changed since it started).
#[derive(Debug, Clone, PartialEq)]
pub struct Running {
    /// The name of the session's program.
    pub program_name: String,
    /// The session's plan: its day's name and exercises, from the session's version.
    pub plan: SessionPlan,
}

/// The header, the preview and the button of the home screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub program_name: String,
    pub day_name: String,
    pub exercises: Vec<PlannedExercise>,
    /// Resume instead of Start.
    pub in_progress: bool,
}

/// The home screen for a session in progress.
#[must_use]
pub fn shown_running(running: &Running) -> Shown {
    Shown {
        program_name: running.program_name.clone(),
        day_name: running.plan.day_name.clone(),
        exercises: running.plan.exercises.clone(),
        in_progress: true,
    }
}

/// What the home screen shows: the session in progress if there is one (its own program, version
/// and day), else the next day of the active program.
#[must_use]
pub fn shown(active_name: &str, next: &NextSessionPlan, running: Option<&Running>) -> Shown {
    match running {
        Some(running) => shown_running(running),
        None => Shown {
            program_name: active_name.to_owned(),
            day_name: next.day_name.clone(),
            exercises: next.exercises.clone(),
            in_progress: false,
        },
    }
}

/// The last-session line's data. A failed read only degrades this line.
#[derive(Debug, Clone, PartialEq)]
pub enum LastSession {
    /// No session has ended yet.
    None,
    Loaded {
        /// Boxed: a summary is much larger than the other variants.
        summary: Box<SessionSummary>,
        /// The day's name in the session's own version, when known.
        day_name: Option<String>,
        /// The session's program, when it is not the active one.
        other_program: Option<String>,
    },
    /// The history could not be read (the error is reported).
    Failed,
}

/// The name of a session's day, from the active program only when the session ran that exact
/// version (day ids such as `a` repeat across programs and versions).
#[must_use]
pub fn day_name_in_version(
    version: ProgramVersionId,
    days: &[Day],
    session_version: ProgramVersionId,
    day: &DayId,
) -> Option<String> {
    if version != session_version {
        return None;
    }
    days.iter()
        .find(|candidate| &candidate.id == day)
        .map(|candidate| candidate.name.clone())
}

/// The session in progress kept on the device, as Home shows it.
fn running_of(record: &LocalSession, fallback_name: &str) -> Option<Running> {
    let (active, _, _) = local::active_of(record)?;
    Some(Running {
        program_name: active
            .program_name
            .unwrap_or_else(|| fallback_name.to_owned()),
        plan: active.plan,
    })
}

/// Loads the home screen. The session kept on the device wins while its start is queued, and
/// keeps Resume available when the server cannot be reached.
async fn load(errors: Errors, outbox: Outbox) -> Result<HomeData, ServerFnError> {
    let user = outbox.user();
    let record = user.and_then(local::load);
    let queued = outbox.queued();
    let kept = local::reconcile(record.clone(), None, &queued);
    match (load_server(errors, outbox, record, &queued).await, kept) {
        // Offline (or the server failing): the session on the device can still be resumed...
        (Err(error), Restore::Resume(kept)) => match running_of(&kept, "Workout") {
            Some(kept) => Ok(HomeData::Ready(Box::new(Today {
                shown: shown_running(&kept),
                last: LastSession::Failed,
                next: None,
            }))),
            None => Err(error),
        },
        // ...and a finished one still waits for its finish.
        (Err(_), Restore::Ended(ended)) => {
            Ok(HomeData::Finishing(finishing(&ended, None, &queued)))
        }
        (loaded, _) => loaded,
    }
}

async fn load_server(
    errors: Errors,
    outbox: Outbox,
    record: Option<LocalSession>,
    queued: &[(WriteKey, Option<String>)],
) -> Result<HomeData, ServerFnError> {
    // Without an active program the next plan is a 409: ask for it only with one.
    let Some(active) = get_active_program().await? else {
        return Ok(HomeData::NoProgram);
    };
    let server = get_in_progress_session().await?;
    let running = match local::reconcile(record, Some(server.as_ref()), queued) {
        Restore::Resume(record) => running_of(&record, &active.program.name),
        // Ended on this device: no Resume, and no Start until the finish reaches the server.
        Restore::Ended(ended) => {
            let program = Some((active.program.id, &active.document));
            return Ok(HomeData::Finishing(finishing(&ended, program, queued)));
        }
        decision => {
            if decision == Restore::Drop
                && let Some(user) = outbox.user()
            {
                local::clear(user);
            }
            match server {
                Some(session) => {
                    let session = session.session;
                    let plan = get_session_plan(session.id).await?;
                    let program_name = if session.program_id == active.program.id {
                        active.program.name.clone()
                    } else {
                        get_program(session.program_id).await?.program.name
                    };
                    Some(Running { program_name, plan })
                }
                None => None,
            }
        }
    };
    let next = get_next_session_plan().await?;
    let last = match history_page(None, Some(1)).await {
        Ok(page) => match page.sessions.into_iter().next() {
            None => LastSession::None,
            Some(summary) => {
                let day_name = match day_name_in_version(
                    active.version.id,
                    &active.document.days,
                    summary.program_version_id,
                    &summary.day_id,
                ) {
                    Some(name) => Some(name),
                    // Another version: its own plan has the day's name. Without it, the id.
                    None => get_session_plan(summary.id)
                        .await
                        .ok()
                        .map(|plan| plan.day_name),
                };
                let other_program =
                    (summary.program_id != active.program.id).then(|| summary.program_name.clone());
                LastSession::Loaded {
                    summary: Box::new(summary),
                    day_name,
                    other_program,
                }
            }
        },
        Err(error) => {
            errors.report(&error);
            LastSession::Failed
        }
    };
    Ok(HomeData::Ready(Box::new(Today {
        shown: shown(&active.program.name, &next, running.as_ref()),
        last,
        next: Some(next),
    })))
}

/// One exercise of the preview: `"5 × 5 · 100 kg"`, `"3 × 45 s"`, `"Training max needed"`.
/// Empty when there is nothing to say (no working sets).
#[must_use]
pub fn exercise_summary(targets: &NextTargets, unit: Unit) -> String {
    let Some(targets) = targets.ready() else {
        return "Training max needed".to_owned();
    };
    let sets = &targets.working;
    let Some(first) = sets.first() else {
        return String::new();
    };
    let count = sets.len();
    let same_goal = sets.iter().all(|set| set.goal == first.goal);
    let work = match first.goal {
        _ if !same_goal => format!("{count} sets"),
        SetGoal::Reps { reps, .. } => format!("{count} × {reps}"),
        SetGoal::Hold { seconds } => format!("{count} × {} s", seconds.get()),
        SetGoal::Intervals { work, rest, rounds } if count == 1 => {
            format!("{rounds} × {}/{} s", work.get(), rest.get())
        }
        SetGoal::Intervals { work, rest, rounds } => {
            format!("{count} × {rounds} × {}/{} s", work.get(), rest.get())
        }
    };
    let weights: Vec<Weight> = sets.iter().filter_map(|set| set.weight).collect();
    let load = match (weights.iter().min(), weights.iter().max()) {
        (Some(&low), Some(&high)) if low == high => Some(weight_text(high, unit)),
        (Some(&low), Some(&high)) => Some(format!(
            "{}–{}",
            weight_number(low, unit),
            weight_text(high, unit)
        )),
        _ => None,
    };
    match load {
        Some(load) => format!("{work} · {load}"),
        None => work,
    }
}

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The local day number of `at` (days since 1970-01-01), `offset_minutes` east of UTC.
const fn local_day(at: Timestamp, offset_minutes: i32) -> i64 {
    (at.epoch_millis() + offset_minutes as i64 * 60_000).div_euclid(DAY_MS)
}

/// The (year, month 1-12, day 1-31) of a day number (Howard Hinnant's `civil_from_days`).
const fn civil_from_days(days: i64) -> (i64, usize, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "month is 1 to 12"
    )]
    (year, month as usize, day)
}

/// When `then` was, seen from `now`, in the user's local time: `"today"`, `"yesterday"`,
/// `"3 days ago"` within a week, else the date: `"12 Sep"`, with the year if it is not this one.
/// `offset_minutes` gives the time zone's offset at a moment (it changes with daylight saving).
#[must_use]
pub fn relative_day(
    then: Timestamp,
    now: Timestamp,
    offset_minutes: impl Fn(Timestamp) -> i32,
) -> String {
    let (then_day, today) = (
        local_day(then, offset_minutes(then)),
        local_day(now, offset_minutes(now)),
    );
    match today - then_day {
        0 => "today".to_owned(),
        1 => "yesterday".to_owned(),
        days @ 2..=6 => format!("{days} days ago"),
        _ => {
            let (year, month, day) = civil_from_days(then_day);
            let (this_year, _, _) = civil_from_days(today);
            let month = MONTHS[month.saturating_sub(1).min(11)];
            if year == this_year {
                format!("{day} {month}")
            } else {
                format!("{day} {month} {year}")
            }
        }
    }
}

/// The last-session line: `"Last session: yesterday · Day A · 15 sets"`, with the program's
/// name when it is not the active one. `None` when there is nothing to say.
#[must_use]
pub fn last_session_text(
    last: &LastSession,
    now: Timestamp,
    offset_minutes: impl Fn(Timestamp) -> i32,
) -> Option<String> {
    let (summary, day_name, other_program) = match last {
        LastSession::None => return None,
        LastSession::Failed => return Some("The last session could not be loaded.".to_owned()),
        LastSession::Loaded {
            summary,
            day_name,
            other_program,
        } => (summary, day_name, other_program),
    };
    let when = relative_day(
        summary.finished_at.unwrap_or(summary.started_at),
        now,
        offset_minutes,
    );
    let day = day_name
        .clone()
        .unwrap_or_else(|| summary.day_id.to_string());
    let what = match summary.status {
        SessionStatus::Skipped => "skipped".to_owned(),
        SessionStatus::Abandoned => "abandoned".to_owned(),
        SessionStatus::Completed | SessionStatus::InProgress => match summary.working_sets {
            1 => "1 set".to_owned(),
            sets => format!("{sets} sets"),
        },
    };
    Some(match other_program {
        Some(program) => format!("Last session: {when} · {program} · {day} · {what}"),
        None => format!("Last session: {when} · {day} · {what}"),
    })
}

/// The user's offset from UTC at `at`, in minutes, east positive (the browser's time zone, with
/// its daylight saving rules).
fn local_offset_minutes(at: Timestamp) -> i32 {
    #[cfg(feature = "web")]
    {
        #[allow(
            clippy::cast_precision_loss,
            reason = "epoch milliseconds stay far below 2^53"
        )]
        let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(at.epoch_millis() as f64));
        // getTimezoneOffset() is UTC minus local time, in whole minutes.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a time zone offset is at most a few hundred minutes"
        )]
        let west = date.get_timezone_offset() as i32;
        -west
    }
    #[cfg(not(feature = "web"))]
    {
        let _ = at;
        0
    }
}

#[component]
pub fn Home() -> Element {
    let errors = use_errors();
    let outbox = use_outbox();
    let mut data = use_resource(move || async move {
        let loaded = load(errors, outbox).await;
        if let Err(error) = &loaded {
            errors.report(error);
        }
        loaded
    });

    let state = data.read().clone();
    match state {
        None => rsx! {
            LoadingState { message: "Loading your program…" }
        },
        // The error is in the banner; this offers a way to try again.
        Some(Err(_)) => rsx! {
            EmptyState { title: "Not loaded", message: "Your program could not be loaded.",
                Button { onclick: move |_| data.restart(), "Try again" }
            }
        },
        Some(Ok(HomeData::NoProgram)) => rsx! {
            EmptyState {
                title: "No program yet",
                message: "Choose a program to train with. Your next workout will show up here.",
                Link { class: "io-button io-button-primary", to: Route::Programs {}, "Choose a program" }
            }
        },
        Some(Ok(HomeData::Finishing(finishing))) => {
            let next = finishing
                .next_day
                .as_ref()
                .map(|day| format!(" {day} comes next."))
                .unwrap_or_default();
            let (title, message) = finishing_text(&finishing);
            rsx! {
                EmptyState { title, message: "{message}{next}",
                    Button { onclick: move |_| data.restart(), "Check again" }
                }
            }
        }
        Some(Ok(HomeData::Ready(today))) => rsx! {
            TodayView { today: *today }
        },
    }
}

/// The loaded home screen.
#[component]
fn TodayView(today: Today) -> Element {
    let unit = use_unit();
    let errors = use_errors();
    let navigator = use_navigator();
    let outbox = use_outbox();
    let settings = use_user_settings();
    let Shown {
        program_name,
        day_name,
        exercises,
        in_progress,
    } = today.shown.clone();
    let status = if in_progress {
        "In progress"
    } else {
        "Next up"
    };
    let last = last_session_text(&today.last, platform::now(), local_offset_minutes);

    let next = today.next.clone();
    let name = program_name.clone();
    let start = move |_| {
        // Inside the tap: lets iOS play the rest timer's beeps later.
        platform::unlock_audio();
        if in_progress {
            navigator.push(Route::Workout {});
            return;
        }
        let Some(next) = next.as_ref() else {
            return;
        };
        // The settings as loaded (or the defaults until they are): the workout keeps a copy.
        let snapshot = settings.peek().unwrap_or_else(Settings::defaults);
        match local::start(outbox, next, snapshot, Some(name.clone())) {
            Ok(_) => {
                navigator.push(Route::Workout {});
            }
            Err(_) => {
                errors.show(BannerKind::Error, NOT_SIGNED_IN);
            }
        }
    };

    rsx! {
        div { class: "io-page-header",
            span { class: "io-label", "{program_name}" }
            h1 { class: "io-title io-home-day", "{day_name}" }
            p { class: "io-muted", "{status}" }
        }
        if !exercises.is_empty() {
            Card {
                ul { class: "io-list io-home-exercises", aria_label: "Exercises",
                    for planned in exercises {
                        li { key: "{planned.exercise.id}", class: "io-row",
                            span { class: "io-row-title", "{planned.exercise.name}" }
                            span { class: "io-muted io-row-meta",
                                "{exercise_summary(&planned.targets, unit)}"
                            }
                        }
                    }
                }
            }
        }
        div { class: "io-actions",
            Button { xl: true, block: true, onclick: start,
                if in_progress { "Resume" } else { "Start" }
            }
            match last {
                Some(line) => rsx! {
                    p { class: "io-muted io-hint io-home-last", "{line}" }
                },
                None if !in_progress => rsx! {
                    p { class: "io-muted io-hint io-home-last", "No session yet." }
                },
                None => rsx! {},
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use iron_oxide_domain::progression::{ExerciseTargets, SetTarget, TargetSource};
    use iron_oxide_domain::{ExerciseId, ProgramId, Reps, Seconds, SessionId};

    use crate::api::sessions::SessionView;

    use super::*;

    fn kg(value: f64) -> Weight {
        Weight::from_kg(value).unwrap()
    }

    fn targets(working: Vec<SetTarget>) -> NextTargets {
        NextTargets::Ready(ExerciseTargets {
            exercise: "back-squat".parse::<ExerciseId>().unwrap(),
            source: TargetSource::ProgramDefault,
            warmup: Vec::new(),
            working,
            training_max: None,
            failed_sessions: 0,
            last_verdict: None,
            change: None,
        })
    }

    fn reps(count: u16, weight: Option<Weight>) -> SetTarget {
        SetTarget {
            weight,
            goal: SetGoal::Reps {
                reps: Reps::new(count),
                range: None,
            },
        }
    }

    #[test]
    fn the_preview_sums_up_the_working_sets() {
        let five = targets(vec![reps(5, Some(kg(100.0))); 5]);
        assert_eq!(exercise_summary(&five, Unit::Kg), "5 × 5 · 100 kg");
        assert_eq!(exercise_summary(&five, Unit::Lb), "5 × 5 · 220.46 lb");

        let ramp = targets(vec![reps(5, Some(kg(80.0))), reps(5, Some(kg(90.0)))]);
        assert_eq!(exercise_summary(&ramp, Unit::Kg), "2 × 5 · 80–90 kg");

        let mixed = targets(vec![reps(5, None), reps(3, None)]);
        assert_eq!(exercise_summary(&mixed, Unit::Kg), "2 sets");

        let plank = targets(vec![
            SetTarget {
                weight: None,
                goal: SetGoal::Hold {
                    seconds: Seconds::new(45)
                },
            };
            3
        ]);
        assert_eq!(exercise_summary(&plank, Unit::Kg), "3 × 45 s");

        let sprints = targets(vec![SetTarget {
            weight: None,
            goal: SetGoal::Intervals {
                work: Seconds::new(30),
                rest: Seconds::new(90),
                rounds: 8,
            },
        }]);
        assert_eq!(exercise_summary(&sprints, Unit::Kg), "8 × 30/90 s");
        let two = targets(vec![
            SetTarget {
                weight: None,
                goal: SetGoal::Intervals {
                    work: Seconds::new(30),
                    rest: Seconds::new(90),
                    rounds: 8,
                },
            };
            2
        ]);
        assert_eq!(exercise_summary(&two, Unit::Kg), "2 × 8 × 30/90 s");

        assert_eq!(exercise_summary(&targets(Vec::new()), Unit::Kg), "");
        let needs = NextTargets::NeedsTrainingMax {
            exercise: "bench-press".parse::<ExerciseId>().unwrap(),
        };
        assert_eq!(exercise_summary(&needs, Unit::Kg), "Training max needed");
    }

    /// 2026-10-03 12:00 UTC.
    const NOW: Timestamp = Timestamp::from_epoch_millis(1_791_028_800_000);

    fn hours_before(hours: i64) -> Timestamp {
        Timestamp::from_epoch_millis(NOW.epoch_millis() - hours * 3_600_000)
    }

    #[test]
    fn days_are_relative_within_a_week() {
        assert_eq!(relative_day(hours_before(1), NOW, |_| 0), "today");
        assert_eq!(relative_day(hours_before(13), NOW, |_| 0), "yesterday");
        assert_eq!(relative_day(hours_before(24 * 3), NOW, |_| 0), "3 days ago");
        assert_eq!(relative_day(hours_before(24 * 6), NOW, |_| 0), "6 days ago");
        assert_eq!(relative_day(hours_before(24 * 7), NOW, |_| 0), "26 Sep");
        assert_eq!(
            relative_day(hours_before(24 * 365), NOW, |_| 0),
            "3 Oct 2025"
        );
        assert_eq!(
            relative_day(Timestamp::from_epoch_millis(0), NOW, |_| 0),
            "1 Jan 1970"
        );
    }

    #[test]
    fn days_follow_the_local_time_zone() {
        // 13 hours before noon UTC is 23:00 UTC yesterday, but 01:00 today at UTC+2.
        assert_eq!(relative_day(hours_before(13), NOW, |_| 120), "today");
        // 11 hours before is 01:00 UTC today, but 20:00 yesterday at UTC−5.
        assert_eq!(relative_day(hours_before(11), NOW, |_| -300), "yesterday");
        assert_eq!(relative_day(hours_before(11), NOW, |_| 0), "today");
    }

    #[test]
    fn leap_days_and_month_ends_are_dated() {
        // 2024-02-29 12:00 UTC.
        let leap = Timestamp::from_epoch_millis(1_709_208_000_000);
        assert_eq!(relative_day(leap, NOW, |_| 0), "29 Feb 2024");
        // 2026-03-31 12:00 UTC.
        let march = Timestamp::from_epoch_millis(1_774_958_400_000);
        assert_eq!(relative_day(march, NOW, |_| 0), "31 Mar");
    }

    fn summary(status: SessionStatus, working_sets: u32) -> SessionSummary {
        SessionSummary {
            id: SessionId::new_v7(),
            program_id: ProgramId::new_v7(),
            program_name: "Full body".to_owned(),
            program_version_id: ProgramVersionId::new_v7(),
            program_version: 1,
            day_id: "a".parse::<DayId>().unwrap(),
            day_name: None,
            status,
            started_at: hours_before(26),
            finished_at: Some(hours_before(25)),
            working_sets,
            volume: iron_oxide_domain::Volume::ZERO,
            set_pr: false,
        }
    }

    fn loaded(summary: SessionSummary, day: Option<&str>) -> LastSession {
        LastSession::Loaded {
            summary: Box::new(summary),
            day_name: day.map(str::to_owned),
            other_program: None,
        }
    }

    #[test]
    fn the_last_session_line_says_when_which_day_and_how_much() {
        let line = |last: LastSession| last_session_text(&last, NOW, |_| 0);
        assert_eq!(
            line(loaded(summary(SessionStatus::Completed, 15), Some("Day A"))).as_deref(),
            Some("Last session: yesterday · Day A · 15 sets")
        );
        assert_eq!(
            line(loaded(summary(SessionStatus::Completed, 1), None)).as_deref(),
            Some("Last session: yesterday · a · 1 set")
        );
        assert_eq!(
            line(loaded(summary(SessionStatus::Skipped, 0), Some("Day A"))).as_deref(),
            Some("Last session: yesterday · Day A · skipped")
        );
        assert_eq!(
            line(loaded(summary(SessionStatus::Abandoned, 3), Some("Day A"))).as_deref(),
            Some("Last session: yesterday · Day A · abandoned")
        );
        assert_eq!(
            line(LastSession::Loaded {
                summary: Box::new(summary(SessionStatus::Completed, 12)),
                day_name: Some("Upper".to_owned()),
                other_program: Some("Full body".to_owned()),
            })
            .as_deref(),
            Some("Last session: yesterday · Full body · Upper · 12 sets")
        );
        assert_eq!(line(LastSession::None), None);
        // A failed history read only degrades this line.
        assert_eq!(
            line(LastSession::Failed).as_deref(),
            Some("The last session could not be loaded.")
        );
    }

    #[test]
    fn days_use_the_offset_of_their_own_moment() {
        // Daylight saving changed in between: UTC+1 when the session ended, UTC+2 now. It ended
        // on 1 Oct at 22:30 UTC: 23:30 local then (two days ago), though 00:30 on 2 Oct (yesterday)
        // with today's offset.
        let then = Timestamp::from_epoch_millis(NOW.epoch_millis() - 37 * 3_600_000 - 1_800_000);
        let offsets = |at: Timestamp| if at == then { 60 } else { 120 };
        assert_eq!(relative_day(then, NOW, offsets), "2 days ago");
        assert_eq!(relative_day(then, NOW, |_| 120), "yesterday");
    }

    fn day(id: &str, name: &str) -> Day {
        Day {
            id: id.parse::<DayId>().unwrap(),
            name: name.to_owned(),
            exercises: Vec::new(),
        }
    }

    #[test]
    fn a_day_is_named_from_the_active_program_only_for_its_own_version() {
        let active = ProgramVersionId::new_v7();
        let days = [day("a", "Push"), day("b", "Pull")];
        let a = "a".parse::<DayId>().unwrap();
        assert_eq!(
            day_name_in_version(active, &days, active, &a).as_deref(),
            Some("Push")
        );
        // Day `a` of another program or version is another day.
        assert_eq!(
            day_name_in_version(active, &days, ProgramVersionId::new_v7(), &a),
            None
        );
        let gone = "z".parse::<DayId>().unwrap();
        assert_eq!(day_name_in_version(active, &days, active, &gone), None);
    }

    fn planned(id: &str, name: &str) -> PlannedExercise {
        PlannedExercise {
            exercise: serde_json::from_value(serde_json::json!({
                "id": id,
                "name": name,
                "work": { "reps": { "sets": 3, "reps": 5 } },
                "rest": 120
            }))
            .unwrap(),
            targets: targets(vec![reps(5, Some(kg(60.0))); 3]),
        }
    }

    #[test]
    fn a_running_session_is_shown_from_its_own_program_and_version() {
        // The reviewer's scenario: a session of program X (day `a`, deadlifts) runs; meanwhile
        // the active program became Y (or X got a new version), whose next day is also `a`.
        let next = NextSessionPlan {
            program_id: ProgramId::new_v7(),
            program_version_id: ProgramVersionId::new_v7(),
            day: "a".parse::<DayId>().unwrap(),
            day_name: "Y day A".to_owned(),
            exercises: vec![planned("bench-press", "Bench press")],
        };
        let session = SessionView {
            id: SessionId::new_v7(),
            program_id: ProgramId::new_v7(),
            program_version_id: ProgramVersionId::new_v7(),
            day: "a".parse::<DayId>().unwrap(),
            status: SessionStatus::InProgress,
            started_at: hours_before(1),
            finished_at: None,
        };
        let running = Running {
            program_name: "Program X".to_owned(),
            plan: SessionPlan {
                session,
                day_name: "X day A".to_owned(),
                exercises: vec![planned("deadlift", "Deadlift")],
            },
        };
        let shown_running = shown("Program Y", &next, Some(&running));
        assert!(shown_running.in_progress);
        assert_eq!(shown_running.program_name, "Program X");
        assert_eq!(shown_running.day_name, "X day A");
        assert_eq!(shown_running.exercises, running.plan.exercises);

        let shown_next = shown("Program Y", &next, None);
        assert!(!shown_next.in_progress);
        assert_eq!(shown_next.program_name, "Program Y");
        assert_eq!(shown_next.day_name, "Y day A");
        assert_eq!(shown_next.exercises, next.exercises);
    }

    /// Review of #113: while a finished workout waits for its finish, Home offers no Start (the
    /// server would offer that same day again), and names the next day by the rotation.
    #[test]
    fn a_finished_workout_waiting_for_its_finish_offers_the_next_day_not_the_same() {
        let program = iron_oxide_domain::program::builtin_programs()
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .program()
            .clone();
        let program_id = ProgramId::new_v7();
        let record = LocalSession {
            session_id: SessionId::new_v7(),
            started_at: Timestamp::from_epoch_millis(1_000),
            program_id,
            program_version_id: ProgramVersionId::new_v7(),
            day: program.rotation[0].clone(),
            sets: Vec::new(),
            finished: Some(crate::offline::LocalFinish {
                outcome: iron_oxide_domain::SessionOutcome::Completed,
                finished_at: Timestamp::from_epoch_millis(2_000),
            }),
            screen: None,
        };
        let first = program.day(&program.rotation[0]).unwrap().name.clone();
        let second = program.day(&program.rotation[1]).unwrap().name.clone();
        let shown = finishing(&record, Some((program_id, &program)), &[]);
        assert_eq!(shown.next_day.as_deref(), Some(second.as_str()));
        assert_ne!(shown.next_day.as_deref(), Some(first.as_str()));
        // Another active program, or offline: no guess.
        assert_eq!(
            finishing(&record, Some((ProgramId::new_v7(), &program)), &[]).next_day,
            None
        );
        assert_eq!(finishing(&record, None, &[]).next_day, None);
        // Abandoned does not move the rotation: the same day comes next.
        let abandoned = LocalSession {
            finished: Some(crate::offline::LocalFinish {
                outcome: iron_oxide_domain::SessionOutcome::Abandoned,
                finished_at: Timestamp::from_epoch_millis(2_000),
            }),
            ..record
        };
        assert_eq!(
            finishing(&abandoned, Some((program_id, &program)), &[])
                .next_day
                .as_deref(),
            Some(first.as_str())
        );
    }

    /// A refused finish waits on the lifter, not the network: Home says so.
    #[test]
    fn a_refused_finish_is_not_said_to_be_on_its_way() {
        let session = SessionId::new_v7();
        let record = LocalSession {
            session_id: session,
            started_at: Timestamp::from_epoch_millis(1_000),
            program_id: ProgramId::new_v7(),
            program_version_id: ProgramVersionId::new_v7(),
            day: "a".parse().unwrap(),
            sets: Vec::new(),
            finished: None,
            screen: None,
        };
        let pending = finishing(&record, None, &[(WriteKey::FinishSession(session), None)]);
        assert_eq!(pending.refused, None);
        let (title, message) = finishing_text(&pending);
        assert_eq!(title, "Finishing your last workout…");
        assert!(message.contains("as soon as it can"));

        let reason = "This session has already ended.";
        let refused = finishing(
            &record,
            None,
            &[(WriteKey::FinishSession(session), Some(reason.to_owned()))],
        );
        assert_eq!(refused.refused.as_deref(), Some(reason));
        let (title, message) = finishing_text(&refused);
        assert_eq!(title, "Last workout not saved");
        assert!(!message.contains("as soon as it can"), "{message}");
        assert!(
            message.contains(reason) && message.contains("Retry or Discard"),
            "{message}"
        );
        // Another session's refusal is not this one's.
        let other = finishing(
            &record,
            None,
            &[(
                WriteKey::FinishSession(SessionId::new_v7()),
                Some(reason.to_owned()),
            )],
        );
        assert_eq!(other.refused, None);
    }
}
