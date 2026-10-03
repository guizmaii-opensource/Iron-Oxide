//! The in-progress session kept on the device, so a reload (or a dead phone battery, or no
//! signal) never loses the sets logged so far.
//!
//! One record per user ([`super::storage::key`]`("session", user)`), versioned
//! ([`LocalSession::VERSION`]); unreadable data is kept aside and reads as no session. The
//! session screens save it after every change, restore it on load (then reconcile with
//! `get_in_progress_session`, the server's view), and clear it once the session is finished.
//! Signing out clears it too.

use iron_oxide_domain::{
    DayId, LoggedSet, ProgramId, ProgramVersionId, SessionId, SessionOutcome, SetId,
    time::Timestamp,
};
use serde::{Deserialize, Serialize};

use super::storage::{self, Storage, StorageError};
use crate::auth::types::UserId;

/// What the device remembers of the session in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalSession {
    pub session_id: SessionId,
    pub started_at: Timestamp,
    pub program_id: ProgramId,
    pub program_version_id: ProgramVersionId,
    pub day: DayId,
    /// The sets logged so far, in logging order.
    pub sets: Vec<LoggedSet<Timestamp>>,
    /// Set once the user ended the session, until the screen clears the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<LocalFinish>,
    /// The session screen's own state, opaque here (`crate::ui::session::local`): the plan, a
    /// settings snapshot, the exercises skipped, the values being entered and the rest timer, so
    /// an offline reload lands on the same set with the same values. Cleared with the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<serde_json::Value>,
}

/// How and when the user ended the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalFinish {
    pub outcome: SessionOutcome,
    pub finished_at: Timestamp,
}

impl LocalSession {
    /// The version of the record. Bump it when the shape changes incompatibly.
    pub const VERSION: u32 = 1;

    /// Adds `set`, or replaces the set with the same id (an edit) in place.
    pub fn record_set(&mut self, set: LoggedSet<Timestamp>) {
        match self.sets.iter_mut().find(|logged| logged.id == set.id) {
            Some(logged) => *logged = set,
            None => self.sets.push(set),
        }
    }

    /// Removes the set with this id, if any.
    pub fn remove_set(&mut self, id: SetId) {
        self.sets.retain(|logged| logged.id != id);
    }

    /// The user's session on this device, if any. Unreadable data reads as none (and is kept
    /// aside); blocked storage too.
    #[must_use]
    pub fn load(storage: &dyn Storage, user: UserId) -> Option<Self> {
        let key = storage::key("session", user);
        let data = storage::read(storage, &key, Self::VERSION).ok()??;
        match serde_json::from_value(data.clone()) {
            Ok(session) => Some(session),
            Err(_) => {
                storage::set_aside(storage, &key, data);
                None
            }
        }
    }

    /// Stores the session for `user`.
    ///
    /// # Errors
    /// When storage is blocked or full: the screen keeps the session in memory and says it is
    /// not saved on the device.
    pub fn save(&self, storage: &dyn Storage, user: UserId) -> Result<(), StorageError> {
        let data = serde_json::to_value(self).map_err(|error| StorageError(error.to_string()))?;
        storage::write(storage, &storage::key("session", user), Self::VERSION, data)
    }

    /// Forgets the user's session on this device (finished, or signed out).
    ///
    /// # Errors
    /// When storage is blocked.
    pub fn clear(storage: &dyn Storage, user: UserId) -> Result<(), StorageError> {
        storage.remove(&storage::key("session", user))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offline::queue::{Write, tests::set};
    use crate::offline::storage::MemoryStorage;

    fn user(n: u128) -> UserId {
        UserId::from_uuid(uuid::Uuid::from_u128(n))
    }

    fn logged(session: SessionId, index: u16) -> LoggedSet<Timestamp> {
        match set(session, index) {
            Write::SaveSet { set, .. } => set,
            _ => unreachable!(),
        }
    }

    fn session() -> LocalSession {
        LocalSession {
            session_id: SessionId::new_v7(),
            started_at: Timestamp::from_epoch_millis(1_000),
            program_id: ProgramId::new_v7(),
            program_version_id: ProgramVersionId::new_v7(),
            day: "a".parse().unwrap(),
            sets: Vec::new(),
            finished: None,
            screen: None,
        }
    }

    #[test]
    fn a_session_round_trips_per_user() {
        let storage = MemoryStorage::default();
        let mut local = session();
        local.record_set(logged(local.session_id, 0));
        local.save(&storage, user(1)).unwrap();
        assert_eq!(LocalSession::load(&storage, user(1)), Some(local));
        assert_eq!(LocalSession::load(&storage, user(2)), None);
        LocalSession::clear(&storage, user(1)).unwrap();
        assert_eq!(LocalSession::load(&storage, user(1)), None);
    }

    #[test]
    fn recording_a_set_again_replaces_it_in_place() {
        let mut local = session();
        let (first, second) = (logged(local.session_id, 0), logged(local.session_id, 1));
        local.record_set(first.clone());
        local.record_set(second.clone());
        let edited = LoggedSet {
            warm_up: true,
            ..first.clone()
        };
        local.record_set(edited.clone());
        assert_eq!(local.sets, vec![edited, second.clone()]);
        local.remove_set(first.id);
        assert_eq!(local.sets, vec![second]);
    }

    #[test]
    fn corrupt_old_or_mismatched_data_reads_as_no_session() {
        let storage = MemoryStorage::default();
        let key = storage::key("session", user(1));
        for raw in [
            "nope".to_owned(),
            r#"{"v": 0, "data": {}}"#.to_owned(),
            r#"{"v": 1, "data": {"session_id": 3}}"#.to_owned(),
        ] {
            storage.set(&key, &raw).unwrap();
            assert_eq!(LocalSession::load(&storage, user(1)), None, "{raw}");
            assert_eq!(storage.get(&key), Ok(None));
        }
        assert!(
            storage
                .get(&storage::unreadable_key(&key))
                .unwrap()
                .is_some()
        );
        // And a new session can be saved over it.
        let local = session();
        local.save(&storage, user(1)).unwrap();
        assert_eq!(LocalSession::load(&storage, user(1)), Some(local));
    }

    #[test]
    fn the_screen_state_is_kept_and_older_records_still_load() {
        let storage = MemoryStorage::default();
        let mut local = session();
        local.screen = Some(serde_json::json!({ "skipped": ["plank"] }));
        local.save(&storage, user(1)).unwrap();
        assert_eq!(LocalSession::load(&storage, user(1)), Some(local.clone()));
        // A record saved before the screen state existed reads with none.
        let key = storage::key("session", user(1));
        let mut old = serde_json::to_value(&local).unwrap();
        old.as_object_mut().unwrap().remove("screen");
        storage
            .set(
                &key,
                &serde_json::json!({ "v": 1, "data": old }).to_string(),
            )
            .unwrap();
        assert_eq!(
            LocalSession::load(&storage, user(1)),
            Some(LocalSession {
                screen: None,
                ..local
            })
        );
    }

    #[test]
    fn a_full_storage_is_reported() {
        let storage = MemoryStorage::default();
        storage.set_full(true);
        assert!(session().save(&storage, user(1)).is_err());
    }
}
