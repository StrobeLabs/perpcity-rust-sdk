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

use crate::constants::{Q96, SCALE_1E6};
use crate::contracts;
use crate::errors::ValidationError;
use crate::math::BlockContext;
use crate::math::range::{MakerBand, TickRange};
use crate::math::swap::amount0_delta;
use crate::units::fixed_point::{Rounding, mul_div};
use crate::units::{LUnits, PerpAtoms, SqrtPrice};

/// A taker direction: a long gains when the price rises, a short when it
/// falls.
///
/// It lives with the capacity math because that is what a side keys: a
/// band's liquidity above the pool price backs longs and below it backs
/// shorts, and every side-keyed read is on [`Capacity`] or
/// [`MarketCapacity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    /// Long exposure (positive perp delta).
    Long,
    /// Short exposure (negative perp delta).
    Short,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Long => "long",
            Self::Short => "short",
        })
    }
}

/// Taker capacity per side: the contract's `Capacity` struct.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Capacity {
    /// Open interest longs can hold against this capacity.
    pub long: PerpAtoms,
    /// Open interest shorts can hold against this capacity.
    pub short: PerpAtoms,
}

impl Capacity {
    /// The capacity on one side.
    pub fn on(&self, side: Side) -> PerpAtoms {
        match side {
            Side::Long => self.long,
            Side::Short => self.short,
        }
    }
}

impl From<contracts::Capacity> for Capacity {
    fn from(cap: contracts::Capacity) -> Self {
        Self {
            long: PerpAtoms::new(cap.long),
            short: PerpAtoms::new(cap.short),
        }
    }
}

/// A market's taker capacity and the open interest drawing on it, read at
/// one block by [`MarketReader::get_capacity`](crate::MarketReader::get_capacity).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketCapacity {
    /// The block both totals were read at.
    pub block: BlockContext,
    /// `capacity()`: the sum of every band's capacity snapshot.
    pub capacity: Capacity,
    /// `openInterest().long`.
    pub long_open_interest: PerpAtoms,
    /// `openInterest().short`.
    pub short_open_interest: PerpAtoms,
}

impl MarketCapacity {
    /// Taker open interest on one side.
    pub fn open_interest(&self, side: Side) -> PerpAtoms {
        match side {
            Side::Long => self.long_open_interest,
            Side::Short => self.short_open_interest,
        }
    }

    /// Open interest `side` can still add before a trade reverts with
    /// `LongUtilizationExceeded` / `ShortUtilizationExceeded`. The contract
    /// reverts only when open interest exceeds capacity, so a trade can
    /// fill the headroom exactly.
    pub fn headroom(&self, side: Side) -> PerpAtoms {
        self.capacity
            .on(side)
            .saturating_sub(self.open_interest(side))
    }

    /// Utilization on one side as the Perp passes it to the fees module:
    /// `openInterest * 1e6 / capacity`, rounded down. `None` when the side
    /// has no capacity, where the contract passes `type(uint24).max`
    /// instead of a ratio.
    ///
    /// Chain state is at most [`SCALE_1E6`] (100%): the contract reverts
    /// any change that leaves open interest above capacity, and allows open
    /// interest equal to it. A hand-built value above `u32::MAX`
    /// saturates.
    pub fn utilization_e6(&self, side: Side) -> Option<u32> {
        // `mul_div` fails only on a zero divisor, which is the same
        // condition the contract answers with `type(uint24).max` instead of
        // a ratio — so the guard and the error are one thing, and `ok()`
        // is the whole of it. Inventing a value for an unreachable branch
        // would put a number here that reads as 4294% utilization, which is
        // the sentinel this signature exists to avoid.
        mul_div(
            U256::from(self.open_interest(side).atoms()),
            U256::from(SCALE_1E6),
            U256::from(self.capacity.on(side).atoms()),
            Rounding::TowardZero,
        )
        .ok()
        .map(|utilization| utilization.saturating_to())
    }
}

