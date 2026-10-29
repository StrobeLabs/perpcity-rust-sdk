//! A maker's full close, as the deployed `PerpLogic.adjustMaker` takes one:
//! removing all of a band's liquidity settles the position, and the perp
//! left decides whether it closes or stays open as a taker.
//!
//! ```text
//! cap'     = capacity − the band's capacity            checkUtilization(oi, cap')
//! delta'   = delta + the band's principal at the pool price, rounded down
//! settled  = margin − funding + util + lpFees
//! no perp left:  payout = settled + delta'.usd          NegativeMargin when < 0
//! perp left:     a taker of delta', settled margin      MarginRatioTooLow at takerMarginRatios().liq
//!                oi' = oi + |delta'.perp| on its side   checkUtilization(oi', cap')
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::MakerEquityBreakdown;
use crate::errors::ValidationError;
use crate::math::capacity::{Capacity, MarketCapacity};
use crate::math::liquidity::amounts_for_liquidity;
use crate::math::taker::{TakerHealth, TakerMarketSnapshot, TakerState};
use crate::units::{Funding, PerSide, PerpAtoms, PerpDelta, Ratio, Side, UsdcAtoms, UsdcDelta};

/// What removing all of a maker's liquidity leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Remainder {
    /// No perp: the position closes and this equity is paid out.
    Payout(UsdcDelta),
    /// Perp: the position stays open as this taker, checkpointed at the
    /// close, so it owes nothing yet.
    Taker(TakerHealth),
}

/// Why the contract would revert a full close, named for the error it
/// reverts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Error)]
pub enum CloseRefusal {
    /// Long open interest exceeds what the bands left back.
    #[error("long open interest would exceed the long capacity left")]
    LongUtilizationExceeded,
    /// Short open interest exceeds what the bands left back.
    #[error("short open interest would exceed the short capacity left")]
    ShortUtilizationExceeded,
    /// The taker it converts to is below the taker liquidation ratio.
    #[error("the taker it converts to would be below the liquidation ratio")]
    MarginRatioTooLow,
    /// It closes with negative equity.
    #[error("it would close with negative equity")]
    NegativeMargin,
}

impl CloseRefusal {
    /// The utilization refusal on `side`.
    const fn utilization(side: Side) -> Self {
        match side {
            Side::Long => Self::LongUtilizationExceeded,
            Side::Short => Self::ShortUtilizationExceeded,
        }
    }
}

/// A maker's full close: what it leaves, and whether the contract would
/// take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullClose {
    remainder: Remainder,
    refusal: Option<CloseRefusal>,
}

impl FullClose {
    /// What the close leaves.
    pub fn remainder(&self) -> &Remainder {
        &self.remainder
    }

    /// The first check the contract would revert on, or `None` when it
    /// would take the close.
    pub fn refusal(&self) -> Option<CloseRefusal> {
        self.refusal
    }

    /// The perp the close leaves the position holding, long positive: zero
    /// when it closes.
    pub fn perp_left(&self) -> PerpDelta {
        match self.remainder {
            Remainder::Payout(_) => PerpDelta::ZERO,
            Remainder::Taker(taker) => taker.delta_perp(),
        }
    }
}

impl MakerEquityBreakdown {
    /// Removing all of this maker's liquidity, at the preview's pool price
    /// and mark, against the market's capacity and the taker liquidation
    /// ratio (`takerMarginRatios().liq`) read at the preview's block.
    ///
    /// # Errors
    ///
    /// [`ValidationError::BlockMismatch`] when `market` was read at another
    /// block; [`ValidationError::Overflow`] when the band's capacity exceeds
    /// the market's, which no block produces, or the principal or the
    /// taker's value leaves the range its type can hold.
    pub fn full_close(
        &self,
        market: &MarketCapacity,
        taker_liquidation: Ratio,
    ) -> Result<FullClose, ValidationError> {
        if market.block != self.block {
            return Err(ValidationError::BlockMismatch {
                expected: self.block.number,
                found: market.block.number,
            });
        }
        let mut capacity = Capacity::default();
        for side in Side::BOTH {
            *capacity.on_mut(side) = market
                .capacity
                .on(side)
                .checked_sub(*self.capacity.on(side))
                .ok_or_else(|| ValidationError::Overflow {
                    context: format!("the band's {side} capacity exceeds the market's"),
                })?;
        }
        let mut refusal = exceeded(market.open_interest, capacity);

        // V4 rounds a removal's principal down, as `valPnl`'s amounts do,
        // and at a bound the pool's tick picks the same legs the clamp does.
        let exposure = &self.exposure;
        let (perp, usd) = amounts_for_liquidity(
            self.sqrt_price,
            exposure.sqrt_lower,
            exposure.sqrt_upper,
            exposure.liquidity,
        )?;
        let delta_perp = exposure.delta_perp + PerpDelta::from(perp);
        let delta_usd = exposure.delta_usd + UsdcDelta::from(usd);
        let settled = self.settled_margin();

        let remainder = match delta_perp.side() {
            None => {
                let payout = settled + delta_usd;
                refusal = refusal.or(payout.is_negative().then_some(CloseRefusal::NegativeMargin));
                Remainder::Payout(payout)
            }
            Some(side) => {
                // `normalizeMargin`: a negative settle moves into the USD leg.
                let (margin, delta_usd) = match UsdcAtoms::try_from(settled) {
                    Ok(margin) => (margin, delta_usd),
                    Err(_) => (UsdcAtoms::ZERO, delta_usd + settled),
                };
                // Checkpointed at the market's cumulatives, so both are zero
                // here and nothing accrues against the margin.
                let checkpointed = TakerMarketSnapshot {
                    block: self.block,
                    funding: Funding::ZERO,
                    util_payments: PerSide::default(),
                    mark: self.mark,
                };
                let taker = checkpointed.taker_health(&TakerState {
                    delta_perp,
                    delta_usd,
                    margin,
                    liquidation_margin_ratio: taker_liquidation,
                    last_cuml_funding: Funding::ZERO,
                    last_util_payments: PerSide::default(),
                })?;
                refusal = refusal.or(taker
                    .is_liquidatable()
                    .then_some(CloseRefusal::MarginRatioTooLow));
                let mut open_interest = market.open_interest;
                *open_interest.on_mut(side) += delta_perp.magnitude();
                refusal = refusal.or(exceeded(open_interest, capacity));
                Remainder::Taker(taker)
            }
        };
        Ok(FullClose { remainder, refusal })
    }
}

