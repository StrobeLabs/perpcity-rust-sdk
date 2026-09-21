//! Taker capacity: how much open interest maker liquidity can back.
//!
//! A maker band's capacity is the perp amount its liquidity spans on each
//! side of the pool price, as the deployed `PerpLogic.calcCapacity`
//! computes it:
//!
//! ```solidity
//! if (sqrtAmmPrice <= sqrtPriceLower) {
//!     cap.long = getAmount0ForLiquidity(sqrtPriceLower, sqrtPriceUpper, liquidity);
//! } else if (sqrtAmmPrice < sqrtPriceUpper) {
//!     cap.long = getAmount0ForLiquidity(sqrtAmmPrice, sqrtPriceUpper, liquidity);
//!     cap.short = getAmount0ForLiquidity(sqrtPriceLower, sqrtAmmPrice, liquidity);
//! } else {
//!     cap.short = getAmount0ForLiquidity(sqrtPriceLower, sqrtPriceUpper, liquidity);
//! }
//! ```
//!
//! The part of the band above the price backs longs and the part below
//! backs shorts, both in 6-decimal perp atoms, the unit of open interest.
//!
//! The price is the pool price at the moment the liquidity changes. The
//! contract stores each band's capacity at that moment and never
//! re-evaluates it as the price moves: the market's `capacity()` is the
//! running sum of those snapshots. Removing liquidity subtracts the
//! capacity of the removed liquidity at the price of the removal, so the
//! market total can drift from the sum of the live bands' stored
//! `makerDetails(id).capacity`.
//!
//! A taker trade or a liquidity removal that would leave a side's open
//! interest above its capacity reverts with `LongUtilizationExceeded` /
//! `ShortUtilizationExceeded`. [`MarketCapacity`] holds both totals and
//! derives each side's headroom and utilization.

use alloy::primitives::{U256, U512};
use serde::{Deserialize, Serialize};

use crate::constants::{MAX_TICK, MIN_TICK, Q96, SCALE_1E6, UTILIZATION_E6_NO_CAPACITY};
use crate::contracts;
use crate::errors::ValidationError;
use crate::math::BlockContext;
use crate::math::fixed_point::{Rounding, div_ceil_512};
use crate::math::swap::amount0_delta;
use crate::math::tick::get_sqrt_ratio_at_tick;
use crate::types::Side;

/// Taker capacity per side, in 6-decimal perp atoms: the contract's
/// `Capacity` struct.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Capacity {
    /// Open interest longs can hold against this capacity.
    pub long_atoms: u128,
    /// Open interest shorts can hold against this capacity.
    pub short_atoms: u128,
}

impl Capacity {
    /// The capacity on one side.
    pub fn side(&self, side: Side) -> u128 {
        match side {
            Side::Long => self.long_atoms,
            Side::Short => self.short_atoms,
        }
    }
}

impl From<contracts::Capacity> for Capacity {
    fn from(cap: contracts::Capacity) -> Self {
        Self {
            long_atoms: cap.long,
            short_atoms: cap.short,
        }
    }
}

/// A market's taker capacity and the open interest drawing on it, read at
/// one block by [`PerpClient::get_capacity`](crate::PerpClient::get_capacity).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketCapacity {
    /// The block both totals were read at.
    pub block: BlockContext,
    /// `capacity()`: the sum of every band's capacity snapshot.
    pub capacity: Capacity,
    /// `openInterest().long`, in 6-decimal perp atoms.
    pub long_open_interest_atoms: u128,
    /// `openInterest().short`, in 6-decimal perp atoms.
    pub short_open_interest_atoms: u128,
}

impl MarketCapacity {
    /// Taker open interest on one side.
    pub fn open_interest_atoms(&self, side: Side) -> u128 {
        match side {
            Side::Long => self.long_open_interest_atoms,
            Side::Short => self.short_open_interest_atoms,
        }
    }

