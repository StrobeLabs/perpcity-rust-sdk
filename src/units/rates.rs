//! The rates the market accrues at, and the dimensionless ratios it
//! compares against.
//!
//! A rate is a quantity per day, WAD encoded. There are two of them rather
//! than one because the contract's two rates differ in sign and width, and
//! collapsing them would cost something: funding is `int88` and flows either
//! way, so a position may receive it; a utilization fee is `uint64` and is
//! only ever charged. One signed type would either lose the "cannot be
//! negative" claim on the fee legs or force a check where the width already
//! guaranteed it.
//!
//! A [`Ratio`] is dimensionless: a margin threshold, a fee share, a
//! utilization fraction. The contract holds all of them as `uint24` scaled
//! by a million, so the integer is exact and the fraction a person reads is
//! the derived view — the same shape as an amount and its dollars.

use crate::errors::ValidationError;

use super::F64_1E6;

/// The largest value a `uint24` holds, which is the domain of every ratio
/// the margin and fee modules store.
const MAX_E6: u32 = (1 << 24) - 1;

/// WAD as `f64`, for the human view of a rate.
const F64_WAD: f64 = 1e18;

/// The funding rate, per day, WAD encoded.
///
/// Signed, and the sign is the direction: positive means longs pay shorts.
/// The contract stores it as `int88`, so it always fits an `i128`.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct FundingRate(i128);

/// A utilization fee rate, per day, WAD encoded.
///
/// Unsigned: the contract charges this and never pays it, and stores it as
/// `uint64`.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct UtilizationRate(u64);

/// A dimensionless ratio: a margin threshold, a fee share, a utilization
/// fraction.
///
/// Held as the contract holds it, a `uint24` scaled by a million, so
/// `50_000` is 5% and `1_000_000` is 100%. The integer is the value and
/// [`Self::fraction`] is the lossy view a person reads, which is why the
/// type carries the scale and not the name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Ratio(u32);

