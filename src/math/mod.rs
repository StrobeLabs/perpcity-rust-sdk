//! Pure math functions for the PerpCity protocol.
//!
//! These operate directly on Alloy primitives (`U256`, `I256`) and f64 —
//! no structs, no state, just math. Each submodule corresponds to a domain:
//!
//! | Module | Purpose |
//! |---|---|
//! | [`tick`] | Tick ↔ price conversions, tick alignment, `getSqrtRatioAtTick` |
//! | [`capacity`] | Taker capacity a maker band adds, and the liquidity for a target capacity |
//! | [`range`] | A maker's geometry: the validated tick range, and the band of liquidity in it, that the maker math is over |
//! | [`liquidity`] | Liquidity sizing and band token amounts for maker positions |
//! | [`position`] | Entry price, size, value, leverage, liquidation price |
//! | [`pricing`] | The pool price, the index, their contract-exact EMAs, and the mark: the deployed fair price |
//! | [`swap`] | Local V4 taker swap simulation over a block-pinned pool |
//! | [`maker_equity`] | Contract-exact maker settle preview over a block-pinned snapshot |
//!
//! Two things that look like they belong here do not. Storage-slot
//! derivation for the deployed contract layouts lives in the crate-internal
//! `storage` module beside `contracts`. And the Solidity-compatible
//! fixed-point arithmetic every port is built on belongs to the units it
//! operates on, so it lives in [`crate::units`]: a mul-div by a scale is
//! what an encoding's multiplication *is*, and the checked add and subtract
//! helpers are what a unit's own operators will replace.

#![doc = "\n\nThe design of this module: [`src/math/DESIGN.md`](https://github.com/StrobeLabs/perpcity-rust-sdk/blob/main/src/math/DESIGN.md)."]

use alloy::primitives::{B256, U256};
use serde::{Deserialize, Serialize};

use crate::units::fixed_point::{Rounding, mul_div};
use crate::units::{BIGINT_1E6, Price, Ratio, UsdcAtoms, UsdcDelta};

pub mod capacity;
pub mod liquidity;
pub mod maker_equity;
pub mod position;
pub mod pricing;
pub mod range;
pub mod swap;
pub mod taker;
pub mod tick;

/// The block a market snapshot's state was read at.
///
/// Shared by [`swap::PoolSnapshot`] and
/// [`maker_equity::MakerMarketSnapshot`]: every field in a snapshot comes
/// from this one block, and chain reads derived from the snapshot pin to
/// [`Self::hash`].
///
/// The client's snapshot loaders pin this block
/// [`SNAPSHOT_BLOCK_LAG`](crate::constants::SNAPSHOT_BLOCK_LAG) behind the
/// chain head, so [`Self::hash`] is generally not the newest head and
/// [`Self::timestamp`] trails wall-clock time by the lag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockContext {
    /// Block number.
    pub number: u64,
    /// Canonical block hash.
    pub hash: B256,
    /// Block timestamp (seconds since the Unix epoch).
    pub timestamp: u64,
}

/// Where a position's health test turns as the mark moves, on each side of
/// the mark it was measured from: the one shape both roles answer "how
/// far" in.
///
/// A side is `None` when no move that way within reach liquidates the
/// position; both sides are the mark when it is liquidatable now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiquidationPrices {
    /// The mark the turns are measured from.
    pub mark: Price,
    /// The first mark below `mark` at which the position is liquidatable.
    pub below: Option<Price>,
    /// The first mark above `mark` at which the position is liquidatable.
    pub above: Option<Price>,
}

impl LiquidationPrices {
    /// The nearer side's distance from the mark, as a fraction of it: a
    /// monitor's bound. `None` when neither side turns, or when the mark is
    /// zero and there is no fraction of it; zero when the position is
    /// liquidatable now, and positive whenever it is not. Lossy in its last
    /// digits; the tests themselves are exact.
    pub fn distance(&self) -> Option<f64> {
        if self.mark.is_zero() {
            return None;
        }
        let mark = f64::from(self.mark.x96());
        // The gap is taken in integers first: a price one atom from a mark
        // near 2^101 is the same `f64` as the mark, and a ratio less one
        // would read it as no distance at all.
        let relative = |price: Price| f64::from(price.x96().abs_diff(self.mark.x96())) / mark;
        [self.below, self.above]
            .into_iter()
            .flatten()
            .map(relative)
            .reduce(f64::min)
    }
}

/// `PerpLogic.isHealthy` of the deployed contracts: equity over the
/// position's value plus one atom, in millionths and floored, at least its
/// ratio, with non-positive equity counting as zero. Both roles'
/// liquidation tests are its negation.
pub(crate) fn is_healthy(equity: UsdcDelta, value: UsdcAtoms, ratio: Ratio) -> bool {
    let equity = equity.atoms();
    let held = if equity <= 0 {
        U256::ZERO
    } else {
        mul_div(
            U256::from(equity.unsigned_abs()),
            BIGINT_1E6,
            U256::from(value.atoms()) + U256::from(1u8),
            Rounding::TowardZero,
        )
        .expect("an i128 equity times a million fits U256, over a nonzero divisor")
    };
    held >= U256::from(ratio.e6())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test is the contract's integers: the plus-one atom and the floor
    /// put the 5% line on a million-atom position between 50,000 and
    /// 50,001 of equity, where an equity-over-value ratio in floating point
    /// would call 50,000 healthy. Non-positive equity counts as zero, so it
    /// is healthy only against a zero ratio, as in the contract.
    #[test]
    fn healthy_is_the_contracts_integer_test() {
        let (value, five) = (UsdcAtoms::new(1_000_000), Ratio::from_e6(50_000).unwrap());
        assert!(!is_healthy(UsdcDelta::new(50_000), value, five));
        assert!(is_healthy(UsdcDelta::new(50_001), value, five));
        assert!(!is_healthy(UsdcDelta::ZERO, value, five));
        assert!(!is_healthy(UsdcDelta::new(-1), value, five));
        assert!(is_healthy(UsdcDelta::new(-1), value, Ratio::ZERO));
        assert!(
            is_healthy(UsdcDelta::new(1), UsdcAtoms::ZERO, Ratio::ONE),
            "a zero value counts as one atom"
        );
    }

    /// A zero mark has no fraction to measure a distance in, so the reading
    /// is `None` rather than a `NaN` a monitor's comparison would silently
    /// pass.
    #[test]
    fn a_zero_mark_has_no_distance() {
        let zero = Price::from_x96(U256::ZERO);
        let prices = LiquidationPrices {
            mark: zero,
            below: Some(zero),
            above: Some(zero),
        };
        assert_eq!(prices.distance(), None);
    }
}