/// The capacity a maker `band` adds at `sqrt_price`, exact to the contract
/// (see the module docs).
///
/// To size a band from margin, derive its liquidity first (for example
/// with [`crate::math::liquidity::estimate_liquidity`]) and pass the band
/// holding it here.
///
/// # Errors
///
/// - [`ValidationError::InvalidPrice`] if `sqrt_price` is zero
/// - [`ValidationError::Overflow`] if a side exceeds the width the
///   contract's `toUint128` accepts
pub fn band_capacity(sqrt_price: SqrtPrice, band: &MakerBand) -> Result<Capacity, ValidationError> {
    PricedRange::new(sqrt_price, &band.range)?.capacity(band.liquidity)
}

/// The least liquidity over `range` whose capacity on `side` at
/// `sqrt_price` is at least `target`: the inverse of [`band_capacity`].
///
/// The result is exact, not an estimate: [`band_capacity`] of it reaches
/// the target and of one unit less does not. A zero target needs zero
/// liquidity.
///
/// # Errors
///
/// - [`ValidationError::NoBandCapacity`] if the price sits at or beyond the
///   range's edge on `side`, so no liquidity in it backs that side
/// - [`ValidationError::InvalidPrice`] if `sqrt_price` is zero
/// - [`ValidationError::Overflow`] if the liquidity exceeds `u128`, or if
///   either side of the band's capacity at that liquidity does, where the
///   contract's `toUint128` reverts
pub fn liquidity_for_capacity(
    sqrt_price: SqrtPrice,
    range: &TickRange,
    side: Side,
    target: PerpAtoms,
) -> Result<LUnits, ValidationError> {
    let priced = PricedRange::new(sqrt_price, range)?;
    if target.is_zero() {
        return Ok(LUnits::ZERO);
    }
    let (lo, hi) = priced.span(side);
    if lo == hi {
        return Err(ValidationError::NoBandCapacity {
            lower: range.lower(),
            upper: range.upper(),
        });
    }
    // `getAmount0ForLiquidity` is floor(floor(L·2^96·(hi − lo) / hi) / lo),
    // and nested floors of integer divisions collapse to one:
    // floor(L·2^96·(hi − lo) / (hi·lo)). The least L reaching the target
    // is therefore ceil(target·hi·lo / (2^96·(hi − lo))). The numerator
    // has three factors, past `mul_div`'s two, so it is built in 512 bits
    // directly (at most 2^448).
    let numerator = U512::from(target.atoms()) * U512::from(hi) * U512::from(lo);
    let denominator = U512::from(Q96) * U512::from(hi - lo);
    let liquidity = numerator.div_ceil(denominator);
    if liquidity > U512::from(u128::MAX) {
        return Err(ValidationError::Overflow {
            context: "liquidity for capacity exceeds u128".into(),
        });
    }
    let liquidity = LUnits::new(liquidity.to::<u128>());
    // Capacity grows with liquidity, so if this band cannot be opened
    // because a side overflows `u128`, no liquidity reaching the target can.
    priced.capacity(liquidity)?;
    Ok(liquidity)
}

/// A range's sqrt bounds with the pool price clamped into them.
struct PricedRange {
    sqrt_lower: U256,
    sqrt_upper: U256,
    sqrt_clamped: U256,
}

