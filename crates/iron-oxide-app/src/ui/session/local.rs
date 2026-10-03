//! The session in progress kept on the device (#107), so a reload, a closed app or no signal never
//! loses the workout: the outbox's [`LocalSession`] record (the session and its sets), plus this
//! screen's own state in its `screen` field ([`Screen`]: the plan, a settings snapshot, the
//! exercises skipped, the values being entered and the rest timer).
//!
//! - Starting a workout queues the start and saves the record at once: nothing waits for the
//!   server ([`start`]).
//! - The workout screen saves the record after every change.
//! - On load, [`reconcile`] decides between the record, the server's session in progress and the
//!   outbox: the record wins while its start is still queued or the server cannot be reached.
//! - Finishing marks the record `finished`; it is cleared once the finish is delivered (until then
//!   Home does not offer to resume it). Signing out clears it (`Outbox::signed_out`).

use std::collections::BTreeSet;

use iron_oxide_domain::time::Timestamp;
use iron_oxide_domain::{ExerciseId, LoggedSet, SessionId, SessionStatus, Weight};
use serde::{Deserialize, Serialize};

use super::flow::Step;
use super::rest::Rest;
use super::{Active, writes};
use crate::api::sessions::{
    NextSessionPlan, SessionPlan, SessionView, SessionWithSets, StartChoice,
};
use crate::api::settings::Settings;
use crate::auth::types::UserId;
use crate::offline::{LocalSession, NotSignedIn, Outbox, WriteKey, platform};

/// The session screen's state inside the record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    /// The session's plan, so the workout opens offline.
    pub plan: SessionPlan,
    /// The settings when the session was opened (unit, plates, default rest, sound).
    pub settings: Settings,
    /// The program's name, for Home's Resume card.
    #[serde(default)]
    pub program_name: Option<String>,
    #[serde(default)]
    pub skipped: BTreeSet<ExerciseId>,
    /// The values being entered on the current set.
    #[serde(default)]
    pub draft: Option<Draft>,
    /// The rest in progress.
    #[serde(default)]
    pub rest: Option<Rest>,
}

/// What the lifter set on the current step, not logged yet: the steppers, and when its timer
/// started. Tagged with its step, so another step starts from its own prefill.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub step: Step,
    pub reps: i64,
    pub weight: Option<Weight>,
    pub started: Option<Timestamp>,
}

/// The record of `active`, with its screen state.
#[must_use]
pub fn record(active: &Active, draft: Option<Draft>, rest: Option<Rest>) -> LocalSession {
    let session = &active.plan.session;
    let screen = Screen {
        plan: active.plan.clone(),
        settings: active.settings.clone(),
        program_name: active.program_name.clone(),
        skipped: active.skipped.clone(),
        draft,
        rest,
    };
    LocalSession {
        session_id: session.id,
        started_at: session.started_at,
        program_id: session.program_id,
        program_version_id: session.program_version_id,
        day: session.day.clone(),
        sets: active.sets.clone(),
        finished: None,
        screen: serde_json::to_value(screen).ok(),
    }
}

/// The screen state of a record, if it has a readable one.
#[must_use]
pub fn screen_of(local: &LocalSession) -> Option<Screen> {
    serde_json::from_value(local.screen.clone()?).ok()
}

/// The workout a record holds, ready for the screen.
#[must_use]
pub fn active_of(local: &LocalSession) -> Option<(Active, Option<Draft>, Option<Rest>)> {
    let screen = screen_of(local)?;
    let active = Active {
        plan: screen.plan,
        settings: screen.settings,
        sets: local.sets.clone(),
        skipped: screen.skipped,
        program_name: screen.program_name,
    };
    Some((active, screen.draft, screen.rest))
}

/// The plan of a session just started from `next`: the server gives the started session the same
/// day and targets (`NextSessionPlan`), so it is built here instead of waiting for the server.
#[must_use]
pub fn plan_from_next(next: &NextSessionPlan, id: SessionId, started_at: Timestamp) -> SessionPlan {
    SessionPlan {
        session: SessionView {
            id,
            program_id: next.program_id,
            program_version_id: next.program_version_id,
            day: next.day.clone(),
            status: SessionStatus::InProgress,
            started_at,
            finished_at: None,
        },
        day_name: next.day_name.clone(),
        exercises: next.exercises.clone(),
    }
}

/// What the start of `next` records: its program version and day, as shown on the device.
#[must_use]
pub fn choice_of(next: &NextSessionPlan) -> StartChoice {
    StartChoice {
        program_id: next.program_id,
        program_version_id: next.program_version_id,
        day: next.day.clone(),
    }
}

