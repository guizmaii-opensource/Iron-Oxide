//! Settings and training maxes (#20): the logic behind `crate::api::settings`.

use iron_oxide_domain::{
    ExerciseId, PlateInventory, PlateStock, Seconds, Unit, ValueError, Weight,
};
use sqlx::{PgPool, types::time::OffsetDateTime};

use super::{ApiError, timestamp};
use crate::api::settings::{
    MAX_DEFAULT_REST, MAX_WEIGHT_STEP_KG, Settings, SettingsUpdate, TrainingMax,
};
use crate::server::db::{
    ids::UserId,
    settings::{self as repo, UserSettings},
    training_maxes,
};

/// Stored data that does not fit its type: a 500, with the column in the log.
fn corrupt(column: &'static str) -> ApiError {
    ApiError::internal(format!("corrupt column {column}"))
}

pub async fn get(pool: &PgPool, owner: UserId) -> Result<Settings, ApiError> {
    match repo::find(pool, owner).await? {
        None => Ok(Settings::defaults()),
        Some(saved) => from_stored(saved),
    }
}

pub async fn update(
    pool: &PgPool,
    owner: UserId,
    update: SettingsUpdate,
) -> Result<Settings, ApiError> {
    // A client built before #103 sends no weight steps or vibration: those keep their saved
    // values. Reading first is fine: a concurrent update of the same fields is last-writer-wins
    // either way.
    let current = get(pool, owner).await?;
    let settings = validate(update, &current)?;
    repo::save(pool, owner, &to_stored(&settings)?).await?;
    Ok(settings)
}

/// Checks an update and turns it into settings; the fields it leaves out keep `current`'s values.
pub fn validate(update: SettingsUpdate, current: &Settings) -> Result<Settings, ApiError> {
    // Fixed messages: the domain's would echo the number, which can print as hundreds of digits.
    let bar_weight = Weight::from_kg(update.bar_weight).map_err(|_| {
        ApiError::invalid_field(
            "bar_weight",
            "The bar weight must be between 0 and 2000 kg.",
        )
    })?;
    if bar_weight.is_zero() {
        return Err(ApiError::invalid_field(
            "bar_weight",
            "The bar must weigh more than zero.",
        ));
    }
    let stock = update
        .plate_inventory
        .into_iter()
        .map(|input| {
            Ok(PlateStock {
                plate: Weight::from_kg(input.plate)?,
                pairs: input.pairs,
            })
        })
        .collect::<Result<Vec<_>, ValueError>>()
        .map_err(|_| {
            ApiError::invalid_field(
                "plate_inventory",
                "Plate weights must be between 0 and 2000 kg.",
            )
        })?;
    let plate_inventory = PlateInventory::new(stock).map_err(|error| {
        ApiError::invalid_field("plate_inventory", format!("Plate inventory: {error}."))
    })?;
    if plate_inventory.is_empty() {
        return Err(ApiError::invalid_field(
            "plate_inventory",
            "Keep at least one plate size.",
        ));
    }
    if update.default_rest > MAX_DEFAULT_REST {
        return Err(ApiError::invalid_field(
            "default_rest",
            format!(
                "The default rest must be at most {} seconds.",
                MAX_DEFAULT_REST.get()
            ),
        ));
    }
    let step = |kg: f64, field: &'static str| {
        let message =
            format!("A weight step must be more than 0 and at most {MAX_WEIGHT_STEP_KG} kg.");
        let max = Weight::from_kg(MAX_WEIGHT_STEP_KG).unwrap_or(Weight::MAX);
        match Weight::from_kg(kg) {
            Ok(step) if !step.is_zero() && step <= max => Ok(step),
            _ => Err(ApiError::invalid_field(field, message)),
        }
    };
    let kg_weight_step = match update.kg_weight_step {
        Some(kg) => step(kg, "kg_weight_step")?,
        None => current.kg_weight_step,
    };
    let lb_weight_step = match update.lb_weight_step {
        Some(kg) => step(kg, "lb_weight_step")?,
        None => current.lb_weight_step,
    };
    Ok(Settings {
        unit: update.unit,
        bar_weight,
        plate_inventory,
        default_rest: update.default_rest,
        sound_enabled: update.sound_enabled,
        kg_weight_step,
        lb_weight_step,
        vibration_enabled: update
            .vibration_enabled
            .unwrap_or(current.vibration_enabled),
    })
}

