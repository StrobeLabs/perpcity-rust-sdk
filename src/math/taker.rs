//! Taker health, exact: the port of the deployed `PerpLogic.liquidateTaker`
//! eligibility test, so a position's standing can be known from a block's
//! state without an `eth_call`, and the distance to its liquidation can be
//! read off the same arithmetic.
//!
//! ```text
//! (val, pnl)     = valPnl(delta, mark)            perpVal = ⌊amount0 · mark / Q96⌉₀; val = |perpVal|; pnl = perpVal + amount1
//! accrued        = takerFeesAccrued(cumls, taker, pos)
//!                  funding = ⌈amount0 · (cumls.funding − pos.lastCumlFunding) / Q96⌉
//!                  util    = ⌈|amount0| · (cumls.utilPayments[side] − taker.lastUtilPayments[side]) / Q96⌉
//! settledMargin  = margin − accrued
//! liquidatable   = !isHealthy(settledMargin + pnl, val, pos.liqMarginRatio)
//! isHealthy(e, v, r) = (e <= 0 ? 0 : ⌊e · E6 / (v + 1)⌋) >= r
//! ```
//!
//! This is the rule of the deployed contracts, `v0.2.2-upgradeable`
//! (`198559ae`): no fee enters eligibility, and the ratio is the one stored
//! on the position at open. The contracts repository's main branch has
//! since changed both.

use alloy::primitives::{I256, U256};
use serde::{Deserialize, Serialize};

use crate::constants::Q96;
use crate::errors::ValidationError;
use crate::math::{BlockContext, LiquidationPrices, is_healthy};
use crate::units::fixed_point::{Rounding, add_i, mul_div, s_full_mul_div, to_i256};
use crate::units::{
    BIGINT_1E6, Earnings, Funding, PerSide, PerpDelta, Price, Ratio, Side, UsdcAtoms, UsdcDelta,
};

/// Block-pinned market-wide inputs shared by every taker's computation.
/// Fields named after the contract's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakerMarketSnapshot {
    /// Block containing all state in this snapshot.
    pub block: BlockContext,
    /// `cumulatives().fundingX96`.
    pub funding: Funding,
    /// `cumulatives().longUtilPaymentsX96` and `shortUtilPaymentsX96`: what
    /// a unit of taker exposure on each side has paid in utilization.
    pub util_payments: PerSide<Earnings>,
    /// The price `valPnl` is computed at: the contract's own mark for the
    /// block, the deployed fair price of the pool price, beacon index and
    /// block-advanced EMAs, as `PerpLogic.accrue` sets `markPrice`.
    pub mark: Price,
}

/// One taker's row as the contract holds it. Fields named after the
/// contract's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakerState {
    /// `positions(id).delta.amount0`: the perp exposure, long positive.
    pub delta_perp: PerpDelta,
    /// `positions(id).delta.amount1`: the USD leg, what the position paid
    /// or received for its exposure.
    pub delta_usd: UsdcDelta,
    /// `positions(id).margin`: last-settled margin.
    pub margin: UsdcAtoms,
    /// `positions(id).liqMarginRatio`: the ratio stored at open, which the
    /// health test compares this position against.
    pub liquidation_margin_ratio: Ratio,
    /// `positions(id).lastCumlFundingX96`: the funding checkpoint.
    pub last_cuml_funding: Funding,
    /// `takerDetails(id).lastLongUtilPaymentsX96` and `lastShort…`: the
    /// utilization checkpoints.
    pub last_util_payments: PerSide<Earnings>,
}

/// A taker's health at a block: every leg of the contract's eligibility
/// test in exact atoms, and the mark the test would turn at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakerHealth {
    /// The block the inputs were read at.
    pub block: BlockContext,
    /// The mark the position was valued at.
    pub mark: Price,
    delta_perp: PerpDelta,
    delta_usd: UsdcDelta,
    margin: UsdcAtoms,
    funding_owed: UsdcDelta,
    util_owed: UsdcAtoms,
    position_value: UsdcAtoms,
    unrealized_pnl: UsdcDelta,
    liquidation_margin_ratio: Ratio,
}

