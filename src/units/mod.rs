//! The units a market is denominated in, one type each.
//!
//! These replace a naming convention. A field used to carry its unit as a
//! suffix, which made the unit a fact about spelling that nothing checked;
//! here it is a fact about the type, and a function's signature is its
//! unit list.
//!
//! Two different things were being spelled the same way, and telling them
//! apart is the rule these names follow.
//!
//! A **unit of account** is a count of something indivisible, so the
//! integer *is* the quantity. On chain there is no continuous USDC: one
//! atom is a millionth of a dollar and nothing smaller exists. So
//! [`UsdcAtoms`] carries its unit in its name, and the `f64` a dashboard
//! shows is the derived, lossy view. [`PerpAtoms`] is the same shape in the
//! market's own token.
//!
//! An **encoding** is how a continuous quantity is packed into an integer.
//! A price is a real number and Q96 is one representation of it, so
//! [`Price`] does not carry `X96` in its name: the type hides the
//! representation and the accessor that hands it back names it
//! ([`Price::x96`]). A call site then reads as the quantity, and the
//! encoding appears exactly where a value leaves the type.
//!
//! The signed twins drop the unit word. A [`UsdcDelta`] is a signed count
//! of the same atoms, and *delta* is the contract's own word for it, in
//! `BalanceDelta` and in a position's `delta`.
//!
//! Each type carries only the operations its quantity admits: two amounts
//! of one asset add, an amount and a price multiply into an amount of the
//! other asset, and amounts of different assets do not combine at all. The
//! ported contract math takes these types at its boundary and destructures
//! to primitives inside, so every port stays a line-by-line transcription
//! of the Solidity it is checked against.
//!
//! The fixed-point arithmetic lives here too, in the crate-internal
//! `fixed_point` submodule, because it is arithmetic over these encodings
//! rather than a neighbour of them: a mul-div by a scale is what an
//! encoding's multiplication is, the WAD exponential is a rate's own
//! function, and the checked add and subtract helpers are the pre-type form
//! of operators the accumulator types will carry. `math` reaches into it
//! for the ports.
//!
//! What is deliberately elsewhere: a tick is a coordinate on a grid rather
//! than a unit, so the conversions between a tick and a [`SqrtPrice`] live
//! in [`crate::math::tick`]. That module may depend on this one; this one
//! depends on nothing above [`crate::errors`] and [`crate::constants`],
//! which is what lets every other module take these types.

#![doc = "\n\nThe design of this module: [`src/units/DESIGN.md`](https://github.com/StrobeLabs/perpcity-rust-sdk/blob/main/src/units/DESIGN.md)."]

use alloy::primitives::U256;

mod amount;
pub(crate) mod fixed_point;
mod price;

pub use amount::{PerpAtoms, PerpDelta, UsdcAtoms, UsdcDelta};
pub use price::{Price, SqrtPrice};

/// 10^6 as `f64`: the scale between a human amount and its atoms, and the
/// 6-decimal intermediate the price conversions keep.
pub(crate) const F64_1E6: f64 = 1_000_000.0;

/// 10^6 as `U256`.
pub(crate) const BIGINT_1E6: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);

/// The largest integer an `f64` holds exactly, 2^53. Past it a value that
/// travels through `f64` stops being faithful, so the conversions refuse
/// instead of rounding.
pub(crate) const MAX_SAFE_F64_INT: u64 = 1 << 53;

