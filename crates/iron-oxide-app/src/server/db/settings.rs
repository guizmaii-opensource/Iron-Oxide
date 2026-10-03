//! User settings (`user_settings`): one row per user, created by the first [`save`].

use sqlx::{PgPool, types::JsonValue};

use super::{
    error::{RepoError, narrow},
    ids::UserId,
};

/// The unit weights are shown and entered in. Weights are always stored in nanograms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Kg,
    Lb,
}

impl Unit {
    /// The stored text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kg => "kg",
            Self::Lb => "lb",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, RepoError> {
        match value {
            "kg" => Ok(Self::Kg),
            "lb" => Ok(Self::Lb),
            _ => Err(RepoError::Corrupt("user_settings.unit")),
        }
    }
}

/// A user's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct UserSettings {
    pub unit: Unit,
    /// Bar weight, in nanograms (the domain `Weight`).
    pub bar_weight_ng: u64,
    /// The domain `PlateInventory` as JSON (an array). Validate it with the domain before saving.
    pub plate_inventory: JsonValue,
    /// Default rest between sets, in seconds (the domain `Seconds`).
    pub default_rest_s: u32,
    pub sound_enabled: bool,
    /// The weight steppers' increment in kg mode, in nanograms.
    pub kg_weight_step_ng: u64,
    /// The weight steppers' increment in lb mode, in nanograms.
    pub lb_weight_step_ng: u64,
    pub vibration_enabled: bool,
}

impl UserSettings {
    /// The column defaults of `user_settings` (a test checks they match): what a row inserted
    /// without values holds. Not what the app shows a user who never saved settings, see
    /// [`find`].
    pub fn defaults() -> Self {
        Self {
            unit: Unit::Kg,
            bar_weight_ng: 20_000_000_000_000,
            // The domain's default kg plates, as the column default since
            // `20261003120000_settings_need_a_bar_and_plates`.
            plate_inventory: serde_json::to_value(iron_oxide_domain::PlateInventory::default_for(
                iron_oxide_domain::Unit::Kg,
            ))
            .unwrap_or(JsonValue::Null),
            default_rest_s: 120,
            sound_enabled: true,
            kg_weight_step_ng: 2_500_000_000_000,
            lb_weight_step_ng: 2_267_961_850_000,
            vibration_enabled: true,
        }
    }
}

/// The user's saved settings, `None` if they never saved any.
///
/// There is deliberately no "or the defaults" variant: what a user who never saved settings gets
/// is decided once, by `crate::api::settings::Settings::defaults` (with the domain's default
/// plate inventory), not by the column defaults.
pub async fn find(pool: &PgPool, user: UserId) -> Result<Option<UserSettings>, RepoError> {
    let row = sqlx::query!(
        "SELECT unit, bar_weight_ng, plate_inventory, default_rest_s, sound_enabled,
                kg_weight_step_ng, lb_weight_step_ng, vibration_enabled
         FROM user_settings WHERE user_id = $1",
        user.as_uuid()
    )
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        Ok(UserSettings {
            unit: Unit::parse(&row.unit)?,
            bar_weight_ng: narrow(row.bar_weight_ng, "user_settings.bar_weight_ng")?,
            plate_inventory: row.plate_inventory,
            default_rest_s: narrow(row.default_rest_s, "user_settings.default_rest_s")?,
            sound_enabled: row.sound_enabled,
            kg_weight_step_ng: narrow(row.kg_weight_step_ng, "user_settings.kg_weight_step_ng")?,
            lb_weight_step_ng: narrow(row.lb_weight_step_ng, "user_settings.lb_weight_step_ng")?,
            vibration_enabled: row.vibration_enabled,
        })
    })
    .transpose()
}

