//! The outbox's state machine: which write to send next, and what a send's result does to the
//! queue. Pure: no browser, no clock, no randomness (the caller passes `now` and the draw).
//!
//! - **One FIFO for all writes.** A write is only sent once everything enqueued before it was
//!   delivered, so the server sees them in the order the user made them: a session's sets after
//!   its start and before its finish, and a session's finish before the next session's start
//!   (which the server refuses while another session is in progress).
//! - **Retryable failures** (network, `429`, `502`-`504`) keep the write at the head and wait:
//!   [`Backoff`] with full jitter, never less than a `429`'s `Retry-After`. [`Queue::nudge`]
//!   (the browser came back online, the app started) skips the backoff but never a `Retry-After`.
//! - **Rejections** (`400`, `403`, `404`, `409`, `413`, `422`, `500`) mark the write failed and
//!   stop the queue there, with the server's message. Nothing is dropped: the user retries
//!   ([`Queue::retry_failed`]) or explicitly discards it ([`Queue::discard_failed`]).
//! - **`401`** pauses the queue without failing anything: every write waits for the user to sign
//!   in again, then [`Queue::nudge`] resumes.
//! - **Replays.** An entry is removed only after a `2xx`. Enqueueing a write that is already
//!   queued (same content) does nothing; a write is identified by the ids it carries
//!   ([`WriteKey`]), which the client generates with `new_v7()`, so the server recognises a
//!   resent write and answers it unchanged.
//! - **Merging copies** ([`Queue::merge`]): tabs keep their own copy and merge it with the stored
//!   one at every step. Every change is stamped with a revision from a Lamport clock: each
//!   entry has its own (enqueue, payload edit, refusal), the retry state has one (backoff,
//!   `Retry-After`, sign-in pause), and delivered or discarded writes leave a tombstone. Per
//!   key the higher revision wins, a tombstone wins over the copies it saw, and entries keep
//!   their original enqueue order (`seq`). A stale copy (a tab that could not save) therefore
//!   never undoes a wait, a refusal, an edit, a delivery or a discard.

use std::collections::VecDeque;
use std::time::Duration;

use iron_oxide_domain::{LoggedSet, SessionId, SessionOutcome, SetId, time::Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::backoff::Backoff;
use crate::api::sessions::StartChoice;

/// A write the outbox delivers: the exact arguments of one server function in
/// `crate::api::sessions`. Retries send them unchanged (ids and client timestamps included),
/// which is what makes them idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Write {
    /// `start_session(session_id, started_at, choice)`.
    StartSession {
        session_id: SessionId,
        started_at: Timestamp,
        /// The program version and day the device started (absent in queues stored before it
        /// existed: the server then picks the next day).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice: Option<StartChoice>,
    },
    /// `save_set(session_id, set)`.
    SaveSet {
        session_id: SessionId,
        set: LoggedSet<Timestamp>,
    },
    /// `finish_session(session_id, outcome, finished_at)`.
    FinishSession {
        session_id: SessionId,
        outcome: SessionOutcome,
        finished_at: Timestamp,
    },
}

impl Write {
    /// What identifies this write on the server.
    #[must_use]
    pub fn key(&self) -> WriteKey {
        match self {
            Self::StartSession { session_id, .. } => WriteKey::StartSession(*session_id),
            Self::SaveSet { set, .. } => WriteKey::SaveSet(set.id),
            Self::FinishSession { session_id, .. } => WriteKey::FinishSession(*session_id),
        }
    }

    /// Whether this write cannot succeed once `other` is given up: every later write of a
    /// session depends on its start (the server answers `404` for a session it never created).
    /// Sets and finishes stand on their own.
    #[must_use]
    pub fn depends_on(&self, other: &Self) -> bool {
        matches!(other, Self::StartSession { session_id, .. }
            if *session_id == self.session_id() && self != other)
    }

    /// The session the write belongs to.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        match self {
            Self::StartSession { session_id, .. }
            | Self::SaveSet { session_id, .. }
            | Self::FinishSession { session_id, .. } => *session_id,
        }
    }
}

/// What identifies a [`Write`]: the client-generated id the server deduplicates on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum WriteKey {
    StartSession(SessionId),
    SaveSet(SetId),
    FinishSession(SessionId),
}

/// A queued write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub write: Write,
    /// When it was enqueued (the client's clock).
    pub enqueued_at: Timestamp,
    /// Its place in the queue: the clock when it was first enqueued. Kept through edits.
    #[serde(default)]
    pub seq: u64,
    /// The clock at its last change (enqueue, payload edit, refusal, retry).
    #[serde(default)]
    pub rev: u64,
    /// Set when the server rejected it (`409`, `422`, …): the message to show. The queue stops
    /// at a failed entry until it is retried or discarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
}

