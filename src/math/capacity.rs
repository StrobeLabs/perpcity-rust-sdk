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
//! On the two earlier builds a taker trade or a liquidity removal that
//! would leave a side's open interest above its capacity reverts with
//! `LongUtilizationExceeded` / `ShortUtilizationExceeded`. [`MarketCapacity`]
//! holds both totals and derives each side's headroom and utilization.
//!
//! On `v1.0.0` `capacity()` keeps its shape and loses that role: it is the
//! utilization band, the backing the fees module prices utilization from,
//! and it gates nothing. It can fall to zero after the index moves, and
//! open interest can then exceed it. What gates a `v1.0.0` trade is the
//! physical close capacity, `physicalCloseCapacity()`: the depth the whole
//! book holds for each side's closes, which every swap rotates (a buy
//! takes short-close depth and gives long-close depth). [`MarketCapacity`]
//! carries it there as [`MarketCapacity::close_capacity`].

use alloy::primitives::{U256, U512};
use serde::{Deserialize, Serialize};

use crate::constants::{Q96, SCALE_1E6};
use crate::contracts;
use crate::errors::ValidationError;
use crate::math::BlockContext;
use crate::math::range::{MakerBand, TickRange};
use crate::math::swap::amount0_delta;
use crate::units::fixed_point::{Rounding, mul_div};
use crate::units::{LUnits, PerSide, PerpAtoms, Side, SqrtPrice};

/// Taker capacity per side, as the contract's `Capacity` struct holds it:
/// the open interest each side can hold against a band or the market.
pub type Capacity = PerSide<PerpAtoms>;

impl From<contracts::Capacity> for Capacity {
    fn from(cap: contracts::Capacity) -> Self {
        Self::new(PerpAtoms::new(cap.long), PerpAtoms::new(cap.short))
    }
}

/// A market's taker capacity and the open interest drawing on it, read at
/// one block by [`MarketReader::get_capacity`](crate::MarketReader::get_capacity).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketCapacity {
    /// The block every total was read at.
    pub block: BlockContext,
    /// `capacity()`: on the earlier builds the sum of every band's
    /// capacity snapshot; on `v1.0.0` the utilization band.
    pub capacity: Capacity,
    /// `openInterest()`: the draw on it.
    pub open_interest: PerSide<PerpAtoms>,
    /// `physicalCloseCapacity()` on a `v1.0.0` market: each side's close
    /// depth, in whole perp atoms, a long close's on the long side. `None`
    /// on the earlier builds, which have no such view and gate on
    /// [`Self::capacity`]; so `Some` exactly when the market is `v1.0.0`,
    /// and the utilization rule follows it.
    pub close_capacity: Option<Capacity>,
}

impl MarketCapacity {
    /// Open interest `side` can still add before a trade on an earlier
    /// build reverts with `LongUtilizationExceeded` /
    /// `ShortUtilizationExceeded`. The contract reverts only when open
    /// interest exceeds capacity, so a trade can fill the headroom exactly.
    ///
    /// On `v1.0.0` the band gates nothing, so this is the band's slack and
    /// no limit; a trade's limit there is the close depth in
    /// [`Self::close_capacity`] with the side rule the swap applies to it.
    pub fn headroom(&self, side: Side) -> PerpAtoms {
        self.capacity
            .on(side)
            .saturating_sub(*self.open_interest.on(side))
    }

    /// Additional open interest a buy (Long) or sell (Short) can add
    /// against physical depth. A buy consumes short-close depth.
    pub fn physical_headroom(&self, direction: Side) -> Option<PerpAtoms> {
        let side = direction.opposite();
        Some(
            self.close_capacity?
                .on(side)
                .saturating_sub(*self.open_interest.on(side)),
        )
    }

    /// Whether the proposed post-change interest fits both post-change depths.
    pub fn fits_close_capacity(interest: Capacity, depth: Capacity) -> bool {
        interest.long <= depth.long && interest.short <= depth.short
    }

    /// Rotate whole-atom close depth for a buy (Long) or sell (Short),
    /// then check both sides against the proposed post-swap interest.
    /// A buy debits the requested size even on a terminal partial fill.
    /// A sell debits the fill. Both directions credit only the fill.
    pub fn fits_swap(
        interest: Capacity,
        depth: Capacity,
        direction: Side,
        requested: PerpAtoms,
        filled: PerpAtoms,
    ) -> bool {
        let debit = match direction {
            Side::Long => requested,
            Side::Short => filled,
        };
        let Some(remaining) = depth
            .on(direction.opposite())
            .atoms()
            .checked_sub(debit.atoms())
        else {
            return false;
        };
        let Some(gained) = depth.on(direction).atoms().checked_add(filled.atoms()) else {
            return false;
        };
        let (long, short) = match direction {
            Side::Long => (gained, remaining),
            Side::Short => (remaining, gained),
        };
        Self::fits_close_capacity(
            interest,
            Capacity::new(PerpAtoms::new(long), PerpAtoms::new(short)),
        )
    }