pub(super) fn from_stored(stored: UserSettings) -> Result<Settings, ApiError> {
    Ok(Settings {
        unit: match stored.unit {
            repo::Unit::Kg => Unit::Kg,
            repo::Unit::Lb => Unit::Lb,
        },
        bar_weight: Weight::from_nanograms(stored.bar_weight_ng)
            .map_err(|_| corrupt("user_settings.bar_weight_ng"))?,
        plate_inventory: serde_json::from_value(stored.plate_inventory)
            .map_err(|_| corrupt("user_settings.plate_inventory"))?,
        default_rest: Seconds::new(stored.default_rest_s),
        sound_enabled: stored.sound_enabled,
        kg_weight_step: Weight::from_nanograms(stored.kg_weight_step_ng)
            .map_err(|_| corrupt("user_settings.kg_weight_step_ng"))?,
        lb_weight_step: Weight::from_nanograms(stored.lb_weight_step_ng)
            .map_err(|_| corrupt("user_settings.lb_weight_step_ng"))?,
        vibration_enabled: stored.vibration_enabled,
    })
}

pub(super) fn to_stored(settings: &Settings) -> Result<UserSettings, ApiError> {
    Ok(UserSettings {
        unit: match settings.unit {
            Unit::Kg => repo::Unit::Kg,
            Unit::Lb => repo::Unit::Lb,
        },
        bar_weight_ng: settings.bar_weight.as_nanograms(),
        plate_inventory: serde_json::to_value(&settings.plate_inventory)
            .map_err(|error| ApiError::internal(format!("plate inventory JSON: {error}")))?,
        default_rest_s: settings.default_rest.get(),
        sound_enabled: settings.sound_enabled,
        kg_weight_step_ng: settings.kg_weight_step.as_nanograms(),
        lb_weight_step_ng: settings.lb_weight_step.as_nanograms(),
        vibration_enabled: settings.vibration_enabled,
    })
}

fn exercise_id(value: &str) -> Result<ExerciseId, ApiError> {
    ExerciseId::new(value).map_err(|_| ApiError::invalid("This is not a valid exercise id."))
}

fn training_max(stored: training_maxes::TrainingMax) -> Result<TrainingMax, ApiError> {
    Ok(TrainingMax {
        exercise_id: ExerciseId::new(stored.exercise_id)
            .map_err(|_| corrupt("training_maxes.exercise_id"))?,
        weight: Weight::from_nanograms(stored.weight_ng)
            .map_err(|_| corrupt("training_maxes.weight_ng"))?,
        set_at: timestamp(stored.set_at)?,
    })
}

pub async fn training_maxes(pool: &PgPool, owner: UserId) -> Result<Vec<TrainingMax>, ApiError> {
    training_maxes::list(pool, owner)
        .await?
        .into_iter()
        .map(training_max)
        .collect()
}

pub async fn set_training_max(
    pool: &PgPool,
    owner: UserId,
    exercise: &str,
    weight_kg: f64,
) -> Result<TrainingMax, ApiError> {
    let exercise = exercise_id(exercise)?;
    let weight = Weight::from_kg(weight_kg)
        .map_err(|_| ApiError::invalid("A training max must be between 0 and 2000 kg."))?;
    if weight.is_zero() {
        return Err(ApiError::invalid("A training max must be more than zero."));
    }
    let stored = training_maxes::TrainingMax {
        exercise_id: exercise.as_str().to_owned(),
        weight_ng: weight.as_nanograms(),
        // Microsecond precision, like the column: what is returned is what is stored.
        set_at: now_micros(),
    };
    training_maxes::set(pool, owner, &stored).await?;
    training_max(stored)
}