/// Starts the workout `next`: queues the start (a new id, the device's clock), saves the record,
/// and returns the workout. Never waits for the server.
///
/// # Errors
/// [`NotSignedIn`] when the outbox has no user.
pub fn start(
    outbox: Outbox,
    next: &NextSessionPlan,
    settings: Settings,
    program_name: Option<String>,
) -> Result<Active, NotSignedIn> {
    let user = outbox.user().ok_or(NotSignedIn)?;
    let id = SessionId::new_v7();
    let started_at = platform::now();
    writes::start_session(outbox, id, started_at, choice_of(next))?;
    let active = Active {
        plan: plan_from_next(next, id, started_at),
        settings,
        sets: Vec::new(),
        skipped: BTreeSet::new(),
        program_name,
    };
    // Best effort: when storage is blocked the outbox already says "Not saved on this device".
    let _ = save(user, &record(&active, None, None));
    Ok(active)
}

/// The user's record on this device.
#[must_use]
pub fn load(user: UserId) -> Option<LocalSession> {
    platform::with_storage(|storage| LocalSession::load(storage, user))
}

/// Saves the record.
///
/// # Errors
/// When storage is blocked or full.
pub fn save(
    user: UserId,
    local: &LocalSession,
) -> Result<(), crate::offline::storage::StorageError> {
    platform::with_storage(|storage| local.save(storage, user))
}

/// Forgets the record.
pub fn clear(user: UserId) {
    platform::with_storage(|storage| {
        let _ = LocalSession::clear(storage, user);
    });
}

/// What to do with the record on load.
#[derive(Debug, Clone, PartialEq)]
pub enum Restore {
    /// No usable record: load from the server.
    Server,
    /// Resume the record (its sets merged with the server's when the server knows the session).
    Resume(Box<LocalSession>),
    /// The session ended on this device and its finish is still queued (or refused).
    Ended(Box<LocalSession>),
    /// The record is over or stale: clear it, then load from the server.
    Drop,
}

/// Decides between the record, the server's session in progress (`None`: not asked, or not
/// reachable) and the queued writes. Call it only once the outbox knows the user, so that an empty
/// queue means "delivered", not "not loaded yet".
#[must_use]
pub fn reconcile(
    local: Option<LocalSession>,
    server: Option<Option<&SessionWithSets>>,
    queued: &[(WriteKey, Option<String>)],
) -> Restore {
    let Some(mut local) = local else {
        return Restore::Server;
    };
    let is_queued = |key: WriteKey| queued.iter().any(|(queued, _)| *queued == key);
    if local.finished.is_some() {
        return if is_queued(WriteKey::FinishSession(local.session_id)) {
            Restore::Ended(Box::new(local))
        } else {
            Restore::Drop
        };
    }
    if local.screen.is_none() {
        return Restore::Server;
    }
    match server {
        // Offline or not asked: the record is all there is.
        None => Restore::Resume(Box::new(local)),
        Some(Some(running)) if running.session.id == local.session_id => {
            merge_sets(&mut local.sets, &running.sets);
            Restore::Resume(Box::new(local))
        }
        // The server does not know it yet: its start is still on its way.
        Some(_) if is_queued(WriteKey::StartSession(local.session_id)) => {
            Restore::Resume(Box::new(local))
        }
        // Delivered, yet the server has another session or none: it ended elsewhere, or its
        // start was given up.
        Some(_) => Restore::Drop,
    }
}

