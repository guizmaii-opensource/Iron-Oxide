//! Every write of a workout session goes through this file. Each one is queued in the offline
//! outbox (#30, `crate::offline`) with exactly the arguments of its server function, and the
//! outbox delivers it when the server can be reached, in order, retrying the same arguments.
//!
//! The ids are client-generated UUIDv7 (`SessionId::new_v7()`, `SetId::new_v7()`) and the
//! timestamps come from the device, so a write sent twice is answered unchanged. Nothing here
//! waits for the server: screens follow a write with `Outbox::is_pending` / `Outbox::queued`.

use dioxus::prelude::*;
use iron_oxide_domain::time::Timestamp;
use iron_oxide_domain::{LoggedSet, SessionId, SessionOutcome};

use crate::api::sessions::{self, SessionSummary, StartChoice};
use crate::offline::{NotSignedIn, Outbox, Write, WriteKey};

/// Queues the start of a session on the program version and day the device chose: the server
/// records exactly those, so a start delivered after the previous finish keeps its day.
///
/// # Errors
/// [`NotSignedIn`] when no user is signed in on this device.
pub fn start_session(
    outbox: Outbox,
    session_id: SessionId,
    started_at: Timestamp,
    choice: StartChoice,
) -> Result<WriteKey, NotSignedIn> {
    outbox.enqueue(Write::StartSession {
        session_id,
        started_at,
        choice: Some(choice),
    })
}

/// Queues one logged set.
///
/// # Errors
/// [`NotSignedIn`] when no user is signed in on this device.
pub fn save_set(
    outbox: Outbox,
    session_id: SessionId,
    set: LoggedSet<Timestamp>,
) -> Result<WriteKey, NotSignedIn> {
    outbox.enqueue(Write::SaveSet { session_id, set })
}

/// Queues the end of the session: completed, or discarded (abandoned).
///
/// # Errors
/// [`NotSignedIn`] when no user is signed in on this device.
pub fn finish_session(
    outbox: Outbox,
    session_id: SessionId,
    outcome: SessionOutcome,
    finished_at: Timestamp,
) -> Result<WriteKey, NotSignedIn> {
    outbox.enqueue(Write::FinishSession {
        session_id,
        outcome,
        finished_at,
    })
}

/// The summary of a finish the outbox delivered: the same call again, which the server answers
/// as a replay with the same summary. Call it once `Outbox::is_pending` is false for the finish.
pub async fn finished_summary(
    session_id: SessionId,
    outcome: SessionOutcome,
    finished_at: Timestamp,
) -> Result<SessionSummary, ServerFnError> {
    sessions::finish_session(session_id, outcome, finished_at).await
}