/// What a failed send means for the queue, from `ApiFailure::classify` (see
/// `super::outbox::failure_of`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Network, `429`, `502`-`504`: send the same write again later. `retry_after` is a `429`'s
    /// delay: the next attempt never comes sooner.
    Retry {
        message: String,
        retry_after: Option<Duration>,
    },
    /// `401`: wait for the user to sign in again; nothing failed.
    SignedOut { message: String },
    /// Any other status: the server will never accept this exact write. Stop and show it.
    Rejected { message: String },
}

/// Whether [`Queue::enqueue`] added the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    Added,
    /// The same write is already queued: nothing changed.
    Duplicate,
    /// A write with the same key (an edited set) was still queued: its payload was replaced in
    /// place, keeping its turn. A refusal on it is cleared, since the content changed.
    Replaced,
}

/// Shown when a set was edited while its earlier values were being sent and the server saved
/// those: the server refuses a second version of the same set id (`409`), so the edit is held
/// here, refused, instead of being sent for a certain `409`.
pub const EDITED_AFTER_SAVE_MESSAGE: &str =
    "This set was already saved with its earlier values. Editing a saved set is not supported yet.";

/// When the queue wants to run next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// A write is ready now.
    Now,
    /// The head write waits for its backoff or `Retry-After`.
    At(Timestamp),
    /// Nothing to do until something changes: empty, stopped at a failed write, or signed out.
    Idle,
}

/// What the "unsaved" indicator shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutboxStatus {
    /// Writes not delivered yet, failed ones included.
    pub pending_count: usize,
    /// Writes the server rejected, waiting for the user.
    pub failed_count: usize,
    /// Why the queue is not empty, when something went wrong: a rejection's message first, then
    /// the sign-in pause, then the last retryable error. `None` while it simply waits for its turn.
    pub last_error: Option<String>,
}

impl OutboxStatus {
    /// Whether the indicator has anything to show.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.pending_count == 0 && self.last_error.is_none()
    }
}

/// A write that left the queue: delivered (`2xx`) or discarded by the user, so that merging a
/// stale copy that still holds it (any version up to this revision) never brings it back.
/// Discards are kept for good, deliveries up to [`MAX_TOMBSTONES`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub write: Write,
    /// The clock when it left.
    pub rev: u64,
    /// Delivered (`true`) or discarded.
    pub delivered: bool,
}

/// How many **delivered** tombstones a queue keeps (the newest). Discard tombstones are never
/// pruned: discards are rare, confirmed user actions, and a pruned one would let a stale copy
/// bring a discarded write back and send it. A pruned delivered tombstone can at worst bring a
/// delivered write back, which is sent once more and answered as a replay, unchanged: every
/// write kind is idempotent on its id (`start_session` returns the session it created, even
/// ended; `save_set` accepts the same set again, even after the session ended; `finish_session`
/// returns the same summary for the same outcome and time; see `docs/api.md`).
pub const MAX_TOMBSTONES: usize = 256;

/// Drops the oldest delivered tombstones past [`MAX_TOMBSTONES`]; keeps every discard.
/// `tombstones` is ordered oldest first.
fn prune(tombstones: &mut Vec<Tombstone>) {
    let mut excess = tombstones
        .iter()
        .filter(|tomb| tomb.delivered)
        .count()
        .saturating_sub(MAX_TOMBSTONES);
    tombstones.retain(|tomb| {
        if tomb.delivered && excess > 0 {
            excess -= 1;
            false
        } else {
            true
        }
    });
}

/// The queue and its retry state, as persisted per user.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Queue {
    #[serde(default)]
    entries: VecDeque<Entry>,
    /// Consecutive retryable failures of the head write.
    #[serde(default)]
    failures: u32,
    /// The backoff: no attempt before this time, unless nudged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_at: Option<Timestamp>,
    /// A `429`'s `Retry-After`: no attempt before this time, even when nudged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    not_before: Option<Timestamp>,
    /// Set by a `401`: the message to show until [`Queue::nudge`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signed_out: Option<String>,
    /// The last retryable error, cleared by the next success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    /// The clock at the retry state's last change (the five fields above).
    #[serde(default)]
    state_rev: u64,
    /// The Lamport clock: every change takes the next value, a merge the larger of both.
    #[serde(default)]
    clock: u64,
    /// Delivered and discarded writes, newest last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tombstones: Vec<Tombstone>,
}

