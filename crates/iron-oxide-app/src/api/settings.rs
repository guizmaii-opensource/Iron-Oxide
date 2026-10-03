//! Settings server functions (#20): units, bar weight, plate inventory, default rest, sound, and
//! the training maxes (#34).
//!
//! Weights travel as the domain [`Weight`] (kg numbers on the wire, stored as exact nanograms);
//! `unit` only says how the UI shows and enters them.

use dioxus::prelude::*;
use iron_oxide_domain::{
    ExerciseId, PlateInventory, PlateStock, Seconds, Unit, Weight, time::Timestamp,
};
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use {
    crate::server::{AppState, api::settings as logic, auth::AuthUser},
    dioxus::server::axum::Extension,
};

/// The largest weight step [`update_settings`] accepts: 25 kg.
#[cfg_attr(
    not(any(feature = "server", test)),
    allow(dead_code, reason = "checked by the server")
)]
pub const MAX_WEIGHT_STEP_KG: f64 = 25.0;

/// The longest default rest [`update_settings`] accepts: one hour.
pub const MAX_DEFAULT_REST: Seconds = Seconds::new(3_600);

/// A user's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// How weights are shown and entered.
    pub unit: Unit,
    pub bar_weight: Weight,
    pub plate_inventory: PlateInventory,
    /// The rest between sets when the program does not say.
    pub default_rest: Seconds,
    pub sound_enabled: bool,
    /// How far the weight steppers move in kg mode. Defaults when missing, so that an export
    /// made before #103 still imports.
    #[serde(default = "default_kg_weight_step")]
    pub kg_weight_step: Weight,
    /// How far the weight steppers move in lb mode (default when missing, as above).
    #[serde(default = "default_lb_weight_step")]
    pub lb_weight_step: Weight,
    /// Whether the rest timer vibrates the phone (where the browser can); on when missing.
    #[serde(default = "default_vibration")]
    pub vibration_enabled: bool,
}

fn default_kg_weight_step() -> Weight {
    Settings::defaults().kg_weight_step
}

fn default_lb_weight_step() -> Weight {
    Settings::defaults().lb_weight_step
}

const fn default_vibration() -> bool {
    true
}

impl Settings {
    /// How far the weight steppers move in `unit`.
    #[must_use]
    pub const fn weight_step(&self, unit: Unit) -> Weight {
        match unit {
            Unit::Kg => self.kg_weight_step,
            Unit::Lb => self.lb_weight_step,
        }
    }

    /// The same settings with `step` as the weight step of `unit`.
    #[must_use]
    pub fn with_weight_step(&self, unit: Unit, step: Weight) -> Self {
        let mut settings = self.clone();
        match unit {
            Unit::Kg => settings.kg_weight_step = step,
            Unit::Lb => settings.lb_weight_step = step,
        }
        settings
    }

    /// The settings of a user who never saved any: kg, a 20 kg bar, the domain's default kg plate
    /// inventory, 2 minutes of rest, sound and vibration on, and steps of 2.5 kg or 5 lb.
    #[must_use]
    #[cfg_attr(
        not(any(feature = "server", test)),
        allow(
            dead_code,
            reason = "the server's answer for a new user; the client asks for it"
        )
    )]
    pub fn defaults() -> Self {
        Self {
            unit: Unit::Kg,
            bar_weight: Weight::from_kg(20.0).unwrap_or(Weight::ZERO),
            plate_inventory: PlateInventory::default_for(Unit::Kg),
            default_rest: Seconds::new(120),
            sound_enabled: true,
            kg_weight_step: Weight::from_kg(2.5).unwrap_or(Weight::ZERO),
            lb_weight_step: Weight::from_lb(5.0).unwrap_or(Weight::ZERO),
            vibration_enabled: true,
        }
    }
}

/// What [`update_settings`] saves: the same fields as [`Settings`], but the values the user types
/// travel unchecked (weights as kg numbers, the plate inventory as a plain list) and the server
/// validates them, so a bad one gets a `422` that says what is wrong instead of the generic
/// `422 Invalid request.` of an argument that does not decode.
///
/// The JSON is the same as [`Settings`]' (a [`Weight`] is a kg number), and converting a
/// [`Weight`] to kg and back is exact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsUpdate {
    pub unit: Unit,
    /// The bar weight in kg.
    pub bar_weight: f64,
    /// Plate sizes with their pair counts, in any order.
    pub plate_inventory: Vec<PlateInput>,
    pub default_rest: Seconds,
    pub sound_enabled: bool,
    /// The kg weight step, in kg. The #103 fields are optional: a client built before #103 does
    /// not send them, and an absent field keeps the saved value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kg_weight_step: Option<f64>,
    /// The lb weight step, in kg (a [`Weight`]'s JSON, like every weight); absent = unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lb_weight_step: Option<f64>,
    /// Absent = unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vibration_enabled: Option<bool>,
}