    /// Subtract the removed band's X64 contribution from the opposite close
    /// depth before rounding down. The last removal clears both depths.
    pub fn fits_maker_removal(
        interest: Capacity,
        depth_x64: PerSide<U256>,
        band_x64: PerSide<U256>,
        last: bool,
    ) -> bool {
        if last {
            return interest == Capacity::default();
        }
        let long = depth_x64.long.saturating_sub(band_x64.short) >> 64;
        let short = depth_x64.short.saturating_sub(band_x64.long) >> 64;
        U256::from(interest.long.atoms()) <= long && U256::from(interest.short.atoms()) <= short
    }

    /// Utilization on one side as the Perp passes it to the fees module:
    /// `openInterest * 1e6 / capacity`, rounded down.
    ///
    /// On the earlier builds, `None` when the side has no capacity, where
    /// the contract passes `type(uint24).max` instead of a ratio; chain
    /// state is at most [`SCALE_1E6`] (100%), since the contract reverts
    /// any change that leaves open interest above capacity, and a
    /// hand-built value above `u32::MAX` saturates.
    ///
    /// On `v1.0.0` (`PerpLogic._utilization`) always `Some`: a side with
    /// no capacity is at [`SCALE_1E6`], and the ratio is capped there,
    /// because the band can fall below the open interest it prices.
    pub fn utilization_e6(&self, side: Side) -> Option<u32> {
        if self.close_capacity.is_some() {
            return Some(v1_utilization_e6(
                *self.open_interest.on(side),
                *self.capacity.on(side),
            ));
        }
        // `mul_div` fails only on a zero divisor, which is the same
        // condition the contract answers with `type(uint24).max` instead of
        // a ratio — so the guard and the error are one thing, and `ok()`
        // is the whole of it. Inventing a value for an unreachable branch
        // would put a number here that reads as 4294% utilization, which is
        // the sentinel this signature exists to avoid.
        mul_div(
            U256::from(self.open_interest.on(side).atoms()),
            U256::from(SCALE_1E6),
            U256::from(self.capacity.on(side).atoms()),
            Rounding::TowardZero,
        )
        .ok()
        .map(|utilization| utilization.saturating_to())
    }
}