/// Declare a unit of account: an unsigned count, where the integer is the
/// quantity. `$raw` names both the constructor's argument and the accessor,
/// so the unit is spoken at every boundary.
macro_rules! count {
    (
        $(#[$doc:meta])*
        $name:ident($prim:ty) as $raw:ident
    ) => {
        $(#[$doc])*
        #[derive(
            Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash,
            serde::Serialize, serde::Deserialize,
        )]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name($prim);

        impl $name {
            /// None of it.
            pub const ZERO: Self = Self(0);

            /// From a raw count.
            pub const fn new($raw: $prim) -> Self {
                Self($raw)
            }

            /// The raw count, in the unit the type names.
            pub const fn $raw(self) -> $prim {
                self.0
            }

            /// Whether there is none of it.
            pub const fn is_zero(self) -> bool {
                self.0 == 0
            }

            /// The sum, or `None` where it would not fit.
            pub const fn checked_add(self, rhs: Self) -> Option<Self> {
                match self.0.checked_add(rhs.0) {
                    Some(sum) => Some(Self(sum)),
                    None => None,
                }
            }

            /// The difference, or `None` where `rhs` is the larger: a
            /// negative count is not a count.
            pub const fn checked_sub(self, rhs: Self) -> Option<Self> {
                match self.0.checked_sub(rhs.0) {
                    Some(difference) => Some(Self(difference)),
                    None => None,
                }
            }

            /// The difference floored at zero, for the figures whose floor
            /// is the answer rather than an error: a headroom that is used
            /// up, a shortfall that is covered.
            pub const fn saturating_sub(self, rhs: Self) -> Self {
                Self(self.0.saturating_sub(rhs.0))
            }
        }
    };
}

/// Declare the signed twin of a [`count!`]: the same unit, able to be
/// negative, so it adds and negates freely. Every component the protocol
/// settles is bounded well inside the primitive (see
/// [`ACCOUNTING_TOKEN_SUPPLY`](crate::constants::ACCOUNTING_TOKEN_SUPPLY)),
/// which is why these are plain operators where the unsigned counts are
/// checked methods.
macro_rules! delta {
    (
        $(#[$doc:meta])*
        $name:ident($prim:ty) as $raw:ident, magnitude $count:ident
    ) => {
        $(#[$doc])*
        #[derive(
            Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash,
            serde::Serialize, serde::Deserialize,
        )]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name($prim);

        impl $name {
            /// Neither owed nor owing.
            pub const ZERO: Self = Self(0);

            /// From a raw signed count.
            ///
            /// The protocol's own quantities are bounded by
            /// [`ACCOUNTING_TOKEN_SUPPLY`](crate::constants::ACCOUNTING_TOKEN_SUPPLY),
            /// a hundred-odd times smaller than this primitive's range, and
            /// that bound is what makes the operators below plain rather
            /// than checked. A value built here from outside that range is
            /// outside the type's domain: the operators will wrap on it in
            /// release and panic in debug, as they would on the primitive.
            pub const fn new($raw: $prim) -> Self {
                Self($raw)
            }

            /// The raw signed count, in the unit the type names.
            pub const fn $raw(self) -> $prim {
                self.0
            }

            /// Whether it is zero.
            pub const fn is_zero(self) -> bool {
                self.0 == 0
            }

            /// Whether it is below zero.
            pub const fn is_negative(self) -> bool {
                self.0 < 0
            }

            /// How much of it there is, without the sign.
            pub const fn magnitude(self) -> $count {
                $count::new(self.0.unsigned_abs())
            }
        }

        impl std::ops::Add for $name {
            type Output = Self;
            fn add(self, rhs: Self) -> Self {
                Self(self.0 + rhs.0)
            }
        }

        impl std::ops::AddAssign for $name {
            fn add_assign(&mut self, rhs: Self) {
                self.0 += rhs.0;
            }
        }

        impl std::ops::Sub for $name {
            type Output = Self;
            fn sub(self, rhs: Self) -> Self {
                Self(self.0 - rhs.0)
            }
        }

        impl std::ops::SubAssign for $name {
            fn sub_assign(&mut self, rhs: Self) {
                self.0 -= rhs.0;
            }
        }

        impl std::ops::Neg for $name {
            type Output = Self;
            fn neg(self) -> Self {
                Self(-self.0)
            }
        }

        impl std::iter::Sum for $name {
            fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
                iter.fold(Self::ZERO, |total, next| total + next)
            }
        }
    };
}

pub(crate) use {count, delta};