/// `checkUtilization`: the side whose open interest exceeds its capacity,
/// long first.
fn exceeded(open_interest: PerSide<PerpAtoms>, capacity: Capacity) -> Option<CloseRefusal> {
    Side::BOTH
        .into_iter()
        .find(|&side| open_interest.on(side) > capacity.on(side))
        .map(CloseRefusal::utilization)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::I256;
    use proptest::prelude::*;

    use super::super::MakerState;
    use super::super::tests::golden_market_and_maker;
    use super::*;
    use crate::constants::Q96;
    use crate::math::liquidity::liquidity_change_delta;
    use crate::math::range::TickRange;
    use crate::math::tick::get_sqrt_ratio_at_tick;
    use crate::units::{LDelta, LUnits};

    /// The deployed taker liquidation ratio, 5%.
    fn taker_liquidation() -> Ratio {
        Ratio::from_e6(50_000).unwrap()
    }

    /// A margin that keeps the golden maker solvent: $1,000.
    const MARGIN: UsdcAtoms = UsdcAtoms::new(1_000_000_000);

    /// A market whose other bands back `room` on each side, with
    /// `open_interest` drawn on them, at `b`'s block.
    fn market(b: &MakerEquityBreakdown, room: u128, open_interest: [u128; 2]) -> MarketCapacity {
        MarketCapacity {
            block: b.block,
            capacity: b.capacity.map(|band| band + PerpAtoms::new(room)),
            open_interest: PerSide::new(
                PerpAtoms::new(open_interest[0]),
                PerpAtoms::new(open_interest[1]),
            ),
        }
    }

    fn close(b: &MakerEquityBreakdown, room: u128, open_interest: [u128; 2]) -> FullClose {
        b.full_close(&market(b, room, open_interest), taker_liquidation())
            .unwrap()
    }

    /// The golden maker with no margin and a residual that leaves `left`
    /// perp once its band is removed, edited by `edit`.
    fn golden(left: i128, edit: impl FnOnce(&mut MakerState)) -> MakerEquityBreakdown {
        let (market, accrual, mut maker) = golden_market_and_maker();
        let (perp, _) = amounts_for_liquidity(
            market.sqrt_price,
            get_sqrt_ratio_at_tick(maker.tick_lower).unwrap(),
            get_sqrt_ratio_at_tick(maker.tick_upper).unwrap(),
            maker.liquidity,
        )
        .unwrap();
        maker.delta_perp = PerpDelta::new(left) - PerpDelta::from(perp);
        maker.margin = UsdcAtoms::ZERO;
        edit(&mut maker);
        market
            .accrued(&accrual)
            .unwrap()
            .maker_equity(&maker)
            .unwrap()
    }

    /// A close changes the position's form, not its worth: the taker holds
    /// the perp the residual leaves and is worth what the maker was, to the
    /// atom the one rounding fewer can move.
    #[test]
    fn a_conversion_keeps_the_equity() {
        let b = golden(-1_000, |maker| maker.margin = MARGIN);
        let close = close(&b, 1_000, [0, 0]);
        let Remainder::Taker(taker) = close.remainder() else {
            panic!("perp is left: {close:?}");
        };
        assert_eq!(close.perp_left(), PerpDelta::new(-1_000));
        assert!((taker.equity() - b.equity()).atoms().abs() <= 1);
        assert_eq!(taker.liquidation_margin_ratio(), taker_liquidation());
        assert_eq!(close.refusal(), None);
    }

    /// With no perp left the position closes, paying out exactly its
    /// equity, which the contract refuses when negative.
    #[test]
    fn nothing_left_pays_out_the_equity() {
        for (margin, refusal) in [
            (UsdcAtoms::ZERO, Some(CloseRefusal::NegativeMargin)),
            (MARGIN, None),
        ] {
            let b = golden(0, |maker| maker.margin = margin);
            let close = close(&b, 0, [0, 0]);
            assert_eq!(close.remainder(), &Remainder::Payout(b.equity()));
            assert_eq!(close.perp_left(), PerpDelta::ZERO);
            assert_eq!(close.refusal(), refusal);
        }
    }

    /// An underwater maker left with an atom of perp converts to a taker the
    /// contract will not hold. A settle owing more than the margin is carried
    /// in the USD leg, as `normalizeMargin` does, and moves no equity.
    #[test]
    fn an_underwater_conversion_is_refused_on_its_margin_ratio() {
        // A funding checkpoint 1,000 units of the cumulative off the
        // market's, so the position owes about $134.
        let b = golden(-1, |maker| {
            let funding = golden_market_and_maker().0.funding.x96();
            let ahead = I256::try_from(Q96).unwrap() * I256::try_from(1_000).unwrap();
            maker.last_cuml_funding = Funding::from_x96(funding + ahead);
        });
        assert!(b.settled_margin().is_negative());
        let close = close(&b, 1, [0, 0]);
        let Remainder::Taker(taker) = close.remainder() else {
            panic!("an atom of perp is left: {close:?}");
        };
        assert_eq!(taker.margin(), UsdcAtoms::ZERO);
        assert!((taker.equity() - b.equity()).atoms().abs() <= 1);
        assert_eq!(close.refusal(), Some(CloseRefusal::MarginRatioTooLow));
    }

    /// Open interest is checked against the capacity the band leaves, then
    /// again with the converted taker on its side; it may fill that capacity
    /// but not exceed it.
    #[test]
    fn open_interest_may_fill_the_capacity_left_but_not_exceed_it() {
        let b = golden(-1_000, |maker| maker.margin = MARGIN);
        let refusal = |open_interest| close(&b, 10_000, open_interest).refusal();
        assert_eq!(refusal([10_000, 0]), None);
        assert_eq!(
            refusal([10_001, 0]),
            Some(CloseRefusal::LongUtilizationExceeded)
        );
        // The taker adds its 1,000 short.
        assert_eq!(refusal([0, 9_000]), None);
        assert_eq!(
            refusal([0, 9_001]),
            Some(CloseRefusal::ShortUtilizationExceeded)
        );
    }

    /// The contract checks utilization as the band leaves, before the
    /// taker, so that is what a close failing both is refused for.
    #[test]
    fn the_first_check_to_fail_is_the_one_reported() {
        let b = golden(-1, |_| ());
        assert_eq!(
            close(&b, 1, [0, 0]).refusal(),
            Some(CloseRefusal::MarginRatioTooLow)
        );
        assert_eq!(
            close(&b, 1, [2, 0]).refusal(),
            Some(CloseRefusal::LongUtilizationExceeded)
        );
    }

    /// Inputs no single block produces are an error, not a close: a market
    /// read at another block, or a band backing more than its market.
    #[test]
    fn inputs_from_no_one_block_are_an_error() {
        let b = golden(0, |maker| maker.margin = MARGIN);
        let mut later = market(&b, 0, [0, 0]);
        later.block.number += 1;
        assert!(matches!(
            b.full_close(&later, taker_liquidation()),
            Err(ValidationError::BlockMismatch { .. })
        ));
        let mut thin = market(&b, 0, [0, 0]);
        thin.capacity.long = PerpAtoms::ZERO;
        assert!(matches!(
            b.full_close(&thin, taker_liquidation()),
            Err(ValidationError::Overflow { .. })
        ));
    }

    proptest! {
        /// V4 removes a band's principal rounded down, choosing its legs by
        /// the pool's tick; the amounts `valPnl` reads clamp the price to the
        /// band instead. They agree wherever the pool stands, on a tick with
        /// the pool's tick either at it or just below, as after a cross down.
        #[test]
        fn the_principal_is_what_v4_removes(
            lower in -50_000i32..50_000,
            width in 1i32..5_000,
            offset in -1_000i32..6_000,
            crossed_down: bool,
            liquidity in 1u128..1 << 80,
        ) {
            let range = TickRange::new(lower, lower + width).unwrap();
            let sqrt = get_sqrt_ratio_at_tick(lower + offset).unwrap();
            let tick = lower + offset - i32::from(crossed_down);
            let (perp, usd) = amounts_for_liquidity(
                sqrt,
                get_sqrt_ratio_at_tick(range.lower()).unwrap(),
                get_sqrt_ratio_at_tick(range.upper()).unwrap(),
                LUnits::new(liquidity),
            )
            .unwrap();
            let removed =
                liquidity_change_delta(sqrt, tick, &range, LDelta::new(-(liquidity as i128)))
                    .unwrap();
            prop_assert_eq!((PerpDelta::from(perp), UsdcDelta::from(usd)), removed);
        }
    }
}