    /// Open interest `side` can still add before a trade reverts with
    /// `LongUtilizationExceeded` / `ShortUtilizationExceeded`.
    pub fn headroom_atoms(&self, side: Side) -> u128 {
        self.capacity
            .side(side)
            .saturating_sub(self.open_interest_atoms(side))
    }

    /// Utilization on one side as the Perp passes it to the fees module:
    /// `openInterest * 1e6 / capacity`, rounded down, or
    /// [`UTILIZATION_E6_NO_CAPACITY`] when the side has no capacity.
    ///
    /// Chain state never exceeds [`SCALE_1E6`] (100%), because the contract
    /// reverts any change that leaves open interest above capacity. A
    /// hand-built value above `u32::MAX` saturates.
    pub fn utilization_e6(&self, side: Side) -> u32 {
        let capacity = self.capacity.side(side);
        if capacity == 0 {
            return UTILIZATION_E6_NO_CAPACITY;
        }
        let scaled = U256::from(self.open_interest_atoms(side)) * U256::from(SCALE_1E6);
        (scaled / U256::from(capacity)).saturating_to::<u32>()
    }
}

/// The capacity a maker band of `liquidity` over `[tick_lower, tick_upper]`
/// adds at `sqrt_price_x96`, exact to the contract (see the module docs).
///
/// To size a band from margin, derive its liquidity first (for example
/// with [`crate::math::liquidity::estimate_liquidity`]) and pass that here.
///
/// # Errors
///
/// - [`ValidationError::InvalidTickRange`] if `tick_lower >= tick_upper` or
///   either tick is outside `[MIN_TICK, MAX_TICK]`
/// - [`ValidationError::InvalidPrice`] if `sqrt_price_x96` is zero
/// - [`ValidationError::Overflow`] if a side exceeds `u128`, where the
///   contract's `toUint128` reverts
pub fn band_capacity(
    sqrt_price_x96: U256,
    tick_lower: i32,
    tick_upper: i32,
    liquidity: u128,
) -> Result<Capacity, ValidationError> {
    let band = Band::new(sqrt_price_x96, tick_lower, tick_upper)?;
    Ok(Capacity {
        long_atoms: band.perp_atoms(Side::Long, liquidity)?,
        short_atoms: band.perp_atoms(Side::Short, liquidity)?,
    })
}

/// The least liquidity over `[tick_lower, tick_upper]` whose capacity on
/// `side` at `sqrt_price_x96` is at least `target_atoms`: the inverse of
/// [`band_capacity`].
///
/// The result is exact, not an estimate: [`band_capacity`] of it reaches
/// the target and of one unit less does not. A zero target needs zero
/// liquidity.
///
/// # Errors
///
/// - [`ValidationError::NoBandCapacity`] if the price sits at or beyond the
///   band's edge on `side`, so no liquidity in the band backs that side
/// - [`ValidationError::InvalidTickRange`] and
///   [`ValidationError::InvalidPrice`] as for [`band_capacity`]
/// - [`ValidationError::Overflow`] if the liquidity exceeds `u128`
pub fn liquidity_for_capacity(
    sqrt_price_x96: U256,
    tick_lower: i32,
    tick_upper: i32,
    side: Side,
    target_atoms: u128,
) -> Result<u128, ValidationError> {
    let band = Band::new(sqrt_price_x96, tick_lower, tick_upper)?;
    if target_atoms == 0 {
        return Ok(0);
    }
    let (lo, hi) = band.span(side);
    if lo == hi {
        return Err(ValidationError::NoBandCapacity {
            lower: tick_lower,
            upper: tick_upper,
            side,
        });
    }
    // `getAmount0ForLiquidity` is floor(floor(L·2^96·(hi − lo) / hi) / lo),
    // and nested floors of integer divisions collapse to one:
    // floor(L·2^96·(hi − lo) / (hi·lo)). The least L reaching the target
    // is therefore ceil(target·hi·lo / (2^96·(hi − lo))).
    let numerator = U512::from(target_atoms) * U512::from(hi) * U512::from(lo);
    let denominator = U512::from(Q96) * U512::from(hi - lo);
    let liquidity = div_ceil_512(numerator, denominator);
    if liquidity > U512::from(u128::MAX) {
        return Err(ValidationError::Overflow {
            context: "liquidity for capacity exceeds u128".into(),
        });
    }
    Ok(liquidity.to::<u128>())
}