/// Deserialising a ratio runs the same domain check as constructing one, so
/// a persisted value outside the contract's `uint24` is refused where it
/// enters rather than carried.
impl<'de> serde::Deserialize<'de> for Ratio {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_e6(u32::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl FundingRate {
    /// Neither side pays.
    pub const ZERO: Self = Self(0);

    /// From the contract's WAD word.
    pub const fn from_wad(per_day_wad: i128) -> Self {
        Self(per_day_wad)
    }

    /// The WAD word, for exact arithmetic.
    pub const fn wad(self) -> i128 {
        self.0
    }

    /// Whether neither side pays.
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Whether shorts pay longs.
    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// The rate as a person reads it: a fraction of position value per day.
    ///
    /// Lossy, and never arithmetic the chain will check.
    pub fn per_day(self) -> f64 {
        self.0 as f64 / F64_WAD
    }
}

impl UtilizationRate {
    /// No utilization fee.
    pub const ZERO: Self = Self(0);

    /// From the contract's WAD word.
    pub const fn from_wad(per_day_wad: u64) -> Self {
        Self(per_day_wad)
    }

    /// The WAD word, for exact arithmetic.
    pub const fn wad(self) -> u64 {
        self.0
    }

    /// Whether the side charges nothing.
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// The rate as a person reads it: a fraction of position value per day.
    ///
    /// Lossy, and never arithmetic the chain will check.
    pub fn per_day(self) -> f64 {
        self.0 as f64 / F64_WAD
    }
}

impl Ratio {
    /// Zero.
    pub const ZERO: Self = Self(0);

    /// The whole, `1_000_000`.
    pub const ONE: Self = Self(1_000_000);

    /// From the contract's millionths.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidMarginRatio`] when the value is outside the
    /// `uint24` the modules store it in, which is state no contract produced.
    pub fn from_e6(e6: u32) -> Result<Self, ValidationError> {
        if e6 <= MAX_E6 {
            Ok(Self(e6))
        } else {
            Err(ValidationError::InvalidMarginRatio {
                value: e6,
                min: 0,
                max: MAX_E6,
            })
        }
    }

    /// The millionths, for exact comparison.
    pub const fn e6(self) -> u32 {
        self.0
    }

    /// Whether it is zero.
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// The ratio as a person reads it, `0.05` for 5%.
    ///
    /// Exact for every value the contract can hold: a `uint24` over a
    /// million round-trips through `f64`.
    pub fn fraction(self) -> f64 {
        self.0 as f64 / F64_1E6
    }

    /// The initial margin ratio that permits `leverage`, which is its
    /// reciprocal: `1_000_000 / leverage`.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidLeverage`] when `leverage` is not a positive
    /// finite number or its reciprocal falls outside the ratio's domain.
    pub fn for_leverage(leverage: f64) -> Result<Self, ValidationError> {
        let invalid = |reason: String| ValidationError::InvalidLeverage { reason };
        if !leverage.is_finite() || leverage <= 0.0 {
            return Err(invalid(format!(
                "leverage must be a positive finite number, got {leverage}"
            )));
        }
        let e6 = (F64_1E6 / leverage).round();
        if e6 < 1.0 || e6 > f64::from(MAX_E6) {
            return Err(invalid(format!(
                "leverage {leverage} implies a margin ratio of {e6}, outside the contract's range"
            )));
        }
        Self::from_e6(e6 as u32)
    }

    /// The leverage this ratio permits, its reciprocal.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidMarginRatio`] when the ratio is zero, which
    /// permits no finite leverage.
    pub fn leverage(self) -> Result<f64, ValidationError> {
        if self.is_zero() {
            return Err(ValidationError::InvalidMarginRatio {
                value: 0,
                min: 1,
                max: MAX_E6,
            });
        }
        Ok(F64_1E6 / f64::from(self.0))
    }
}

impl TryFrom<f64> for Ratio {
    type Error = ValidationError;

    /// A ratio a person wrote, as a fraction.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidConfig`] when the fraction is not a finite
    /// non-negative number, and [`ValidationError::InvalidMarginRatio`] when
    /// it scales past the `uint24` domain.
    fn try_from(fraction: f64) -> Result<Self, ValidationError> {
        if !fraction.is_finite() || fraction < 0.0 {
            return Err(ValidationError::InvalidConfig {
                reason: format!("a ratio must be a finite non-negative fraction, got {fraction}"),
            });
        }
        let scaled = (fraction * F64_1E6).round();
        if scaled > f64::from(MAX_E6) {
            return Err(ValidationError::InvalidMarginRatio {
                // The rejected value, not the bound: a float-to-integer cast
                // saturates, so a vast fraction reports `u32::MAX`.
                value: scaled as u32,
                min: 0,
                max: MAX_E6,
            });
        }
        Self::from_e6(scaled as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ratio is the contract's integer, and the fraction a person reads
    /// round-trips back to it exactly.
    #[test]
    fn a_ratio_round_trips_through_its_fraction() {
        let five_percent = Ratio::from_e6(50_000).unwrap();
        assert_eq!(five_percent.fraction(), 0.05);
        assert_eq!(Ratio::try_from(0.05).unwrap(), five_percent);
        assert_eq!(Ratio::ONE.fraction(), 1.0);
    }

    /// The domain is the contract's `uint24`, at both doors.
    #[test]
    fn a_ratio_outside_the_contracts_domain_is_refused() {
        assert!(Ratio::from_e6(MAX_E6).is_ok());
        assert!(Ratio::from_e6(MAX_E6 + 1).is_err());
        assert!(Ratio::try_from(20.0).is_err(), "2000% is past uint24");
        for bad in [-0.1, f64::NAN, f64::INFINITY] {
            assert!(Ratio::try_from(bad).is_err(), "{bad}");
        }
    }

    /// The funding rate carries its direction; a utilization rate has none
    /// to carry.
    #[test]
    fn a_funding_rate_is_signed_and_a_fee_rate_is_not() {
        let longs_pay = FundingRate::from_wad(840_374_978_539_967_329);
        assert!(!longs_pay.is_negative());
        assert!((longs_pay.per_day() - 0.840_374_978_539_967_3).abs() < 1e-15);
        assert!(FundingRate::from_wad(-1).is_negative());

        let fee = UtilizationRate::from_wad(10_000_000_000_000_000);
        assert!((fee.per_day() - 0.01).abs() < 1e-18);
        assert!(UtilizationRate::ZERO.is_zero());
    }

    /// The wire form is the bare number, so persisted rates and ratios read
    /// back as what they were — and a ratio outside the domain is refused
    /// where it enters rather than carried.
    #[test]
    fn the_wire_form_is_the_number_alone() {
        let five_percent = Ratio::from_e6(50_000).unwrap();
        assert_eq!(serde_json::to_string(&five_percent).unwrap(), "50000");
        assert_eq!(
            serde_json::from_str::<Ratio>("50000").unwrap(),
            five_percent
        );
        assert!(serde_json::from_str::<Ratio>("16777216").is_err());

        assert_eq!(
            serde_json::to_string(&FundingRate::from_wad(-5)).unwrap(),
            "-5"
        );
        assert_eq!(serde_json::from_str::<FundingRate>("-5").unwrap().wad(), -5);
        assert_eq!(
            serde_json::to_string(&UtilizationRate::from_wad(7)).unwrap(),
            "7"
        );
    }
}