impl PricedRange {
    fn new(sqrt_price: SqrtPrice, range: &TickRange) -> Result<Self, ValidationError> {
        if sqrt_price.is_zero() {
            return Err(ValidationError::InvalidPrice {
                reason: "zero sqrt price".into(),
            });
        }
        let (sqrt_lower, sqrt_upper) = range.sqrt_bounds();
        let (sqrt_lower, sqrt_upper) = (sqrt_lower.x96(), sqrt_upper.x96());
        Ok(Self {
            sqrt_lower,
            sqrt_upper,
            sqrt_clamped: sqrt_price.x96().clamp(sqrt_lower, sqrt_upper),
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

    fn capacity(&self, liquidity: LUnits) -> Result<Capacity, ValidationError> {
        Ok(Capacity {
            long: self.perp(Side::Long, liquidity)?,
            short: self.perp(Side::Short, liquidity)?,
        })
    }

    fn perp(&self, side: Side, liquidity: LUnits) -> Result<PerpAtoms, ValidationError> {
        let (lo, hi) = self.span(side);
        let atoms = amount0_delta(lo, hi, liquidity.units(), Rounding::TowardZero)?;
        u128::try_from(atoms)
            .map(PerpAtoms::new)
            .map_err(|_| ValidationError::Overflow {
                context: "band capacity exceeds u128".into(),
            })
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{B256, uint};

    use super::*;
    use crate::constants::{MAX_TICK, MIN_TICK};
    use crate::math::tick::get_sqrt_ratio_at_tick;

    fn range(lower: i32, upper: i32) -> TickRange {
        TickRange::new(lower, upper).unwrap()
    }

    fn band(lower: i32, upper: i32, liquidity: u128) -> MakerBand {
        MakerBand::new(range(lower, upper), LUnits::new(liquidity))
    }

    /// A mainnet maker open, reproduced exactly: the pool price just before
    /// the open, the band, its liquidity, and the capacity the contract
    /// stored (`makerDetails(pos_id).capacity`, equal to the `capacity()`
    /// delta across the open).
    struct GoldenOpen {
        sqrt_price: SqrtPrice,
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
            sqrt_price: SqrtPrice::from_x96(uint!(434377632981895034013411082410_U256)),
            tick_lower: 27090,
            tick_upper: 38100,
            liquidity: 1_757_959,
            capacity: Capacity {
                long: PerpAtoms::new(58_993),
                short: PerpAtoms::new(133_075),
            },
        },
        // pos 1202, block 506149884: band straddles the price.
        GoldenOpen {
            sqrt_price: SqrtPrice::from_x96(uint!(510281871168875509382435561417_U256)),
            tick_lower: 35610,
            tick_upper: 38670,
            liquidity: 97_506_535,
            capacity: Capacity {
                long: PerpAtoms::new(1_034_395),
                short: PerpAtoms::new(1_297_356),
            },
        },
        // pos 1377, block 506873284: band entirely below the price.
        GoldenOpen {
            sqrt_price: SqrtPrice::from_x96(uint!(431621243212719145650373590467_U256)),
            tick_lower: 31050,
            tick_upper: 33090,
            liquidity: 39_437_273,
            capacity: Capacity {
                long: PerpAtoms::ZERO,
                short: PerpAtoms::new(809_687),
            },
        },
    ];

    #[test]
    fn band_capacity_matches_mainnet_opens() {
        for g in &GOLDEN {
            let band = band(g.tick_lower, g.tick_upper, g.liquidity);
            assert_eq!(
                band_capacity(g.sqrt_price, &band).unwrap(),
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
        let full = PerpAtoms::new(
            amount0_delta(
                sqrt_lower.x96(),
                sqrt_upper.x96(),
                liquidity,
                Rounding::TowardZero,
            )
            .unwrap()
            .to::<u128>(),
        );

        let full_band = band(-600, 600, liquidity);
        let at_lower = band_capacity(sqrt_lower, &full_band).unwrap();
        assert_eq!(
            at_lower,
            Capacity {
                long: full,
                short: PerpAtoms::ZERO
            }
        );
        let at_upper = band_capacity(sqrt_upper, &full_band).unwrap();
        assert_eq!(
            at_upper,
            Capacity {
                long: PerpAtoms::ZERO,
                short: full
            }
        );
        assert_eq!(
            band_capacity(SqrtPrice::from_x96(Q96), &band(-600, 600, 0)).unwrap(),
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
            long: PerpAtoms::new(60_883_605),
            short: PerpAtoms::new(51_603_209),
        },
        long_open_interest: PerpAtoms::new(37_772_806),
        short_open_interest: PerpAtoms::new(42_582_564),
    };

    #[test]
    fn market_utilization_matches_the_contract_formula() {
        // floor(37772806e6 / 60883605) and floor(42582564e6 / 51603209).
        assert_eq!(HORMUZ_TRAFFIC.utilization_e6(Side::Long), Some(620_410));
        assert_eq!(HORMUZ_TRAFFIC.utilization_e6(Side::Short), Some(825_192));
        assert_eq!(
            HORMUZ_TRAFFIC.headroom(Side::Long),
            PerpAtoms::new(23_110_799)
        );
        assert_eq!(
            HORMUZ_TRAFFIC.headroom(Side::Short),
            PerpAtoms::new(9_020_645)
        );
        assert_eq!(
            HORMUZ_TRAFFIC.open_interest(Side::Short),
            PerpAtoms::new(42_582_564)
        );
    }

    #[test]
    fn market_utilization_edges() {
        let empty = MarketCapacity::default();
        assert_eq!(empty.utilization_e6(Side::Long), None);
        assert_eq!(empty.headroom(Side::Short), PerpAtoms::ZERO);

        let full = MarketCapacity {
            block: BlockContext::default(),
            capacity: Capacity {
                long: PerpAtoms::new(u128::MAX),
                short: PerpAtoms::new(1),
            },
            long_open_interest: PerpAtoms::new(u128::MAX),
            short_open_interest: PerpAtoms::new(u128::MAX),
        };
        assert_eq!(full.utilization_e6(Side::Long), Some(SCALE_1E6));
        assert_eq!(full.headroom(Side::Long), PerpAtoms::ZERO);
        assert_eq!(full.utilization_e6(Side::Short), Some(u32::MAX));
        assert_eq!(full.headroom(Side::Short), PerpAtoms::ZERO);
    }

    /// The range is valid by construction; the price is the one input
    /// left to check.
    #[test]
    fn band_capacity_rejects_a_zero_price() {
        assert!(matches!(
            band_capacity(SqrtPrice::from_x96(U256::ZERO), &band(-600, 600, 1)),
            Err(ValidationError::InvalidPrice { .. })
        ));
    }

    /// The inverse is the least liquidity reaching the target: one unit
    /// less falls short.
    fn assert_least(sqrt_price: SqrtPrice, lower: i32, upper: i32, side: Side, target: PerpAtoms) {
        let range = range(lower, upper);
        let liquidity = liquidity_for_capacity(sqrt_price, &range, side, target).unwrap();
        let reached = band_capacity(sqrt_price, &MakerBand::new(range, liquidity)).unwrap();
        assert!(
            reached.on(side) >= target,
            "{side} target {target:?}: liquidity {} gives {:?}",
            liquidity.units(),
            reached.on(side)
        );
        let one_less = liquidity.checked_sub(LUnits::new(1)).unwrap();
        let below = band_capacity(sqrt_price, &MakerBand::new(range, one_less)).unwrap();
        assert!(
            below.on(side) < target,
            "{side} target {target:?}: liquidity {} already gives {:?}",
            one_less.units(),
            below.on(side)
        );
    }

    #[test]
    fn liquidity_for_capacity_inverts_mainnet_opens() {
        for g in &GOLDEN {
            for side in [Side::Long, Side::Short] {
                let target = g.capacity.on(side);
                if target.is_zero() {
                    continue;
                }
                assert_least(g.sqrt_price, g.tick_lower, g.tick_upper, side, target);
                let liquidity = liquidity_for_capacity(
                    g.sqrt_price,
                    &range(g.tick_lower, g.tick_upper),
                    side,
                    target,
                )
                .unwrap();
                assert!(liquidity.units() <= g.liquidity);
            }
        }
    }

    #[test]
    fn liquidity_for_capacity_is_least_across_scales() {
        let sqrt_price = get_sqrt_ratio_at_tick(34_000).unwrap();
        for target in [1, 7, 999, 1_000_000, 123_456_789_012, u64::MAX as u128] {
            let target = PerpAtoms::new(target);
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
            liquidity_for_capacity(
                sqrt_price,
                &range(20_000, 30_000),
                Side::Long,
                PerpAtoms::new(1)
            ),
            Err(ValidationError::NoBandCapacity {
                lower: 20_000,
                upper: 30_000
            })
        ));
        assert!(matches!(
            liquidity_for_capacity(
                sqrt_price,
                &range(36_000, 40_000),
                Side::Short,
                PerpAtoms::new(1)
            ),
            Err(ValidationError::NoBandCapacity { .. })
        ));
        assert_eq!(
            liquidity_for_capacity(
                sqrt_price,
                &range(20_000, 30_000),
                Side::Long,
                PerpAtoms::ZERO
            )
            .unwrap(),
            LUnits::ZERO
        );
    }

    /// The inverse at the contract's branch edges: with the price exactly on
    /// the lower tick the whole band backs longs, exactly on the upper tick
    /// the whole band backs shorts, and the empty side is rejected.
    #[test]
    fn liquidity_for_capacity_at_band_edges() {
        let sqrt_lower = get_sqrt_ratio_at_tick(30_000).unwrap();
        let sqrt_upper = get_sqrt_ratio_at_tick(38_000).unwrap();
        for target in [1, 1_000_000, u64::MAX as u128] {
            let target = PerpAtoms::new(target);
            assert_least(sqrt_lower, 30_000, 38_000, Side::Long, target);
            assert_least(sqrt_upper, 30_000, 38_000, Side::Short, target);
        }
        let one = PerpAtoms::new(1);
        assert!(matches!(
            liquidity_for_capacity(sqrt_lower, &range(30_000, 38_000), Side::Short, one),
            Err(ValidationError::NoBandCapacity { .. })
        ));
        assert!(matches!(
            liquidity_for_capacity(sqrt_upper, &range(30_000, 38_000), Side::Long, one),
            Err(ValidationError::NoBandCapacity { .. })
        ));
    }

    #[test]
    fn liquidity_for_capacity_of_zero_is_zero() {
        let sqrt_price = get_sqrt_ratio_at_tick(34_000).unwrap();
        for side in [Side::Long, Side::Short] {
            assert_eq!(
                liquidity_for_capacity(sqrt_price, &range(30_000, 38_000), side, PerpAtoms::ZERO)
                    .unwrap(),
                LUnits::ZERO
            );
        }
    }

    /// The largest target that still fits reaches it exactly: the inverse
    /// of a band's full-range capacity at `u128::MAX` liquidity is that
    /// liquidity.
    #[test]
    fn liquidity_for_capacity_at_the_largest_target() {
        let sqrt_price = get_sqrt_ratio_at_tick(MAX_TICK).unwrap();
        let full = band_capacity(sqrt_price, &band(0, MAX_TICK, u128::MAX))
            .unwrap()
            .short;
        assert_least(sqrt_price, 0, MAX_TICK, Side::Short, full);
        assert_eq!(
            liquidity_for_capacity(sqrt_price, &range(0, MAX_TICK), Side::Short, full).unwrap(),
            LUnits::new(u128::MAX)
        );
    }

    /// Below a price of one a unit of liquidity backs more than one perp
    /// atom, so the least liquidity for a `u128::MAX` target fits in
    /// `u128` but its capacity does not. The contract would revert on
    /// `toUint128`, so the inverse reports the overflow.
    #[test]
    fn liquidity_for_capacity_rejects_a_capacity_past_u128() {
        let sqrt_price = get_sqrt_ratio_at_tick(1_000).unwrap();
        assert!(matches!(
            liquidity_for_capacity(
                sqrt_price,
                &range(MIN_TICK, 0),
                Side::Short,
                PerpAtoms::new(u128::MAX)
            ),
            Err(ValidationError::Overflow { .. })
        ));
    }

    #[test]
    fn liquidity_for_capacity_reports_overflow() {
        let sqrt_price = get_sqrt_ratio_at_tick(0).unwrap();
        assert!(matches!(
            liquidity_for_capacity(
                sqrt_price,
                &range(0, 1),
                Side::Short,
                PerpAtoms::new(u128::MAX)
            ),
            Err(ValidationError::NoBandCapacity { .. })
        ));
        assert!(matches!(
            liquidity_for_capacity(
                sqrt_price,
                &range(-1, 1),
                Side::Short,
                PerpAtoms::new(u128::MAX)
            ),
            Err(ValidationError::Overflow { .. })
        ));
    }
}