/// A plate size (in kg) and how many pairs of it are available: a [`PlateStock`] before
/// validation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlateInput {
    pub plate: f64,
    pub pairs: u32,
}

impl From<PlateStock> for PlateInput {
    fn from(stock: PlateStock) -> Self {
        Self {
            plate: stock.plate.as_kg(),
            pairs: stock.pairs,
        }
    }
}

impl From<Settings> for SettingsUpdate {
    fn from(settings: Settings) -> Self {
        Self {
            unit: settings.unit,
            bar_weight: settings.bar_weight.as_kg(),
            plate_inventory: settings
                .plate_inventory
                .stock()
                .iter()
                .copied()
                .map(PlateInput::from)
                .collect(),
            default_rest: settings.default_rest,
            sound_enabled: settings.sound_enabled,
            kg_weight_step: Some(settings.kg_weight_step.as_kg()),
            lb_weight_step: Some(settings.lb_weight_step.as_kg()),
            vibration_enabled: Some(settings.vibration_enabled),
        }
    }
}

/// One exercise's training max.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingMax {
    pub exercise_id: ExerciseId,
    pub weight: Weight,
    /// When it was set: the progression replays the sets logged after it.
    pub set_at: Timestamp,
}

/// The signed-in user's settings, or [`Settings::defaults`] if they never saved any.
#[post("/api/settings/get", state: Extension<AppState>, user: AuthUser)]
pub async fn get_settings() -> Result<Settings, ServerFnError> {
    Ok(logic::get(&state.db, user.owner()).await?)
}

/// Replaces the signed-in user's settings and returns them as saved (the plate inventory sorted
/// heaviest first). Saving the same settings again changes nothing.
///
/// # Errors
/// 422 for an invalid bar weight (or none) or plate inventory (or an empty one), with the reason
/// and the field in the details (`{"field": "bar_weight"}`), or a default rest above
/// [`MAX_DEFAULT_REST`].
#[post("/api/settings/update", state: Extension<AppState>, user: AuthUser)]
pub async fn update_settings(settings: SettingsUpdate) -> Result<Settings, ServerFnError> {
    Ok(logic::update(&state.db, user.owner(), settings).await?)
}

/// The signed-in user's training maxes, by exercise id.
#[post("/api/settings/training-maxes", state: Extension<AppState>, user: AuthUser)]
pub async fn training_maxes() -> Result<Vec<TrainingMax>, ServerFnError> {
    Ok(logic::training_maxes(&state.db, user.owner()).await?)
}

/// Sets (or replaces) the training max of an exercise, `weight` in kg (a [`Weight`]'s JSON). Its
/// `set_at` becomes now (the server's clock): the progression starts again from this value and
/// only replays sets logged after it.
///
/// # Errors
/// 422 when `exercise_id` is not a valid exercise id, or `weight` is zero or not a valid weight.
#[post("/api/settings/training-max/set", state: Extension<AppState>, user: AuthUser)]
pub async fn set_training_max(
    exercise_id: String,
    weight: f64,
) -> Result<TrainingMax, ServerFnError> {
    Ok(logic::set_training_max(&state.db, user.owner(), &exercise_id, weight).await?)
}

/// Removes the training max of an exercise.
///
/// # Errors
/// 404 when the user has none for that exercise; 422 for an invalid exercise id.
#[post("/api/settings/training-max/delete", state: Extension<AppState>, user: AuthUser)]
pub async fn delete_training_max(exercise_id: String) -> Result<(), ServerFnError> {
    Ok(logic::delete_training_max(&state.db, user.owner(), &exercise_id).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_saved_before_the_weight_steps_still_read() {
        // An export made before #103 has no weight steps or vibration.
        let mut json = serde_json::to_value(Settings::defaults()).unwrap();
        let object = json.as_object_mut().unwrap();
        object.remove("kg_weight_step");
        object.remove("lb_weight_step");
        object.remove("vibration_enabled");
        let read: Settings = serde_json::from_value(json).unwrap();
        assert_eq!(read, Settings::defaults());
    }

    #[test]
    fn the_weight_step_follows_the_unit() {
        let settings =
            Settings::defaults().with_weight_step(Unit::Lb, Weight::from_lb(2.5).unwrap());
        assert_eq!(
            settings.weight_step(Unit::Kg),
            Weight::from_kg(2.5).unwrap()
        );
        assert_eq!(
            settings.weight_step(Unit::Lb),
            Weight::from_lb(2.5).unwrap()
        );
    }
}
