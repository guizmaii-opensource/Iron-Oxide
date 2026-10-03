//! The lifter's settings that the engine needs.

use serde::{Deserialize, Serialize};

use crate::{PlateInventory, Rounding, Unit, ValueError, Weight};

/// The unit the lifter loads in, and the step the engine rounds weights to.
///
/// The step is the smallest change the lifter can load: 2.5 kg (two 1.25 kg plates) or 5 lb (two
/// 2.5 lb plates) by default, and anything above zero otherwise: a lifter whose smallest plates are
/// 2.5 kg steps by 5 kg (#60). The unit decides whether a weight written in the program must be
/// converted: a program load written in the other unit is rounded to the step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "ProgressionSettingsRepr")]
pub struct ProgressionSettings {
    unit: Unit,
    step: Weight,
}

/// The unchecked shape of stored [`ProgressionSettings`].
#[derive(Deserialize)]
struct ProgressionSettingsRepr {
    unit: Unit,
    step: Weight,
}

impl TryFrom<ProgressionSettingsRepr> for ProgressionSettings {
    type Error = ValueError;

    fn try_from(repr: ProgressionSettingsRepr) -> Result<Self, Self::Error> {
        Self::new(repr.unit, repr.step)
    }
}

impl ProgressionSettings {
    /// Loads in `unit` and rounds to `step`.
    ///
    /// Any step above zero is accepted, in either unit. Offering steps the lifter's plates can
    /// make (1.25 kg, 2.5 kg, 5 kg, 2.5 lb, 5 lb…) is the settings screen's job; an odd step such
    /// as 5.5 lb just rounds targets to multiples of it.
    ///
    /// Past training max sessions are judged against the target stored with each set
    /// ([`LoggedSet::target`](crate::LoggedSet::target)), never against the settings, so the step
    /// needs no upper bound (until #60 it was capped at 2.5 kg, the bound the legacy tolerance
    /// relies on; sets logged before #60 are still judged that way).
    ///
    /// # Errors
    /// [`ValueError::ZeroIncrement`] when `step` is zero.
    pub const fn new(unit: Unit, step: Weight) -> Result<Self, ValueError> {
        if step.is_zero() {
            Err(ValueError::ZeroIncrement)
        } else {
            Ok(Self { unit, step })
        }
    }

    /// The settings of a lifter who loads in `unit`, with `weight_step` (their weight step for that
    /// unit, #103) and the plates they own (#120).
    ///
    /// The step is `weight_step`, but never finer than what the plates can load: the smallest
    /// change their plates make is a pair of the smallest plate they have (one per side, as the
    /// plate calculator loads them), and the step is raised to the first multiple of it at or
    /// above `weight_step`. A 1 kg step with 1.25 kg plates steps by 2.5 kg; a 3 kg step with
    /// them by 5 kg; a 5 kg step with them stays 5 kg. Without plates (none listed, or none with a
    /// pair), the step is `weight_step` as is. A zero `weight_step` (never stored) falls back to
    /// [`for_unit`](Self::for_unit).
    #[must_use]
    pub fn for_lifter(unit: Unit, weight_step: Weight, plates: &PlateInventory) -> Self {
        let smallest_change = plates
            .stock()
            .iter()
            .filter(|stock| stock.pairs > 0)
            .map(|stock| stock.plate)
            .min()
            .and_then(|plate| plate.checked_mul(2).ok())
            .filter(|change| !change.is_zero());
        let step = match smallest_change {
            Some(change) => weight_step
                .round_to(change, Rounding::Up)
                .unwrap_or(weight_step),
            None => weight_step,
        };
        Self::new(unit, step).unwrap_or_else(|_| Self::for_unit(unit))
    }

    /// The default for a lifter who loads in `unit`: a step of 2.5 kg or 5 lb.
    #[must_use]
    pub fn for_unit(unit: Unit) -> Self {
        let step = match unit {
            // 2.5 kg and 5 lb in nanograms: exact.
            Unit::Kg => 2_500_000_000_000,
            Unit::Lb => 2_267_961_850_000,
        };
        match Weight::from_nanograms(step) {
            Ok(step) => Self { unit, step },
            // Unreachable: both values are far below Weight::MAX.
            Err(_) => Self {
                unit,
                step: Weight::MAX,
            },
        }
    }

    /// The unit the lifter loads in.
    #[must_use]
    pub const fn unit(self) -> Unit {
        self.unit
    }