impl TakerMarketSnapshot {
    /// The health of `taker` at this snapshot.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when a checkpoint is ahead of the
    /// market's cumulative, which no block produces, or a leg leaves the
    /// range its type can hold.
    pub fn taker_health(&self, taker: &TakerState) -> Result<TakerHealth, ValidationError> {
        let perp = I256::try_from(taker.delta_perp.atoms()).expect("i128 fits i256");
        let mark = to_i256(self.mark.x96(), "mark")?;

        // valPnl(delta, markP)
        let perp_val = s_full_mul_div(perp, mark, Q96, Rounding::TowardZero)?;
        let position_value = usdc_atoms(perp_val.unsigned_abs(), "position value")?;
        let unrealized_pnl = usdc_delta(
            add_i(
                perp_val,
                I256::try_from(taker.delta_usd.atoms()).expect("i128 fits i256"),
                "unrealized pnl",
            )?,
            "unrealized pnl",
        )?;

        // takerFeesAccrued(cumls, taker, pos)
        let funding_owed = usdc_delta(
            s_full_mul_div(
                perp,
                self.funding
                    .since(
                        taker.last_cuml_funding,
                        "funding checkpoint ahead of the market",
                    )?
                    .x96(),
                Q96,
                Rounding::Up,
            )?,
            "funding owed",
        )?;
        let util_owed = match taker.delta_perp.atoms() {
            0 => UsdcAtoms::ZERO,
            atoms => {
                let side = if atoms > 0 { Side::Long } else { Side::Short };
                let growth = self
                    .util_payments
                    .on(side)
                    .since(
                        *taker.last_util_payments.on(side),
                        "utilization checkpoint ahead of the market",
                    )?
                    .x96();
                usdc_atoms(
                    mul_div(U256::from(atoms.unsigned_abs()), growth, Q96, Rounding::Up)?,
                    "utilization owed",
                )?
            }
        };

        Ok(TakerHealth {
            block: self.block,
            mark: self.mark,
            delta_perp: taker.delta_perp,
            delta_usd: taker.delta_usd,
            margin: taker.margin,
            funding_owed,
            util_owed,
            position_value,
            unrealized_pnl,
            liquidation_margin_ratio: taker.liquidation_margin_ratio,
        })
    }
}

impl TakerHealth {
    /// The perp exposure, long positive.
    pub fn delta_perp(&self) -> PerpDelta {
        self.delta_perp
    }

    /// The USD leg of the position.
    pub fn delta_usd(&self) -> UsdcDelta {
        self.delta_usd
    }

    /// `positions(id).margin`, as last settled.
    pub fn margin(&self) -> UsdcAtoms {
        self.margin
    }

    /// Funding accrued since the position's checkpoint, positive when the
    /// position owes it.
    pub fn funding_owed(&self) -> UsdcDelta {
        self.funding_owed
    }

    /// Utilization fees accrued since the position's checkpoint.
    pub fn util_owed(&self) -> UsdcAtoms {
        self.util_owed
    }

    /// `valPnl`'s value: the exposure at the mark.
    pub fn position_value(&self) -> UsdcAtoms {
        self.position_value
    }

    /// `valPnl`'s PnL: the exposure at the mark plus the USD leg.
    pub fn unrealized_pnl(&self) -> UsdcDelta {
        self.unrealized_pnl
    }

    /// The ratio the health test compares against.
    pub fn liquidation_margin_ratio(&self) -> Ratio {
        self.liquidation_margin_ratio
    }

    /// Margin less what has accrued against it: the contract's
    /// `settledMargin`.
    pub fn settled_margin(&self) -> UsdcDelta {
        UsdcDelta::from(self.margin) - self.funding_owed - UsdcDelta::from(self.util_owed)
    }

    /// Settled margin plus unrealized PnL: what the position is worth to
    /// its holder at the mark.
    pub fn equity(&self) -> UsdcDelta {
        self.settled_margin() + self.unrealized_pnl
    }