/// Adds the server's sets this device does not have (logged elsewhere), keeping the device's own
/// order and appending the others in their completion order.
fn merge_sets(sets: &mut Vec<LoggedSet<Timestamp>>, server: &[LoggedSet<Timestamp>]) {
    let mut missing: Vec<_> = server
        .iter()
        .filter(|set| sets.iter().all(|own| own.id != set.id))
        .cloned()
        .collect();
    missing.sort_by_key(|set| set.completed_at);
    sets.extend(missing);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::sessions::PlannedExercise;
    use iron_oxide_domain::{
        DayId, ProgramId, ProgramVersionId, Reps, SessionOutcome, SetId, Unit,
    };
    use uuid::Uuid;

    fn next() -> NextSessionPlan {
        NextSessionPlan {
            program_id: ProgramId::from_uuid(Uuid::from_u128(2)),
            program_version_id: ProgramVersionId::from_uuid(Uuid::from_u128(3)),
            day: DayId::new("a").unwrap(),
            day_name: "Day A".to_owned(),
            exercises: Vec::<PlannedExercise>::new(),
        }
    }

    fn set(n: u128, at: i64) -> LoggedSet<Timestamp> {
        LoggedSet {
            id: SetId::from_uuid(Uuid::from_u128(n)),
            exercise: ExerciseId::new("squat").unwrap(),
            set_index: 0,
            reps: Reps::new(5),
            weight: None,
            duration: None,
            warm_up: false,
            completed_at: Timestamp::from_epoch_millis(at),
            target: None,
        }
    }

    fn active(id: u128) -> Active {
        Active {
            plan: plan_from_next(
                &next(),
                SessionId::from_uuid(Uuid::from_u128(id)),
                Timestamp::from_epoch_millis(1_000),
            ),
            settings: Settings::defaults(),
            sets: vec![set(10, 2_000)],
            skipped: BTreeSet::from([ExerciseId::new("plank").unwrap()]),
            program_name: Some("Full body".to_owned()),
        }
    }

    fn running(id: u128, sets: Vec<LoggedSet<Timestamp>>) -> SessionWithSets {
        SessionWithSets {
            session: active(id).plan.session,
            sets,
        }
    }

    fn session(id: u128) -> SessionId {
        SessionId::from_uuid(Uuid::from_u128(id))
    }

    #[test]
    fn a_started_session_has_the_next_days_plan() {
        let plan = plan_from_next(&next(), session(1), Timestamp::from_epoch_millis(7));
        assert_eq!(plan.session.id, session(1));
        assert_eq!(plan.session.day, next().day);
        assert_eq!(plan.session.program_version_id, next().program_version_id);
        assert_eq!(plan.session.status, SessionStatus::InProgress);
        assert_eq!(plan.session.started_at, Timestamp::from_epoch_millis(7));
        assert_eq!(plan.day_name, "Day A");
    }

    #[test]
    fn the_start_names_the_day_shown_on_the_device() {
        let choice = choice_of(&next());
        assert_eq!(choice.program_id, next().program_id);
        assert_eq!(choice.program_version_id, next().program_version_id);
        assert_eq!(choice.day, next().day);
    }

    #[test]
    fn the_record_round_trips_the_screen_state() {
        let workout = active(1);
        let mut settings = Settings::defaults();
        settings.unit = Unit::Lb;
        let workout = Active {
            settings,
            ..workout
        };
        let rest = Rest::start(
            SetId::from_uuid(Uuid::from_u128(10)),
            Timestamp::from_epoch_millis(3_000),
            iron_oxide_domain::Seconds::new(90),
        );
        let local = record(&workout, None, Some(rest));
        let json = serde_json::to_value(&local).unwrap();
        let back: LocalSession = serde_json::from_value(json).unwrap();
        let (restored, draft, restored_rest) = active_of(&back).unwrap();
        assert_eq!(restored, workout);
        assert_eq!(draft, None);
        assert_eq!(restored_rest, Some(rest));
    }

    #[test]
    fn reconcile_keeps_the_record_while_its_start_is_queued_or_the_server_is_unreachable() {
        let local = record(&active(1), None, None);
        let start = vec![(WriteKey::StartSession(session(1)), None)];
        // Offline: the record.
        assert_eq!(
            reconcile(Some(local.clone()), None, &[]),
            Restore::Resume(Box::new(local.clone()))
        );
        // The server knows nothing yet, the start is queued: the record.
        assert_eq!(
            reconcile(Some(local.clone()), Some(None), &start),
            Restore::Resume(Box::new(local.clone()))
        );
        // Delivered, and the server has no session (ended elsewhere, or given up): dropped.
        assert_eq!(
            reconcile(Some(local.clone()), Some(None), &[]),
            Restore::Drop
        );
        let other = running(2, Vec::new());
        assert_eq!(
            reconcile(Some(local.clone()), Some(Some(&other)), &[]),
            Restore::Drop
        );
        // No record: the server.
        assert_eq!(reconcile(None, Some(None), &[]), Restore::Server);
    }

    #[test]
    fn reconcile_adds_the_servers_sets_the_device_lacks() {
        let local = record(&active(1), None, None);
        let server = running(1, vec![set(10, 2_000), set(12, 5_000), set(11, 4_000)]);
        let Restore::Resume(merged) = reconcile(Some(local), Some(Some(&server)), &[]) else {
            panic!("expected the record");
        };
        let ids: Vec<_> = merged
            .sets
            .iter()
            .map(|set| set.id.as_uuid().as_u128())
            .collect();
        assert_eq!(ids, [10, 11, 12]);
    }

    #[test]
    fn an_ended_session_waits_for_its_finish_then_goes() {
        let mut local = record(&active(1), None, None);
        local.finished = Some(crate::offline::LocalFinish {
            outcome: SessionOutcome::Completed,
            finished_at: Timestamp::from_epoch_millis(9_000),
        });
        let finish = vec![(WriteKey::FinishSession(session(1)), None)];
        assert_eq!(
            reconcile(Some(local.clone()), None, &finish),
            Restore::Ended(Box::new(local.clone()))
        );
        // Even though the server still lists it as in progress.
        let server = running(1, Vec::new());
        assert_eq!(
            reconcile(Some(local.clone()), Some(Some(&server)), &finish),
            Restore::Ended(Box::new(local.clone()))
        );
        assert_eq!(reconcile(Some(local), None, &[]), Restore::Drop);
    }

    #[test]
    fn a_record_without_screen_state_loads_from_the_server() {
        let mut local = record(&active(1), None, None);
        local.screen = None;
        assert_eq!(reconcile(Some(local), None, &[]), Restore::Server);
    }
}