/// A validated band with the pool price clamped into it.
struct Band {
    sqrt_lower: U256,
    sqrt_upper: U256,
    sqrt_clamped: U256,
}

impl Band {
    fn new(
        sqrt_price_x96: U256,
        tick_lower: i32,
        tick_upper: i32,
    ) -> Result<Self, ValidationError> {
        if tick_lower >= tick_upper || tick_lower < MIN_TICK || tick_upper > MAX_TICK {
            return Err(ValidationError::InvalidTickRange {
                lower: tick_lower,
                upper: tick_upper,
            });
        }
        if sqrt_price_x96.is_zero() {
            return Err(ValidationError::InvalidPrice {
                reason: "zero sqrt price".into(),
            });
        }
        let sqrt_lower = get_sqrt_ratio_at_tick(tick_lower)?;
        let sqrt_upper = get_sqrt_ratio_at_tick(tick_upper)?;
        Ok(Self {
            sqrt_lower,
            sqrt_upper,
            sqrt_clamped: sqrt_price_x96.clamp(sqrt_lower, sqrt_upper),
        })
    }

    /// The sqrt-price span backing `side`: above the price for longs,
    /// below it for shorts. Empty when the price sits at or beyond the
    /// band's edge on that side, which reproduces the contract's three
    /// branches.
    fn span(&self, side: Side) -> (U256, U256) {
        match side {
            Side::Long => (self.sqrt_clamped, self.sqrt_upper),
            Side::Short => (self.sqrt_lower, self.sqrt_clamped),
        }
    }