    /// Whether the contract would liquidate the position now: the negation
    /// of `isHealthy(equity, posVal, liqMarginRatio)`, in the contract's
    /// integers. The oracle `simulate_liquidate_taker` agrees with this at
    /// the same block.
    pub fn is_liquidatable(&self) -> bool {
        !is_healthy(
            self.equity(),
            self.position_value,
            self.liquidation_margin_ratio,
        )
    }

    /// The position's margin ratio as `isHealthy` computes it, equity over
    /// value plus one atom, zero when equity is not positive. Lossy: a
    /// dashboard figure, not the test.
    pub fn margin_ratio(&self) -> f64 {
        let equity = self.equity().atoms();
        if equity <= 0 {
            return 0.0;
        }
        equity as f64 / (self.position_value.atoms() as f64 + 1.0)
    }

    /// Where the test turns as the mark moves, in the shape a maker's search
    /// answers in: a long is liquidated from below and a short from above,
    /// so one side is the turn and the other `None`. Both are `None` for a
    /// long whose settled margin covers its USD leg, which no fall of the
    /// mark can liquidate, and for a position with no exposure; both are
    /// the mark when [`Self::is_liquidatable`] says so now, and only then.
    /// The turn is the first mark the integer test fails, found from the
    /// closed form by a gallop and a bisection to the atom.
    pub fn liquidation_prices(&self) -> LiquidationPrices {
        let (below, above) = if self.is_liquidatable() {
            (Some(self.mark), Some(self.mark))
        } else {
            let up = self.delta_perp.atoms() < 0;
            match self
                .liquidation_mark()
                .and_then(|turn| self.first_failing(turn, up))
            {
                Some(turn) if up => (None, Some(turn)),
                Some(turn) => (Some(turn), None),
                None => (None, None),
            }
        };
        LiquidationPrices {
            mark: self.mark,
            below,
            above,
        }
    }

    /// The closed form behind [`Self::liquidation_prices`]: the mark at
    /// which equity over value meets the position's ratio, with the
    /// exposure and the margin as they stand.
    fn liquidation_mark(&self) -> Option<Price> {
        let perp = self.delta_perp.atoms();
        if perp == 0 {
            return None;
        }
        // The part of equity that does not move with the mark.
        let fixed = self.settled_margin().atoms() + self.delta_usd.atoms();
        let ratio = U256::from(self.liquidation_margin_ratio.e6());
        let size = U256::from(perp.unsigned_abs());
        // Long: liquidatable when fixed + a·m < r·a·m, so when m < −fixed / (a(1 − r)).
        // Short: liquidatable when fixed − |a|·m < r·|a|·m, so when m > fixed / (|a|(1 + r)).
        let (numerator, scale) = if perp > 0 {
            if fixed >= 0 {
                return None;
            }
            // A ratio of 100% or more has no turn: such a long fails the test
            // at every mark.
            (
                U256::from(fixed.unsigned_abs()),
                BIGINT_1E6.checked_sub(ratio)?,
            )
        } else {
            if fixed <= 0 {
                return Some(Price::from_x96(U256::ZERO));
            }
            (U256::from(fixed.unsigned_abs()), BIGINT_1E6 + ratio)
        };
        // m · Q96 = numerator · E6 · Q96 / (size · scale)
        let mark_x96 = mul_div(
            numerator * BIGINT_1E6,
            Q96,
            size * scale,
            Rounding::TowardZero,
        )
        .ok()?;
        Some(Price::from_x96(mark_x96))
    }

