//! What a quantity is multiplied by.
//!
//! Every count, delta and price scales with `*`, and the thing on the right
//! is a [`Factor`]: the SDK's own [`Share`] and [`Mult`], a plain `f64` for
//! the literal in a strategy's hand, or a type the strategy defines for
//! itself — a `Leverage`, a `Skew` — whose invariant lives in its own
//! constructor and which multiplies the SDK's quantities directly. The
//! trait is open for exactly that reason, and it is the one trait this
//! module defines: scaling is the one verb that is the same on every
//! quantity and the one place a consumer adds a type of its own.

use alloy::primitives::U256;

use crate::constants::WAD;

use super::mult::Mult;
use super::share::Share;

/// A dimensionless factor applied to a quantity.
///
/// [`apply`](Self::apply) scales a magnitude exactly, truncating toward
/// zero as the chain's division does; the quantity owns its own width and
/// sign and narrows the result back. A factor that is negative says so
/// through [`is_negative`](Self::is_negative), and only a signed quantity
/// may take one: a delta flips, a count panics, as the primitive does on an
/// operation its type cannot hold.
///
/// Implement it for a strategy's own scalar by delegating to the type it
/// wraps:
///
/// ```
/// # use alloy::primitives::U256;
/// # use perpcity_sdk::{Factor, UsdcAtoms};
/// #[derive(Clone, Copy)]
/// struct Leverage(f64);
///
/// impl Factor for Leverage {
///     fn apply(self, x: U256) -> U256 {
///         self.0.apply(x)
///     }
/// }
///
/// let margin = UsdcAtoms::new(100_000_000);
/// assert_eq!(margin * Leverage(5.0), UsdcAtoms::new(500_000_000));
/// ```
pub trait Factor: Copy {
    /// `x` scaled by this factor's magnitude, exact and truncated toward
    /// zero.
    fn apply(self, x: U256) -> U256;

    /// Whether the factor flips a sign. Only a signed quantity may take a
    /// negative factor.
    fn is_negative(self) -> bool {
        false
    }

    /// The factor's magnitude as the WAD word, for a port that wants the
    /// number rather than its effect.
    fn wad(self) -> U256 {
        self.apply(WAD)
    }
}

/// The literal in a strategy's hand: taken to WAD once, which holds every
/// digit an `f64` has, and exact from there.
///
/// # Panics
///
/// On a factor that is not finite: a bug, not a market condition, as an
/// overflowing `+` is on the primitive.
impl Factor for f64 {
    fn apply(self, x: U256) -> U256 {
        assert!(
            self.is_finite(),
            "a quantity scales by a finite factor, not {self}"
        );
        let wad = (self.abs() * 1e18).round();
        assert!(
            wad <= u128::MAX as f64,
            "factor {self} is past the WAD width"
        );
        x * U256::from(wad as u128) / WAD
    }

    fn is_negative(self) -> bool {
        self < 0.0
    }
}

impl Factor for Share {
    fn apply(self, x: U256) -> U256 {
        x * U256::from(self.e6()) / super::BIGINT_1E6
    }
}

impl Factor for Mult {
    fn apply(self, x: U256) -> U256 {
        x * U256::from(self.wad()) / WAD
    }
}

/// `x` scaled by `factor`, narrowed back to the count's width.
///
/// # Panics
///
/// On a negative factor, which a count cannot take, or a result past
/// `u128`.
pub(crate) fn scale_count<F: Factor>(x: u128, factor: F) -> u128 {
    assert!(
        !factor.is_negative(),
        "a count cannot scale by a negative factor"
    );
    u128::try_from(factor.apply(U256::from(x))).expect("the scaled count left u128")
}

/// `x` scaled by `factor`, the magnitude through [`scale_count`] and the
/// sign the product's own.
pub(crate) fn scale_delta<F: Factor>(x: i128, factor: F) -> i128 {
    let magnitude = u128::try_from(factor.apply(U256::from(x.unsigned_abs())))
        .expect("the scaled delta left u128");
    let magnitude = i128::try_from(magnitude).expect("the scaled delta left i128");
    if (x < 0) != factor.is_negative() {
        -magnitude
    } else {
        magnitude
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every factor scales the same way, and `wad` reads the number back.
    #[test]
    fn every_factor_scales_the_same_way() {
        let x = U256::from(1_000u64);
        assert_eq!(0.25.apply(x), U256::from(250u64));
        assert_eq!(Share::try_from(0.25).unwrap().apply(x), U256::from(250u64));
        assert_eq!(Mult::try_from(0.25).unwrap().apply(x), U256::from(250u64));
        assert_eq!(2.5.wad(), U256::from(2_500_000_000_000_000_000u128));
        assert_eq!(Share::ONE.wad(), WAD);
    }

    /// A count takes a magnitude and refuses a sign; a delta takes both.
    #[test]
    fn a_count_refuses_a_sign_and_a_delta_takes_it() {
        assert_eq!(scale_count(10, 0.5), 5);
        assert_eq!(scale_delta(-10, 0.5), -5);
        assert_eq!(scale_delta(-10, -0.5), 5);
        assert_eq!(scale_delta(10, -0.5), -5);
        assert_eq!(scale_count(3, 0.1), 0, "truncated, not rounded");
    }

    #[test]
    #[should_panic(expected = "negative factor")]
    fn a_count_panics_on_a_negative_factor() {
        let _ = scale_count(1, -0.5);
    }

    #[test]
    #[should_panic(expected = "finite factor")]
    fn a_factor_must_be_a_number() {
        let _ = 1.0.apply(U256::ZERO) + f64::NAN.apply(U256::ZERO);
    }
}
