//! The pool's own unit: how much liquidity stands in a band.
//!
//! Uniswap's `L` is neither of the two assets. It is the depth a range
//! holds, and the amount of each asset it represents depends on where the
//! price is inside the range. As a primitive it is the same `u128` as a
//! count of atoms, which is the mistake these types exist to prevent: the
//! band-amounts formula takes a root price and a liquidity and returns the
//! two assets, and before this three of its four arguments were typed and
//! the fourth was bare.
//!
//! The arithmetic is all checked, and unlike the asset deltas there is no
//! supply bound to argue from. A balance is bounded by the accounting
//! token's supply, far inside `i128`; a liquidity delta is bounded only by
//! what the pool will accept per tick, which the SDK does not know. So
//! [`LDelta`] has no operators, only methods that can fail, and the
//! difference from [`UsdcDelta`](super::UsdcDelta) is deliberate.

use alloy::primitives::U256;

use crate::errors::ValidationError;

use super::count;

count! {
    /// Liquidity, in the pool's own units: what `makerDetails` stores for a
    /// band and what every concentrated-liquidity formula is linear in.
    ///
    /// Unsigned, because the pool cannot hold a negative depth. A change to
    /// it is an [`LDelta`].
    LUnits(u128) as units
}

/// A change in liquidity: positive adds depth to a band, negative removes
/// it, as `AdjustMakerParams` and the pool's per-tick `liquidityNet` store
/// it.
///
/// Every operation is checked. There is no bound on how much liquidity a
/// band may hold that this crate can assert, so a sum that leaves `i128` is
/// an error rather than a wrap.
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
pub struct LDelta(i128);

impl LUnits {
    /// This liquidity after `delta` is applied.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when the result would leave `u128`, or
    /// when `delta` would take it below zero: the pool holds no negative
    /// depth, so that is inconsistent state rather than an empty band.
    pub fn checked_add_signed(
        self,
        delta: LDelta,
        context: &'static str,
    ) -> Result<Self, ValidationError> {
        let applied = if delta.0 >= 0 {
            self.units().checked_add(delta.0 as u128)
        } else {
            self.units().checked_sub(delta.0.unsigned_abs())
        };
        applied.map(Self::new).ok_or(ValidationError::Overflow {
            context: context.into(),
        })
    }

    /// The delta that removes all of it, for closing a band.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when the liquidity is past `i128::MAX`
    /// and so has no negation.
    pub fn negated(self) -> Result<LDelta, ValidationError> {
        i128::try_from(self.units())
            .map(|l| LDelta(-l))
            .map_err(|_| ValidationError::Overflow {
                context: "liquidity has no negation in i128".into(),
            })
    }
}

impl LDelta {
    /// Neither added nor removed.
    pub const ZERO: Self = Self(0);

    /// From a raw signed amount of liquidity.
    pub const fn new(units: i128) -> Self {
        Self(units)
    }

    /// The raw signed amount, in the pool's units.
    pub const fn units(self) -> i128 {
        self.0
    }

    /// Whether it is zero.
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Whether it removes liquidity.
    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// How much liquidity it moves, without the direction.
    pub const fn magnitude(self) -> LUnits {
        LUnits::new(self.0.unsigned_abs())
    }

    /// The change that undoes this one.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] for `i128::MIN`, the one value with no
    /// negation. A band's upper tick carries the negation of what its lower
    /// tick carries, so a delta that cannot be negated is not a delta a
    /// band could hold.
    pub fn negated(self) -> Result<Self, ValidationError> {
        self.0
            .checked_neg()
            .map(Self)
            .ok_or(ValidationError::Overflow {
                context: "liquidity delta has no negation".into(),
            })
    }

    /// The sum of two changes.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when the sum leaves `i128`.
    pub fn checked_add(self, rhs: Self, context: &'static str) -> Result<Self, ValidationError> {
        self.0
            .checked_add(rhs.0)
            .map(Self)
            .ok_or(ValidationError::Overflow {
                context: context.into(),
            })
    }
}

impl TryFrom<U256> for LUnits {
    type Error = ValidationError;

    /// Narrow a liquidity the formulas computed in 256 bits.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when the value exceeds `u128`, which no
    /// pool can hold: the depth is stored as a `uint128`, so a larger figure
    /// is a size nothing could place rather than a large position.
    fn try_from(units: U256) -> Result<Self, ValidationError> {
        u128::try_from(units)
            .map(Self::new)
            .map_err(|_| ValidationError::Overflow {
                context: "liquidity exceeds the uint128 the pool stores".into(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A delta applies to a depth in either direction, and the two failures
    /// are the ones the pool itself could not represent.
    #[test]
    fn a_delta_applies_and_both_ends_are_checked() {
        let depth = LUnits::new(1_000);
        assert_eq!(
            depth
                .checked_add_signed(LDelta::new(-400), "test")
                .unwrap()
                .units(),
            600
        );
        assert_eq!(
            depth
                .checked_add_signed(LDelta::new(400), "test")
                .unwrap()
                .units(),
            1_400
        );
        assert!(
            depth
                .checked_add_signed(LDelta::new(-1_001), "test")
                .is_err(),
            "removing more depth than there is, is not an empty band"
        );
        assert!(
            LUnits::new(u128::MAX)
                .checked_add_signed(LDelta::new(1), "test")
                .is_err()
        );
    }

    /// Closing a band is the negation of its depth, and it fails only past
    /// the signed range.
    #[test]
    fn a_band_closes_by_negating_its_depth() {
        assert_eq!(LUnits::new(7).negated().unwrap(), LDelta::new(-7));
        assert_eq!(LDelta::new(-7).magnitude(), LUnits::new(7));
        assert!(LUnits::new(1 << 127).negated().is_err());
    }

    /// A liquidity the formulas computed in 256 bits is a liquidity only if
    /// the pool could store it.
    #[test]
    fn a_wide_liquidity_narrows_or_fails() {
        assert_eq!(
            LUnits::try_from(U256::from(42u64)).unwrap(),
            LUnits::new(42)
        );
        assert!(LUnits::try_from(U256::from(u128::MAX) + U256::from(1u64)).is_err());
    }

    /// The wire form is the bare number, so a persisted band reads back.
    #[test]
    fn the_wire_form_is_the_number_alone() {
        assert_eq!(serde_json::to_string(&LUnits::new(5)).unwrap(), "5");
        assert_eq!(serde_json::to_string(&LDelta::new(-5)).unwrap(), "-5");
    }
}