/// `v1.0.0`'s `_utilization`: `min(floor(openInterest · 1e6 / capacity),
/// 1e6)`, and `1e6` for no capacity.
fn v1_utilization_e6(open_interest: PerpAtoms, capacity: PerpAtoms) -> u32 {
    if capacity.is_zero() {
        return SCALE_1E6;
    }
    let utilization =
        U512::from(open_interest.atoms()) * U512::from(SCALE_1E6) / U512::from(capacity.atoms());
    utilization.min(U512::from(SCALE_1E6)).to::<u32>()
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

/// A removed band's contribution in X64 perp atoms, at the current pool price.
pub fn band_capacity_x64(
    sqrt_price: SqrtPrice,
    band: &MakerBand,
) -> Result<PerSide<U256>, ValidationError> {
    let range = PricedRange::new(sqrt_price, &band.range)?;
    let amount = |side| {
        let (lo, hi) = range.span(side);
        let numerator = mul_div(
            U256::from(band.liquidity.units()) << 96,
            hi - lo,
            hi,
            Rounding::TowardZero,
        )?;
        mul_div(numerator, U256::ONE << 64, lo, Rounding::TowardZero)
    };
    Ok(PerSide::new(amount(Side::Long)?, amount(Side::Short)?))
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
    let _ = priced.capacity(liquidity)?;
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

    #[test]
    fn physical_depth_is_directional() {
        let pair = |long, short| Capacity::new(PerpAtoms::new(long), PerpAtoms::new(short));
        let market = MarketCapacity {
            open_interest: pair(6, 2),
            close_capacity: Some(pair(9, 12)),
            ..Default::default()
        };
        assert_eq!(
            market.physical_headroom(Side::Long),
            Some(PerpAtoms::new(10))
        );
        assert_eq!(
            market.physical_headroom(Side::Short),
            Some(PerpAtoms::new(3))
        );
        // A close or liquidation of two shorts buys two. Long OI fills long depth.
        let depth = pair(6, 2);
        for size in [1, 2] {
            assert!(MarketCapacity::fits_swap(
                pair(6, 2 - size),
                depth,
                Side::Long,
                PerpAtoms::new(size),
                PerpAtoms::new(size)
            ));
            assert!(MarketCapacity::fits_swap(
                pair(2 - size, 6),
                pair(2, 6),
                Side::Short,
                PerpAtoms::new(size),
                PerpAtoms::new(size)
            ));
        }
        assert!(!MarketCapacity::fits_swap(
            pair(6, 0),
            depth,
            Side::Long,
            PerpAtoms::new(3),
            PerpAtoms::new(3)
        ));
        // A terminal buy requests three and fills one.
        assert!(!MarketCapacity::fits_swap(
            pair(7, 0),
            depth,
            Side::Long,
            PerpAtoms::new(3),
            PerpAtoms::new(1)
        ));
        assert!(!MarketCapacity::fits_swap(
            pair(8, 0),
            pair(6, 3),
            Side::Long,
            PerpAtoms::new(3),
            PerpAtoms::new(1)
        ));
        let quarter = U256::ONE << 62;
        let depth = PerSide::new(quarter * U256::from(41), U256::ZERO);
        let band = PerSide::new(U256::ZERO, quarter * U256::from(3));
        // 10.25 - 0.75 leaves 9.5 atoms for long closes.
        assert!(!MarketCapacity::fits_maker_removal(
            pair(10, 0),
            depth,
            band,
            false
        ));
        assert!(MarketCapacity::fits_maker_removal(
            pair(9, 0),
            depth,
            band,
            false
        ));
        assert!(!MarketCapacity::fits_maker_removal(
            pair(1, 0),
            depth,
            band,
            true
        ));
    }

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
        open_interest: PerSide::new(PerpAtoms::new(37_772_806), PerpAtoms::new(42_582_564)),
        close_capacity: None,
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
            *HORMUZ_TRAFFIC.open_interest.on(Side::Short),
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
            open_interest: PerSide::uniform(PerpAtoms::new(u128::MAX)),
            close_capacity: None,
        };
        assert_eq!(full.utilization_e6(Side::Long), Some(SCALE_1E6));
        assert_eq!(full.headroom(Side::Long), PerpAtoms::ZERO);
        assert_eq!(full.utilization_e6(Side::Short), Some(u32::MAX));
        assert_eq!(full.headroom(Side::Short), PerpAtoms::ZERO);
    }

    /// `v1.0.0`'s rule, which a market with a close capacity follows: a
    /// side with no band is full, a ratio is floored, and open interest
    /// past the band is capped at 100% rather than reported above it.
    #[test]
    fn v1_utilization_is_full_at_no_capacity_and_capped() {
        let v1 = |capacity: [u128; 2], open_interest: [u128; 2]| MarketCapacity {
            block: BlockContext::default(),
            capacity: Capacity::new(PerpAtoms::new(capacity[0]), PerpAtoms::new(capacity[1])),
            open_interest: PerSide::new(
                PerpAtoms::new(open_interest[0]),
                PerpAtoms::new(open_interest[1]),
            ),
            close_capacity: Some(Capacity::default()),
        };
        let empty = v1([0, 0], [0, 0]);
        assert_eq!(empty.utilization_e6(Side::Long), Some(SCALE_1E6));
        assert_eq!(empty.utilization_e6(Side::Short), Some(SCALE_1E6));

        // The W12 scenario after its first touch: floor(400000e6 / 558864).
        let touched = v1([577_953, 558_864], [850_000, 400_000]);
        assert_eq!(touched.utilization_e6(Side::Long), Some(SCALE_1E6));
        assert_eq!(touched.utilization_e6(Side::Short), Some(715_737));

        let huge = v1([1, u128::MAX], [u128::MAX, u128::MAX]);
        assert_eq!(huge.utilization_e6(Side::Long), Some(SCALE_1E6));
        assert_eq!(huge.utilization_e6(Side::Short), Some(SCALE_1E6));

        // The same totals on an earlier build keep that build's rule.
        let earlier = MarketCapacity {
            close_capacity: None,
            ..touched
        };
        assert_eq!(earlier.utilization_e6(Side::Long), Some(1_470_707));
        assert_eq!(
            MarketCapacity {
                close_capacity: None,
                ..empty
            }
            .utilization_e6(Side::Long),
            None
        );
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
            *reached.on(side) >= target,
            "{side} target {target:?}: liquidity {} gives {:?}",
            liquidity.units(),
            reached.on(side)
        );
        let one_less = liquidity.checked_sub(LUnits::new(1)).unwrap();
        let below = band_capacity(sqrt_price, &MakerBand::new(range, one_less)).unwrap();
        assert!(
            *below.on(side) < target,
            "{side} target {target:?}: liquidity {} already gives {:?}",
            one_less.units(),
            below.on(side)
        );
    }

    #[test]
    fn liquidity_for_capacity_inverts_mainnet_opens() {
        for g in &GOLDEN {
            for side in [Side::Long, Side::Short] {
                let target = *g.capacity.on(side);
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