    fn perp_atoms(&self, side: Side, liquidity: u128) -> Result<u128, ValidationError> {
        let (lo, hi) = self.span(side);
        let atoms = amount0_delta(lo, hi, liquidity, Rounding::TowardZero)?;
        u128::try_from(atoms).map_err(|_| ValidationError::Overflow {
            context: "band capacity exceeds u128".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{B256, uint};

    use super::*;

    /// A mainnet maker open, reproduced exactly: the pool price just before
    /// the open, the band, its liquidity, and the capacity the contract
    /// stored (`makerDetails(pos_id).capacity`, equal to the `capacity()`
    /// delta across the open).
    struct GoldenOpen {
        sqrt_price_x96: U256,
        tick_lower: i32,
        tick_upper: i32,
        liquidity: u128,
        capacity: Capacity,
    }

    /// HORMUZ-TRAFFIC (`0x137e00487dc079dad69ba149994320a8ff4c5b17`,
    /// Arbitrum One). The price is `poolState().sqrtPrice` one block before
    /// each open; it is unchanged at the open block, so no other trade
    /// moved it first.
    const GOLDEN: [GoldenOpen; 3] = [
        // pos 1430, block 507274986: band straddles the price.
        GoldenOpen {
            sqrt_price_x96: uint!(434377632981895034013411082410_U256),
            tick_lower: 27090,
            tick_upper: 38100,
            liquidity: 1_757_959,
            capacity: Capacity {
                long_atoms: 58_993,
                short_atoms: 133_075,
            },
        },
        // pos 1202, block 506149884: band straddles the price.
        GoldenOpen {
            sqrt_price_x96: uint!(510281871168875509382435561417_U256),
            tick_lower: 35610,
            tick_upper: 38670,
            liquidity: 97_506_535,
            capacity: Capacity {
                long_atoms: 1_034_395,
                short_atoms: 1_297_356,
            },
        },
        // pos 1377, block 506873284: band entirely below the price.
        GoldenOpen {
            sqrt_price_x96: uint!(431621243212719145650373590467_U256),
            tick_lower: 31050,
            tick_upper: 33090,
            liquidity: 39_437_273,
            capacity: Capacity {
                long_atoms: 0,
                short_atoms: 809_687,
            },
        },
    ];

    #[test]
    fn band_capacity_matches_mainnet_opens() {
        for g in &GOLDEN {
            assert_eq!(
                band_capacity(g.sqrt_price_x96, g.tick_lower, g.tick_upper, g.liquidity).unwrap(),
                g.capacity,
                "band [{}, {}]",
                g.tick_lower,
                g.tick_upper
            );
        }
    }

    /// The contract's branch edges: a price exactly on the lower bound is
    /// all long, exactly on the upper bound all short.
    #[test]
    fn band_capacity_edges_are_one_sided() {
        let liquidity = 1_000_000_000;
        let sqrt_lower = get_sqrt_ratio_at_tick(-600).unwrap();
        let sqrt_upper = get_sqrt_ratio_at_tick(600).unwrap();
        let full = amount0_delta(sqrt_lower, sqrt_upper, liquidity, Rounding::TowardZero)
            .unwrap()
            .to::<u128>();

        let at_lower = band_capacity(sqrt_lower, -600, 600, liquidity).unwrap();
        assert_eq!(
            at_lower,
            Capacity {
                long_atoms: full,
                short_atoms: 0
            }
        );
        let at_upper = band_capacity(sqrt_upper, -600, 600, liquidity).unwrap();
        assert_eq!(
            at_upper,
            Capacity {
                long_atoms: 0,
                short_atoms: full
            }
        );
        assert_eq!(
            band_capacity(Q96, -600, 600, 0).unwrap(),
            Capacity::default()
        );
    }

    /// HORMUZ-TRAFFIC `capacity()` and `openInterest()` at block
    /// 507526321.
    const HORMUZ_TRAFFIC: MarketCapacity = MarketCapacity {
        block: BlockContext {
            number: 507_526_321,
            hash: B256::ZERO,
            timestamp: 0,
        },
        capacity: Capacity {
            long_atoms: 60_883_605,
            short_atoms: 51_603_209,
        },
        long_open_interest_atoms: 37_772_806,
        short_open_interest_atoms: 42_582_564,
    };

    #[test]
    fn market_utilization_matches_the_contract_formula() {
        // floor(37772806e6 / 60883605) and floor(42582564e6 / 51603209).
        assert_eq!(HORMUZ_TRAFFIC.utilization_e6(Side::Long), 620_410);
        assert_eq!(HORMUZ_TRAFFIC.utilization_e6(Side::Short), 825_192);
        assert_eq!(HORMUZ_TRAFFIC.headroom_atoms(Side::Long), 23_110_799);
        assert_eq!(HORMUZ_TRAFFIC.headroom_atoms(Side::Short), 9_020_645);
        assert_eq!(HORMUZ_TRAFFIC.open_interest_atoms(Side::Short), 42_582_564);
    }

    #[test]
    fn market_utilization_edges() {
        let empty = MarketCapacity::default();
        assert_eq!(empty.utilization_e6(Side::Long), UTILIZATION_E6_NO_CAPACITY);
        assert_eq!(empty.headroom_atoms(Side::Short), 0);

        let full = MarketCapacity {
            block: BlockContext::default(),
            capacity: Capacity {
                long_atoms: u128::MAX,
                short_atoms: 1,
            },
            long_open_interest_atoms: u128::MAX,
            short_open_interest_atoms: u128::MAX,
        };
        assert_eq!(full.utilization_e6(Side::Long), SCALE_1E6);
        assert_eq!(full.headroom_atoms(Side::Long), 0);
        assert_eq!(full.utilization_e6(Side::Short), u32::MAX);
        assert_eq!(full.headroom_atoms(Side::Short), 0);
    }

    #[test]
    fn band_capacity_validates_inputs() {
        assert!(matches!(
            band_capacity(Q96, 600, 600, 1),
            Err(ValidationError::InvalidTickRange { .. })
        ));
        assert!(matches!(
            band_capacity(Q96, 600, -600, 1),
            Err(ValidationError::InvalidTickRange { .. })
        ));
        assert!(matches!(
            band_capacity(Q96, MIN_TICK - 1, 0, 1),
            Err(ValidationError::InvalidTickRange { .. })
        ));
        assert!(matches!(
            band_capacity(Q96, 0, MAX_TICK + 1, 1),
            Err(ValidationError::InvalidTickRange { .. })
        ));
        assert!(matches!(
            band_capacity(U256::ZERO, -600, 600, 1),
            Err(ValidationError::InvalidPrice { .. })
        ));
    }

    /// The inverse is the least liquidity reaching the target: one unit
    /// less falls short.
    fn assert_least(sqrt_price_x96: U256, lower: i32, upper: i32, side: Side, target: u128) {
        let liquidity = liquidity_for_capacity(sqrt_price_x96, lower, upper, side, target).unwrap();
        let reached = band_capacity(sqrt_price_x96, lower, upper, liquidity).unwrap();
        assert!(
            reached.side(side) >= target,
            "{side} target {target}: liquidity {liquidity} gives {}",
            reached.side(side)
        );
        let below = band_capacity(sqrt_price_x96, lower, upper, liquidity - 1).unwrap();
        assert!(
            below.side(side) < target,
            "{side} target {target}: liquidity {} already gives {}",
            liquidity - 1,
            below.side(side)
        );
    }

    #[test]
    fn liquidity_for_capacity_inverts_mainnet_opens() {
        for g in &GOLDEN {
            for side in [Side::Long, Side::Short] {
                let target = g.capacity.side(side);
                if target == 0 {
                    continue;
                }
                assert_least(g.sqrt_price_x96, g.tick_lower, g.tick_upper, side, target);
                let liquidity = liquidity_for_capacity(
                    g.sqrt_price_x96,
                    g.tick_lower,
                    g.tick_upper,
                    side,
                    target,
                )
                .unwrap();
                assert!(liquidity <= g.liquidity);
            }
        }
    }

    #[test]
    fn liquidity_for_capacity_is_least_across_scales() {
        let sqrt_price = get_sqrt_ratio_at_tick(34_000).unwrap();
        for target in [1, 7, 999, 1_000_000, 123_456_789_012, u64::MAX as u128] {
            for side in [Side::Long, Side::Short] {
                assert_least(sqrt_price, 30_000, 38_000, side, target);
            }
            assert_least(sqrt_price, 36_000, 40_000, Side::Long, target);
            assert_least(sqrt_price, 20_000, 30_000, Side::Short, target);
        }
    }

    #[test]
    fn liquidity_for_capacity_rejects_a_side_the_band_cannot_back() {
        let sqrt_price = get_sqrt_ratio_at_tick(34_000).unwrap();
        assert!(matches!(
            liquidity_for_capacity(sqrt_price, 20_000, 30_000, Side::Long, 1),
            Err(ValidationError::NoBandCapacity {
                lower: 20_000,
                upper: 30_000,
                side: Side::Long
            })
        ));
        assert!(matches!(
            liquidity_for_capacity(sqrt_price, 36_000, 40_000, Side::Short, 1),
            Err(ValidationError::NoBandCapacity {
                side: Side::Short,
                ..
            })
        ));
        assert_eq!(
            liquidity_for_capacity(sqrt_price, 20_000, 30_000, Side::Long, 0).unwrap(),
            0
        );
    }

    #[test]
    fn liquidity_for_capacity_reports_overflow() {
        let sqrt_price = get_sqrt_ratio_at_tick(0).unwrap();
        assert!(matches!(
            liquidity_for_capacity(sqrt_price, 0, 1, Side::Short, u128::MAX),
            Err(ValidationError::NoBandCapacity { .. })
        ));
        assert!(matches!(
            liquidity_for_capacity(sqrt_price, -1, 1, Side::Short, u128::MAX),
            Err(ValidationError::Overflow { .. })
        ));
    }
}
