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

    use super::super::MakerState;
    use super::super::tests::{golden_market_and_maker, healthy_maker, healthy_maker_at_the_mark};
    use super::*;
    use crate::constants::Q96;
    use crate::math::liquidity::liquidity_change_delta;
    use crate::math::range::TickRange;
    use crate::math::tick::get_sqrt_ratio_at_tick;
    use crate::units::{LDelta, LUnits, Side, SqrtPrice};

    /// The deployed taker liquidation ratio, 5%.
    fn taker_liquidation() -> Ratio {
        Ratio::from_e6(50_000).unwrap()
    }

    /// A market whose bands back `room` on each side beyond this one's, with
    /// `open_interest` drawn on them.
    fn market(
        b: &MakerEquityBreakdown,
        room: u128,
        open_interest: PerSide<PerpAtoms>,
    ) -> MarketCapacity {
        MarketCapacity {
            block: b.block,
            capacity: Capacity::new(
                b.capacity.long + PerpAtoms::new(room),
                b.capacity.short + PerpAtoms::new(room),
            ),
            open_interest,
        }
    }

    fn open(b: &MakerEquityBreakdown, room: u128) -> MarketCapacity {
        market(b, room, PerSide::default())
    }

    /// A band straddling the pool, which the golden mark puts at tick 28543.
    const STRADDLING: (i32, i32) = (28_200, 28_800);

    /// The golden maker with no margin, its perp residual `left` atoms short
    /// of the band's perp at the pool price, and `edit` applied, accrued.
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

    /// A close changes the position's form, not its worth: the taker it
    /// leaves is worth what the maker was, to the atom the one rounding
    /// fewer can move, and it holds the band's perp plus the residual.
    #[test]
    fn a_conversion_keeps_the_equity_and_takes_the_band_into_the_residual() {
        let b = healthy_maker(1000, STRADDLING);
        let close = b
            .full_close(&open(&b, 1_000_000_000), taker_liquidation())
            .unwrap();
        let Remainder::Taker(taker) = close.remainder() else {
            panic!("a straddling band leaves perp: {close:?}");
        };
        let e = &b.exposure;
        let (perp, usd) =
            amounts_for_liquidity(b.sqrt_price, e.sqrt_lower, e.sqrt_upper, e.liquidity).unwrap();
        assert_eq!(close.perp_left(), e.delta_perp + PerpDelta::from(perp));
        assert_eq!(taker.delta_usd(), e.delta_usd + UsdcDelta::from(usd));
        assert!((taker.equity() - b.equity()).atoms().abs() <= 1);
        assert_eq!(taker.liquidation_margin_ratio(), taker_liquidation());
        assert_eq!(close.refusal(), None);
    }

    /// With no perp left the position closes, paying exactly its equity,
    /// and the contract refuses the close when that is negative.
    #[test]
    fn nothing_left_pays_out_the_equity_and_a_negative_one_is_refused() {
        let underwater = golden(0, |_| ());
        let close = underwater
            .full_close(&open(&underwater, 0), taker_liquidation())
            .unwrap();
        assert_eq!(close.remainder(), &Remainder::Payout(underwater.equity()));
        assert!(underwater.equity().is_negative());
        assert_eq!(close.refusal(), Some(CloseRefusal::NegativeMargin));
        assert_eq!(close.perp_left(), PerpDelta::ZERO);

        let covered = golden(0, |maker| maker.margin = UsdcAtoms::new(1_000_000_000));
        let close = covered
            .full_close(&open(&covered, 0), taker_liquidation())
            .unwrap();
        assert_eq!(close.remainder(), &Remainder::Payout(covered.equity()));
        assert_eq!(close.refusal(), None);
    }

    /// An atom of perp left makes an underwater maker a taker the contract
    /// will not hold. A settle that owes more than the margin is carried in
    /// the USD leg, as `normalizeMargin` does, and moves no equity.
    #[test]
    fn an_underwater_conversion_is_refused_on_its_margin_ratio() {
        // A funding checkpoint a thousand units of the cumulative ahead of the
        // market, so the position owes about $134 in funding.
        let owing = |maker: &mut MakerState| {
            let ahead = I256::try_from(Q96).unwrap() * I256::try_from(1000).unwrap();
            maker.last_cuml_funding = Funding::from_x96(golden_funding() + ahead);
        };
        let b = golden(-1, owing);
        assert!(b.settled_margin().is_negative());
        let close = b.full_close(&open(&b, 0), taker_liquidation()).unwrap();
        let Remainder::Taker(taker) = close.remainder() else {
            panic!("an atom of perp is left: {close:?}");
        };
        assert_eq!(close.perp_left(), PerpDelta::new(-1));
        assert_eq!(taker.margin(), UsdcAtoms::ZERO);
        assert!((taker.equity() - b.equity()).atoms().abs() <= 1);
        assert_eq!(close.refusal(), Some(CloseRefusal::MarginRatioTooLow));
    }

    fn golden_funding() -> I256 {
        golden_market_and_maker().0.funding.x96()
    }

    /// Open interest is checked twice, as the contract does: against the
    /// capacity the band leaves, then with the taker it converts to added
    /// on its side. Open interest may fill capacity exactly.
    #[test]
    fn open_interest_is_checked_against_the_capacity_left_before_and_after_the_conversion() {
        let b = healthy_maker(1000, STRADDLING);
        let room = 1_000_000_000;
        let converted = b
            .full_close(&open(&b, room), taker_liquidation())
            .unwrap()
            .perp_left();
        let side = converted.side().unwrap();
        let other = match side {
            Side::Long => Side::Short,
            Side::Short => Side::Long,
        };
        let refusal = |side: Side| match side {
            Side::Long => CloseRefusal::LongUtilizationExceeded,
            Side::Short => CloseRefusal::ShortUtilizationExceeded,
        };
        let drawn = |side: Side, atoms: PerpAtoms| {
            let mut open_interest = PerSide::default();
            *open_interest.on_mut(side) = atoms;
            b.full_close(&market(&b, room, open_interest), taker_liquidation())
                .unwrap()
                .refusal()
        };
        // What the bands left back, on each side.
        let room = PerpAtoms::new(room);

        assert_eq!(drawn(other, room), None);
        assert_eq!(drawn(other, room + PerpAtoms::new(1)), Some(refusal(other)));
        let headroom = room.checked_sub(converted.magnitude()).unwrap();
        assert_eq!(drawn(side, headroom), None);
        assert_eq!(
            drawn(side, headroom + PerpAtoms::new(1)),
            Some(refusal(side))
        );
    }

    /// The contract checks utilization when the band leaves, before it
    /// checks the taker, so that is the refusal a close failing both meets.
    #[test]
    fn the_first_check_to_fail_is_the_one_reported() {
        let b = golden(-1, |_| ());
        let alone = b.full_close(&open(&b, 0), taker_liquidation()).unwrap();
        assert_eq!(alone.refusal(), Some(CloseRefusal::MarginRatioTooLow));
        let crowded = market(&b, 0, PerSide::new(PerpAtoms::new(1), PerpAtoms::ZERO));
        let close = b.full_close(&crowded, taker_liquidation()).unwrap();
        assert_eq!(close.refusal(), Some(CloseRefusal::LongUtilizationExceeded));
    }

    /// Inputs no single block produces are an error, not a close: a market
    /// read at another block, or a band backing more than its market.
    #[test]
    fn inputs_from_no_one_block_are_an_error() {
        let b = healthy_maker_at_the_mark();
        let mut later = open(&b, 0);
        later.block.number += 1;
        assert!(matches!(
            b.full_close(&later, taker_liquidation()),
            Err(ValidationError::BlockMismatch { .. })
        ));
        let mut thin = open(&b, 0);
        thin.capacity.long = PerpAtoms::ZERO;
        assert!(matches!(
            b.full_close(&thin, taker_liquidation()),
            Err(ValidationError::Overflow { .. })
        ));
    }

    /// V4 removes a band's principal rounded down, choosing its legs by the
    /// pool's tick; the amounts `valPnl` reads clamp the price to the band
    /// instead. They agree wherever the pool can stand, the bounds included
    /// with the tick on either side of them.
    #[test]
    fn the_principal_is_what_v4_removes() {
        let range = TickRange::new(28_200, 28_800).unwrap();
        let liquidity = LUnits::new(570_282_387);
        let at = |sqrt: SqrtPrice, tick: i32| {
            let (perp, usd) = amounts_for_liquidity(
                sqrt,
                get_sqrt_ratio_at_tick(range.lower()).unwrap(),
                get_sqrt_ratio_at_tick(range.upper()).unwrap(),
                liquidity,
            )
            .unwrap();
            let removed =
                liquidity_change_delta(sqrt, tick, &range, LDelta::new(-570_282_387)).unwrap();
            assert_eq!(
                (PerpDelta::from(perp), UsdcDelta::from(usd)),
                removed,
                "tick {tick}"
            );
        };
        for tick in [28_000, 28_543, 29_000] {
            at(get_sqrt_ratio_at_tick(tick).unwrap(), tick);
        }
        for bound in [range.lower(), range.upper()] {
            let sqrt = get_sqrt_ratio_at_tick(bound).unwrap();
            at(sqrt, bound);
            at(sqrt, bound - 1);
        }
    }
}