impl Queue {
    /// The queued writes, oldest first.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether there is nothing to keep at all: no entries and no tombstones.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.entries.is_empty() && self.tombstones.is_empty()
    }

    /// The next clock value.
    fn tick(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }

    /// Stamps a change of the retry state.
    fn touch_state(&mut self) {
        self.state_rev = self.tick();
    }

    fn bury(&mut self, write: Write, delivered: bool) {
        let rev = self.tick();
        self.tombstones
            .retain(|tomb| tomb.write.key() != write.key());
        self.tombstones.push(Tombstone {
            write,
            rev,
            delivered,
        });
        prune(&mut self.tombstones);
    }

    /// Appends `write`. The same write already queued is not added twice, and a queued write
    /// with the same key (the same `SetId` with edited values) gets the new payload in place:
    /// two versions of one id would make the second a certain `409`.
    ///
    /// A write identical to one already delivered is a [`Enqueued::Duplicate`] too.
    pub fn enqueue(&mut self, write: Write, now: Timestamp) -> Enqueued {
        let delivered = |tomb: &Tombstone| tomb.delivered && tomb.write == write;
        if self.entries.iter().any(|entry| entry.write == write)
            || self.tombstones.iter().any(delivered)
        {
            return Enqueued::Duplicate;
        }
        let key = write.key();
        let rev = self.tick();
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.write.key() == key)
        {
            entry.write = write;
            entry.failed = None;
            entry.rev = rev;
            return Enqueued::Replaced;
        }
        let failed = self
            .tombstones
            .iter()
            .any(|tomb| tomb.delivered && tomb.write.key() == key)
            .then(|| EDITED_AFTER_SAVE_MESSAGE.to_owned());
        self.entries.push_back(Entry {
            write,
            enqueued_at: now,
            seq: rev,
            rev,
            failed,
        });
        Enqueued::Added
    }

    /// When the queue wants to run next.
    #[must_use]
    pub fn wake(&self, now: Timestamp) -> Wake {
        let Some(head) = self.entries.front() else {
            return Wake::Idle;
        };
        if head.failed.is_some() || self.signed_out.is_some() {
            return Wake::Idle;
        }
        match self.retry_at.max(self.not_before) {
            Some(at) if at > now => Wake::At(at),
            _ => Wake::Now,
        }
    }

    /// The write to send now, if any: the head of the queue, once its wait is over.
    #[must_use]
    pub fn next_ready(&self, now: Timestamp) -> Option<&Write> {
        match self.wake(now) {
            Wake::Now => self.entries.front().map(|entry| &entry.write),
            Wake::At(_) | Wake::Idle => None,
        }
    }

    /// `write` was delivered (`2xx`): removes it and resets the retry state. If the queue
    /// meanwhile holds an edited version of it (same key, other values), that version is marked
    /// refused with [`EDITED_AFTER_SAVE_MESSAGE`]: the server would answer it `409`.
    ///
    /// A stale success (another tab delivered and removed it first) only records the delivery:
    /// the retry state then belongs to another head (a `429`'s `Retry-After`, a backoff) and is
    /// left alone.
    pub fn on_success(&mut self, write: &Write) {
        let removed = self
            .position(write)
            .and_then(|index| self.entries.remove(index))
            .is_some();
        self.bury(write.clone(), true);
        // An edit queued while the earlier values were in flight: newer than the tombstone, so
        // it stays, refused.
        let rev = self.tick();
        let edited = self
            .entries
            .iter_mut()
            .find(|entry| entry.write.key() == write.key())
            .map(|edited| {
                edited.failed = Some(EDITED_AFTER_SAVE_MESSAGE.to_owned());
                edited.rev = rev;
            })
            .is_some();
        if removed || edited {
            self.failures = 0;
            self.retry_at = None;
            self.not_before = None;
            self.last_error = None;
            self.touch_state();
        }
    }

    /// Sending `write` failed. `random` is a uniform draw in `[0, 1)` for the jitter.
    pub fn on_failure(
        &mut self,
        write: &Write,
        failure: Failure,
        now: Timestamp,
        random: f64,
        backoff: &Backoff,
    ) {
        let Some(index) = self.position(write) else {
            return;
        };
        match failure {
            Failure::Retry {
                message,
                retry_after,
            } => {
                self.failures = self.failures.saturating_add(1);
                let delay = backoff.next_delay(self.failures, random, retry_after);
                self.retry_at = Some(now.saturating_add(delay));
                self.not_before = retry_after.map(|after| now.saturating_add(after));
                self.last_error = Some(message);
            }
            Failure::SignedOut { message } => self.signed_out = Some(message),
            Failure::Rejected { message } => {
                let rev = self.tick();
                if let Some(entry) = self.entries.get_mut(index) {
                    entry.failed = Some(message);
                    entry.rev = rev;
                }
                self.failures = 0;
                self.retry_at = None;
                self.not_before = None;
                self.last_error = None;
            }
        }
        self.touch_state();
    }

    /// Try again now: the browser is back online, the app started, the user signed in. Skips the
    /// backoff and the sign-in pause, never a `429`'s `Retry-After`. Failed writes stay failed.
    pub fn nudge(&mut self) {
        if self.retry_at.is_some() || self.signed_out.is_some() {
            self.retry_at = None;
            self.signed_out = None;
            self.touch_state();
        }
    }

    /// Sends the failed writes again (the user asked to).
    pub fn retry_failed(&mut self) {
        let rev = self.tick();
        for entry in &mut self.entries {
            if entry.failed.take().is_some() {
                entry.rev = rev;
            }
        }
        self.nudge();
    }

    /// The writes [`Queue::discard_failed`] would remove, in queue order: the failed writes, and
    /// after each the writes that cannot succeed without it (see [`Write::depends_on`]).
    #[must_use]
    pub fn discard_plan(&self) -> Vec<&Write> {
        let mut doomed: Vec<&Write> = Vec::new();
        for entry in &self.entries {
            if entry.failed.is_some() || doomed.iter().any(|gone| entry.write.depends_on(gone)) {
                doomed.push(&entry.write);
            }
        }
        doomed
    }

    /// Removes the failed writes and the writes that depend on them (the user chose to give
    /// them up, in one action) and returns them, in queue order.
    pub fn discard_failed(&mut self) -> Vec<Write> {
        let doomed: Vec<Write> = self.discard_plan().into_iter().cloned().collect();
        let mut removed = doomed.iter().peekable();
        let mut kept = VecDeque::with_capacity(self.entries.len());
        for entry in std::mem::take(&mut self.entries) {
            if removed.peek().is_some_and(|gone| **gone == entry.write) {
                removed.next();
            } else {
                kept.push_back(entry);
            }
        }
        self.entries = kept;
        for write in &doomed {
            self.bury(write.clone(), false);
        }
        if !self.entries.iter().any(|entry| entry.failed.is_some()) {
            self.nudge();
        }
        doomed
    }

    /// Merges two copies of the queue: `local` (this tab's) and `stored` (just read, possibly
    /// written by another tab, or stale because this tab could not save). Per key, the entry
    /// with the higher revision wins (`local` on a tie); an entry is dropped when a tombstone of
    /// the same write is at least as recent; the retry state with the higher revision wins
    /// (`local` on a tie); entries keep their original enqueue order. An edit made elsewhere of
    /// a write this copy delivered is marked refused ([`EDITED_AFTER_SAVE_MESSAGE`]).
    #[must_use]
    pub fn merge(local: Self, stored: Self) -> Self {
        let clock = local.clock.max(stored.clock);
        let Self {
            entries: local_entries,
            tombstones: local_tombstones,
            ..
        } = local.clone();
        let Self {
            entries: stored_entries,
            tombstones: stored_tombstones,
            ..
        } = stored.clone();
        let state = if stored.state_rev > local.state_rev {
            stored
        } else {
            local
        };
        let mut merged = Self {
            entries: VecDeque::new(),
            tombstones: Vec::new(),
            clock,
            ..state
        };

        // Tombstones: one per key, the newest.
        for tomb in local_tombstones.iter().chain(&stored_tombstones) {
            match merged
                .tombstones
                .iter_mut()
                .find(|kept| kept.write.key() == tomb.write.key())
            {
                Some(kept) if kept.rev >= tomb.rev => {}
                Some(kept) => *kept = tomb.clone(),
                None => merged.tombstones.push(tomb.clone()),
            }
        }
        merged.tombstones.sort_by_key(|tomb| tomb.rev);
        prune(&mut merged.tombstones);

        // Entries: one per key, the newest; `local` wins ties.
        let mut entries: Vec<Entry> = Vec::new();
        for entry in local_entries.into_iter().chain(stored_entries) {
            match entries
                .iter_mut()
                .find(|kept| kept.write.key() == entry.write.key())
            {
                Some(kept) if kept.rev >= entry.rev => {}
                Some(kept) => *kept = entry,
                None => entries.push(entry),
            }
        }
        for mut entry in entries {
            let tomb = merged
                .tombstones
                .iter()
                .find(|tomb| tomb.write.key() == entry.write.key());
            match tomb {
                // Delivered or discarded after this version was made: it covers every earlier
                // version of the id (an edit superseded before the delivery included). Gone.
                Some(tomb) if tomb.rev >= entry.rev => continue,
                // An edit made after the server got other values: a certain `409`.
                Some(tomb)
                    if tomb.delivered && tomb.write != entry.write && entry.failed.is_none() =>
                {
                    entry.failed = Some(EDITED_AFTER_SAVE_MESSAGE.to_owned());
                }
                _ => {}
            }
            merged.entries.push_back(entry);
        }
        merged
            .entries
            .make_contiguous()
            .sort_by_key(|entry| (entry.seq, entry.enqueued_at));
        merged
    }

    /// Whether `key` is still waiting to be delivered.
    #[must_use]
    pub fn contains(&self, key: WriteKey) -> bool {
        self.entries.iter().any(|entry| entry.write.key() == key)
    }

    /// What the indicator shows.
    #[must_use]
    pub fn status(&self) -> OutboxStatus {
        let failed = self
            .entries
            .iter()
            .filter_map(|entry| entry.failed.as_ref());
        let failed_count = failed.clone().count();
        let last_error = failed
            .into_iter()
            .next()
            .or(self.signed_out.as_ref())
            .or(self.last_error.as_ref())
            .filter(|_| !self.entries.is_empty())
            .cloned();
        OutboxStatus {
            pending_count: self.entries.len(),
            failed_count,
            last_error,
        }
    }

    fn position(&self, write: &Write) -> Option<usize> {
        self.entries.iter().position(|entry| entry.write == *write)
    }

    /// Reads a persisted queue, entry by entry: an entry that does not decode is returned apart
    /// (to be kept aside, never silently dropped), and the others are kept in order. Retry state
    /// that does not decode is reset.
    #[must_use]
    pub fn from_json(value: Value) -> (Self, Vec<Value>) {
        let Value::Object(mut fields) = value else {
            return (Self::default(), vec![value]);
        };
        let raw_entries = match fields.remove("entries") {
            Some(Value::Array(entries)) => entries,
            Some(Value::Null) | None => Vec::new(),
            Some(other) => vec![other],
        };
        let mut queue: Self = serde_json::from_value(Value::Object(fields)).unwrap_or_default();
        let mut rejected = Vec::new();
        for raw in raw_entries {
            match serde_json::from_value::<Entry>(raw.clone()) {
                Ok(entry) => queue.entries.push_back(entry),
                Err(_) => rejected.push(raw),
            }
        }
        (queue, rejected)
    }

    /// The persisted form.
    ///
    /// # Errors
    /// Never in practice: every field serializes to JSON.
    pub fn to_json(&self) -> Result<Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use iron_oxide_domain::{ExerciseId, Reps};

    pub(crate) fn at(millis: i64) -> Timestamp {
        Timestamp::from_epoch_millis(millis)
    }

    pub(crate) fn start(session: SessionId) -> Write {
        Write::StartSession {
            session_id: session,
            started_at: at(1_000),
            choice: None,
        }
    }

    pub(crate) fn set(session: SessionId, index: u16) -> Write {
        Write::SaveSet {
            session_id: session,
            set: LoggedSet {
                id: SetId::new_v7(),
                exercise: "back-squat".parse::<ExerciseId>().unwrap(),
                set_index: index,
                reps: Reps::new(5),
                weight: None,
                duration: None,
                warm_up: false,
                completed_at: at(2_000 + i64::from(index)),
                target: None,
            },
        }
    }

    pub(crate) fn finish(session: SessionId) -> Write {
        Write::FinishSession {
            session_id: session,
            outcome: SessionOutcome::Completed,
            finished_at: at(9_000),
        }
    }

    fn retry(message: &str) -> Failure {
        Failure::Retry {
            message: message.to_owned(),
            retry_after: None,
        }
    }

    fn rejected(message: &str) -> Failure {
        Failure::Rejected {
            message: message.to_owned(),
        }
    }

    const B: Backoff = Backoff::DEFAULT;

    /// Sends every ready write in order, all succeeding, and returns them.
    fn deliver_all(queue: &mut Queue, now: Timestamp) -> Vec<Write> {
        let mut sent = Vec::new();
        while let Some(write) = queue.next_ready(now).cloned() {
            queue.on_success(&write);
            sent.push(write);
        }
        sent
    }

    #[test]
    fn writes_are_delivered_in_fifo_order() {
        let (a, b) = (SessionId::new_v7(), SessionId::new_v7());
        let writes = vec![
            start(a),
            set(a, 0),
            set(a, 1),
            finish(a),
            start(b),
            set(b, 0),
        ];
        let mut queue = Queue::default();
        for write in &writes {
            assert_eq!(queue.enqueue(write.clone(), at(0)), Enqueued::Added);
        }
        assert_eq!(queue.status().pending_count, 6);
        assert_eq!(deliver_all(&mut queue, at(0)), writes);
        assert!(queue.is_empty());
        assert_eq!(queue.status(), OutboxStatus::default());
        assert!(queue.status().is_clean());
    }

    #[test]
    fn a_retryable_failure_keeps_the_head_and_waits_for_the_backoff() {
        let session = SessionId::new_v7();
        let mut queue = Queue::default();
        let second = set(session, 0);
        queue.enqueue(start(session), at(0));
        queue.enqueue(second.clone(), at(0));
        let head = queue.next_ready(at(0)).cloned().unwrap();
        assert_eq!(head, start(session));

        queue.on_failure(&head, retry("Cannot reach the server."), at(0), 1.0, &B);
        assert_eq!(queue.wake(at(0)), Wake::At(at(1_000)));
        assert_eq!(queue.next_ready(at(999)), None);
        assert_eq!(queue.next_ready(at(1_000)), Some(&head));
        let status = queue.status();
        assert_eq!(status.pending_count, 2);
        assert_eq!(status.failed_count, 0);
        assert_eq!(
            status.last_error.as_deref(),
            Some("Cannot reach the server.")
        );

        // The second failure doubles the ceiling.
        queue.on_failure(&head, retry("x"), at(1_000), 1.0, &B);
        assert_eq!(queue.wake(at(1_000)), Wake::At(at(3_000)));

        // Delivered: the retry state resets and the next write follows.
        queue.on_success(&head);
        assert_eq!(queue.next_ready(at(1_001)), Some(&second));
        assert_eq!(queue.status().last_error, None);
        assert_eq!(queue.wake(at(1_001)), Wake::Now);
    }

    #[test]
    fn a_stale_success_keeps_the_next_heads_retry_after() {
        // Another tab delivered `first` and got a `429` on the next head; this tab's late
        // success for `first` must not clear that wait (#104).
        let session = SessionId::new_v7();
        let (first, second) = (start(session), set(session, 0));
        let mut queue = Queue::default();
        queue.enqueue(first.clone(), at(0));
        queue.enqueue(second.clone(), at(0));
        queue.on_success(&first);
        queue.on_failure(
            &second,
            Failure::Retry {
                message: "Too many requests.".to_owned(),
                retry_after: Some(Duration::from_secs(30)),
            },
            at(0),
            0.0,
            &B,
        );
        let waiting = queue.clone();
        assert_eq!(queue.wake(at(0)), Wake::At(at(30_000)));

        queue.on_success(&first);
        assert_eq!(queue.wake(at(0)), Wake::At(at(30_000)));
        assert_eq!(queue.next_ready(at(0)), None);
        assert_eq!(
            queue.status().last_error.as_deref(),
            Some("Too many requests.")
        );
        // Merged with the other tab's copy, the wait still holds.
        assert_eq!(
            Queue::merge(queue, waiting).wake(at(0)),
            Wake::At(at(30_000))
        );
    }

    #[test]
    fn retry_after_takes_precedence_and_survives_a_nudge() {
        let session = SessionId::new_v7();
        let mut queue = Queue::default();
        queue.enqueue(start(session), at(0));
        let head = start(session);
        queue.on_failure(
            &head,
            Failure::Retry {
                message: "Too many requests.".to_owned(),
                retry_after: Some(Duration::from_secs(30)),
            },
            at(0),
            0.0,
            &B,
        );
        assert_eq!(queue.wake(at(0)), Wake::At(at(30_000)));
        // Online again, or the app restarted: the 429's delay still holds.
        queue.nudge();
        assert_eq!(queue.wake(at(10_000)), Wake::At(at(30_000)));
        assert_eq!(queue.next_ready(at(30_000)), Some(&head));
    }

    #[test]
    fn a_nudge_skips_the_backoff() {
        let session = SessionId::new_v7();
        let mut queue = Queue::default();
        queue.enqueue(start(session), at(0));
        queue.on_failure(&start(session), retry("offline"), at(0), 1.0, &B);
        for _ in 0..8 {
            queue.on_failure(&start(session), retry("offline"), at(0), 1.0, &B);
        }
        assert_eq!(queue.wake(at(0)), Wake::At(at(256_000)));
        queue.nudge();
        assert_eq!(queue.wake(at(0)), Wake::Now);
    }

    #[test]
    fn a_rejection_stops_the_queue_and_keeps_the_write() {
        let session = SessionId::new_v7();
        let (first, second) = (set(session, 0), set(session, 1));
        let mut queue = Queue::default();
        queue.enqueue(first.clone(), at(0));
        queue.enqueue(second.clone(), at(0));
        queue.on_failure(
            &first,
            rejected("This session has already ended."),
            at(0),
            0.5,
            &B,
        );

        // Stopped: nothing is sent, not even the next write, and nothing was dropped.
        assert_eq!(queue.wake(at(0)), Wake::Idle);
        assert_eq!(queue.next_ready(at(1_000_000)), None);
        queue.nudge();
        assert_eq!(queue.next_ready(at(1_000_000)), None);
        let status = queue.status();
        assert_eq!(status.pending_count, 2);
        assert_eq!(status.failed_count, 1);
        assert_eq!(
            status.last_error.as_deref(),
            Some("This session has already ended.")
        );
        assert!(!status.is_clean());

        // The user retries: the same write is sent again, then the rest in order.
        queue.retry_failed();
        assert_eq!(deliver_all(&mut queue, at(0)), vec![first, second]);
    }

    #[test]
    fn discarding_removes_only_the_failed_writes() {
        let session = SessionId::new_v7();
        let (first, second) = (set(session, 0), set(session, 1));
        let mut queue = Queue::default();
        queue.enqueue(first.clone(), at(0));
        queue.enqueue(second.clone(), at(0));
        queue.on_failure(
            &first,
            rejected("Some values are not valid."),
            at(0),
            0.5,
            &B,
        );
        assert_eq!(queue.discard_failed(), vec![first]);
        assert_eq!(deliver_all(&mut queue, at(0)), vec![second]);
    }

    #[test]
    fn signed_out_pauses_without_failing_anything() {
        let session = SessionId::new_v7();
        let mut queue = Queue::default();
        queue.enqueue(start(session), at(0));
        queue.on_failure(
            &start(session),
            Failure::SignedOut {
                message: "Please sign in.".to_owned(),
            },
            at(0),
            0.5,
            &B,
        );
        assert_eq!(queue.wake(at(1_000_000)), Wake::Idle);
        let status = queue.status();
        assert_eq!(status.failed_count, 0);
        assert_eq!(status.last_error.as_deref(), Some("Please sign in."));
        // Signed in again.
        queue.nudge();
        assert_eq!(deliver_all(&mut queue, at(0)), vec![start(session)]);
    }

    #[test]
    fn the_same_write_is_queued_once_and_replays_are_harmless() {
        let session = SessionId::new_v7();
        let write = set(session, 0);
        let mut queue = Queue::default();
        assert_eq!(queue.enqueue(write.clone(), at(0)), Enqueued::Added);
        assert_eq!(queue.enqueue(write.clone(), at(5)), Enqueued::Duplicate);
        assert!(queue.contains(write.key()));
        assert_eq!(queue.status().pending_count, 1);

        // The answer was lost (a 503 after the commit): the very same write, same id, is sent again.
        let sent = queue.next_ready(at(0)).cloned().unwrap();
        queue.on_failure(&sent, retry("busy"), at(0), 0.0, &B);
        let resent = queue.next_ready(at(0)).cloned().unwrap();
        assert_eq!(resent, sent);
        assert_eq!(resent.key(), write.key());
        queue.on_success(&resent);
        // Another tab delivered it too: a second success changes nothing.
        queue.on_success(&resent);
        assert!(queue.is_empty());
        assert!(!queue.contains(write.key()));
    }

    #[test]
    fn keys_identify_writes_by_their_client_ids() {
        let session = SessionId::new_v7();
        assert_eq!(start(session).key(), WriteKey::StartSession(session));
        assert_eq!(finish(session).key(), WriteKey::FinishSession(session));
        let Write::SaveSet { set: logged, .. } = set(session, 3) else {
            unreachable!()
        };
        let write = Write::SaveSet {
            session_id: session,
            set: logged.clone(),
        };
        assert_eq!(write.key(), WriteKey::SaveSet(logged.id));
        assert_eq!(write.session_id(), session);
    }

    #[test]
    fn the_queue_round_trips_through_json() {
        let session = SessionId::new_v7();
        let mut queue = Queue::default();
        queue.enqueue(start(session), at(0));
        queue.enqueue(set(session, 0), at(1));
        queue.enqueue(finish(session), at(2));
        queue.on_failure(&start(session), retry("offline"), at(3), 0.5, &B);
        let (decoded, rejected) = Queue::from_json(queue.to_json().unwrap());
        assert!(rejected.is_empty());
        assert_eq!(decoded, queue);
    }

    #[test]
    fn entries_that_do_not_decode_are_set_aside_and_the_rest_kept_in_order() {
        let session = SessionId::new_v7();
        let (first, last) = (start(session), finish(session));
        let mut json = serde_json::json!({
            "entries": [
                serde_json::to_value(Entry { write: first.clone(), enqueued_at: at(0), seq: 1, rev: 1, failed: None }).unwrap(),
                { "write": { "kind": "teleport" }, "enqueued_at": 1 },
                42,
                serde_json::to_value(Entry { write: last.clone(), enqueued_at: at(2), seq: 2, rev: 2, failed: None }).unwrap(),
            ],
            "failures": "many",
        });
        let (queue, rejected) = Queue::from_json(json.take());
        assert_eq!(rejected.len(), 2);
        assert_eq!(
            queue
                .entries()
                .map(|entry| entry.write.clone())
                .collect::<Vec<_>>(),
            vec![first, last]
        );
        // The retry state did not decode: it starts afresh.
        assert_eq!(queue.wake(at(0)), Wake::Now);

        let (queue, rejected) = Queue::from_json(serde_json::json!("garbage"));
        assert!(queue.is_empty());
        assert_eq!(rejected, vec![serde_json::json!("garbage")]);
    }

    fn edited(write: &Write) -> Write {
        let Write::SaveSet { session_id, set } = write else {
            unreachable!()
        };
        Write::SaveSet {
            session_id: *session_id,
            set: LoggedSet {
                reps: Reps::new(3),
                ..set.clone()
            },
        }
    }

    #[test]
    fn discarding_a_refused_start_discards_its_session_in_one_action() {
        let (a, b) = (SessionId::new_v7(), SessionId::new_v7());
        let (a0, a1) = (set(a, 0), set(a, 1));
        let mut queue = Queue::default();
        for write in [start(a), a0.clone(), a1.clone(), finish(a), start(b)] {
            queue.enqueue(write, at(0));
        }
        queue.on_failure(
            &start(a),
            rejected("Another session is in progress."),
            at(0),
            0.5,
            &B,
        );
        // What the confirmation counts.
        assert_eq!(queue.discard_plan(), vec![&start(a), &a0, &a1, &finish(a)]);
        assert_eq!(queue.discard_failed(), vec![start(a), a0, a1, finish(a)]);
        // The next session goes out at once, not behind orphaned writes.
        assert_eq!(deliver_all(&mut queue, at(0)), vec![start(b)]);
    }

    #[test]
    fn discarding_a_refused_set_keeps_the_rest_of_its_session() {
        let session = SessionId::new_v7();
        let (s0, s1) = (set(session, 0), set(session, 1));
        let mut queue = Queue::default();
        for write in [start(session), s0.clone(), s1.clone(), finish(session)] {
            queue.enqueue(write, at(0));
        }
        let first = queue.next_ready(at(0)).cloned().unwrap();
        queue.on_success(&first);
        queue.on_failure(&s0, rejected("Some values are not valid."), at(0), 0.5, &B);
        assert_eq!(queue.discard_plan(), vec![&s0]);
        assert_eq!(queue.discard_failed(), vec![s0]);
        assert_eq!(deliver_all(&mut queue, at(0)), vec![s1, finish(session)]);
    }

    #[test]
    fn an_edited_set_replaces_its_queued_version() {
        let session = SessionId::new_v7();
        let original = set(session, 0);
        let edit = edited(&original);
        let mut queue = Queue::default();
        queue.enqueue(start(session), at(0));
        queue.enqueue(original.clone(), at(0));
        queue.enqueue(finish(session), at(0));
        assert_eq!(queue.enqueue(edit.clone(), at(1)), Enqueued::Replaced);
        // One write per id, in its original turn, with the new values.
        assert_eq!(
            deliver_all(&mut queue, at(0)),
            vec![start(session), edit, finish(session)]
        );
    }

    #[test]
    fn an_edit_made_while_the_original_was_being_saved_is_held_as_refused() {
        let session = SessionId::new_v7();
        let original = set(session, 0);
        let mut queue = Queue::default();
        queue.enqueue(original.clone(), at(0));
        let in_flight = queue.next_ready(at(0)).cloned().unwrap();
        queue.enqueue(edited(&original), at(1));
        // The original values reached the server: the edit would be a certain 409.
        queue.on_success(&in_flight);
        let status = queue.status();
        assert_eq!(status.failed_count, 1);
        assert_eq!(
            status.last_error.as_deref(),
            Some(EDITED_AFTER_SAVE_MESSAGE)
        );
        assert_eq!(queue.next_ready(at(0)), None);
    }

    #[test]
    fn a_memory_copy_merges_into_the_stored_queue_without_losing_either_side() {
        let session = SessionId::new_v7();
        let (mine, theirs) = (set(session, 0), set(session, 1));
        let mut stored = Queue::default();
        stored.enqueue(start(session), at(0));
        stored.enqueue(theirs.clone(), at(1));
        let mut memory = Queue::default();
        memory.enqueue(start(session), at(0));
        memory.enqueue(mine.clone(), at(2));
        let merged = Queue::merge(memory, stored);
        assert_eq!(
            merged
                .entries()
                .map(|entry| entry.write.clone())
                .collect::<Vec<_>>(),
            vec![start(session), theirs, mine]
        );
    }

    #[test]
    fn a_start_stored_before_its_choice_existed_still_loads() {
        let session = SessionId::new_v7();
        let stored = serde_json::json!({
            "kind": "start_session",
            "session_id": session,
            "started_at": 1_000,
        });
        assert_eq!(
            serde_json::from_value::<Write>(stored).unwrap(),
            start(session)
        );
        // Without a choice, nothing new is written either.
        let json = serde_json::to_value(start(session)).unwrap();
        assert!(json.get("choice").is_none());
    }

    /// The outbox sends a set exactly as the screen logged it, its prescribed target (#60)
    /// included, and a stored queue keeps it.
    #[test]
    fn a_queued_set_keeps_its_target() {
        use iron_oxide_domain::Weight;
        use iron_oxide_domain::progression::{SetGoal, SetTarget};
        let session = SessionId::new_v7();
        let Write::SaveSet { set, .. } = set(session, 0) else {
            unreachable!()
        };
        let target = SetTarget {
            weight: Some(Weight::from_kg(102.5).unwrap()),
            goal: SetGoal::Reps {
                reps: Reps::new(5),
                range: None,
            },
        };
        let write = Write::SaveSet {
            session_id: session,
            set: LoggedSet {
                target: Some(target),
                ..set
            },
        };
        let mut queue = Queue::default();
        queue.enqueue(write.clone(), at(3_000));
        let stored = queue.to_json().unwrap();
        let (loaded, unreadable) = Queue::from_json(stored);
        assert!(unreadable.is_empty());
        assert_eq!(loaded.next_ready(at(3_000)), Some(&write));
    }
}
