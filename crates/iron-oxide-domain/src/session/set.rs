//! One logged set.

use serde::{Deserialize, Serialize};

use crate::duration::Seconds;
use crate::ids::{ExerciseId, SetId};
use crate::progression::SetTarget;
use crate::reps::Reps;
use crate::weight::Weight;

/// A set the user has done, as logged on the device.
///
/// `id` is generated on the client when the set is logged, so a save that is retried (offline queue,
/// flaky network) is recognised as the same set instead of creating a duplicate. `T` is the timestamp
/// type, as for [`Session`](super::Session).
///
/// Every combination of fields is a valid value; the rules that involve the session (timestamps,
/// unique IDs) are enforced by [`SessionLog`](super::SessionLog).
///
/// Serializes as an object with the field names below; `weight` and `duration` are `null` when
/// absent, and `target` is left out when absent (so a set saved before #60, in an offline store or
/// an export, still loads). Unknown fields are ignored on load, for forward compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LoggedSet<T> {
    /// Client-generated ID, the idempotency key.
    pub id: SetId,
    /// The exercise.
    pub exercise: ExerciseId,
    /// Position of the set among the exercise's sets of the same kind (warm-up or work) in this
    /// session, from 0.
    pub set_index: u16,
    /// Repetitions done. Zero is a failed attempt; a timed hold usually logs 1.
    pub reps: Reps,
    /// Load, or `None` for body-weight work.
    pub weight: Option<Weight>,
    /// Time under tension for timed work (plank, carry), or `None`.
    pub duration: Option<Seconds>,
    /// Whether this is a warm-up set (excluded from volume, PRs and progression).
    pub warm_up: bool,
    /// When the set was completed.
    pub completed_at: T,
    /// What the app prescribed for this set when it was logged (the prefill the lifter saw: the
    /// progression's target, or last session's set), or `None` when nothing was prescribed (an
    /// extra set, an added exercise) or the set was logged before #60.
    ///
    /// The progression engine judges a training max session's set against it exactly ("lifted at
    /// least what was prescribed then"), so the verdict depends neither on today's settings nor on
    /// a tolerance; a set without it is judged with the legacy tolerance (see `progression`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<SetTarget>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn serde_round_trips_with_a_stable_shape() {
        let set = LoggedSet {
            id: SetId::from_uuid(Uuid::from_u128(7)),
            exercise: ExerciseId::new("back-squat").unwrap(),
            set_index: 2,
            reps: Reps::new(5),
            weight: Some(Weight::from_kg(102.5).unwrap()),
            duration: None,
            warm_up: false,
            completed_at: 1_700_000_000_000_i64,
            target: None,
        };
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id": "00000000-0000-0000-0000-000000000007",
                "exercise": "back-squat",
                "set_index": 2,
                "reps": 5,
                "weight": 102.5,
                "duration": null,
                "warm_up": false,
                "completed_at": 1_700_000_000_000_i64,
            })
        );
        assert_eq!(serde_json::from_value::<LoggedSet<i64>>(json).unwrap(), set);

        let plank = LoggedSet {
            id: SetId::from_uuid(Uuid::from_u128(8)),
            exercise: ExerciseId::new("plank").unwrap(),
            set_index: 0,
            reps: Reps::new(1),
            weight: None,
            duration: Some(Seconds::new(60)),
            warm_up: true,
            completed_at: 5_i64,
            target: None,
        };
        let text = serde_json::to_string(&plank).unwrap();
        assert_eq!(
            serde_json::from_str::<LoggedSet<i64>>(&text).unwrap(),
            plank
        );
    }

    #[test]
    fn deserialization_validates_fields() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000007",
            "exercise": "Back Squat",
            "set_index": 0,
            "reps": 5,
            "weight": null,
            "duration": null,
            "warm_up": false,
            "completed_at": 0,
        });
        assert!(serde_json::from_value::<LoggedSet<i64>>(json).is_err());
    }

    #[test]
    fn the_prescribed_target_round_trips_and_is_optional() {
        use crate::progression::SetGoal;
        let set = LoggedSet {
            id: SetId::from_uuid(Uuid::from_u128(9)),
            exercise: ExerciseId::new("bench-press").unwrap(),
            set_index: 0,
            reps: Reps::new(5),
            weight: Some(Weight::from_kg(80.0).unwrap()),
            duration: None,
            warm_up: false,
            completed_at: 7_i64,
            target: Some(SetTarget {
                weight: Some(Weight::from_kg(77.5).unwrap()),
                goal: SetGoal::Reps {
                    reps: Reps::new(5),
                    range: None,
                },
            }),
        };
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(
            json["target"],
            serde_json::json!({ "weight": 77.5, "goal": { "reps": { "reps": 5, "range": null } } })
        );
        assert_eq!(
            serde_json::from_value::<LoggedSet<i64>>(json.clone()).unwrap(),
            set
        );
        // A set saved before #60 (offline store, export v1) has no `target`: it loads as `None`.
        let mut old = json;
        old.as_object_mut().unwrap().remove("target");
        let loaded = serde_json::from_value::<LoggedSet<i64>>(old).unwrap();
        assert_eq!(loaded.target, None);
        assert_eq!(
            loaded,
            LoggedSet {
                target: None,
                ..set
            }
        );
    }
}