pub async fn delete_training_max(
    pool: &PgPool,
    owner: UserId,
    exercise: &str,
) -> Result<(), ApiError> {
    let exercise = exercise_id(exercise)?;
    training_maxes::delete(pool, owner, exercise.as_str()).await?;
    Ok(())
}

/// The current time, truncated to microseconds (Postgres' precision).
fn now_micros() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    now.replace_nanosecond(now.nanosecond() / 1_000 * 1_000)
        .unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use crate::api::settings::PlateInput;
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::server::api::testing::{self, TestApi, TestUser};

    const GET: &str = "/api/settings/get";
    const UPDATE: &str = "/api/settings/update";
    const MAXES: &str = "/api/settings/training-maxes";
    const SET_MAX: &str = "/api/settings/training-max/set";
    const DELETE_MAX: &str = "/api/settings/training-max/delete";

    fn kg(value: f64) -> Weight {
        Weight::from_kg(value).unwrap()
    }

    fn plate(weight: f64, pairs: u32) -> PlateInput {
        PlateInput {
            plate: weight,
            pairs,
        }
    }

    fn stock(weight: f64, pairs: u32) -> PlateStock {
        PlateStock {
            plate: kg(weight),
            pairs,
        }
    }

    /// Custom settings, with the plates out of order.
    fn custom() -> SettingsUpdate {
        SettingsUpdate {
            unit: Unit::Lb,
            bar_weight: Weight::from_lb(45.0).unwrap().as_kg(),
            plate_inventory: vec![plate(5.0, 1), plate(20.0, 4)],
            default_rest: Seconds::new(90),
            sound_enabled: false,
            kg_weight_step: Some(1.25),
            lb_weight_step: Some(Weight::from_lb(2.5).unwrap().as_kg()),
            vibration_enabled: Some(false),
        }
    }

    #[test]
    fn validate_sorts_plates_and_rejects_bad_inventories_and_rests() {
        let settings = validate(custom(), &Settings::defaults()).unwrap();
        assert_eq!(
            settings.plate_inventory.stock(),
            &[stock(20.0, 4), stock(5.0, 1)]
        );
        // 45 lb, exactly.
        assert_eq!(settings.bar_weight, Weight::from_lb(45.0).unwrap());
        // Settings convert to an update with the same JSON, and back to themselves.
        let update = SettingsUpdate::from(settings.clone());
        assert_eq!(
            serde_json::to_value(&update).unwrap(),
            serde_json::to_value(&settings).unwrap()
        );
        assert_eq!(validate(update, &Settings::defaults()).unwrap(), settings);
        let rest = SettingsUpdate {
            default_rest: MAX_DEFAULT_REST,
            ..custom()
        };
        assert!(validate(rest, &Settings::defaults()).is_ok());

        for (update, message) in [
            (
                SettingsUpdate {
                    plate_inventory: vec![plate(20.0, 1), plate(20.0, 2)],
                    ..custom()
                },
                "Plate inventory: plate size 20 kg is listed more than once.",
            ),
            (
                SettingsUpdate {
                    plate_inventory: vec![plate(0.0, 1)],
                    ..custom()
                },
                "Plate inventory: a plate must weigh more than zero.",
            ),
            (
                SettingsUpdate {
                    plate_inventory: vec![plate(-5.0, 1)],
                    ..custom()
                },
                "Plate weights must be between 0 and 2000 kg.",
            ),
            (
                SettingsUpdate {
                    bar_weight: -1e300,
                    ..custom()
                },
                "The bar weight must be between 0 and 2000 kg.",
            ),
            (
                SettingsUpdate {
                    plate_inventory: vec![plate(-5e-324, 1)],
                    ..custom()
                },
                "Plate weights must be between 0 and 2000 kg.",
            ),
            (
                SettingsUpdate {
                    bar_weight: 2_000.5,
                    ..custom()
                },
                "The bar weight must be between 0 and 2000 kg.",
            ),
            (
                SettingsUpdate {
                    plate_inventory: vec![plate(20.0, 51)],
                    ..custom()
                },
                "Plate inventory: plate size 20 kg has 51 pairs; at most 50 are allowed.",
            ),
            (
                SettingsUpdate {
                    default_rest: Seconds::new(3_601),
                    ..custom()
                },
                "The default rest must be at most 3600 seconds.",
            ),
            (
                SettingsUpdate {
                    bar_weight: 0.0,
                    ..custom()
                },
                "The bar must weigh more than zero.",
            ),
            (
                SettingsUpdate {
                    plate_inventory: Vec::new(),
                    ..custom()
                },
                "Keep at least one plate size.",
            ),
            (
                SettingsUpdate {
                    kg_weight_step: Some(0.0),
                    ..custom()
                },
                "A weight step must be more than 0 and at most 25 kg.",
            ),
            (
                SettingsUpdate {
                    lb_weight_step: Some(25.5),
                    ..custom()
                },
                "A weight step must be more than 0 and at most 25 kg.",
            ),
        ] {
            assert_eq!(
                validate(update, &Settings::defaults())
                    .unwrap_err()
                    .public(),
                (422, message)
            );
        }
    }

    #[test]
    fn stored_settings_round_trip_and_corrupt_ones_are_internal_errors() {
        let settings = validate(custom(), &Settings::defaults()).unwrap();
        assert_eq!(
            from_stored(to_stored(&settings).unwrap()).unwrap(),
            settings
        );
        let defaults = Settings::defaults();
        assert_eq!(
            from_stored(to_stored(&defaults).unwrap()).unwrap(),
            defaults
        );
        let corrupt_plates = UserSettings {
            plate_inventory: json!([{ "plate": 0.0, "pairs": 1 }]),
            ..to_stored(&settings).unwrap()
        };
        assert_eq!(from_stored(corrupt_plates).unwrap_err().public().0, 500);
        let corrupt_bar = UserSettings {
            bar_weight_ng: u64::MAX,
            ..to_stored(&settings).unwrap()
        };
        assert_eq!(from_stored(corrupt_bar).unwrap_err().public().0, 500);
    }

    #[test]
    fn fields_an_update_leaves_out_keep_their_current_values() {
        let current = Settings {
            kg_weight_step: kg(1.0),
            lb_weight_step: Weight::from_lb(10.0).unwrap(),
            vibration_enabled: false,
            ..Settings::defaults()
        };
        let old_client = SettingsUpdate {
            kg_weight_step: None,
            lb_weight_step: None,
            vibration_enabled: None,
            ..custom()
        };
        let settings = validate(old_client, &current).unwrap();
        assert_eq!(settings.kg_weight_step, current.kg_weight_step);
        assert_eq!(settings.lb_weight_step, current.lb_weight_step);
        assert!(!settings.vibration_enabled);
        // What is sent replaces it.
        let settings = validate(custom(), &current).unwrap();
        assert_eq!(settings.kg_weight_step, kg(1.25));
    }

    #[test]
    fn the_api_defaults_are_the_column_defaults() {
        let stored = from_stored(UserSettings::defaults()).unwrap();
        let defaults = Settings::defaults();
        assert_eq!(stored.kg_weight_step, defaults.kg_weight_step);
        assert_eq!(stored.lb_weight_step, defaults.lb_weight_step);
        assert_eq!(stored.vibration_enabled, defaults.vibration_enabled);
        assert_eq!(stored.bar_weight, defaults.bar_weight);
        assert_eq!(stored.plate_inventory, defaults.plate_inventory);
    }

    #[test]
    fn now_has_microsecond_precision() {
        assert_eq!(now_micros().nanosecond() % 1_000, 0);
    }

    async fn settings_of(user: &mut TestUser) -> Settings {
        user.call(GET, json!({})).await.unwrap()
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn settings_default_then_save_and_replace(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let defaults = settings_of(&mut a).await;
        assert_eq!(defaults, Settings::defaults());
        assert_eq!(
            defaults.plate_inventory,
            PlateInventory::default_for(Unit::Kg)
        );
        assert_eq!(defaults.bar_weight, kg(20.0));

        let saved: Settings = a
            .call(UPDATE, json!({ "settings": custom() }))
            .await
            .unwrap();
        assert_eq!(saved, validate(custom(), &Settings::defaults()).unwrap());
        assert_eq!(settings_of(&mut a).await, saved);
        // Saving the same again is harmless.
        let again: Settings = a
            .call(UPDATE, json!({ "settings": custom() }))
            .await
            .unwrap();
        assert_eq!(again, saved);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn invalid_settings_are_422_and_change_nothing(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let _: Settings = a
            .call(UPDATE, json!({ "settings": custom() }))
            .await
            .unwrap();
        let before = settings_of(&mut a).await;

        let duplicate = SettingsUpdate {
            plate_inventory: vec![plate(20.0, 1), plate(20.0, 2)],
            ..custom()
        };
        let error = a.call_err(UPDATE, json!({ "settings": duplicate })).await;
        assert_eq!(
            (error.status.as_u16(), error.message.as_str()),
            (
                422,
                "Plate inventory: plate size 20 kg is listed more than once."
            )
        );
        let long_rest = SettingsUpdate {
            default_rest: Seconds::new(3_601),
            ..custom()
        };
        let error = a.call_err(UPDATE, json!({ "settings": long_rest })).await;
        assert_eq!(error.status.as_u16(), 422, "{error:?}");

        let heavy_bar = SettingsUpdate {
            bar_weight: 2_000.5,
            ..custom()
        };
        let error = a.call_err(UPDATE, json!({ "settings": heavy_bar })).await;
        assert_eq!(
            (error.status.as_u16(), error.message.as_str()),
            (422, "The bar weight must be between 0 and 2000 kg.")
        );
        // A client built before #103 sends no weight steps or vibration: the saved ones stay.
        let saved_before: Settings = a
            .call(UPDATE, json!({ "settings": custom() }))
            .await
            .unwrap();
        assert!(!saved_before.vibration_enabled);
        let mut old_shape = serde_json::to_value(SettingsUpdate {
            default_rest: Seconds::new(150),
            ..custom()
        })
        .unwrap();
        for field in ["kg_weight_step", "lb_weight_step", "vibration_enabled"] {
            old_shape.as_object_mut().unwrap().remove(field);
        }
        let saved: Settings = a
            .call(UPDATE, json!({ "settings": old_shape }))
            .await
            .unwrap();
        assert_eq!(saved.default_rest, Seconds::new(150));
        assert_eq!(saved.kg_weight_step, saved_before.kg_weight_step);
        assert_eq!(saved.lb_weight_step, saved_before.lb_weight_step);
        assert_eq!(saved.vibration_enabled, saved_before.vibration_enabled);
        assert_eq!(settings_of(&mut a).await, saved);
        let _: Settings = a
            .call(UPDATE, json!({ "settings": custom() }))
            .await
            .unwrap();
        // Weight steps are refused out of range, naming their field (#103).
        for (update, field) in [
            (
                SettingsUpdate {
                    kg_weight_step: Some(0.0),
                    ..custom()
                },
                "kg_weight_step",
            ),
            (
                SettingsUpdate {
                    lb_weight_step: Some(30.0),
                    ..custom()
                },
                "lb_weight_step",
            ),
        ] {
            let (status, body) = a.call_raw(UPDATE, json!({ "settings": update })).await;
            assert_eq!(status.as_u16(), 422);
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                body["data"]["ServerError"]["details"]["field"], field,
                "{body}"
            );
        }
        // No bar and no plates are refused, each naming its field.
        for (update, field, message) in [
            (
                SettingsUpdate {
                    bar_weight: 0.0,
                    ..custom()
                },
                "bar_weight",
                "The bar must weigh more than zero.",
            ),
            (
                SettingsUpdate {
                    plate_inventory: Vec::new(),
                    ..custom()
                },
                "plate_inventory",
                "Keep at least one plate size.",
            ),
        ] {
            let (status, body) = a.call_raw(UPDATE, json!({ "settings": update })).await;
            assert_eq!(status.as_u16(), 422);
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["message"], message, "{body}");
            assert_eq!(
                body["data"]["ServerError"]["details"]["field"], field,
                "{body}"
            );
        }
        // A value that does not even decode (an unknown unit) is the generic 422.
        let mut bad_unit = serde_json::to_value(custom()).unwrap();
        bad_unit["unit"] = json!("stone");
        let error = a.call_err(UPDATE, json!({ "settings": bad_unit })).await;
        assert_eq!(
            (error.status.as_u16(), error.message.as_str()),
            (422, "Invalid request.")
        );
        assert_eq!(settings_of(&mut a).await, before);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn training_maxes_are_set_reset_and_deleted(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let none: Vec<TrainingMax> = a.call(MAXES, json!({})).await.unwrap();
        assert!(none.is_empty());

        let before = timestamp(OffsetDateTime::now_utc()).unwrap();
        let squat: TrainingMax = a
            .call(
                SET_MAX,
                json!({ "exercise_id": "back-squat", "weight": 100.0 }),
            )
            .await
            .unwrap();
        assert_eq!(squat.exercise_id.as_str(), "back-squat");
        assert_eq!(squat.weight, kg(100.0));
        assert!(squat.set_at >= before, "{squat:?}");
        let bench: TrainingMax = a
            .call(SET_MAX, json!({ "exercise_id": "bench", "weight": 80.0 }))
            .await
            .unwrap();
        let listed: Vec<TrainingMax> = a.call(MAXES, json!({})).await.unwrap();
        assert_eq!(listed, vec![squat.clone(), bench.clone()]);

        // Setting it again, even to the same weight, moves the progression anchor to now.
        let reset: TrainingMax = a
            .call(
                SET_MAX,
                json!({ "exercise_id": "back-squat", "weight": 100.0 }),
            )
            .await
            .unwrap();
        assert!(reset.set_at >= squat.set_at, "{reset:?} {squat:?}");
        let stored = crate::server::db::training_maxes::list(&api.db, a.id)
            .await
            .unwrap();
        assert_eq!(timestamp(stored[0].set_at).unwrap(), reset.set_at);
        assert!(stored[0].set_at > crate::server::db::testing::at(0));

        let _: () = a
            .call(DELETE_MAX, json!({ "exercise_id": "bench" }))
            .await
            .unwrap();
        let error = a
            .call_err(DELETE_MAX, json!({ "exercise_id": "bench" }))
            .await;
        assert_eq!(error.status.as_u16(), 404, "{error:?}");
        let listed: Vec<TrainingMax> = a.call(MAXES, json!({})).await.unwrap();
        assert_eq!(listed, vec![reset]);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn invalid_training_maxes_are_422(db: PgPool) {
        let api = TestApi::new(db).await;
        let mut a = api.user("A").await;
        let cases: [(&str, Value, &str); 5] = [
            (
                SET_MAX,
                json!({ "exercise_id": "Back Squat", "weight": 100.0 }),
                "This is not a valid exercise id.",
            ),
            (
                SET_MAX,
                json!({ "exercise_id": "back-squat", "weight": 0.0 }),
                "A training max must be more than zero.",
            ),
            (
                DELETE_MAX,
                json!({ "exercise_id": "" }),
                "This is not a valid exercise id.",
            ),
            (
                SET_MAX,
                json!({ "exercise_id": "back-squat", "weight": -1e300 }),
                "A training max must be between 0 and 2000 kg.",
            ),
            (
                SET_MAX,
                json!({ "exercise_id": "back-squat", "weight": 2_000.5 }),
                "A training max must be between 0 and 2000 kg.",
            ),
        ];
        for (path, body, message) in cases {
            let error = a.call_err(path, body).await;
            assert_eq!(
                (error.status.as_u16(), error.message.as_str()),
                (422, message)
            );
        }
        let listed: Vec<TrainingMax> = a.call(MAXES, json!({})).await.unwrap();
        assert!(listed.is_empty());
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_settings_are_neither_seen_nor_changed(db: PgPool) {
        let api = TestApi::new(db).await;
        let (mut a, mut b) = api.users_a_and_b().await;
        let saved: Settings = a
            .call(UPDATE, json!({ "settings": custom() }))
            .await
            .unwrap();
        // B still has the defaults, and saving B's own leaves A's alone.
        assert_eq!(settings_of(&mut b).await, Settings::defaults());
        let b_update = SettingsUpdate {
            unit: Unit::Kg,
            ..custom()
        };
        let _: Settings = b
            .call(UPDATE, json!({ "settings": b_update }))
            .await
            .unwrap();
        assert_eq!(settings_of(&mut a).await, saved);
        assert_eq!(settings_of(&mut b).await.unit, Unit::Kg);
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn another_users_training_maxes_are_neither_seen_nor_changed(db: PgPool) {
        let api = TestApi::new(db).await;
        let (mut a, mut b) = api.users_a_and_b().await;
        let squat: TrainingMax = a
            .call(
                SET_MAX,
                json!({ "exercise_id": "back-squat", "weight": 100.0 }),
            )
            .await
            .unwrap();
        let listed: Vec<TrainingMax> = b.call(MAXES, json!({})).await.unwrap();
        assert!(listed.is_empty());

        // Deleting A's exercise is the same 404 as one nobody has.
        let for_a = b
            .call_err(DELETE_MAX, json!({ "exercise_id": "back-squat" }))
            .await;
        let for_nobody = b
            .call_err(DELETE_MAX, json!({ "exercise_id": "zercher-squat" }))
            .await;
        assert_eq!(for_a, for_nobody);
        assert_eq!(for_a.status.as_u16(), 404, "{for_a:?}");

        // Setting the same exercise creates B's own training max.
        let _: TrainingMax = b
            .call(
                SET_MAX,
                json!({ "exercise_id": "back-squat", "weight": 50.0 }),
            )
            .await
            .unwrap();
        let a_maxes: Vec<TrainingMax> = a.call(MAXES, json!({})).await.unwrap();
        assert_eq!(a_maxes, vec![squat]);
        let b_maxes: Vec<TrainingMax> = b.call(MAXES, json!({})).await.unwrap();
        assert_eq!(b_maxes.len(), 1);
        assert_eq!(b_maxes[0].weight, kg(50.0));
    }

    #[sqlx::test(migrator = "crate::server::db::MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn settings_need_a_signed_in_user(db: PgPool) {
        let api = TestApi::new(db).await;
        let bodies: [(&str, Value); 5] = [
            (GET, json!({})),
            (UPDATE, json!({ "settings": custom() })),
            (MAXES, json!({})),
            (SET_MAX, json!({ "exercise_id": "bench", "weight": 80.0 })),
            (DELETE_MAX, json!({ "exercise_id": "bench" })),
        ];
        for (path, body) in bodies {
            testing::assert_unauthorized_when_signed_out(&api, path, body).await;
        }
    }
}
