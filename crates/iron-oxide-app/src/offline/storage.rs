//! Versioned, per-user records in a key-value store (`localStorage` in the browser, memory
//! elsewhere and when it is unavailable), tolerant of missing, unreadable and full storage.
//!
//! Every record is `{"v": <version>, "data": …}` under a key that names the user:
//! `iron-oxide:<record>:<user id>`. A record that does not parse, or has a version this build
//! does not read, is moved aside to `<key>:unreadable` and reads as missing. The newest
//! [`MAX_UNREADABLE`] records set aside are kept per key; older ones are dropped, with a warning,
//! so a record that keeps failing cannot fill the storage.
//! Bump the record's version when its shape changes incompatibly, and teach its reader the old
//! one if the data is worth keeping.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use dioxus::logger::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::queue::{OutboxStatus, Queue, WriteKey};
use crate::auth::types::UserId;

/// A failed storage call: storage blocked (private mode, disabled cookies) or full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageError(pub String);

/// A string key-value store with `localStorage`'s semantics.
pub trait Storage {
    /// The value under `key`, if any.
    ///
    /// # Errors
    /// When the storage cannot be read.
    fn get(&self, key: &str) -> Result<Option<String>, StorageError>;

    /// Stores `value` under `key`.
    ///
    /// # Errors
    /// When the storage cannot be written (full, blocked).
    fn set(&self, key: &str, value: &str) -> Result<(), StorageError>;

    /// Removes `key`.
    ///
    /// # Errors
    /// When the storage cannot be written.
    fn remove(&self, key: &str) -> Result<(), StorageError>;

    /// Whether what is stored survives a reload. `false` for the memory fallback used when
    /// `localStorage` is blocked.
    fn is_persistent(&self) -> bool {
        true
    }
}

/// An in-memory store: the fallback when `localStorage` is unavailable, and the tests' store.
#[derive(Debug, Default)]
pub struct MemoryStorage {
    items: RefCell<BTreeMap<String, String>>,
    /// Test switch: every write fails, as with a full `localStorage`.
    full: Cell<bool>,
    /// The fallback for a blocked `localStorage`: lost on reload.
    volatile: bool,
}

impl MemoryStorage {
    /// The fallback when `localStorage` is unavailable: works, but [`Storage::is_persistent`]
    /// says it is lost on reload.
    #[must_use]
    pub fn volatile() -> Self {
        Self {
            volatile: true,
            ..Self::default()
        }
    }

    /// Makes every write fail (`true`) or succeed again.
    #[cfg(test)]
    pub fn set_full(&self, full: bool) {
        self.full.set(full);
    }
}

