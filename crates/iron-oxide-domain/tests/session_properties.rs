//! Property tests for the session aggregate and the day rotation.

// Helpers outside `#[test]` functions are not covered by clippy's `allow-unwrap-in-tests`.
#![allow(clippy::unwrap_used)]

use iron_oxide_domain::{
    Change, DayId, ExerciseId, LoggedSet, ProgramVersionId, Reps, Seconds, Session, SessionId,
    SessionLog, SessionOutcome, SetId, Weight, next_day,
};
use proptest::prelude::*;
use uuid::Uuid;

const START: i64 = 1_000;

fn new_log() -> SessionLog<i64> {
    SessionLog::start(
        SessionId::from_uuid(Uuid::from_u128(1)),
        ProgramVersionId::from_uuid(Uuid::from_u128(2)),
        DayId::new("day-a").unwrap(),
        START,
    )
}

prop_compose! {
    /// A set whose ID is drawn from a small pool, so that collisions happen often.
    fn arb_set()(
        id in 0_u128..8,
        exercise in prop::sample::select(vec!["back-squat", "bench-press", "plank"]),
        set_index in 0_u16..5,
        reps in 0_u16..20,
        weight_grams in prop::option::of(0_u64..300_000),
        duration in prop::option::of(0_u32..600),
        warm_up in any::<bool>(),
        offset in 0_i64..10_000,
    ) -> LoggedSet<i64> {
        LoggedSet {
            id: SetId::from_uuid(Uuid::from_u128(id)),
            exercise: ExerciseId::new(exercise).unwrap(),
            set_index,
            reps: Reps::new(reps),
            weight: weight_grams.map(|g| Weight::from_nanograms(g * 1_000_000).unwrap()),
            duration: duration.map(Seconds::new),
            warm_up,
            completed_at: START + offset,
            target: None,
        }
    }
}

fn outcome() -> impl Strategy<Value = SessionOutcome> {
    prop::sample::select(vec![
        SessionOutcome::Completed,
        SessionOutcome::Skipped,
        SessionOutcome::Abandoned,
    ])
}

proptest! {
    /// Re-adding any already-logged set, any number of times, changes nothing.
    #[test]
    fn re_adding_logged_sets_is_idempotent(
        sets in prop::collection::vec(arb_set(), 0..20),
        retries in prop::collection::vec(any::<prop::sample::Index>(), 1..10),
    ) {
        let mut log = new_log();
        for set in sets {
            let _ = log.add_set(set);
        }
        let snapshot = log.clone();
        if !snapshot.sets().is_empty() {
            for index in retries {
                let set = index.get(snapshot.sets()).clone();
                prop_assert_eq!(log.add_set(set), Ok(Change::Unchanged));
            }
        }
        prop_assert_eq!(log, snapshot);
    }

    /// Whatever sequence of adds and ends is applied, the aggregate keeps its invariants: it
    /// survives a serde round trip (which re-checks everything) and set IDs stay unique.
    #[test]
    fn any_sequence_of_operations_keeps_the_invariants(
        ops in prop::collection::vec(
            prop_oneof![
                3 => arb_set().prop_map(Ok),
                1 => (outcome(), 0_i64..12_000).prop_map(Err),
            ],
            0..30,
        ),
    ) {
        let mut log = new_log();
        for op in ops {
            let before = log.clone();
            let result = match op {
                Ok(set) => log.add_set(set),
                Err((outcome, at)) => log.end(outcome, at),
            };
            // A failed operation never changes anything.
            if result.is_err() {
                prop_assert_eq!(&log, &before);
            }
            if result == Ok(Change::Unchanged) {
                prop_assert_eq!(&log, &before);
            }
        }
        let json = serde_json::to_string(&log).unwrap();
        prop_assert_eq!(serde_json::from_str::<SessionLog<i64>>(&json).unwrap(), log.clone());
        let mut ids: Vec<_> = log.sets().iter().map(|set| set.id).collect();
        ids.sort();
        ids.dedup();
        prop_assert_eq!(ids.len(), log.sets().len());
    }

    /// The next day is always one of the rotation's days, whatever the history.
    #[test]
    fn next_day_is_always_in_the_rotation(
        rotation_len in 1_usize..6,
        history in prop::collection::vec(
            (0_usize..9, prop::option::of(outcome()), 0_i64..100),
            0..20,
        ),
    ) {
        let rotation: Vec<DayId> = (0..rotation_len)
            .map(|i| DayId::new(format!("day-{i}")).unwrap())
            .collect();
        let sessions: Vec<Session<i64>> = history
            .into_iter()
            .enumerate()
            .map(|(n, (day, outcome, at))| {
                // Day numbers past the rotation length stand for days of an older program.
                let mut log = SessionLog::start(
                    SessionId::from_uuid(Uuid::from_u128(n as u128)),
                    ProgramVersionId::from_uuid(Uuid::from_u128(1)),
                    DayId::new(format!("day-{day}")).unwrap(),
                    0,
                );
                if let Some(outcome) = outcome {
                    log.end(outcome, at).unwrap();
                }
                log.into_parts().0
            })
            .collect();
        let next = next_day(&rotation, &sessions).unwrap();
        prop_assert!(rotation.contains(next));
    }

    /// Completing the suggested day each time walks the rotation in order, wrapping around.
    #[test]
    fn completing_each_suggested_day_cycles_through_the_rotation(
        rotation_len in 1_usize..6,
        rounds in 1_usize..20,
    ) {
        let rotation: Vec<DayId> = (0..rotation_len)
            .map(|i| DayId::new(format!("day-{i}")).unwrap())
            .collect();
        let mut history: Vec<Session<i64>> = Vec::new();
        for round in 0..rounds {
            let next = next_day(&rotation, &history).unwrap().clone();
            prop_assert_eq!(&next, &rotation[round % rotation_len]);
            let at = i64::try_from(round).unwrap();
            let mut log = SessionLog::start(
                SessionId::from_uuid(Uuid::from_u128(round as u128)),
                ProgramVersionId::from_uuid(Uuid::from_u128(1)),
                next,
                at,
            );
            log.complete(at).unwrap();
            history.push(log.into_parts().0);
        }
    }
}