/// Saves the user's settings, replacing the previous ones.
///
/// # Errors
/// [`RepoError::Invalid`] when a value is out of range (bar weight above 2000 kg, a plate inventory
/// that is not an array of at most 16 entries).
pub async fn save(pool: &PgPool, user: UserId, settings: &UserSettings) -> Result<(), RepoError> {
    let bar_weight_ng = i64::try_from(settings.bar_weight_ng).map_err(|_| RepoError::Invalid {
        constraint: Some("user_settings_bar_weight_ng_check".to_owned()),
    })?;
    let step = |value: u64, constraint: &str| {
        i64::try_from(value).map_err(|_| RepoError::Invalid {
            constraint: Some(constraint.to_owned()),
        })
    };
    let kg_weight_step_ng = step(
        settings.kg_weight_step_ng,
        "user_settings_kg_weight_step_ng_check",
    )?;
    let lb_weight_step_ng = step(
        settings.lb_weight_step_ng,
        "user_settings_lb_weight_step_ng_check",
    )?;
    sqlx::query!(
        "INSERT INTO user_settings
             (user_id, unit, bar_weight_ng, plate_inventory, default_rest_s, sound_enabled,
              kg_weight_step_ng, lb_weight_step_ng, vibration_enabled)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (user_id) DO UPDATE SET
             unit = EXCLUDED.unit,
             bar_weight_ng = EXCLUDED.bar_weight_ng,
             plate_inventory = EXCLUDED.plate_inventory,
             default_rest_s = EXCLUDED.default_rest_s,
             sound_enabled = EXCLUDED.sound_enabled,
             kg_weight_step_ng = EXCLUDED.kg_weight_step_ng,
             lb_weight_step_ng = EXCLUDED.lb_weight_step_ng,
             vibration_enabled = EXCLUDED.vibration_enabled,
             updated_at = now()",
        user.as_uuid(),
        settings.unit.as_str(),
        bar_weight_ng,
        settings.plate_inventory,
        i64::from(settings.default_rest_s),
        settings.sound_enabled,
        kg_weight_step_ng,
        lb_weight_step_ng,
        settings.vibration_enabled,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::{MIGRATOR, testing};
    use serde_json::json;

    fn custom() -> UserSettings {
        UserSettings {
            unit: Unit::Lb,
            bar_weight_ng: 15_000_000_000_000,
            plate_inventory: json!([{"plate": 20.0, "pairs": 4}]),
            default_rest_s: u32::MAX,
            sound_enabled: false,
            kg_weight_step_ng: 1_000_000_000_000,
            lb_weight_step_ng: 4_535_923_700_000,
            vibration_enabled: false,
        }
    }

    #[test]
    fn unit_text_round_trips_and_rejects_unknown_values() {
        for unit in [Unit::Kg, Unit::Lb] {
            assert_eq!(Unit::parse(unit.as_str()).unwrap(), unit);
        }
        assert!(matches!(Unit::parse("st"), Err(RepoError::Corrupt(_))));
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn a_user_without_settings_has_none_until_the_first_save(pool: PgPool) {
        let user = testing::user(&pool).await;
        assert_eq!(find(&pool, user).await.unwrap(), None);
        save(&pool, user, &UserSettings::defaults()).await.unwrap();
        assert_eq!(
            find(&pool, user).await.unwrap(),
            Some(UserSettings::defaults())
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn rust_defaults_match_the_column_defaults(pool: PgPool) {
        let user = testing::user(&pool).await;
        sqlx::query!(
            "INSERT INTO user_settings (user_id) VALUES ($1)",
            user.as_uuid()
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            find(&pool, user).await.unwrap(),
            Some(UserSettings::defaults())
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn save_then_find_round_trips_and_save_replaces(pool: PgPool) {
        let user = testing::user(&pool).await;
        save(&pool, user, &custom()).await.unwrap();
        assert_eq!(find(&pool, user).await.unwrap(), Some(custom()));
        save(&pool, user, &UserSettings::defaults()).await.unwrap();
        assert_eq!(
            find(&pool, user).await.unwrap(),
            Some(UserSettings::defaults())
        );
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn out_of_range_values_are_rejected(pool: PgPool) {
        let user = testing::user(&pool).await;
        let max = UserSettings {
            bar_weight_ng: 2_000_000_000_000_000,
            ..UserSettings::defaults()
        };
        save(&pool, user, &max).await.unwrap();
        for bad in [
            UserSettings {
                bar_weight_ng: 2_000_000_000_000_001,
                ..UserSettings::defaults()
            },
            UserSettings {
                bar_weight_ng: u64::MAX,
                ..UserSettings::defaults()
            },
            UserSettings {
                plate_inventory: json!({"plate": 20}),
                ..UserSettings::defaults()
            },
            UserSettings {
                plate_inventory: JsonValue::Array(vec![json!({}); 17]),
                ..UserSettings::defaults()
            },
            // No bar, no plates (#34).
            UserSettings {
                bar_weight_ng: 0,
                ..UserSettings::defaults()
            },
            // A zero or out-of-range weight step (#103).
            UserSettings {
                kg_weight_step_ng: 0,
                ..UserSettings::defaults()
            },
            UserSettings {
                lb_weight_step_ng: u64::MAX,
                ..UserSettings::defaults()
            },
            UserSettings {
                plate_inventory: JsonValue::Array(Vec::new()),
                ..UserSettings::defaults()
            },
        ] {
            let error = save(&pool, user, &bad).await.unwrap_err();
            assert!(matches!(error, RepoError::Invalid { .. }), "{error:?}");
        }
        assert_eq!(find(&pool, user).await.unwrap(), Some(max));
    }

    #[sqlx::test(migrator = "MIGRATOR")]
    #[ignore = "needs Postgres"]
    async fn users_only_see_and_change_their_own_settings(pool: PgPool) {
        let (a, b) = testing::users_a_and_b(&pool).await;
        save(&pool, a, &custom()).await.unwrap();
        // B has none, not A's settings.
        assert_eq!(find(&pool, b).await.unwrap(), None);
        // B saving creates B's row and leaves A's alone.
        save(&pool, b, &UserSettings::defaults()).await.unwrap();
        assert_eq!(find(&pool, a).await.unwrap(), Some(custom()));
    }

    /// The migration that requires a bar and plates fixes the rows saved before it: a row
    /// violating the new rule before the migration is valid after it, and others are untouched.
    #[sqlx::test(migrations = false)]
    #[ignore = "needs Postgres"]
    async fn the_bar_and_plates_migration_fixes_stored_rows(pool: PgPool) {
        const FIX: i64 = 20_261_003_120_000;
        for migration in MIGRATOR.iter().filter(|migration| migration.version < FIX) {
            sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
        }
        let (broken, fine) = testing::users_a_and_b(&pool).await;
        sqlx::query(
            "INSERT INTO user_settings (user_id, unit, bar_weight_ng, plate_inventory) \
             VALUES ($1, 'lb', 0, '[]'), ($2, 'lb', 15000000000000, '[{\"plate\": 20.0, \"pairs\": 4}]')",
        )
        .bind(broken.as_uuid())
        .bind(fine.as_uuid())
        .execute(&pool)
        .await
        .unwrap();

        let fix = MIGRATOR
            .iter()
            .find(|migration| migration.version == FIX)
            .expect("the migration exists");
        sqlx::raw_sql(&fix.sql).execute(&pool).await.unwrap();
        for migration in MIGRATOR.iter().filter(|migration| migration.version > FIX) {
            sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
        }

        let fixed = find(&pool, broken).await.unwrap().unwrap();
        assert_eq!(fixed.bar_weight_ng, UserSettings::defaults().bar_weight_ng);
        assert_eq!(
            fixed.plate_inventory,
            UserSettings::defaults().plate_inventory
        );
        assert_eq!(fixed.unit, Unit::Lb);
        // It parses as the domain's inventory, and the rest of the row is kept.
        let inventory: iron_oxide_domain::PlateInventory =
            serde_json::from_value(fixed.plate_inventory).unwrap();
        assert_eq!(
            inventory,
            iron_oxide_domain::PlateInventory::default_for(iron_oxide_domain::Unit::Kg)
        );
        assert_eq!(
            find(&pool, fine).await.unwrap(),
            Some(UserSettings {
                unit: Unit::Lb,
                bar_weight_ng: 15_000_000_000_000,
                plate_inventory: json!([{"plate": 20.0, "pairs": 4}]),
                ..UserSettings::defaults()
            })
        );
        // And the rule now holds in the database.
        let error = save(
            &pool,
            fine,
            &UserSettings {
                bar_weight_ng: 0,
                ..UserSettings::defaults()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, RepoError::Invalid { .. }), "{error:?}");
    }
}
