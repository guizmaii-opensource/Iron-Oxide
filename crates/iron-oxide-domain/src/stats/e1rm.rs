//! Estimated one-rep max (e1RM).

use serde::{Deserialize, Serialize};

use crate::{Reps, Weight};

/// The highest rep count an estimate is made for. Both formulas drift apart and lose accuracy past
/// about 10 to 12 reps (Brzycki even diverges at 37), so a set with more reps gets no estimate
/// rather than a misleading one.
pub const MAX_E1RM_REPS: Reps = Reps::new(12);

/// How a one-rep max is estimated from a set of `r` reps at weight `w`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum E1rmFormula {
    /// `w × (1 + r / 30)`, the default. A single rep is taken as is (the raw formula would add
    /// 1/30).
    #[default]
    Epley,
    /// `w × 36 / (37 − r)`. Gives lower estimates than Epley below 10 reps and higher above.
    Brzycki,
}

impl E1rmFormula {
    /// The formula of every statistic the app reports: the records of the end-of-session summary,
    /// the history's e1RM, PR flags and charts. One constant, so they cannot disagree.
    pub const STANDARD: Self = Self::Epley;

    /// Both formulas, for pickers.
    pub const ALL: [Self; 2] = [Self::Epley, Self::Brzycki];

    /// Estimates the one-rep max of `weight` lifted for `reps`.
    ///
    /// - 1 rep: the weight itself, with either formula.
    /// - 2 to [`MAX_E1RM_REPS`] reps: the formula, computed exactly on nanograms and rounded to the
    ///   nearest nanogram (a halfway value rounds up).
    /// - `None` for 0 reps (nothing was lifted), for more than [`MAX_E1RM_REPS`] reps (no reliable
    ///   estimate) and when the estimate is above [`Weight::MAX`] (impossible for a human lift).
    #[must_use]
    pub fn estimate(self, weight: Weight, reps: Reps) -> Option<Weight> {
        let reps = reps.get();
        if reps == 0 || reps > MAX_E1RM_REPS.get() {
            return None;
        }
        if reps == 1 {
            return Some(weight);
        }
        let (numerator, denominator) = match self {
            Self::Epley => (30 + reps, 30),
            Self::Brzycki => (36, 37 - reps),
        };
        let product = u128::from(weight.as_nanograms()) * u128::from(numerator);
        let denominator = u128::from(denominator);
        let rounded = (product + denominator / 2) / denominator;
        u64::try_from(rounded)
            .ok()
            .and_then(|nanograms| Weight::from_nanograms(nanograms).ok())
    }
}