    /// The first mark past `turn`, moving away from a healthy mark in the
    /// direction `up` names, at which the integer test fails: a gallop from
    /// `turn` to bracket it, then a bisection. Equity over value is monotone
    /// in the mark, so the bracket holds. `None` when the price range's end
    /// is reached first, or a mark's value leaves the range atoms can hold.
    fn first_failing(&self, turn: Price, up: bool) -> Option<Price> {
        let mark = self.mark.x96();
        let away = |x: U256, width: U256| {
            if up {
                x.saturating_add(width)
            } else {
                x.saturating_sub(width)
            }
        };
        let toward = |x: U256, width: U256| {
            if up {
                x.saturating_sub(width).max(mark)
            } else {
                x.saturating_add(width).min(mark)
            }
        };
        let fails = |x: U256| self.is_liquidatable_at(Price::from_x96(x));

        let start = if up {
            turn.x96().max(mark.saturating_add(U256::from(1u8)))
        } else {
            turn.x96().min(mark.saturating_sub(U256::from(1u8)))
        };
        let mut width = U256::from(1u8);
        let (mut healthy, mut failing) = if fails(start)? {
            let mut failing = start;
            loop {
                let next = toward(start, width);
                if !fails(next)? {
                    break (next, failing);
                }
                failing = next;
                width <<= 1;
            }
        } else {
            let mut healthy = start;
            loop {
                let next = away(start, width);
                if next == healthy {
                    return None;
                }
                if fails(next)? {
                    break (healthy, next);
                }
                healthy = next;
                width <<= 1;
            }
        };
        while healthy.abs_diff(failing) > U256::from(1u8) {
            let mid = healthy.min(failing) + healthy.abs_diff(failing) / U256::from(2u8);
            if fails(mid)? {
                failing = mid;
            } else {
                healthy = mid;
            }
        }
        Some(Price::from_x96(failing))
    }

    /// [`Self::is_liquidatable`] with the mark moved and everything else as
    /// it stands; `None` where the value leaves the range atoms can hold.
    fn is_liquidatable_at(&self, mark: Price) -> Option<bool> {
        let perp = I256::try_from(self.delta_perp.atoms()).ok()?;
        let perp_val = s_full_mul_div(
            perp,
            to_i256(mark.x96(), "mark").ok()?,
            Q96,
            Rounding::TowardZero,
        )
        .ok()?;
        let fixed = I256::try_from(self.settled_margin().atoms() + self.delta_usd.atoms()).ok()?;
        let equity = usdc_delta(add_i(fixed, perp_val, "equity").ok()?, "equity").ok()?;
        let value = usdc_atoms(perp_val.unsigned_abs(), "position value").ok()?;
        Some(!is_healthy(equity, value, self.liquidation_margin_ratio))
    }
}

fn usdc_atoms(value: U256, context: &'static str) -> Result<UsdcAtoms, ValidationError> {
    u128::try_from(value)
        .map(UsdcAtoms::new)
        .map_err(|_| ValidationError::Overflow {
            context: context.into(),
        })
}

