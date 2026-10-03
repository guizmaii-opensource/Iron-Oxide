//! The weight steps offered, and the one-time move of the old device-only preferences (#103).
//!
//! The weight step and vibration were kept on the device (`localStorage`, one key per user) until
//! #103 moved them into the user's settings on the server. When a user's settings load, any value
//! still on this device is carried over, but only into a setting the server still has at its
//! default (a choice made on another device since wins), and the device's copy is removed.

use iron_oxide_domain::{Unit, Weight};
use serde::Deserialize;

use crate::api::settings::Settings;
use crate::auth::types::UserId;

/// The start of the old `localStorage` keys, one per user (`….<user id>`).
const STORAGE_KEY_PREFIX: &str = "iron-oxide.device-prefs.v1";

/// The weight steps offered, per unit, lightest first.
#[must_use]
pub const fn step_choices(unit: Unit) -> &'static [f64] {
    match unit {
        Unit::Kg => &[0.5, 1.0, 1.25, 2.5, 5.0],
        Unit::Lb => &[1.0, 2.5, 5.0, 10.0],
    }
}

/// What the device kept before #103.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct DevicePrefs {
    #[serde(default)]
    pub kg_step: Option<Weight>,
    #[serde(default)]
    pub lb_step: Option<Weight>,
    #[serde(default)]
    pub vibration: Option<bool>,
}

impl DevicePrefs {
    /// Reads a stored entry; anything unreadable is nothing to carry over.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        serde_json::from_str(stored).ok()
    }
}

/// The storage key of `user`'s old preferences on this device.
#[must_use]
pub fn storage_key(user: UserId) -> String {
    format!("{STORAGE_KEY_PREFIX}.{user}")
}

/// `settings` with the device's old preferences carried over into every setting the server still
/// has at its default.
#[must_use]
pub fn carry_over(settings: &Settings, device: DevicePrefs) -> Settings {
    let defaults = Settings::defaults();
    let mut carried = settings.clone();
    if let Some(step) = device.kg_step.filter(|step| !step.is_zero())
        && settings.kg_weight_step == defaults.kg_weight_step
    {
        carried.kg_weight_step = step;
    }
    if let Some(step) = device.lb_step.filter(|step| !step.is_zero())
        && settings.lb_weight_step == defaults.lb_weight_step
    {
        carried.lb_weight_step = step;
    }
    if let Some(vibration) = device.vibration
        && settings.vibration_enabled == defaults.vibration_enabled
    {
        carried.vibration_enabled = vibration;
    }
    carried
}

/// Takes `user`'s old preferences off this device, if any.
#[must_use]
pub fn take_device_prefs(user: UserId) -> Option<DevicePrefs> {
    let key = storage_key(user);
    let stored = storage::read(&key)?;
    storage::remove(&key);
    DevicePrefs::parse(&stored)
}

/// `localStorage`, best effort.
#[cfg(feature = "web")]
mod storage {
    fn local_storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }

    pub fn read(key: &str) -> Option<String> {
        local_storage()?.get_item(key).ok().flatten()
    }

    pub fn remove(key: &str) {
        if let Some(storage) = local_storage() {
            let _ = storage.remove_item(key);
        }
    }
}

/// No storage outside the browser.
#[cfg(not(feature = "web"))]
mod storage {
    pub const fn read(_key: &str) -> Option<String> {
        None
    }

    pub const fn remove(_key: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kg(value: f64) -> Weight {
        Weight::from_kg(value).unwrap()
    }

    #[test]
    fn old_entries_are_read_as_they_were_written() {
        // What #34's build stored.
        let stored = r#"{"kg_step":1.0,"lb_step":null,"vibration":false}"#;
        assert_eq!(
            DevicePrefs::parse(stored),
            Some(DevicePrefs {
                kg_step: Some(kg(1.0)),
                lb_step: None,
                vibration: Some(false),
            })
        );
        assert_eq!(DevicePrefs::parse("not json"), None);
    }

    #[test]
    fn device_values_fill_settings_still_at_their_defaults() {
        let device = DevicePrefs {
            kg_step: Some(kg(1.0)),
            lb_step: None,
            vibration: Some(false),
        };
        let carried = carry_over(&Settings::defaults(), device);
        assert_eq!(carried.kg_weight_step, kg(1.0));
        assert_eq!(carried.lb_weight_step, Settings::defaults().lb_weight_step);
        assert!(!carried.vibration_enabled);
    }

    #[test]
    fn a_choice_already_on_the_server_wins() {
        let server = Settings {
            kg_weight_step: kg(5.0),
            ..Settings::defaults()
        };
        let device = DevicePrefs {
            kg_step: Some(kg(1.0)),
            lb_step: None,
            vibration: None,
        };
        assert_eq!(carry_over(&server, device), server);
    }

    #[test]
    fn each_user_had_their_own_key() {
        let a = UserId::from_uuid(uuid::Uuid::from_u128(1));
        assert_eq!(storage_key(a), format!("iron-oxide.device-prefs.v1.{a}"));
    }

    #[test]
    fn every_step_choice_is_a_valid_weight_and_includes_the_defaults() {
        for unit in Unit::ALL {
            let max = Weight::from_kg(crate::api::settings::MAX_WEIGHT_STEP_KG).unwrap();
            for &value in step_choices(unit) {
                let step = Weight::new(value, unit).unwrap();
                assert!(!step.is_zero() && step <= max, "{value} {unit}");
            }
            let default = Settings::defaults().weight_step(unit);
            assert!(
                step_choices(unit)
                    .iter()
                    .any(|&value| Weight::new(value, unit).unwrap() == default)
            );
        }
    }
}