    /// The loadable step computed weights are rounded to.
    #[must_use]
    pub const fn step(self) -> Weight {
        self.step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_per_unit() {
        let kg = ProgressionSettings::for_unit(Unit::Kg);
        assert_eq!(kg.unit(), Unit::Kg);
        assert_eq!(kg.step(), Weight::from_kg(2.5).unwrap());
        let lb = ProgressionSettings::for_unit(Unit::Lb);
        assert_eq!(lb.unit(), Unit::Lb);
        assert_eq!(lb.step(), Weight::from_lb(5.0).unwrap());
    }

    #[test]
    fn custom_step() {
        let step = Weight::from_kg(1.0).unwrap();
        let settings = ProgressionSettings::new(Unit::Kg, step).unwrap();
        assert_eq!(settings.step(), step);
        assert_eq!(settings.unit(), Unit::Kg);
        assert_eq!(
            ProgressionSettings::new(Unit::Lb, Weight::ZERO),
            Err(ValueError::ZeroIncrement)
        );
        // No upper bound since #60: a 5 kg step (2.5 kg plates), or any other.
        for step in [
            Weight::from_kg(5.0).unwrap(),
            Weight::from_kg(20.0).unwrap(),
            Weight::MAX,
        ] {
            assert_eq!(
                ProgressionSettings::new(Unit::Kg, step).unwrap().step(),
                step
            );
        }
    }

    fn plates(sizes: &[(f64, Unit)]) -> PlateInventory {
        PlateInventory::new(sizes.iter().map(|&(size, unit)| crate::PlateStock {
            plate: match unit {
                Unit::Kg => Weight::from_kg(size).unwrap(),
                Unit::Lb => Weight::from_lb(size).unwrap(),
            },
            pairs: 2,
        }))
        .unwrap()
    }

    #[test]
    fn the_lifters_step_is_never_finer_than_their_plates() {
        let kg = |value| Weight::from_kg(value).unwrap();
        let lb = |value| Weight::from_lb(value).unwrap();
        let with_quarter_kilos = plates(&[(20.0, Unit::Kg), (1.25, Unit::Kg)]);
        let with_two_and_a_half = plates(&[(20.0, Unit::Kg), (2.5, Unit::Kg)]);
        // (unit, the lifter's step, plates, the step used)
        let cases = [
            (Unit::Kg, kg(1.25), &with_quarter_kilos, kg(2.5)),
            (Unit::Kg, kg(2.5), &with_quarter_kilos, kg(2.5)),
            (Unit::Kg, kg(5.0), &with_quarter_kilos, kg(5.0)),
            (Unit::Kg, kg(3.0), &with_quarter_kilos, kg(5.0)),
            (Unit::Kg, kg(1.25), &with_two_and_a_half, kg(5.0)),
            (Unit::Kg, kg(2.5), &with_two_and_a_half, kg(5.0)),
            (Unit::Kg, kg(5.0), &with_two_and_a_half, kg(5.0)),
            (Unit::Kg, kg(1.25), &PlateInventory::empty(), kg(1.25)),
            (
                Unit::Lb,
                lb(2.5),
                &PlateInventory::default_for(Unit::Lb),
                lb(5.0),
            ),
            (
                Unit::Lb,
                lb(5.0),
                &PlateInventory::default_for(Unit::Lb),
                lb(5.0),
            ),
            (
                Unit::Lb,
                lb(10.0),
                &PlateInventory::default_for(Unit::Lb),
                lb(10.0),
            ),
            (
                Unit::Lb,
                lb(2.5),
                &plates(&[(45.0, Unit::Lb), (1.25, Unit::Lb)]),
                lb(2.5),
            ),
            (
                Unit::Lb,
                lb(5.0),
                &plates(&[(45.0, Unit::Lb), (5.0, Unit::Lb)]),
                lb(10.0),
            ),
        ];
        for (unit, step, plates, expected) in cases {
            let settings = ProgressionSettings::for_lifter(unit, step, plates);
            assert_eq!(settings.unit(), unit);
            assert_eq!(settings.step(), expected, "{step:?} with {plates:?}");
        }
        // Plates with no pair do not count; a zero step falls back to the default.
        let unusable = PlateInventory::new([crate::PlateStock {
            plate: kg(0.5),
            pairs: 0,
        }])
        .unwrap();
        assert_eq!(
            ProgressionSettings::for_lifter(Unit::Kg, kg(1.0), &unusable).step(),
            kg(1.0)
        );
        assert_eq!(
            ProgressionSettings::for_lifter(Unit::Kg, Weight::ZERO, &with_quarter_kilos),
            ProgressionSettings::for_unit(Unit::Kg)
        );
    }

    #[test]
    fn serde_round_trip_and_rejects_zero() {
        let settings = ProgressionSettings::for_unit(Unit::Kg);
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(json, r#"{"unit":"kg","step":2.5}"#);
        assert_eq!(
            serde_json::from_str::<ProgressionSettings>(&json).unwrap(),
            settings
        );
        let error =
            serde_json::from_str::<ProgressionSettings>(r#"{"unit":"lb","step":0}"#).unwrap_err();
        assert!(error.to_string().contains("greater than zero"), "{error}");
    }
}