fn usdc_delta(value: I256, context: &'static str) -> Result<UsdcDelta, ValidationError> {
    i128::try_from(value)
        .map(UsdcDelta::new)
        .map_err(|_| ValidationError::Overflow {
            context: context.into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x96_to_f64(x96: U256) -> f64 {
        f64::from(x96) / 2f64.powi(96)
    }

    fn q96(units: u64) -> U256 {
        Q96 * U256::from(units)
    }

    fn price(units: u64) -> Price {
        Price::from_x96(q96(units))
    }

    fn ratio(e6: u32) -> Ratio {
        Ratio::from_e6(e6).unwrap()
    }

    /// A market whose cumulatives have grown since the position's
    /// checkpoints: funding by 0.5 USDC per perp, long utilization by 0.25.
    fn market(mark: u64) -> TakerMarketSnapshot {
        TakerMarketSnapshot {
            block: BlockContext::default(),
            funding: Funding::from_x96(I256::try_from(Q96 / U256::from(2u8)).unwrap()),
            util_payments: PerSide::new(
                Earnings::from_x96(Q96 / U256::from(4u8)),
                Earnings::from_x96(U256::ZERO),
            ),
            mark: price(mark),
        }
    }

    /// Long 10 perps bought at 40 with 100 USDC of margin, a 5% ratio.
    fn long() -> TakerState {
        TakerState {
            delta_perp: PerpDelta::new(10_000_000),
            delta_usd: UsdcDelta::new(-400_000_000),
            margin: UsdcAtoms::new(100_000_000),
            liquidation_margin_ratio: ratio(50_000),
            last_cuml_funding: Funding::from_x96(I256::ZERO),
            last_util_payments: PerSide::new(
                Earnings::from_x96(U256::ZERO),
                Earnings::from_x96(U256::ZERO),
            ),
        }
    }

    #[test]
    fn every_leg_is_the_contracts_arithmetic() {
        let health = market(42).taker_health(&long()).unwrap();
        assert_eq!(health.position_value(), UsdcAtoms::new(420_000_000));
        assert_eq!(health.unrealized_pnl(), UsdcDelta::new(20_000_000));
        assert_eq!(
            health.funding_owed(),
            UsdcDelta::new(5_000_000),
            "10 perps × 0.5"
        );
        assert_eq!(
            health.util_owed(),
            UsdcAtoms::new(2_500_000),
            "10 perps × 0.25"
        );
        assert_eq!(health.settled_margin(), UsdcDelta::new(92_500_000));
        assert_eq!(health.equity(), UsdcDelta::new(112_500_000));
        assert!(!health.is_liquidatable());
        assert!((health.margin_ratio() - 112.5 / 420.0).abs() < 1e-9);
    }

    #[test]
    fn the_health_test_turns_where_the_contract_says() {
        // Equity at mark m: 92.5 + 10m − 400 = 10m − 307.5; value 10m.
        // Healthy iff (10m − 307.5) / 10m ≥ 5%, so m ≥ 32.368…
        let liquidating = market(42)
            .taker_health(&long())
            .unwrap()
            .liquidation_mark()
            .unwrap();
        let expected = 307.5 / (10.0 * 0.95);
        assert!((x96_to_f64(liquidating.x96()) - expected).abs() < 1e-6);

        assert!(!market(33).taker_health(&long()).unwrap().is_liquidatable());
        assert!(market(32).taker_health(&long()).unwrap().is_liquidatable());
        // The integer test turns within a hair of the closed form: one part
        // in ten thousand either side of the mark decides it.
        let at = |scale_e4: u64| {
            TakerMarketSnapshot {
                mark: Price::from_x96(
                    liquidating.x96() * U256::from(scale_e4) / U256::from(10_000u64),
                ),
                ..market(42)
            }
            .taker_health(&long())
            .unwrap()
        };
        assert!(at(9_999).is_liquidatable());
        assert!(!at(10_001).is_liquidatable());
        let at_x96 = |mark: U256| {
            TakerMarketSnapshot {
                mark: Price::from_x96(mark),
                ..market(42)
            }
            .taker_health(&long())
            .unwrap()
        };

        // A long turns from below only, at the highest mark the test fails,
        // and its distance is the mark's from there; once liquidatable both
        // sides are the mark.
        let healthy = market(42).taker_health(&long()).unwrap();
        let prices = healthy.liquidation_prices();
        assert_eq!(prices.mark, healthy.mark);
        assert_eq!(prices.above, None);
        let below = prices.below.unwrap().x96();
        assert!(at_x96(below).is_liquidatable());
        assert!(!at_x96(below + U256::from(1u8)).is_liquidatable());
        assert!((prices.distance().unwrap() - (42.0 - expected) / 42.0).abs() < 1e-6);
        let under = market(30).taker_health(&long()).unwrap();
        let prices = under.liquidation_prices();
        assert_eq!(
            (prices.below, prices.above),
            (Some(under.mark), Some(under.mark))
        );
        assert_eq!(prices.distance(), Some(0.0));
    }

    #[test]
    fn a_short_is_liquidated_from_above_and_a_covered_long_never_by_price() {
        // Short 10 perps sold at 40 with 100 of margin: equity = 92.5 + 400 − 10m.
        // Healthy iff (492.5 − 10m) / 10m ≥ 5%, so m ≤ 46.904…
        let short = TakerState {
            delta_perp: PerpDelta::new(-10_000_000),
            delta_usd: UsdcDelta::new(400_000_000),
            ..long()
        };
        let market_short = |mark: u64| TakerMarketSnapshot {
            util_payments: PerSide::new(
                Earnings::from_x96(U256::ZERO),
                Earnings::from_x96(Q96 / U256::from(4u8)),
            ),
            ..market(mark)
        };
        let health = market_short(42).taker_health(&short).unwrap();
        assert_eq!(
            health.funding_owed(),
            UsdcDelta::new(-5_000_000),
            "a short receives"
        );
        assert_eq!(
            health.util_owed(),
            UsdcAtoms::new(2_500_000),
            "the short side's growth"
        );
        let expected = (102.5 + 400.0) / (10.0 * 1.05);
        assert!((x96_to_f64(health.liquidation_mark().unwrap().x96()) - expected).abs() < 1e-6);
        let prices = health.liquidation_prices();
        assert_eq!(prices.below, None, "a fall never liquidates a short");
        assert!((x96_to_f64(prices.above.unwrap().x96()) - expected).abs() < 1e-6);
        assert!(
            !market_short(46)
                .taker_health(&short)
                .unwrap()
                .is_liquidatable()
        );
        assert!(
            market_short(48)
                .taker_health(&short)
                .unwrap()
                .is_liquidatable()
        );

        // A long whose margin covers what it paid: equity never falls to
        // the ratio however far the mark falls.
        let covered = TakerState {
            margin: UsdcAtoms::new(500_000_000),
            ..long()
        };
        let health = market(1).taker_health(&covered).unwrap();
        assert_eq!(health.liquidation_mark(), None);
        let prices = health.liquidation_prices();
        assert_eq!((prices.below, prices.above), (None, None));
        assert_eq!(prices.distance(), None);
        assert!(!health.is_liquidatable());

        // No exposure: nothing to liquidate at any mark.
        let flat = TakerState {
            delta_perp: PerpDelta::ZERO,
            ..long()
        };
        assert_eq!(
            market(42).taker_health(&flat).unwrap().liquidation_mark(),
            None
        );
    }

    /// Both sides at the mark means liquidatable now, and only that. The
    /// floor on a short's value can leave the exact test calling it healthy
    /// at its closed-form turn; the reported turn is then the first mark
    /// above it that the test fails.
    #[test]
    fn a_healthy_short_at_its_turn_keeps_a_distance() {
        let short = TakerState {
            delta_perp: PerpDelta::new(-10_000_000),
            delta_usd: UsdcDelta::new(400_000_000),
            ..long()
        };
        let at = |mark: Price| {
            TakerMarketSnapshot {
                util_payments: PerSide::new(
                    Earnings::from_x96(U256::ZERO),
                    Earnings::from_x96(Q96 / U256::from(4u8)),
                ),
                mark,
                ..market(42)
            }
            .taker_health(&short)
            .unwrap()
        };
        let turn = at(price(42)).liquidation_mark().unwrap();
        let health = at(turn);
        assert!(
            !health.is_liquidatable(),
            "the floors leave it healthy at the closed form"
        );
        let prices = health.liquidation_prices();
        assert_eq!(prices.below, None);
        let above = prices.above.unwrap();
        assert!(above > prices.mark, "{prices:?}");
        assert!(at(above).is_liquidatable());
        assert!(!at(Price::from_x96(above.x96() - U256::from(1u8))).is_liquidatable());
        assert!(prices.distance().unwrap() > 0.0);
    }

    #[test]
    fn non_positive_equity_is_liquidatable_and_a_checkpoint_ahead_is_refused() {
        let underwater = TakerState {
            margin: UsdcAtoms::new(1_000_000),
            ..long()
        };
        let health = market(30).taker_health(&underwater).unwrap();
        assert!(health.equity().is_negative());
        assert!(health.is_liquidatable());
        assert_eq!(health.margin_ratio(), 0.0);

        // Funding may run either way, so a checkpoint above the cumulative is
        // a receipt, not an error; utilization only ever grows, so a
        // checkpoint ahead of it is a state no block produced.
        let received = TakerState {
            last_cuml_funding: Funding::from_x96(I256::try_from(Q96).unwrap()),
            ..long()
        };
        assert_eq!(
            market(42).taker_health(&received).unwrap().funding_owed(),
            UsdcDelta::new(-5_000_000)
        );
        let ahead = TakerState {
            last_util_payments: PerSide::new(
                Earnings::from_x96(Q96),
                Earnings::from_x96(U256::ZERO),
            ),
            ..long()
        };
        assert!(matches!(
            market(42).taker_health(&ahead),
            Err(ValidationError::Overflow { .. })
        ));
    }
}
