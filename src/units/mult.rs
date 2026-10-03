//! A dimensionless factor: what a price or an amount is multiplied by.
//!
//! A leverage, a stop at half the mark, a landing zone a few percent either
//! side of the index, a margin buffer of one percent: each is a scalar the
//! strategy chose, applied to a typed quantity. A [`Share`] covers the ones
//! bounded by one; this is the rest, and a share is one of these that
//! happens to be small. Held as WAD, which keeps every digit an `f64` has,
//! so the float is converted once and the arithmetic after it is exact.

use crate::errors::ValidationError;

use super::F64_WAD;
use super::share::Share;

/// A non-negative factor, WAD encoded: `1e18` is one.
///
/// Serialises as the number a person writes (`1.5`), because the places
/// that hold one are a strategy's configuration rather than a contract's
/// word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Mult(u128);

impl Default for Mult {
    fn default() -> Self {
        Self::ONE
    }
}

impl serde::Serialize for Mult {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.factor())
    }
}

impl<'de> serde::Deserialize<'de> for Mult {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(f64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl Mult {
    /// Unchanged.
    pub const ONE: Self = Self(1_000_000_000_000_000_000);

    /// From the WAD word.
    pub const fn from_wad(wad: u128) -> Self {
        Self(wad)
    }

    /// The WAD word, for exact arithmetic.
    pub const fn wad(self) -> u128 {
        self.0
    }

    /// The factor as a person reads it, `1.5` for one and a half. Lossy
    /// past 2^53 WAD, which no strategy's factor reaches.
    pub fn factor(self) -> f64 {
        self.0 as f64 / F64_WAD
    }

    /// Whether it leaves a quantity unchanged.
    pub const fn is_one(self) -> bool {
        self.0 == Self::ONE.0
    }

    /// One plus a share: a buffer, as in a margin one percent over the
    /// requirement.
    pub fn one_plus(share: Share) -> Self {
        Self(Self::ONE.0 + Self::from(share).0)
    }

    /// One minus a share: a discount, as in a floor a few percent under
    /// the index.
    pub fn one_minus(share: Share) -> Self {
        Self(Self::ONE.0 - Self::from(share).0)
    }
}

/// WAD per millionth: what a share's `e6` widens by.
const WAD_PER_E6: u128 = 1_000_000_000_000;

impl From<Share> for Mult {
    /// A share is a factor of at most one; millionths widen to WAD exactly.
    fn from(share: Share) -> Self {
        Self(u128::from(share.e6()) * WAD_PER_E6)
    }
}

impl TryFrom<f64> for Mult {
    type Error = ValidationError;

    /// A factor a person wrote, rounded to the nearest WAD.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidMultiplier`] when it is not a finite
    /// non-negative number, or is too large for the WAD word.
    fn try_from(factor: f64) -> Result<Self, ValidationError> {
        if !factor.is_finite() || factor < 0.0 {
            return Err(ValidationError::InvalidMultiplier {
                reason: format!("a factor is a finite non-negative number, got {factor}"),
            });
        }
        let wad = (factor * F64_WAD).round();
        if wad > u128::MAX as f64 {
            return Err(ValidationError::InvalidMultiplier {
                reason: format!("factor {factor} is past the WAD width"),
            });
        }
        Ok(Self(wad as u128))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A factor is built once from a float, exact from there, and reads
    /// back as the number written.
    #[test]
    fn a_factor_is_the_number_written() {
        let lev = Mult::try_from(5.0).unwrap();
        assert_eq!(lev.wad(), 5_000_000_000_000_000_000);
        assert_eq!(lev.factor(), 5.0);
        assert!(Mult::ONE.is_one());
        assert_eq!(Mult::default(), Mult::ONE);
        for bad in [-0.5, f64::NAN, f64::INFINITY] {
            assert!(Mult::try_from(bad).is_err(), "{bad}");
        }
        assert_eq!(serde_json::to_string(&lev).unwrap(), "5.0");
        assert_eq!(
            serde_json::from_str::<Mult>("1.5").unwrap().wad(),
            1_500_000_000_000_000_000
        );
    }

    /// A share widens to a factor exactly, and the buffer and discount
    /// forms are one plus and one minus it.
    #[test]
    fn a_share_is_a_small_factor() {
        let pct = Share::try_from(0.01).unwrap();
        assert_eq!(Mult::from(pct).wad(), 10_000_000_000_000_000);
        assert_eq!(Mult::one_plus(pct).factor(), 1.01);
        assert_eq!(Mult::one_minus(pct).factor(), 0.99);
        assert_eq!(Mult::from(Share::ONE), Mult::ONE);
        assert_eq!(Mult::one_minus(Share::ONE).wad(), 0);
    }
}