/// Estimates the one-rep max with the app's formula ([`E1rmFormula::STANDARD`], Epley). See
/// [`E1rmFormula::estimate`].
#[must_use]
pub fn estimate_1rm(weight: Weight, reps: Reps) -> Option<Weight> {
    E1rmFormula::STANDARD.estimate(weight, reps)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::Unit;
    use crate::stats::test_support::{kg, reps};

    fn epley(weight: f64, count: u16) -> Option<Weight> {
        E1rmFormula::Epley.estimate(kg(weight), reps(count))
    }

    fn brzycki(weight: f64, count: u16) -> Option<Weight> {
        E1rmFormula::Brzycki.estimate(kg(weight), reps(count))
    }

    fn formatted(estimate: Option<Weight>) -> String {
        estimate.unwrap().format_value(Unit::Kg, 2)
    }

    #[test]
    fn epley_table_for_100_kg() {
        // 100 × (1 + r / 30), with 1 rep taken as is.
        let expected = [
            "100", "106.67", "110", "113.33", "116.67", "120", "123.33", "126.67", "130", "133.33",
            "136.67", "140",
        ];
        for (count, value) in (1..=12).zip(expected) {
            assert_eq!(formatted(epley(100.0, count)), value, "{count} reps");
        }
    }

    #[test]
    fn brzycki_table_for_100_kg() {
        // 100 × 36 / (37 − r).
        let expected = [
            "100", "102.86", "105.88", "109.09", "112.5", "116.13", "120", "124.14", "128.57",
            "133.33", "138.46", "144",
        ];
        for (count, value) in (1..=12).zip(expected) {
            assert_eq!(formatted(brzycki(100.0, count)), value, "{count} reps");
        }
    }

    #[test]
    fn exact_values_are_exact() {
        assert_eq!(epley(100.0, 3), Some(kg(110.0)));
        assert_eq!(epley(150.0, 10), Some(kg(200.0)));
        assert_eq!(brzycki(100.0, 5), Some(kg(112.5)));
        assert_eq!(brzycki(100.0, 7), Some(kg(120.0)));
        assert_eq!(brzycki(100.0, 12), Some(kg(144.0)));
    }

    #[test]
    fn rounds_to_the_nearest_nanogram() {
        // 100 kg × 35 / 30 = 116 666 666 666 666.67 ng.
        assert_eq!(epley(100.0, 5).unwrap().as_nanograms(), 116_666_666_666_667);
        // 100 kg × 32 / 30 = 106 666 666 666 666.67 ng.
        assert_eq!(epley(100.0, 2).unwrap().as_nanograms(), 106_666_666_666_667);
        // 100 kg × 36 / 35 = 102 857 142 857 142.86 ng.
        assert_eq!(
            brzycki(100.0, 2).unwrap().as_nanograms(),
            102_857_142_857_143
        );
        // 100 kg × 36 / 34 = 105 882 352 941 176.47 ng: rounds down.
        assert_eq!(
            brzycki(100.0, 3).unwrap().as_nanograms(),
            105_882_352_941_176
        );
    }

    #[test]
    fn rounds_an_exact_half_up() {
        let ng = |value: u64| Weight::from_nanograms(value).unwrap();
        // 3 ng × 35 / 30 = 3.5 ng → 4 ng.
        assert_eq!(E1rmFormula::Epley.estimate(ng(3), reps(5)), Some(ng(4)));
        // 1 ng × 33 / 30 = 1.1 ng → 1 ng.
        assert_eq!(E1rmFormula::Epley.estimate(ng(1), reps(3)), Some(ng(1)));
        // 1 ng × 36 / 25 = 1.44 ng → 1 ng; 2 ng × 36 / 25 = 2.88 ng → 3 ng.
        assert_eq!(E1rmFormula::Brzycki.estimate(ng(1), reps(12)), Some(ng(1)));
        assert_eq!(E1rmFormula::Brzycki.estimate(ng(2), reps(12)), Some(ng(3)));
    }

    #[test]
    fn one_rep_is_the_weight_itself() {
        for formula in E1rmFormula::ALL {
            assert_eq!(formula.estimate(kg(142.5), reps(1)), Some(kg(142.5)));
            assert_eq!(formula.estimate(Weight::MAX, reps(1)), Some(Weight::MAX));
        }
    }

    #[test]
    fn zero_reps_has_no_estimate() {
        for formula in E1rmFormula::ALL {
            assert_eq!(formula.estimate(kg(100.0), Reps::ZERO), None);
        }
    }

    #[test]
    fn rep_cap_boundary() {
        for formula in E1rmFormula::ALL {
            assert!(formula.estimate(kg(100.0), MAX_E1RM_REPS).is_some());
            assert_eq!(formula.estimate(kg(100.0), reps(13)), None);
            assert_eq!(formula.estimate(kg(100.0), reps(37)), None);
            assert_eq!(formula.estimate(kg(100.0), Reps::MAX), None);
        }
    }

    #[test]
    fn zero_weight_estimates_zero() {
        for formula in E1rmFormula::ALL {
            assert_eq!(formula.estimate(Weight::ZERO, reps(8)), Some(Weight::ZERO));
        }
    }

    #[test]
    fn estimate_above_the_weight_cap_is_none() {
        // 1 500 kg × 40 / 30 = 2 000 kg: the cap itself is fine.
        assert_eq!(epley(1_500.0, 10), Some(Weight::MAX));
        // 1 500 kg × 41 / 30 > 2 000 kg.
        assert_eq!(epley(1_500.0, 11), None);
        assert_eq!(E1rmFormula::Epley.estimate(Weight::MAX, reps(2)), None);
        assert_eq!(
            E1rmFormula::Brzycki.estimate(Weight::MAX, MAX_E1RM_REPS),
            None
        );
    }

    #[test]
    fn default_formula_is_epley() {
        assert_eq!(E1rmFormula::default(), E1rmFormula::Epley);
        assert_eq!(E1rmFormula::STANDARD, E1rmFormula::default());
        assert_eq!(estimate_1rm(kg(100.0), reps(5)), epley(100.0, 5));
        assert_eq!(estimate_1rm(kg(100.0), reps(13)), None);
    }

    #[test]
    fn formula_serde() {
        assert_eq!(
            serde_json::to_string(&E1rmFormula::Brzycki).unwrap(),
            "\"brzycki\""
        );
        assert_eq!(
            serde_json::from_str::<E1rmFormula>("\"epley\"").unwrap(),
            E1rmFormula::Epley
        );
        assert!(serde_json::from_str::<E1rmFormula>("\"lombardi\"").is_err());
    }

    #[test]
    fn works_in_pounds() {
        // 225 lb × 1 + 5 / 30 = 262.5 lb, exactly.
        let lb = |value: f64| Weight::from_lb(value).unwrap();
        assert_eq!(
            E1rmFormula::Epley.estimate(lb(225.0), reps(5)),
            Some(lb(262.5))
        );
    }

    fn weight_up_to(max_kg: u64) -> impl Strategy<Value = Weight> {
        (0..=max_kg * 1_000_000_000_000).prop_map(|ng| Weight::from_nanograms(ng).unwrap())
    }

    fn formula() -> impl Strategy<Value = E1rmFormula> {
        prop_oneof![Just(E1rmFormula::Epley), Just(E1rmFormula::Brzycki)]
    }

    proptest! {
        // 1 380 kg × 1.44 (Brzycki at 12 reps) stays below the 2 000 kg cap, so every estimate
        // exists in this range.
        #[test]
        fn monotonic_in_weight(
            formula in formula(),
            a in weight_up_to(1_380),
            b in weight_up_to(1_380),
            count in 1..=12_u16,
        ) {
            let (light, heavy) = if a <= b { (a, b) } else { (b, a) };
            let light = formula.estimate(light, reps(count)).unwrap();
            let heavy = formula.estimate(heavy, reps(count)).unwrap();
            prop_assert!(light <= heavy);
        }

        #[test]
        fn monotonic_in_reps(
            formula in formula(),
            weight in weight_up_to(1_380),
            count in 1..12_u16,
        ) {
            let fewer = formula.estimate(weight, reps(count)).unwrap();
            let more = formula.estimate(weight, reps(count + 1)).unwrap();
            prop_assert!(fewer <= more);
            if !weight.is_zero() {
                // Each extra rep adds at least 1/35 of the weight, so the estimate grows strictly once
                // that is at least one nanogram.
                prop_assert!(fewer < more || weight.as_nanograms() < 35);
            }
        }

        #[test]
        fn never_below_the_lifted_weight(
            formula in formula(),
            weight in weight_up_to(1_380),
            count in 1..=12_u16,
        ) {
            prop_assert!(formula.estimate(weight, reps(count)).unwrap() >= weight);
        }

        #[test]
        fn none_exactly_outside_the_rep_range(
            formula in formula(),
            weight in weight_up_to(1_380),
            count in any::<u16>(),
        ) {
            let estimate = formula.estimate(weight, reps(count));
            prop_assert_eq!(estimate.is_some(), (1..=12).contains(&count));
        }
    }
}