impl Storage for MemoryStorage {
    fn get(&self, key: &str) -> Result<Option<String>, StorageError> {
        Ok(self.items.borrow().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), StorageError> {
        if self.full.get() {
            return Err(StorageError("QuotaExceededError".to_owned()));
        }
        self.items
            .borrow_mut()
            .insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    fn remove(&self, key: &str) -> Result<(), StorageError> {
        self.items.borrow_mut().remove(key);
        Ok(())
    }

    fn is_persistent(&self) -> bool {
        !self.volatile
    }
}

/// The record under the user's key.
#[must_use]
pub fn key(record: &str, user: UserId) -> String {
    format!("iron-oxide:{record}:{user}")
}

/// The key the unreadable data of `key` is moved to.
#[must_use]
pub fn unreadable_key(key: &str) -> String {
    format!("{key}:unreadable")
}

/// How many unreadable records are kept aside per key: the newest; older ones are dropped (with a
/// warning). Ten is plenty to recover from a bad release by hand, and bounds what a record that
/// keeps failing can take of the storage quota.
pub const MAX_UNREADABLE: usize = 10;

#[derive(Serialize, Deserialize)]
struct Envelope {
    v: u32,
    data: Value,
}

/// Reads the record under `key` written with `version`. Missing data is `Ok(None)`; unreadable
/// data or another version is moved aside ([`unreadable_key`]) and also reads as `Ok(None)`.
///
/// # Errors
/// When the storage cannot be read.
pub fn read(storage: &dyn Storage, key: &str, version: u32) -> Result<Option<Value>, StorageError> {
    let Some(raw) = storage.get(key)? else {
        return Ok(None);
    };
    match serde_json::from_str::<Envelope>(&raw) {
        Ok(envelope) if envelope.v == version => Ok(Some(envelope.data)),
        Ok(envelope) => {
            tracing::warn!(
                key,
                version = envelope.v,
                "unknown record version, kept aside"
            );
            set_aside(storage, key, Value::String(raw));
            Ok(None)
        }
        Err(error) => {
            tracing::warn!(key, %error, "unreadable record, kept aside");
            set_aside(storage, key, Value::String(raw));
            Ok(None)
        }
    }
}

/// Writes `data` under `key` as `version`.
///
/// # Errors
/// When the storage cannot be written (full, blocked).
pub fn write(
    storage: &dyn Storage,
    key: &str,
    version: u32,
    data: Value,
) -> Result<(), StorageError> {
    let raw = serde_json::to_string(&Envelope { v: version, data })
        .map_err(|error| StorageError(error.to_string()))?;
    storage.set(key, &raw)
}

/// Keeps `data` under [`unreadable_key`] (best effort: a full storage loses it, with a warning),
/// and removes `key`.
pub fn set_aside(storage: &dyn Storage, key: &str, data: Value) {
    let aside = unreadable_key(key);
    let mut kept: Vec<Value> = storage
        .get(&aside)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    kept.push(data);
    let excess = kept.len().saturating_sub(MAX_UNREADABLE);
    if excess > 0 {
        tracing::warn!(
            key,
            dropped = excess,
            "too many unreadable records, the oldest dropped"
        );
    }
    kept.drain(..excess);
    let stored = serde_json::to_string(&kept)
        .map_err(|error| StorageError(error.to_string()))
        .and_then(|raw| storage.set(&aside, &raw));
    if let Err(error) = stored {
        tracing::warn!(key, ?error, "could not keep the unreadable record aside");
    }
    let _ = storage.remove(key);
}

/// Shown while the outbox cannot be saved on the device.
pub const NOT_PERSISTED_MESSAGE: &str =
    "Not saved on this device yet. Keep the app open until it syncs.";

/// One user's outbox in storage. Every change reads the stored queue, applies the change and
/// writes it back in the same synchronous step, so writes queued by another tab are kept.
///
/// That step is not a cross-tab lock: `localStorage` has none, and the Web Locks API is
/// asynchronous while [`super::Outbox::enqueue`] stores the write before it returns. Two tabs
/// writing at the very same moment can overwrite each other's change. Each tab keeps its own copy,
/// though, and every load (a change, a refresh, the other tab's `storage` event) merges it with
/// the stored one and writes back the entries the stored one lost. An overwritten write is
/// therefore stored again as soon as its tab next looks at the queue; it is lost only if that tab
/// is closed first.
///
/// When storage fails (blocked, full, or only the memory fallback is there), the queue lives on
/// in memory: changes keep working for as long as the page is open, and
/// [`OutboxStatus::last_error`] says they are not on the device. Storage is still read before
/// every change and merged with the memory copy by revision ([`Queue::merge`]), so neither
/// another tab's writes nor this tab's own progress (waits, refusals, edits, deliveries,
/// discards) are lost or undone, whichever copy is stale.
#[derive(Debug)]
pub struct QueueStore {
    key: String,
    /// The queue as last seen. Authoritative while `not_persisted`.
    mirror: Queue,
    not_persisted: bool,
}

impl QueueStore {
    /// The version of the outbox record.
    pub const VERSION: u32 = 1;

    #[must_use]
    pub fn new(user: UserId) -> Self {
        Self {
            key: key("outbox", user),
            mirror: Queue::default(),
            not_persisted: false,
        }
    }

    /// The storage key, to recognise changes made by other tabs.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The current queue: the stored one, or the memory copy while storage fails. Entries that do
    /// not decode are moved aside. Entries the stored copy lost (another tab overwrote it with a
    /// copy read just before this tab's change) are stored again.
    pub fn load(&mut self, storage: &dyn Storage) -> &Queue {
        match read(storage, &self.key, Self::VERSION) {
            Ok(stored) => {
                let (queue, unreadable) = stored.map(Queue::from_json).unwrap_or_default();
                let stored_keys: Vec<WriteKey> =
                    queue.entries().map(|entry| entry.write.key()).collect();
                // Always merged: the stored copy may be newer (another tab) or older (this tab
                // could not save); revisions and tombstones decide (`Queue::merge`).
                self.mirror = Queue::merge(std::mem::take(&mut self.mirror), queue);
                // Only lost entries are written back, not every difference: two tabs whose retry
                // states tie would otherwise keep overwriting each other through `storage`
                // events. The set of entries only grows until a tombstone removes one, so this
                // settles.
                let lost = self
                    .mirror
                    .entries()
                    .any(|entry| !stored_keys.contains(&entry.write.key()));
                if lost && unreadable.is_empty() {
                    tracing::info!(
                        key = self.key,
                        "outbox entries lost by another tab, stored again"
                    );
                    self.save(storage);
                }
                if !unreadable.is_empty() {
                    tracing::warn!(
                        key = self.key,
                        count = unreadable.len(),
                        "unreadable outbox entries, kept aside"
                    );
                    for entry in unreadable {
                        set_aside(storage, &self.key, entry);
                    }
                    self.save(storage);
                }
            }
            // Storage blocked: keep using the memory copy.
            Err(_) => self.not_persisted = true,
        }
        &self.mirror
    }

    /// Applies `change` to the current queue and stores the result.
    pub fn update<R>(&mut self, storage: &dyn Storage, change: impl FnOnce(&mut Queue) -> R) -> R {
        self.load(storage);
        let result = change(&mut self.mirror);
        self.save(storage);
        result
    }

    /// The queue as last loaded or changed.
    #[must_use]
    pub const fn queue(&self) -> &Queue {
        &self.mirror
    }

    /// What the indicator shows.
    #[must_use]
    pub fn status(&self) -> OutboxStatus {
        let mut status = self.mirror.status();
        if self.not_persisted && status.pending_count > 0 {
            // Both: why the writes are not delivered, and that closing the page loses them.
            status.last_error = Some(match status.last_error.take() {
                Some(error) => format!("{error} {NOT_PERSISTED_MESSAGE}"),
                None => NOT_PERSISTED_MESSAGE.to_owned(),
            });
        }
        status
    }

    fn save(&mut self, storage: &dyn Storage) {
        let saved = if self.mirror.is_blank() {
            storage.remove(&self.key)
        } else {
            self.mirror
                .to_json()
                .map_err(|error| StorageError(error.to_string()))
                .and_then(|data| write(storage, &self.key, Self::VERSION, data))
        };
        match saved {
            Ok(()) => self.not_persisted = !storage.is_persistent(),
            Err(error) => {
                if !self.not_persisted {
                    tracing::warn!(key = self.key, ?error, "outbox kept in memory only");
                }
                self.not_persisted = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::offline::backoff::Backoff;
    use crate::offline::queue::tests::{at, finish, set, start};
    use crate::offline::queue::{Enqueued, Failure, OutboxStatus, Queue, Wake, Write};
    use iron_oxide_domain::{LoggedSet, SessionId};

    fn user() -> UserId {
        UserId::from_uuid(uuid::Uuid::from_u128(7))
    }

    #[test]
    fn keys_are_per_user() {
        let other = UserId::from_uuid(uuid::Uuid::from_u128(8));
        assert_eq!(
            key("outbox", user()),
            "iron-oxide:outbox:00000000-0000-0000-0000-000000000007"
        );
        assert_ne!(key("outbox", user()), key("outbox", other));
        assert_ne!(key("outbox", user()), key("session", user()));
    }

    #[test]
    fn a_record_round_trips_with_its_version() {
        let storage = MemoryStorage::default();
        assert_eq!(read(&storage, "k", 1), Ok(None));
        write(&storage, "k", 1, serde_json::json!({"a": 1})).unwrap();
        assert_eq!(
            read(&storage, "k", 1),
            Ok(Some(serde_json::json!({"a": 1})))
        );
    }

    #[test]
    fn corrupt_or_other_version_data_is_kept_aside_and_reads_as_missing() {
        let storage = MemoryStorage::default();
        storage.set("k", "{not json").unwrap();
        assert_eq!(read(&storage, "k", 1), Ok(None));
        assert_eq!(storage.get("k"), Ok(None));
        write(&storage, "k", 2, serde_json::json!([])).unwrap();
        assert_eq!(read(&storage, "k", 1), Ok(None));
        let aside: Vec<Value> =
            serde_json::from_str(&storage.get("k:unreadable").unwrap().unwrap()).unwrap();
        assert_eq!(aside.len(), 2);
        assert_eq!(aside[0], Value::String("{not json".to_owned()));
        // Bounded: the newest are kept, the oldest dropped (#104).
        for index in 0..20 {
            storage.set("k", &format!("x{index}")).unwrap();
            assert_eq!(read(&storage, "k", 1), Ok(None));
        }
        let aside: Vec<Value> =
            serde_json::from_str(&storage.get("k:unreadable").unwrap().unwrap()).unwrap();
        let newest: Vec<Value> = (10..20)
            .map(|index| Value::String(format!("x{index}")))
            .collect();
        assert_eq!(aside.len(), MAX_UNREADABLE);
        assert_eq!(aside, newest);
    }

    #[test]
    fn the_outbox_recovers_from_corrupt_storage() {
        let storage = MemoryStorage::default();
        let mut store = QueueStore::new(user());
        storage.set(store.key(), "\u{0}garbage").unwrap();
        assert!(store.load(&storage).is_empty());
        // Usable again, and the garbage is kept aside.
        let session = SessionId::new_v7();
        store.update(&storage, |queue| queue.enqueue(start(session), at(0)));
        let mut fresh = QueueStore::new(user());
        assert_eq!(fresh.load(&storage).status().pending_count, 1);
        assert!(storage.get(&unreadable_key(store.key())).unwrap().is_some());
    }

    #[test]
    fn unreadable_entries_are_kept_aside_and_the_others_kept() {
        let storage = MemoryStorage::default();
        let mut store = QueueStore::new(user());
        let session = SessionId::new_v7();
        store.update(&storage, |queue| {
            queue.enqueue(start(session), at(0));
            queue.enqueue(set(session, 0), at(0));
        });
        let mut raw: Value =
            serde_json::from_str(&storage.get(store.key()).unwrap().unwrap()).unwrap();
        raw["data"]["entries"][0] = serde_json::json!({"write": "?"});
        storage.set(store.key(), &raw.to_string()).unwrap();

        let mut fresh = QueueStore::new(user());
        assert_eq!(fresh.load(&storage).status().pending_count, 1);
        // The repair is stored, so the bad entry is set aside once.
        let mut again = QueueStore::new(user());
        assert_eq!(again.load(&storage).status().pending_count, 1);
        let aside: Vec<Value> =
            serde_json::from_str(&storage.get(&unreadable_key(store.key())).unwrap().unwrap())
                .unwrap();
        assert_eq!(aside, vec![serde_json::json!({"write": "?"})]);
    }

    #[test]
    fn changes_from_another_tab_are_kept() {
        let storage = MemoryStorage::default();
        let (mut tab_a, mut tab_b) = (QueueStore::new(user()), QueueStore::new(user()));
        let session = SessionId::new_v7();
        tab_a.load(&storage);
        tab_b.update(&storage, |queue| queue.enqueue(start(session), at(0)));
        tab_a.update(&storage, |queue| queue.enqueue(set(session, 0), at(1)));
        assert_eq!(tab_b.load(&storage).status().pending_count, 2);
    }

    #[test]
    fn a_full_storage_keeps_the_outbox_in_memory_and_says_so() {
        let storage = MemoryStorage::default();
        let mut store = QueueStore::new(user());
        let session = SessionId::new_v7();
        storage.set_full(true);
        store.update(&storage, |queue| queue.enqueue(start(session), at(0)));
        store.update(&storage, |queue| queue.enqueue(set(session, 0), at(0)));
        let status = store.status();
        assert_eq!(status.pending_count, 2);
        assert_eq!(status.last_error.as_deref(), Some(NOT_PERSISTED_MESSAGE));

        // Space again: the next change stores everything.
        storage.set_full(false);
        store.update(&storage, |queue| queue.enqueue(set(session, 1), at(0)));
        assert_eq!(store.status().last_error, None);
        assert_eq!(
            QueueStore::new(user())
                .load(&storage)
                .status()
                .pending_count,
            3
        );
    }

    #[test]
    fn a_delivery_error_does_not_hide_the_not_saved_warning() {
        let storage = MemoryStorage::default();
        let mut store = QueueStore::new(user());
        let session = SessionId::new_v7();
        store.update(&storage, |queue| queue.enqueue(start(session), at(0)));
        storage.set_full(true);
        let rejected = Failure::Rejected {
            message: "This session has already ended.".to_owned(),
        };
        store.update(&storage, |queue| {
            queue.on_failure(&start(session), rejected, at(0), 0.5, &Backoff::DEFAULT);
        });
        // Both: why it is not delivered, and that it is not on the device either (#104).
        assert_eq!(
            store.status().last_error,
            Some(format!(
                "This session has already ended. {NOT_PERSISTED_MESSAGE}"
            ))
        );
        let retry = Failure::Retry {
            message: "Cannot reach the server.".to_owned(),
            retry_after: None,
        };
        let mut other = QueueStore::new(user());
        other.update(&storage, |queue| queue.enqueue(start(session), at(0)));
        other.update(&storage, |queue| {
            queue.on_failure(&start(session), retry, at(0), 0.5, &Backoff::DEFAULT);
        });
        assert_eq!(
            other.status().last_error,
            Some(format!("Cannot reach the server. {NOT_PERSISTED_MESSAGE}"))
        );
    }

    #[test]
    fn an_entry_another_tab_overwrote_is_stored_again() {
        let storage = MemoryStorage::default();
        let mut tab_a = QueueStore::new(user());
        let session = SessionId::new_v7();
        let (from_a, from_b) = (start(session), set(session, 0));
        // Tab B read the (empty) queue just before tab A stored its write, and stores its own
        // change on top of that stale read: A's write is gone from storage.
        let mut stale_b = Queue::default();
        tab_a.update(&storage, |queue| queue.enqueue(from_a.clone(), at(0)));
        stale_b.enqueue(from_b.clone(), at(1));
        write(
            &storage,
            tab_a.key(),
            QueueStore::VERSION,
            stale_b.to_json().unwrap(),
        )
        .unwrap();
        let stored = |storage: &MemoryStorage| -> Vec<Write> {
            QueueStore::new(user())
                .load(storage)
                .entries()
                .map(|entry| entry.write.clone())
                .collect()
        };
        assert_eq!(stored(&storage), vec![from_b.clone()]);

        // B's write fires a `storage` event in A, which reloads: A's write is stored again.
        tab_a.load(&storage);
        assert_eq!(stored(&storage), vec![from_a, from_b]);

        // Settled: loading again, in either tab, writes nothing more.
        let raw = storage.get(tab_a.key()).unwrap();
        let mut tab_b = QueueStore::new(user());
        tab_b.load(&storage);
        tab_a.load(&storage);
        assert_eq!(storage.get(tab_a.key()).unwrap(), raw);
    }

    #[test]
    fn the_memory_fallback_says_the_outbox_is_not_on_the_device() {
        let fallback = MemoryStorage::volatile();
        let mut store = QueueStore::new(user());
        store.update(&fallback, |queue| {
            queue.enqueue(start(SessionId::new_v7()), at(0))
        });
        let status = store.status();
        assert_eq!(status.pending_count, 1);
        assert_eq!(status.last_error.as_deref(), Some(NOT_PERSISTED_MESSAGE));
    }

    #[test]
    fn a_tab_that_could_not_save_never_overwrites_another_tabs_writes() {
        let storage = MemoryStorage::default();
        let (mut tab_a, mut tab_b) = (QueueStore::new(user()), QueueStore::new(user()));
        let session = SessionId::new_v7();
        let from_b = set(session, 0);
        let from_a = set(session, 1);
        storage.set_full(true);
        tab_a.update(&storage, |queue| queue.enqueue(start(session), at(0)));
        storage.set_full(false);
        tab_b.update(&storage, |queue| queue.enqueue(from_b.clone(), at(1)));
        tab_a.update(&storage, |queue| queue.enqueue(from_a.clone(), at(2)));
        let kept: Vec<_> = QueueStore::new(user())
            .load(&storage)
            .entries()
            .map(|entry| entry.write.clone())
            .collect();
        assert_eq!(kept, vec![start(session), from_b, from_a]);
        assert_eq!(tab_a.status().last_error, None);
    }

    #[test]
    fn a_delivered_outbox_keeps_only_tombstones() {
        let storage = MemoryStorage::default();
        let mut store = QueueStore::new(user());
        let session = SessionId::new_v7();
        store.update(&storage, |queue| queue.enqueue(start(session), at(0)));
        store.update(&storage, |queue| queue.on_success(&start(session)));
        let mut fresh = QueueStore::new(user());
        assert!(fresh.load(&storage).is_empty());
        assert_eq!(fresh.status(), OutboxStatus::default());
    }

    // --- While saving fails, a stale stored record never undoes this tab's work (#90 re-check)

    /// A store whose storage holds `writes` (saved), then becomes full.
    fn stale_after(writes: &[Write]) -> (MemoryStorage, QueueStore) {
        let storage = MemoryStorage::default();
        let mut store = QueueStore::new(user());
        store.update(&storage, |queue| {
            for write in writes {
                queue.enqueue(write.clone(), at(0));
            }
        });
        storage.set_full(true);
        (storage, store)
    }

    fn writes(store: &mut QueueStore, storage: &MemoryStorage) -> Vec<Write> {
        store
            .load(storage)
            .entries()
            .map(|entry| entry.write.clone())
            .collect()
    }

    #[test]
    fn retry_after_holds_while_storage_is_full() {
        let session = SessionId::new_v7();
        let (storage, mut tab) = stale_after(&[start(session)]);
        let head = tab
            .update(&storage, |queue| queue.next_ready(at(0)).cloned())
            .unwrap();
        let limited = Failure::Retry {
            message: "429".to_owned(),
            retry_after: Some(Duration::from_secs(60)),
        };
        tab.update(&storage, |queue| {
            queue.on_failure(&head, limited, at(0), 0.5, &Backoff::DEFAULT);
        });
        assert_eq!(
            tab.update(&storage, |queue| queue.next_ready(at(2_000)).cloned()),
            None
        );
        assert_eq!(
            tab.update(&storage, |queue| queue.next_ready(at(60_000)).cloned()),
            Some(head)
        );
    }

    #[test]
    fn the_backoff_grows_while_storage_is_full() {
        let session = SessionId::new_v7();
        let (storage, mut tab) = stale_after(&[start(session)]);
        let offline = || Failure::Retry {
            message: "offline".to_owned(),
            retry_after: None,
        };
        for _ in 0..4 {
            tab.update(&storage, |queue| {
                queue.on_failure(&start(session), offline(), at(0), 1.0, &Backoff::DEFAULT);
            });
        }
        // Four failures: an 8 s ceiling, drawn at the top.
        assert_eq!(tab.load(&storage).wake(at(0)), Wake::At(at(8_000)));
    }

    #[test]
    fn a_refusal_holds_while_storage_is_full() {
        let session = SessionId::new_v7();
        let (storage, mut tab) = stale_after(&[start(session), set(session, 0)]);
        let refused = Failure::Rejected {
            message: "Another session is in progress.".to_owned(),
        };
        tab.update(&storage, |queue| {
            queue.on_failure(&start(session), refused, at(0), 0.5, &Backoff::DEFAULT);
        });
        let status = tab.load(&storage).status();
        assert_eq!(status.failed_count, 1);
        assert_eq!(tab.load(&storage).next_ready(at(0)), None);
    }

    #[test]
    fn a_delivered_write_does_not_come_back_while_storage_is_full() {
        let session = SessionId::new_v7();
        let (storage, mut tab) = stale_after(&[start(session), finish(session)]);
        tab.update(&storage, |queue| queue.on_success(&start(session)));
        for _ in 0..3 {
            assert_eq!(writes(&mut tab, &storage), vec![finish(session)]);
        }
        // Saving works again: the stored record catches up, nothing is resent.
        storage.set_full(false);
        tab.update(&storage, |queue| queue.nudge());
        assert_eq!(
            writes(&mut QueueStore::new(user()), &storage),
            vec![finish(session)]
        );
    }

    #[test]
    fn discarded_writes_do_not_come_back_while_storage_is_full() {
        let (a, b) = (SessionId::new_v7(), SessionId::new_v7());
        let (storage, mut tab) = stale_after(&[start(a), set(a, 0), finish(a), start(b)]);
        let refused = Failure::Rejected {
            message: "409".to_owned(),
        };
        tab.update(&storage, |queue| {
            queue.on_failure(&start(a), refused, at(0), 0.5, &Backoff::DEFAULT);
        });
        let discarded = tab.update(&storage, Queue::discard_failed);
        assert_eq!(discarded.len(), 3);
        assert_eq!(writes(&mut tab, &storage), vec![start(b)]);
        assert_eq!(writes(&mut tab, &storage), vec![start(b)]);
    }

    #[test]
    fn a_stale_set_never_resurfaces_next_to_its_edit_while_storage_is_full() {
        let session = SessionId::new_v7();
        let original = set(session, 0);
        let (storage, mut tab) = stale_after(&[start(session), original.clone()]);
        let Write::SaveSet { set: logged, .. } = &original else {
            unreachable!()
        };
        let edit = Write::SaveSet {
            session_id: session,
            set: LoggedSet {
                warm_up: true,
                ..logged.clone()
            },
        };
        assert_eq!(
            tab.update(&storage, |queue| queue.enqueue(edit.clone(), at(1))),
            Enqueued::Replaced
        );
        assert_eq!(writes(&mut tab, &storage), vec![start(session), edit]);
    }

    #[test]
    fn another_tabs_stale_copy_never_brings_back_what_this_tab_delivered() {
        let storage = MemoryStorage::default();
        let session = SessionId::new_v7();
        let (mut tab_a, mut tab_b) = (QueueStore::new(user()), QueueStore::new(user()));
        tab_a.update(&storage, |queue| {
            queue.enqueue(start(session), at(0));
            queue.enqueue(finish(session), at(0));
        });
        // Tab B holds both writes and cannot save; then A delivers the start.
        storage.set_full(true);
        tab_b.update(&storage, |queue| queue.nudge());
        storage.set_full(false);
        tab_a.update(&storage, |queue| queue.on_success(&start(session)));
        // B's stale copy is merged with A's record: the start stays delivered.
        let extra = set(session, 0);
        tab_b.update(&storage, |queue| queue.enqueue(extra.clone(), at(5)));
        assert_eq!(
            writes(&mut QueueStore::new(user()), &storage),
            vec![finish(session), extra]
        );
    }

    #[test]
    fn a_superseded_version_stays_gone_once_its_edit_is_delivered() {
        let session = SessionId::new_v7();
        let original = set(session, 0);
        let (storage, mut tab) = stale_after(&[start(session), original.clone()]);
        let Write::SaveSet { set: logged, .. } = &original else {
            unreachable!()
        };
        let edit = Write::SaveSet {
            session_id: session,
            set: LoggedSet {
                warm_up: true,
                ..logged.clone()
            },
        };
        tab.update(&storage, |queue| queue.on_success(&start(session)));
        assert_eq!(
            tab.update(&storage, |queue| queue.enqueue(edit.clone(), at(1))),
            Enqueued::Replaced
        );
        tab.update(&storage, |queue| queue.on_success(&edit));
        // The stale stored copy of the pre-edit set is covered by the edit's delivery.
        assert_eq!(writes(&mut tab, &storage), Vec::<Write>::new());
        assert_eq!(tab.load(&storage).status(), OutboxStatus::default());
        storage.set_full(false);
        tab.update(&storage, |queue| queue.nudge());
        assert_eq!(
            QueueStore::new(user()).load(&storage).status(),
            OutboxStatus::default()
        );
    }

    #[test]
    fn an_edit_made_after_delivery_is_held_as_refused() {
        let storage = MemoryStorage::default();
        let session = SessionId::new_v7();
        let original = set(session, 0);
        let mut tab = QueueStore::new(user());
        tab.update(&storage, |queue| queue.enqueue(original.clone(), at(0)));
        tab.update(&storage, |queue| queue.on_success(&original));
        let Write::SaveSet { set: logged, .. } = &original else {
            unreachable!()
        };
        let edit = Write::SaveSet {
            session_id: session,
            set: LoggedSet {
                warm_up: true,
                ..logged.clone()
            },
        };
        tab.update(&storage, |queue| queue.enqueue(edit.clone(), at(1)));
        let status = QueueStore::new(user()).load(&storage).status();
        assert_eq!(status.failed_count, 1);
        assert_eq!(
            status.last_error.as_deref(),
            Some(crate::offline::queue::EDITED_AFTER_SAVE_MESSAGE)
        );
    }

    #[test]
    fn a_long_suspended_tab_never_brings_back_a_discarded_write() {
        let storage = MemoryStorage::default();
        let (a, other) = (SessionId::new_v7(), SessionId::new_v7());
        let (mut tab_x, mut tab_y) = (QueueStore::new(user()), QueueStore::new(user()));
        tab_y.update(&storage, |queue| {
            queue.enqueue(start(a), at(0));
            queue.enqueue(set(a, 0), at(0));
        });
        // Tab X loads the queue, then sleeps.
        tab_x.load(&storage);
        let refused = Failure::Rejected {
            message: "Another session is in progress.".to_owned(),
        };
        tab_y.update(&storage, |queue| {
            queue.on_failure(&start(a), refused, at(0), 0.5, &Backoff::DEFAULT);
        });
        assert_eq!(tab_y.update(&storage, Queue::discard_failed).len(), 2);
        // Many more writes are delivered than delivered tombstones are kept.
        for index in 0..=u16::try_from(crate::offline::queue::MAX_TOMBSTONES).unwrap() {
            let write = set(other, index);
            tab_y.update(&storage, |queue| {
                queue.enqueue(write.clone(), at(1));
                queue.on_success(&write);
            });
        }
        // X wakes up and writes its stale copy back: the discarded writes stay gone.
        tab_x.update(&storage, |queue| queue.nudge());
        assert_eq!(writes(&mut tab_x, &storage), Vec::<Write>::new());
        assert_eq!(
            writes(&mut QueueStore::new(user()), &storage),
            Vec::<Write>::new()
        );
    }
}
