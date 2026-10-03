//! Resilience (#30): the session in progress kept on the device, and an outbox that delivers
//! the session writes when the server can be reached, in order and exactly as made.
//!
//! # For screens: write through the outbox
//!
//! Screens never call `start_session`, `save_set` or `finish_session` directly. They build the
//! write with fresh client ids and the device's clock, and enqueue it:
//!
//! ```ignore
//! let outbox = use_outbox();
//! let set = LoggedSet { id: SetId::new_v7(), completed_at: now, .. };
//! local_session.record_set(set.clone()); // and save it, see below
//! outbox.enqueue(Write::SaveSet { session_id, set })?; // Err(NotSignedIn) when signed out
//! ```
//!
//! - [`Outbox::enqueue`] stores the write on the device before it returns, then sends it in
//!   the background. Enqueueing the same write twice does nothing.
//! - **Edits.** Enqueueing a set again with the same `SetId` and edited values replaces the
//!   queued payload in place, so only one version of the id is ever sent. If the original
//!   values were already delivered, the server keeps them: an edit enqueued while they were in
//!   flight is held as refused with `queue::EDITED_AFTER_SAVE_MESSAGE`, and one enqueued after
//!   delivery is answered `409` and shown. Editing a saved set needs its own server write,
//!   which does not exist yet.
//! - Ids come from the domain's `new_v7()` (`SessionId`, `SetId`) and timestamps from the
//!   client. A retry resends exactly the same arguments, which the server answers unchanged
//!   (`docs/api.md`, "Idempotency"). Never regenerate an id or a time for a retry.
//! - Writes go out one at a time, oldest first, across sessions: a set never arrives before its
//!   session's start, and a finish always arrives before the next session's start.
//! - Results are not returned. A screen that needs one, such as the summary after
//!   `finish_session`, waits until [`Outbox::is_pending`] turns false, then calls the server
//!   function again with the same arguments. It is a replay, so it returns the same answer.
//!   Online, that is a fraction of a second.
//! - Reading the state: [`Outbox::status`] (or [`Outbox::pending_count`] and
//!   [`Outbox::last_error`]) is reactive. The [`crate::ui::unsaved::Unsaved`] indicator shows
//!   it whenever something is pending or failed.
//! - Rejected writes (`409`, `422`, …) stop the queue at that write, with the server's message
//!   in `last_error`. They are never dropped on their own. The user either fixes the cause and
//!   calls [`Outbox::retry_failed`], or gives the writes up with [`Outbox::discard_failed`].
//!   Discarding also gives up the queued writes that cannot succeed without them: a refused
//!   start takes its session's sets and finish (the server would answer each `404`).
//!   [`Outbox::discard_count`] gives the number for the confirmation. [`Outbox::retry_now`]
//!   skips the backoff.
//!
//! The session screen (`crate::ui::session::local`) keeps a [`LocalSession`] and saves it after
//! every change (`LocalSession::save(storage, user)`, with [`platform::with_storage`]). On load it
//! restores the session, then reconciles with `get_in_progress_session`. Finishing marks it
//! `finished`; it is cleared once the finish is delivered. Signing out clears it as well
//! ([`Outbox::signed_out`]).
//!
//! # Delivery
//!
//! - Retryable failures (network, `408`, `429`, `502`-`504`) retry with exponential backoff and full
//!   jitter ([`backoff::Backoff::DEFAULT`]: 1 s doubling, capped at 5 min). A `429` is never
//!   retried before its `retry_after_secs`. The `online` event, the app start and a sign-in
//!   retry at once, but never before a `429`'s delay.
//! - `401` pauses the queue until the user signs in again. Before **each** send, the drain
//!   checks with `me()` that the browser's session is still the queue's user (another tab may
//!   have switched accounts): a mismatch stops the drain and pauses the queue until the user
//!   signs in again. A failed check is retried later and never refuses the write. Each write
//!   also carries `X-Io-Expected-User` (`auth::types::EXPECTED_USER_HEADER`), so the server
//!   itself refuses it (`409` account changed) if the cookie changed after the check; the outbox
//!   treats that `409` as the same pause, not as a refusal.
//! - Everything else, `500` included (`docs/api.md` classifies it as not retryable), is a
//!   rejection.
//! - Single flight. In one tab a single task sends. Across tabs, a Web Lock
//!   (`navigator.locks`, `ifAvailable`) per user lets only one tab drain at a time. Without the
//!   Web Locks API (old browsers, insecure origins), two tabs may send the same write. The
//!   server answers the second copy unchanged, so the only cost is a request.
//! - Every change to the queue re-reads it from storage, applies the change and writes it back
//!   in one synchronous step, and the `storage` event refreshes the other tabs. That step is not
//!   a cross-tab lock (`localStorage` has none; Web Locks are asynchronous, and `enqueue` stores
//!   the write before it returns), so two tabs writing at the same instant can overwrite each
//!   other's change. Each tab's next load (its own change, a refresh, the `storage` event the
//!   overwrite fires) stores the entries it holds that the stored copy lost; only closing that
//!   tab first loses them (`storage::QueueStore`). The memory copy and the stored one are merged by
//!   revision (`queue::Queue::merge`: per-entry and retry-state revisions from a Lamport clock,
//!   tombstones for delivered and discarded writes), so a stale copy, such as a tab that could
//!   not save for a while, never undoes a wait, a refusal, an edit, a delivery or a discard.
//!   A delivery covers every earlier version of its id. Discard tombstones are kept for good;
//!   only the newest 256 delivered ones are, so a long-stale copy can at worst bring back a
//!   delivered write. It is then sent once more, which is harmless: every write kind is
//!   idempotent on its id (`start_session` returns the session it created, even ended;
//!   `save_set` accepts the same set again, even after the session ended; `finish_session`
//!   returns the same summary for the same outcome and time).
//!
//! # Storage
//!
//! `localStorage`, per user (`iron-oxide:outbox:<user id>`, `iron-oxide:session:<user id>`),
//! versioned records ([`storage`]). Unreadable records or entries are moved to
//! `<key>:unreadable`, which keeps the newest ten per key (`storage::MAX_UNREADABLE`; older ones
//! are dropped with a warning). When storage is blocked (the memory fallback) or full, the outbox
//! carries on in memory and says so in `last_error` ("Not saved on this device"), after the
//! delivery error if there is one. A user's undelivered writes stay on the device after sign-out
//! and are sent at their next sign-in.
//!
//! The pure parts ([`backoff`], [`queue`], [`storage`], [`session`]) have no browser
//! dependency and are unit-tested on the host. [`platform`] holds the browser calls and
//! [`outbox`] the Dioxus glue.

// The screens that enqueue writes and keep the local session land with #28 and #29.
#![allow(dead_code)]

pub mod backoff;
pub mod outbox;
pub mod platform;
pub mod queue;
pub mod session;
pub mod storage;

#[allow(unused_imports, reason = "the API screens use (#28, #29)")]
pub use outbox::{NotSignedIn, Outbox, use_outbox, use_outbox_provider};
#[allow(unused_imports, reason = "the API screens use (#28, #29)")]
pub use queue::{OutboxStatus, Write, WriteKey};
#[allow(unused_imports, reason = "the API screens use (#28, #29)")]
pub use session::{LocalFinish, LocalSession};
