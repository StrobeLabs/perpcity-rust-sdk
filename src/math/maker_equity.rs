//! Live maker equity: what the contract would settle for an OPEN maker
//! position if it were touched now.
//!
//! Ports the deployed-era `PerpLogic` maker settlement math (commit
//! `83d90aea` of perpcity-contracts — the fee/funding math is unchanged
//! through the deployed window `#165..#168`). For an open maker position the
//! math produces exactly what the contract would settle on a touch:
//!
//! - accrued range funding (`makerCumlFunding` + `makerFeesAccrued`),
//! - utilization earnings (capacity × earnings-checkpoint delta),
//! - uncollected Uniswap V4 LP fees (donated taker fees, from the
//!   PoolManager's fee-growth accounting),
//! - inventory PnL (`valPnl`) and the resulting equity.
//!
//! All arithmetic is X96/X128 integer math (`U256`/`I256`), transcribed 1:1
//! from the Solidity. The resulting [`MakerEquityBreakdown`] carries exact
//! signed 6-decimal USDC atoms — the units the contract settles in — and
//! converts to f64 USD only in its accessors. The
//! computation is validated end-to-end against a real on-chain settle: the
//! golden test reproduces the `MakerConverted` event of the CHINA-PC pos-54
//! liquidation from pre-liquidation chain state.
//!
//! This module is pure math over pre-fetched inputs, mirroring
//! [`swap`](crate::math::swap): a block-pinned [`MakerMarketSnapshot`] plus
//! per-position [`MakerState`] rows. The chain-read layer that populates
//! them (including the raw storage-slot reads, see
//! `crate::storage`) lives on the state handle:
//! [`StateAt::maker_equities`](crate::StateAt::maker_equities).

use alloy::primitives::{I256, U256, U512};
use serde::{Deserialize, Serialize};

use crate::constants::{ACCOUNTING_TOKEN_SUPPLY, INTERVAL, Q96, WAD};
use crate::errors::ValidationError;
use crate::math::BlockContext;
use crate::math::liquidity::amounts_for_liquidity;
use crate::math::swap::amount0_delta;
use crate::math::tick::get_sqrt_ratio_at_tick;
use crate::units::fixed_point::{
    Rounding, add_i, add_u, mul_div, s_full_mul_div, sub_i, to_i256, u512_to_u256,
};
use crate::units::{
    Earnings, FeeGrowth, Funding, FundingPerSqrtPrice, FundingRate, LUnits, PerSide, PerpAtoms,
    PerpDelta, Price, Ratio, Side, SqrtPrice, UsdcAtoms, UsdcDelta, UtilizationRate,
};

/// One `TickInfo` from the Perp's tick funding mapping (`s.ticks[tick]`),
/// fields named after the contract's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickFunding {
    /// `TickInfo.cumlFundingOppX96`: cumulative funding checkpointed on the
    /// opposite side of the tick.
    pub cuml_funding_opp: Funding,
    /// `TickInfo.cumlFundingDivSqrtPOppX96`: cumulative funding divided by
    /// sqrt price, checkpointed on the opposite side of the tick.
    pub cuml_funding_div_sqrt_p_opp: FundingPerSqrtPrice,
}

/// Block-pinned market-wide inputs shared by every position's computation.
///
/// The funding/earnings cumulatives are stored on chain as of the market's
/// last touch; call [`Self::accrued`] to replay them to the snapshot's
/// timestamp before computing equities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MakerMarketSnapshot {
    /// Block containing all state in this snapshot.
    pub block: BlockContext,
    /// Cumulative funding.
    pub funding: Funding,
    /// Cumulative funding divided by sqrt price.
    pub funding_div_sqrt_p: FundingPerSqrtPrice,
    /// Cumulative utilization earnings, per side.
    pub util_earnings: PerSide<Earnings>,
    /// Current pool tick.
    pub tick: i32,
    /// The pool's current price.
    pub sqrt_price: SqrtPrice,
    /// The price `valPnl` and the accrual replay's utilization leg are
    /// computed at. The client loads the contract's own mark for the block:
    /// the deployed fair price
    /// ([`crate::math::pricing::fair_price`]) of the pool price, beacon
    /// index, and block-advanced EMAs, as `PerpLogic.accrue` sets
    /// `markPrice`.
    pub mark: Price,
    /// The fees module's `liqFee` at the block: the rate the health test
    /// deducts from equity, as a share of the band's value, before it
    /// compares against the position's ratio.
    pub liquidation_fee: Ratio,
}

/// Raw rates + accrual context for replaying `accrue()` from `lastTouch` to
/// `now`: the on-chain cumulatives are only current as of the last touch.
/// Fields named after the contract's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccrualInputs {
    /// `rates().fundingPerDay`: the daily funding rate, positive when longs
    /// pay shorts.
    pub funding_per_day: FundingRate,
    /// `rates().longUtilFeePerDay` and `shortUtilFeePerDay`: the daily
    /// utilization fee rate, per side.
    pub util_fee_per_day: PerSide<UtilizationRate>,
    /// `rates().lastTouch`: timestamp the cumulatives were last advanced.
    pub last_touch: u64,
    /// Timestamp to accrue to — the snapshot block's timestamp, never a
    /// wall clock (a local clock ahead of the chain fabricates accrual;
    /// one behind erases it).
    pub accrue_to: u64,
    /// `openInterest()`.
    pub open_interest: PerSide<PerpAtoms>,
    /// `capacity()`.
    pub capacity: PerSide<PerpAtoms>,
}

/// Per-position inputs: the position row, maker row, its band's tick funding
/// checkpoints, and the V4 fee-growth state for its liquidity position.
/// Fields named after the contract's.
///
/// `tick_lower < tick_upper` is required; [`AccruedMakerSnapshot::maker_equity`]
/// validates the ordering (and the Uniswap tick domain) before computing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MakerState {
    /// `positions(id).margin`: last-settled margin.
    pub margin: UsdcAtoms,
    /// `positions(id).liqMarginRatio`: the liquidation margin ratio stored
    /// on the position. The contract's health check compares the position's
    /// equity/value ratio against this, not against the market-wide module
    /// value.
    pub liquidation_margin_ratio: Ratio,
    /// `positions(id).delta` amount0, unpacked from the packed
    /// `BalanceDelta`. Negative = owed to the pool.
    pub delta_perp: PerpDelta,
    /// `positions(id).delta` amount1, unpacked from the packed
    /// `BalanceDelta`. Negative = owed to the pool.
    pub delta_usd: UsdcDelta,
    /// `positions(id).lastCumlFundingX96`: market funding cumulative at the
    /// position's last settle.
    pub last_cuml_funding: Funding,
    /// `makerDetails(id).tickLower`: band lower tick.
    pub tick_lower: i32,
    /// `makerDetails(id).tickUpper`: band upper tick.
    pub tick_upper: i32,
    /// `makerDetails(id).liquidity`: V4 liquidity in the band.
    pub liquidity: LUnits,
    /// `makerDetails(id).lastLongUtilEarningsX96` and
    /// `lastShortUtilEarningsX96`: the utilization earnings cumulatives at
    /// the last settle, per side.
    pub last_util_earnings: PerSide<Earnings>,
    /// `makerDetails(id).capacity`.
    pub capacity: PerSide<PerpAtoms>,
    /// `makerDetails(id).lastCumlFunding.belowX96`: below-band funding
    /// cumulative at the last settle.
    pub last_below: Funding,
    /// `makerDetails(id).lastCumlFunding.withinX96`: within-band funding
    /// cumulative at the last settle.
    pub last_within: Funding,
    /// `makerDetails(id).lastCumlFunding.divSqrtPriceWithinX96`:
    /// within-band funding/sqrtP cumulative at the last settle.
    pub last_div_sqrt_within: FundingPerSqrtPrice,
    /// `ticks[tickLower]`: the lower tick's live funding checkpoints.
    pub tick_lower_funding: TickFunding,
    /// `ticks[tickUpper]`: the upper tick's live funding checkpoints.
    pub tick_upper_funding: TickFunding,
    /// V4 `feeGrowthInside1X128` of the band now.
    pub fee_growth_inside1: FeeGrowth,
    /// V4 `feeGrowthInside1LastX128` at the position's last checkpoint.
    pub fee_growth_inside1_last: FeeGrowth,
}

/// The legs of a maker's equity that move with the price: the band, and
/// the deposit residual the contract recorded against it. Kept on the
/// breakdown so a preview can be revalued along a price shock without the
/// market it was computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Exposure {
    sqrt_lower: SqrtPrice,
    sqrt_upper: SqrtPrice,
    liquidity: LUnits,
    delta_perp: PerpDelta,
    delta_usd: UsdcDelta,
}

impl Exposure {
    /// `valPnl` at a pool price and a mark: the band's value, and the
    /// unrealized PnL as that value plus the deposit residual, both the
    /// signed sums the contract computes.
    fn valued(&self, sqrt_price: SqrtPrice, mark: Price) -> Result<(U256, I256), ValidationError> {
        let (perps, usd) =
            amounts_for_liquidity(sqrt_price, self.sqrt_lower, self.sqrt_upper, self.liquidity)?;
        let liquidity_val = add_u(
            mul_div(
                U256::from(perps.atoms()),
                mark.x96(),
                Q96,
                Rounding::TowardZero,
            )?,
            U256::from(usd.atoms()),
            "band liquidity value",
        )?;
        let residual_val = add_i(
            s_full_mul_div(
                I256::unchecked_from(self.delta_perp.atoms()),
                to_i256(mark.x96(), "mark price")?,
                Q96,
                Rounding::TowardZero,
            )?,
            I256::unchecked_from(self.delta_usd.atoms()),
            "deposit residual value",
        )?;
        let unrealized = add_i(
            to_i256(liquidity_val, "band liquidity value")?,
            residual_val,
            "unrealized PnL",
        )?;
        Ok((liquidity_val, unrealized))
    }
}

/// Where the health test turns along a price shock, on each side of the
/// mark it was searched from.
///
/// The shock moves the pool price and the mark by one factor from where
/// each stood, so the gap between them carries through it. A maker's equity
/// along the shock is the LP curve, concave, so it can cross the test's
/// line below the mark, above it, both, or neither; a side is `None` when
/// the position stays healthy as far as the search looks, a hundredfold of
/// the mark either way. A position liquidatable now has both sides at the
/// mark. The deployed mark follows the pool only in part, so the shock
/// moves the mark faster than a trade would.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiquidationPrices {
    /// The first mark below the current one at which the position is
    /// liquidatable.
    pub below: Option<Price>,
    /// The first mark above the current one at which the position is
    /// liquidatable.
    pub above: Option<Price>,
}

impl LiquidationPrices {
    /// The nearer side's distance from `mark`, as a fraction of it: a
    /// monitor's bound. `None` when neither side turns; zero when the
    /// position is liquidatable at the mark.
    pub fn distance_from(&self, mark: Price) -> Option<f64> {
        let relative = |price: Price| (f64::from(price.x96()) / f64::from(mark.x96()) - 1.0).abs();
        match (self.below, self.above) {
            (None, None) => None,
            (Some(below), None) => Some(relative(below)),
            (None, Some(above)) => Some(relative(above)),
            (Some(below), Some(above)) => Some(relative(below).min(relative(above))),
        }
    }
}

/// What the contract would settle if the position were touched now.
///
/// Every component is exact USDC, the units the contract settles in; the
/// `usdc()` on each is the `f64` a display wants.
///
/// Every component is bounded to ±[`MAX_COMPONENT_ATOMS`] (the protocol's
/// accounting-token supply) at construction — including deserialization,
/// which rejects out-of-bound values — so the derived sums
/// ([`Self::settled_margin`], [`Self::equity`],
/// [`Self::accrued_income`]) can never overflow `i128`.
///
/// The preview also carries the mark and the pool price it was priced at
/// and the legs that move with them, so it can be revalued along a price
/// shock with nothing else in hand: [`Self::equity_at`],
/// [`Self::is_liquidatable_at`] and [`Self::liquidation_prices`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawMakerEquityBreakdown")]
pub struct MakerEquityBreakdown {
    margin: UsdcDelta,
    funding_owed: UsdcDelta,
    util_earnings: PerSide<UsdcDelta>,
    lp_fees: UsdcDelta,
    unrealized_pnl: UsdcDelta,
    position_value: UsdcAtoms,
    liquidation_margin_ratio: Ratio,
    liquidation_fee: Ratio,
    mark: Price,
    sqrt_price: SqrtPrice,
    exposure: Exposure,
}

/// Deserialization shadow of [`MakerEquityBreakdown`]: identical fields,
/// no invariant. Values enter the real type only through the validating
/// `TryFrom`.
#[derive(Deserialize)]
struct RawMakerEquityBreakdown {
    margin: UsdcDelta,
    funding_owed: UsdcDelta,
    util_earnings: PerSide<UsdcDelta>,
    lp_fees: UsdcDelta,
    unrealized_pnl: UsdcDelta,
    position_value: UsdcAtoms,
    liquidation_margin_ratio: Ratio,
    liquidation_fee: Ratio,
    mark: Price,
    sqrt_price: SqrtPrice,
    exposure: Exposure,
}

impl TryFrom<RawMakerEquityBreakdown> for MakerEquityBreakdown {
    type Error = ValidationError;

    fn try_from(raw: RawMakerEquityBreakdown) -> Result<Self, ValidationError> {
        let bounded = |v: UsdcDelta, context: &'static str| {
            if v.magnitude().atoms() <= MAX_COMPONENT_ATOMS {
                Ok(v)
            } else {
                Err(ValidationError::Overflow {
                    context: context.into(),
                })
            }
        };
        Ok(Self {
            margin: bounded(raw.margin, "deserialized margin")?,
            funding_owed: bounded(raw.funding_owed, "deserialized funding")?,
            util_earnings: PerSide::new(
                bounded(
                    raw.util_earnings.long,
                    "deserialized long utilization earnings",
                )?,
                bounded(
                    raw.util_earnings.short,
                    "deserialized short utilization earnings",
                )?,
            ),
            lp_fees: bounded(raw.lp_fees, "deserialized LP fees")?,
            unrealized_pnl: bounded(raw.unrealized_pnl, "deserialized unrealized PnL")?,
            // A position value cannot be negative, which its type already
            // says; only the supply bound is left to check.
            position_value: if raw.position_value.atoms() <= MAX_COMPONENT_ATOMS {
                raw.position_value
            } else {
                return Err(ValidationError::Overflow {
                    context: "deserialized position value".into(),
                });
            },
            // The ratio's own domain, the contract's `uint24`, is checked
            // where a `Ratio` is deserialised.
            liquidation_margin_ratio: raw.liquidation_margin_ratio,
            liquidation_fee: raw.liquidation_fee,
            mark: raw.mark,
            sqrt_price: raw.sqrt_price,
            exposure: raw.exposure,
        })
    }
}

#[cfg(test)]
impl MakerEquityBreakdown {
    /// A breakdown of nothing, for a test that needs a `Computed` payload
    /// and reads none of it.
    pub(crate) fn placeholder() -> Self {
        Self {
            margin: UsdcDelta::ZERO,
            funding_owed: UsdcDelta::ZERO,
            util_earnings: PerSide::default(),
            lp_fees: UsdcDelta::ZERO,
            unrealized_pnl: UsdcDelta::ZERO,
            position_value: UsdcAtoms::ZERO,
            liquidation_margin_ratio: Ratio::ZERO,
            liquidation_fee: Ratio::ZERO,
            mark: Price::from_x96(Q96),
            sqrt_price: SqrtPrice::from_x96(Q96),
            exposure: Exposure {
                sqrt_lower: SqrtPrice::from_x96(Q96),
                sqrt_upper: SqrtPrice::from_x96(Q96),
                liquidity: LUnits::ZERO,
                delta_perp: PerpDelta::ZERO,
                delta_usd: UsdcDelta::ZERO,
            },
        }
    }
}

impl MakerEquityBreakdown {
    /// Last-settled margin (`positions(id).margin`).
    pub fn margin(&self) -> UsdcDelta {
        self.margin
    }

    /// Funding owed since the last settle. **Positive = the position pays**
    /// (it is subtracted when settling margin).
    pub fn funding_owed(&self) -> UsdcDelta {
        self.funding_owed
    }

    /// Accrued utilization earnings, per side; `total()` is what the
    /// settle credits.
    pub fn util_earnings(&self) -> PerSide<UsdcDelta> {
        self.util_earnings
    }

    /// Uncollected V4 LP fees (donated taker fees).
    pub fn lp_fees(&self) -> UsdcDelta {
        self.lp_fees
    }

    /// `valPnl`: current band value minus the recorded deposit value, both
    /// priced at the mark.
    pub fn unrealized_pnl(&self) -> UsdcDelta {
        self.unrealized_pnl
    }

    /// Margin as the contract would settle it now.
    pub fn settled_margin(&self) -> UsdcDelta {
        self.margin - self.funding_owed + self.util_earnings.total() + self.lp_fees
    }

    /// Settled margin plus inventory PnL — the position's live equity.
    pub fn equity(&self) -> UsdcDelta {
        self.settled_margin() + self.unrealized_pnl
    }

    /// What the position earned since its last settle, on its own.
    pub fn accrued_income(&self) -> UsdcDelta {
        self.util_earnings.total() + self.lp_fees - self.funding_owed
    }

    /// `posVal`: the band's liquidity value priced at the mark — the
    /// denominator of the contract's health check, and never negative,
    /// which is why it is a count.
    pub fn position_value(&self) -> UsdcAtoms {
        self.position_value
    }

    /// `positions(id).liqMarginRatio`: the ratio the contract's health check
    /// compares this position against.
    pub fn liquidation_margin_ratio(&self) -> Ratio {
        self.liquidation_margin_ratio
    }

    /// The position's margin ratio as `PerpLogic.isHealthy` computes it:
    /// live equity over position value, zero when equity is not positive,
    /// with a zero position value counted as one atom (as in the
    /// contract).
    pub fn margin_ratio(&self) -> f64 {
        Self::health_ratio(self.equity().atoms() as f64, self.position_value)
    }

    /// The market's liquidation fee rate at the block, the share of the
    /// band's value the health test deducts from equity. Carried here so
    /// the test has every input it needs: the fee a liquidation *settles*
    /// is a USDC amount under the same name, and when the rate was a
    /// parameter the two substituted for each other.
    pub fn liquidation_fee(&self) -> Ratio {
        self.liquidation_fee
    }

    /// Whether the contract would liquidate the position now: the negation
    /// of `isHealthy(equity − posVal·liqFee, posVal, liqMarginRatio)`.
    ///
    /// A screening gate, not the oracle: the fee leg is applied in `f64`,
    /// so a position within an atom of the boundary can go either way.
    /// Confirm with `simulate_liquidate_maker` before sending.
    pub fn is_liquidatable(&self) -> bool {
        self.fails_health(self.equity(), self.position_value)
    }

    /// The contract's test on an equity and a band value: the fee leg off
    /// the equity, then the ratio against the position's.
    fn fails_health(&self, equity: UsdcDelta, position_value: UsdcAtoms) -> bool {
        let fee_atoms = position_value.atoms() as f64 * self.liquidation_fee.fraction();
        let equity_after_fee = equity.atoms() as f64 - fee_atoms;
        Self::health_ratio(equity_after_fee, position_value)
            < self.liquidation_margin_ratio.fraction()
    }

    fn health_ratio(equity_atoms: f64, position_value: UsdcAtoms) -> f64 {
        if equity_atoms <= 0.0 {
            return 0.0;
        }
        equity_atoms / position_value.atoms().max(1) as f64
    }

    /// The mark the preview was priced at: where a price shock starts.
    pub fn mark(&self) -> Price {
        self.mark
    }

    /// The band's value and the unrealized PnL along a price shock that
    /// takes the mark to `mark`: the pool moves by the same factor, its root
    /// by the root of it, so the band's composition is read at the shocked
    /// pool price and valued, with the deposit residual, at the shocked
    /// mark. The factor is exactly one at the preview's own mark, where the
    /// pool's root comes back unrounded.
    fn valued_at(&self, mark: Price) -> Result<(UsdcAtoms, UsdcDelta), ValidationError> {
        let sqrt_price = SqrtPrice::from_x96(mul_div(
            self.sqrt_price.x96(),
            SqrtPrice::try_from(mark)?.x96(),
            SqrtPrice::try_from(self.mark)?.x96(),
            Rounding::TowardZero,
        )?);
        let (value, unrealized) = self.exposure.valued(sqrt_price, mark)?;
        Ok((
            UsdcAtoms::new(
                atoms(to_i256(value, "position value")?, "position value")?.unsigned_abs(),
            ),
            UsdcDelta::new(atoms(unrealized, "unrealized PnL")?),
        ))
    }

    /// Equity if the mark stood at `mark` and the pool price had moved by
    /// the same factor from where it stood, the accrued legs as they are:
    /// the shock a maker is liquidated along, takers trading the pool and
    /// the mark following. At the preview's own mark it is exactly
    /// [`Self::equity`].
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidPrice`] for a mark whose root is outside
    /// the protocol's range, and [`ValidationError::Overflow`] when a leg
    /// leaves its domain.
    pub fn equity_at(&self, mark: Price) -> Result<UsdcDelta, ValidationError> {
        Ok(self.settled_margin() + self.valued_at(mark)?.1)
    }

    /// The band's value along the same shock, the test's denominator.
    ///
    /// # Errors
    ///
    /// As [`Self::equity_at`].
    pub fn position_value_at(&self, mark: Price) -> Result<UsdcAtoms, ValidationError> {
        Ok(self.valued_at(mark)?.0)
    }

    /// [`Self::is_liquidatable`] along the same shock; at the preview's own
    /// mark, the same answer.
    ///
    /// # Errors
    ///
    /// As [`Self::equity_at`].
    pub fn is_liquidatable_at(&self, mark: Price) -> Result<bool, ValidationError> {
        let (value, unrealized) = self.valued_at(mark)?;
        Ok(self.fails_health(self.settled_margin() + unrealized, value))
    }

    /// Where the test turns on each side of the mark along the shock: the
    /// first mark, to the Q96 atom, at which the position is liquidatable, found by a
    /// bracket that steps outward from a quarter of a percent, doubling to
    /// a hundredfold, then a bisection. A pocket narrower than a bracket
    /// step is not seen, so a side is the nearest turn the search saw.
    ///
    /// # Errors
    ///
    /// As [`Self::equity_at`], at any price the search valued.
    pub fn liquidation_prices(&self) -> Result<LiquidationPrices, ValidationError> {
        if self.is_liquidatable() {
            return Ok(LiquidationPrices {
                below: Some(self.mark),
                above: Some(self.mark),
            });
        }
        Ok(LiquidationPrices {
            below: self.first_turn(false)?,
            above: self.first_turn(true)?,
        })
    }

    /// The first liquidatable price from the mark in one direction, or
    /// `None` when the position is healthy out to a hundredfold.
    fn first_turn(&self, up: bool) -> Result<Option<Price>, ValidationError> {
        // Bracket: the last healthy price and the first liquidatable one,
        // at factors 1 + 2^k / 512 for k up to 16, a hundredfold and more.
        let mut healthy = self.mark;
        let mut liquidatable = None;
        for k in 0..=16 {
            let price = stepped(self.mark, k, up)?;
            if self.is_liquidatable_at(price)? {
                liquidatable = Some(price);
                break;
            }
            healthy = price;
        }
        let Some(liquidatable) = liquidatable else {
            return Ok(None);
        };
        // Bisect to the atom. The midpoint is taken from the lower end so
        // the sum of two words near the top of the range cannot wrap.
        let (mut healthy, mut liquidatable) = (healthy.x96(), liquidatable.x96());
        while healthy.abs_diff(liquidatable) > U256::from(1u8) {
            let mid = healthy.min(liquidatable) + healthy.abs_diff(liquidatable) / U256::from(2u8);
            if self.is_liquidatable_at(Price::from_x96(mid))? {
                liquidatable = mid;
            } else {
                healthy = mid;
            }
        }
        Ok(Some(Price::from_x96(liquidatable)))
    }
}

/// `mark` scaled by `(512 + 2^k) / 512`, up or down: the search's `k`th
/// step, as the exact quotient or an overflow, never a wrapped product.
fn stepped(mark: Price, k: u32, up: bool) -> Result<Price, ValidationError> {
    const GRAIN: u32 = 512;
    let (grain, factor) = (U256::from(GRAIN), U256::from(GRAIN + (1u32 << k)));
    Ok(Price::from_x96(if up {
        mul_div(mark.x96(), factor, grain, Rounding::TowardZero)?
    } else {
        mul_div(mark.x96(), grain, factor, Rounding::TowardZero)?
    }))
}

impl MakerMarketSnapshot {
    /// Replay `PerpLogic.accrue` from `last_touch` to `accrue_to`,
    /// returning an [`AccruedMakerSnapshot`] with the cumulatives advanced.
    /// Mirrors the contract exactly, using [`Self::mark`] for the
    /// utilization leg. `accrue` recomputes the mark as the deployed fair
    /// price of the block's spot pair and advanced EMAs; a client-loaded
    /// snapshot carries that same mark, so the replay is the contract's.
    ///
    /// Consumes `self` and returns a distinct type: the replay is not
    /// idempotent (each application adds another rate × dt), and equities
    /// computed from an un-accrued snapshot would silently be stale to the
    /// market's last touch — so [`AccruedMakerSnapshot::maker_equity`] is
    /// only reachable through this replay.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError::InvalidConfig`] when `accrue_to` precedes
    /// `last_touch` — the chain cannot have touched the market in the
    /// future of the snapshot block, so the inputs come from different
    /// blocks (or a wall clock behind the chain); silently replaying zero
    /// accrual would hide that. Returns [`ValidationError::Overflow`] when
    /// a replayed cumulative exceeds its integer domain — chain-consistent
    /// rates over a sane dt stay far in range, so an error indicates
    /// corrupt inputs (e.g. a wall-clock timestamp fed as the accrual
    /// target against a stale `last_touch`).
    pub fn accrued(
        mut self,
        accrual: &AccrualInputs,
    ) -> Result<AccruedMakerSnapshot, ValidationError> {
        if accrual.accrue_to < accrual.last_touch {
            return Err(ValidationError::InvalidConfig {
                reason: format!(
                    "accrual target {} precedes the market's last touch {}",
                    accrual.accrue_to, accrual.last_touch
                ),
            });
        }
        let dt = accrual.accrue_to - accrual.last_touch;
        if dt == 0 {
            return Ok(AccruedMakerSnapshot(self));
        }
        let dt_days = mul_div(
            U256::from(dt),
            Q96,
            U256::from(INTERVAL),
            Rounding::TowardZero,
        )?;
        let funding_accrued = Funding::from_x96(s_full_mul_div(
            I256::unchecked_from(accrual.funding_per_day.wad()),
            to_i256(dt_days, "accrual dt in days")?,
            WAD,
            Rounding::TowardZero,
        )?);
        self.funding = self
            .funding
            .advanced_by(funding_accrued, "accrued funding cumulative")?;
        self.funding_div_sqrt_p = self.funding_div_sqrt_p.advanced_by(
            funding_accrued.per_sqrt_price(self.sqrt_price)?,
            "accrued funding/sqrtP cumulative",
        )?;

        let dt_days_mult_mark = mul_div(dt_days, self.mark.x96(), Q96, Rounding::TowardZero)?;
        for side in Side::BOTH {
            let capacity = accrual.capacity.on(side);
            if capacity.is_zero() {
                continue;
            }
            let accrued = mul_div(
                U256::from(accrual.util_fee_per_day.on(side).wad()),
                dt_days_mult_mark,
                WAD,
                Rounding::TowardZero,
            )?;
            let earnings = self.util_earnings.on_mut(side);
            *earnings = earnings.advanced_by(
                Earnings::from_x96(mul_div(
                    accrued,
                    U256::from(accrual.open_interest.on(side).atoms()),
                    U256::from(capacity.atoms()),
                    Rounding::TowardZero,
                )?),
                "accrued utilization cumulative",
            )?;
        }
        Ok(AccruedMakerSnapshot(self))
    }
}

/// A [`MakerMarketSnapshot`] whose cumulatives have been replayed to the
/// snapshot block's timestamp via [`MakerMarketSnapshot::accrued`].
///
/// This is the only type that can compute equities: making the accrual a
/// type-state means a stale, un-accrued snapshot cannot silently price a
/// settle preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccruedMakerSnapshot(MakerMarketSnapshot);

impl AccruedMakerSnapshot {
    /// The underlying snapshot (block context, prices, replayed
    /// cumulatives). Read-only: mutating access would break the accrual
    /// type-state.
    pub fn snapshot(&self) -> &MakerMarketSnapshot {
        &self.0
    }

    /// Reprice this accrued snapshot at a what-if mark.
    ///
    /// Only the pricing input changes: the accrual replay already ran at
    /// the mark the chain would have used, so the replayed funding and
    /// utilization cumulatives are untouched. The new mark prices
    /// `valPnl` (the band's liquidity value and the inventory legs) in
    /// every subsequent [`Self::maker_equity`].
    #[must_use]
    pub fn with_mark(mut self, mark: Price) -> Self {
        self.0.mark = mark;
        self
    }

    /// Compute the full settle preview for one maker position.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError::InvalidTickRange`] for out-of-range or
    /// mis-ordered ticks and [`ValidationError::Overflow`] if an
    /// intermediate exceeds its integer domain — both indicate corrupt
    /// inputs, since chain-consistent state stays in range.
    pub fn maker_equity(
        &self,
        maker: &MakerState,
    ) -> Result<MakerEquityBreakdown, ValidationError> {
        if maker.tick_lower >= maker.tick_upper {
            return Err(ValidationError::InvalidTickRange {
                lower: maker.tick_lower,
                upper: maker.tick_upper,
            });
        }
        let sqrt_l = get_sqrt_ratio_at_tick(maker.tick_lower)?;
        let sqrt_u = get_sqrt_ratio_at_tick(maker.tick_upper)?;
        let (mf_below, mf_within, mf_div_sqrt_within) = self.0.maker_cuml_funding(maker)?;

        // ── makerFeesAccrued ────────────────────────────────────────────
        let base_funding = s_full_mul_div(
            I256::unchecked_from(maker.delta_perp.atoms()),
            self.0
                .funding
                .since(maker.last_cuml_funding, "funding cumulative delta")?
                .x96(),
            Q96,
            Rounding::Up,
        )?;
        let perp_below = to_i256(
            amount0_delta(
                sqrt_l.x96(),
                sqrt_u.x96(),
                maker.liquidity.units(),
                Rounding::TowardZero,
            )?,
            "band perp amount",
        )?;
        let funding_below = s_full_mul_div(
            perp_below,
            mf_below
                .since(maker.last_below, "below-band funding delta")?
                .x96(),
            Q96,
            Rounding::Up,
        )?;
        let div_amm = mf_div_sqrt_within
            .since(
                maker.last_div_sqrt_within,
                "within-band funding/sqrtP delta",
            )?
            .x96();
        let div_upper = mf_within
            .since(maker.last_within, "within-band funding delta")?
            .per_sqrt_price(sqrt_u)?
            .x96();
        let funding_within = s_full_mul_div(
            I256::unchecked_from(maker.liquidity.units()),
            sub_i(div_amm, div_upper, "within-band funding components")?,
            Q96,
            Rounding::Up,
        )?;
        let funding = add_i(
            add_i(base_funding, funding_below, "accrued funding")?,
            funding_within,
            "accrued funding",
        )?;

        let util = |side: Side| {
            mul_div(
                U256::from(maker.capacity.on(side).atoms()),
                self.0
                    .util_earnings
                    .on(side)
                    .since(
                        *maker.last_util_earnings.on(side),
                        "utilization checkpoint ahead of market cumulative",
                    )?
                    .x96(),
                Q96,
                Rounding::TowardZero,
            )
        };
        let util = PerSide::new(util(Side::Long)?, util(Side::Short)?);

        // ── V4 LP fees: liquidity × Δ feeGrowthInside1 / 2^128 ──────────
        let fee_growth_delta = maker
            .fee_growth_inside1
            .since(maker.fee_growth_inside1_last);
        let lp_fees = u512_to_u256(
            (U512::from(maker.liquidity.units()) * U512::from(fee_growth_delta.x128())) >> 128,
        )?;

        // ── valPnl (maker overload) ─────────────────────────────────────
        // A SIGNED sum, per `PerpLogic.valPnl` (`perpcity-contracts@4bbe554f`):
        //   unrealizedPnl = liquidityVal.toInt256()
        //       + delta.amount0().sFullMulDiv(markP.toInt256(), Q96, false)
        //       + delta.amount1();
        // (For an open maker both deltas are usually negative — owed to the
        // pool — but a mixed-sign delta must not collapse to magnitudes.)
        let exposure = Exposure {
            sqrt_lower: sqrt_l,
            sqrt_upper: sqrt_u,
            liquidity: maker.liquidity,
            delta_perp: maker.delta_perp,
            delta_usd: maker.delta_usd,
        };
        let (liquidity_val, unrealized) = exposure.valued(self.0.sqrt_price, self.0.mark)?;

        let delta = |value, what| atoms(value, what).map(UsdcDelta::new);
        Ok(MakerEquityBreakdown {
            margin: delta(
                to_i256(U256::from(maker.margin.atoms()), "margin")?,
                "margin",
            )?,
            funding_owed: delta(funding, "accrued funding")?,
            util_earnings: PerSide::new(
                delta(
                    to_i256(util.long, "long utilization earnings")?,
                    "long utilization earnings",
                )?,
                delta(
                    to_i256(util.short, "short utilization earnings")?,
                    "short utilization earnings",
                )?,
            ),
            lp_fees: delta(to_i256(lp_fees, "LP fees")?, "LP fees")?,
            unrealized_pnl: delta(unrealized, "unrealized PnL")?,
            // A band's value at the mark is a sum of non-negative legs, so
            // the count's own floor is the only bound left to assert.
            position_value: UsdcAtoms::new(
                atoms(to_i256(liquidity_val, "position value")?, "position value")?.unsigned_abs(),
            ),
            liquidation_margin_ratio: maker.liquidation_margin_ratio,
            liquidation_fee: self.0.liquidation_fee,
            mark: self.0.mark,
            sqrt_price: self.0.sqrt_price,
            exposure,
        })
    }
}

impl MakerMarketSnapshot {
    /// `PerpLogic.makerCumlFunding`: assemble the band's cumulative funding
    /// (below / within / within-div-sqrtP) from the two ticks' opposite-side
    /// checkpoints, branching on which side of each tick the current tick is.
    fn maker_cuml_funding(
        &self,
        maker: &MakerState,
    ) -> Result<(Funding, Funding, FundingPerSqrtPrice), ValidationError> {
        let lower = &maker.tick_lower_funding;
        let upper = &maker.tick_upper_funding;

        let (below, div_below_lower) = if self.tick >= maker.tick_lower {
            (lower.cuml_funding_opp, lower.cuml_funding_div_sqrt_p_opp)
        } else {
            (
                self.funding
                    .since(lower.cuml_funding_opp, "lower-tick funding checkpoint")?,
                self.funding_div_sqrt_p.since(
                    lower.cuml_funding_div_sqrt_p_opp,
                    "lower-tick funding/sqrtP checkpoint",
                )?,
            )
        };
        let (below_upper, div_below_upper) = if self.tick >= maker.tick_upper {
            (upper.cuml_funding_opp, upper.cuml_funding_div_sqrt_p_opp)
        } else {
            (
                self.funding
                    .since(upper.cuml_funding_opp, "upper-tick funding checkpoint")?,
                self.funding_div_sqrt_p.since(
                    upper.cuml_funding_div_sqrt_p_opp,
                    "upper-tick funding/sqrtP checkpoint",
                )?,
            )
        };
        Ok((
            below,
            below_upper.since(below, "within-band cumulative funding")?,
            div_below_upper.since(div_below_lower, "within-band cumulative funding/sqrtP")?,
        ))
    }
}

/// Compute `feeGrowthInside1X128` for a band from the global growth and the
/// two ticks' `feeGrowthOutside1X128`, per Uniswap's `getFeeGrowthInside`.
pub(crate) fn fee_growth_inside1(
    global: FeeGrowth,
    outside_lower: FeeGrowth,
    outside_upper: FeeGrowth,
    tick_lower: i32,
    tick_upper: i32,
    current_tick: i32,
) -> FeeGrowth {
    let below = if current_tick >= tick_lower {
        outside_lower
    } else {
        global.since(outside_lower)
    };
    let above = if current_tick < tick_upper {
        outside_upper
    } else {
        global.since(outside_upper)
    };
    global.since(below).since(above)
}

/// Maximum settle-component magnitude accepted into a
/// [`MakerEquityBreakdown`]: the protocol's total accounting-token supply
/// ([`ACCOUNTING_TOKEN_SUPPLY`], `type(uint120).max` atoms) — no
/// chain-consistent settle component can exceed every atom in existence.
/// Bounding at construction makes the breakdown's `i128` component sums
/// provably overflow-free (6 × 2^120 ≪ 2^127).
pub const MAX_COMPONENT_ATOMS: u128 = {
    let limbs = ACCOUNTING_TOKEN_SUPPLY.as_limbs();
    // The supply is uint120: limbs 2 and 3 are zero, so it fits u128.
    ((limbs[1] as u128) << 64) | limbs[0] as u128
};

/// Narrow a settle component to 6-decimal atoms, erroring — never
/// saturating — when the value exceeds [`MAX_COMPONENT_ATOMS`]. Chain-
/// consistent state stays far inside the bound; exceeding it means corrupt
/// inputs.
fn atoms(v: I256, context: &'static str) -> Result<i128, ValidationError> {
    i128::try_from(v)
        .ok()
        .filter(|a| a.unsigned_abs() <= MAX_COMPONENT_ATOMS)
        .ok_or(ValidationError::Overflow {
            context: context.into(),
        })
}

#[cfg(test)]
mod tests {
    use alloy::primitives::B256;

    use super::*;

    /// A ratio from its millionths, as the contract stores one.
    fn ratio(e6: u32) -> Ratio {
        Ratio::from_e6(e6).unwrap()
    }

    /// Golden vector: CHINA-PC (`0x796f…8ed0`) position 54, chain state at
    /// block 500612175 — the block before its liquidation. The liquidation's
    /// `MakerConverted` event settled at timestamp 1788260191 with funding
    /// 209.633223, longUtil 0.432735, shortUtil 24.630722, lpFees 7.722360.
    /// The fixture's recorded `mark_price_x96` is the pool's AMM price at
    /// that block, not the fair price `accrue` recomputes (the client now
    /// loads the fair price; this fixture predates that), which costs a
    /// few micro-dollars on the utilization legs over the 5775s replay
    /// window.
    fn golden_market_and_maker() -> (MakerMarketSnapshot, AccrualInputs, MakerState) {
        let i = |s: &str| I256::from_dec_str(s).unwrap();
        let u = |s: &str| U256::from_str_radix(s, 10).unwrap();
        let market = MakerMarketSnapshot {
            block: BlockContext {
                number: 500612175,
                hash: B256::ZERO,
                timestamp: 1788260191,
            },
            funding: Funding::from_x96(i("-5817301051923220663714693204286")),
            funding_div_sqrt_p: FundingPerSqrtPrice::from_x96(i(
                "-1332253658657311045256648214058",
            )),
            util_earnings: PerSide::new(
                Earnings::from_x96(u("361206840527920163630096383165")),
                Earnings::from_x96(u("512938731932611361114843741066")),
            ),
            tick: 28543,
            sqrt_price: SqrtPrice::from_x96(u("330115084885190701587787251116")),
            mark: Price::from_x96(u("1375470108235016714305503507110")),
            liquidation_fee: ratio(10_000),
        };
        let accrual = AccrualInputs {
            funding_per_day: FundingRate::from_wad(840374978539967329),
            util_fee_per_day: PerSide::uniform(UtilizationRate::from_wad(10000000000000000)),
            last_touch: 1788254416,
            accrue_to: 1788260191,
            open_interest: PerSide::new(PerpAtoms::new(2587247), PerpAtoms::new(175795732)),
            capacity: PerSide::new(PerpAtoms::new(303811186), PerpAtoms::new(223153047)),
        };
        let maker = MakerState {
            margin: UsdcAtoms::new(143730198),
            // A pass-through the settle event does not exercise; 5% is the
            // maker liquidation ratio the client tests use.
            liquidation_margin_ratio: ratio(50_000),
            delta_perp: PerpDelta::new(-134328),
            delta_usd: UsdcDelta::new(-137992489),
            last_cuml_funding: Funding::from_x96(i("-10162710870332004796583430787875")),
            tick_lower: 33810,
            tick_upper: 34710,
            liquidity: LUnits::new(570282387),
            last_util_earnings: PerSide::new(
                Earnings::from_x96(u("105980308075601242205274025040")),
                Earnings::from_x96(u("79412639757423009537924209956")),
            ),
            capacity: PerSide::new(PerpAtoms::new(134327), PerpAtoms::new(4493830)),
            last_below: Funding::from_x96(i("-10162710870332004796583430787875")),
            last_within: Funding::ZERO,
            last_div_sqrt_within: FundingPerSqrtPrice::ZERO,
            tick_lower_funding: TickFunding {
                cuml_funding_opp: Funding::from_x96(i("2413781515094096341489935830192")),
                cuml_funding_div_sqrt_p_opp: FundingPerSqrtPrice::from_x96(i(
                    "440051787484224301957495580026",
                )),
            },
            tick_upper_funding: TickFunding::default(),
            fee_growth_inside1: fee_growth_inside1(
                FeeGrowth::from_x128(u("28998515790711655837734081581084912609")),
                FeeGrowth::from_x128(u("4607862979514044838473387691959359354")),
                FeeGrowth::ZERO,
                33810,
                34710,
                28543,
            ),
            fee_growth_inside1_last: FeeGrowth::ZERO,
        };
        (market, accrual, maker)
    }

    #[test]
    fn golden_vector_reproduces_pos54_liquidation_settle() {
        let (market, accrual, maker) = golden_market_and_maker();
        let market = market.accrued(&accrual).unwrap();
        let b = market.maker_equity(&maker).unwrap();

        // The funding replay lands within one atom of the event's exact
        // 209_633_223 (the settle's own rounding happens at a different
        // cumulative granularity); the short-util leg is priced with the
        // fixture's recorded AMM mark instead of the fair price `accrue`
        // recomputes, costing a few atoms over the 5775s window. The
        // unreplayed legs are exact.
        assert!(
            (b.funding_owed().atoms() - 209_633_223).abs() <= 1,
            "funding {}",
            b.funding_owed().atoms()
        );
        assert_eq!(b.util_earnings().long, UsdcDelta::new(432_735));
        assert!(
            (b.util_earnings().short.atoms() - 24_630_722).abs() <= 10,
            "short util {}",
            b.util_earnings().short.atoms()
        );
        assert_eq!(
            b.lp_fees(),
            UsdcDelta::new(7_722_360),
            "lp {}",
            b.lp_fees().usdc()
        );

        // The position was fee-insolvent (the contracts#292 wedge): equity
        // deeply negative, dominated by accrued funding + inventory loss.
        assert_eq!(b.margin(), UsdcDelta::new(143_730_198));
        let equity = b.equity().usdc();
        assert!(equity < -80.0 && equity > -110.0, "equity {equity}");
        assert!(b.accrued_income().is_negative());
    }

    /// The chain cannot have touched the market after the snapshot block,
    /// so an accrual target behind `last_touch` means the inputs came from
    /// different blocks. Returning an un-accrued snapshot would hide that.
    #[test]
    fn accrual_target_before_last_touch_is_an_error() {
        let (market, accrual, _) = golden_market_and_maker();
        let backwards = AccrualInputs {
            accrue_to: accrual.last_touch - 1,
            ..accrual
        };
        assert!(matches!(
            market.accrued(&backwards),
            Err(ValidationError::InvalidConfig { .. })
        ));
    }

    /// The health gate mirrors `PerpLogic.isHealthy`: equity over the
    /// band's liquidity value against the POSITION's stored ratio, with the
    /// liquidation fee taken off equity first. Pos 54 was fee-insolvent
    /// (negative equity), so its ratio floors at zero and it is
    /// liquidatable under any fee; a healthy synthetic sibling is not.
    #[test]
    fn health_gate_mirrors_the_contracts_is_healthy() {
        let (market, accrual, maker) = golden_market_and_maker();
        let market = market.accrued(&accrual).unwrap();
        let b = market.maker_equity(&maker).unwrap();

        assert!(!b.position_value().is_zero());
        assert!(
            (b.position_value().usdc() - b.unrealized_pnl().usdc()).abs() > 100.0,
            "position value is the band value, not the PnL"
        );
        assert_eq!(b.liquidation_margin_ratio(), ratio(50_000));
        assert_eq!(b.liquidation_margin_ratio().fraction(), 0.05);
        assert!(b.equity().is_negative());
        assert_eq!(b.margin_ratio(), 0.0);
        assert!(b.is_liquidatable());
        let unfeed = MakerEquityBreakdown {
            liquidation_fee: Ratio::ZERO,
            ..b
        };
        assert!(unfeed.is_liquidatable(), "under any fee");

        // Same band, no accrued liabilities, a fat margin: healthy, and the
        // fee leg alone must not flip it.
        let value = b.position_value();
        let healthy = MakerEquityBreakdown {
            margin: UsdcDelta::from(value),
            funding_owed: UsdcDelta::ZERO,
            util_earnings: PerSide::default(),
            lp_fees: UsdcDelta::ZERO,
            unrealized_pnl: UsdcDelta::ZERO,
            position_value: value,
            liquidation_margin_ratio: ratio(50_000),
            liquidation_fee: ratio(10_000),
            ..b
        };
        assert!((healthy.margin_ratio() - 1.0).abs() < 1e-12);
        assert!(!healthy.is_liquidatable());
        // Equity of 5.5% of value: healthy at a 0% fee, liquidatable once
        // a 1% fee takes it under the 5% line.
        let thin = MakerEquityBreakdown {
            margin: UsdcDelta::new(value.atoms() as i128 * 55 / 1000),
            ..healthy
        };
        let thin_unfeed = MakerEquityBreakdown {
            liquidation_fee: Ratio::ZERO,
            ..thin
        };
        assert!(!thin_unfeed.is_liquidatable());
        assert!(thin.is_liquidatable());
    }

    /// The golden maker's band, wholly above the golden price, so it holds
    /// one asset wherever the pool sits below it.
    const GOLDEN_BAND: (i32, i32) = (33_810, 34_710);

    /// A band the golden price, near tick 28,544, sits inside, so where the
    /// pool stands decides what the band holds.
    const BAND_AROUND_THE_PRICE: (i32, i32) = (28_200, 28_860);

    /// The golden maker with the pool standing at the mark and a margin
    /// that leaves it at one and a half times its band's value.
    fn healthy_maker_at_the_mark() -> MakerEquityBreakdown {
        healthy_maker(1000, GOLDEN_BAND)
    }

    /// The golden maker in `band`, with the pool's root at `pool_root_e3`
    /// thousandths of the mark's, and a margin that leaves it at one and a
    /// half times its band's value.
    fn healthy_maker(pool_root_e3: u16, (lower, upper): (i32, i32)) -> MakerEquityBreakdown {
        let (market, accrual, mut maker) = golden_market_and_maker();
        (maker.tick_lower, maker.tick_upper) = (lower, upper);
        let mark_root = SqrtPrice::try_from(market.mark).unwrap().x96();
        let market = MakerMarketSnapshot {
            sqrt_price: SqrtPrice::from_x96(
                mark_root * U256::from(pool_root_e3) / U256::from(1000u16),
            ),
            ..market
        }
        .accrued(&accrual)
        .unwrap();
        let thin = market.maker_equity(&maker).unwrap();
        let target = thin.position_value().atoms() as i128 * 3 / 2;
        let top_up = u128::try_from(target - thin.equity().atoms()).unwrap();
        maker.margin = UsdcAtoms::new(maker.margin.atoms() + top_up);
        market.maker_equity(&maker).unwrap()
    }

    /// Along a price shock the test turns where the search says: the first
    /// Q96 atom on each side at which the position is liquidatable, with
    /// the atom before it still healthy. At the mark, with the pool
    /// standing there, the shock valuation is the preview itself.
    #[test]
    fn the_liquidating_prices_are_where_the_test_turns() {
        let b = healthy_maker_at_the_mark();
        assert!(!b.is_liquidatable());
        assert_eq!(b.equity_at(b.mark()).unwrap(), b.equity());
        assert_eq!(b.position_value_at(b.mark()).unwrap(), b.position_value());
        assert!(!b.is_liquidatable_at(b.mark()).unwrap());

        let prices = b.liquidation_prices().unwrap();
        // Both deposit legs are owed to the pool, so a rally values the
        // residual ever lower against a band that is all USD past its
        // upper tick: the test turns above. A fall shrinks both the band's
        // value and the residual toward zero while the margin stands, so
        // it never turns below.
        let above = prices
            .above
            .expect("a long residual is liquidated from above");
        assert_eq!(
            prices.below, None,
            "the margin covers a fall of a hundredfold"
        );
        assert!(above > b.mark());
        assert!(b.is_liquidatable_at(above).unwrap());
        assert!(
            !b.is_liquidatable_at(Price::from_x96(above.x96() - U256::from(1u8)))
                .unwrap(),
            "the atom before the turn is healthy"
        );
        let distance = prices.distance_from(b.mark()).unwrap();
        let expected = f64::from(above.x96()) / f64::from(b.mark().x96()) - 1.0;
        assert!((distance - expected).abs() < 1e-12);
        assert!(distance > 0.0);
    }

    /// With the pool off the mark, as it is on a live market, the shock
    /// still starts at the preview: at the preview's own mark the band is
    /// valued at the real pool price, so equity, value and the verdict agree
    /// exactly, and the search measures a price move rather than the gap
    /// between the pool and the mark. The band straddles the price, so
    /// where the pool stands changes what the band holds, which is the only
    /// case in which putting the pool at the mark would show.
    #[test]
    fn the_shock_starts_where_the_pool_stands() {
        let b = healthy_maker(1010, BAND_AROUND_THE_PRICE);
        assert!(!b.is_liquidatable());
        assert_eq!(b.equity_at(b.mark()).unwrap(), b.equity());
        assert_eq!(b.position_value_at(b.mark()).unwrap(), b.position_value());
        assert_eq!(b.is_liquidatable_at(b.mark()).unwrap(), b.is_liquidatable());

        let prices = b.liquidation_prices().unwrap();
        let one = U256::from(1u8);
        let turns = [
            prices.below.map(|p| (p, Price::from_x96(p.x96() + one))),
            prices.above.map(|p| (p, Price::from_x96(p.x96() - one))),
        ];
        assert!(turns.iter().any(Option::is_some), "{prices:?}");
        for (turn, inside) in turns.into_iter().flatten() {
            assert!(b.is_liquidatable_at(turn).unwrap());
            assert!(
                !b.is_liquidatable_at(inside).unwrap(),
                "the atom before the turn is healthy"
            );
        }
    }

    /// A position liquidatable at the mark has no distance left: both
    /// sides are the mark and the distance is zero. A margin large enough
    /// never turns within the search's reach, and says so with `None`.
    #[test]
    fn no_distance_when_liquidatable_and_none_when_covered() {
        let (market, accrual, maker) = golden_market_and_maker();
        let b = market
            .accrued(&accrual)
            .unwrap()
            .maker_equity(&maker)
            .unwrap();
        assert!(b.is_liquidatable());
        let prices = b.liquidation_prices().unwrap();
        assert_eq!(prices.below, Some(b.mark()));
        assert_eq!(prices.above, Some(b.mark()));
        assert_eq!(prices.distance_from(b.mark()), Some(0.0));

        let covered = MakerEquityBreakdown {
            margin: UsdcDelta::new(MAX_COMPONENT_ATOMS as i128 / 8),
            ..healthy_maker_at_the_mark()
        };
        assert_eq!(
            covered.liquidation_prices().unwrap(),
            LiquidationPrices::default(),
            "healthy out to a hundredfold either way"
        );
        assert_eq!(
            covered
                .liquidation_prices()
                .unwrap()
                .distance_from(covered.mark()),
            None
        );
    }

    /// A step is the exact quotient in both directions, and a step past the
    /// top of the range is an overflow rather than a wrapped product.
    #[test]
    fn a_step_is_exact_or_refused_never_wrapped() {
        let one = Price::from_x96(Q96);
        let (q, up, down) = (Q96, U256::from(513u16), U256::from(512u16));
        assert_eq!(stepped(one, 0, true).unwrap().x96(), q * up / down);
        assert_eq!(stepped(one, 0, false).unwrap().x96(), q * down / up);
        assert!(matches!(
            stepped(Price::from_x96(U256::MAX), 0, true),
            Err(ValidationError::Overflow { .. })
        ));
    }

    /// Without the accrual replay the funding is stale to lastTouch — the
    /// replay must move it by the rate × dt amount, not by orders of
    /// magnitude.
    #[test]
    fn accrual_replay_moves_funding_forward() {
        let (market, accrual, maker) = golden_market_and_maker();
        // A dt-0 replay (accrue exactly to last_touch) leaves the
        // cumulatives stale — the only way to see the pre-replay numbers.
        let stale_accrual = AccrualInputs {
            accrue_to: accrual.last_touch,
            ..accrual
        };
        let stale = market
            .accrued(&stale_accrual)
            .unwrap()
            .maker_equity(&maker)
            .unwrap();
        let fresh = market
            .accrued(&accrual)
            .unwrap()
            .maker_equity(&maker)
            .unwrap();
        assert!(
            fresh.funding_owed() > stale.funding_owed(),
            "funding accrues over dt"
        );
        assert!(
            (fresh.funding_owed() - stale.funding_owed()).atoms() < 5_000_000,
            "dt is ~1.6h"
        );
        // Utilization also accrues.
        assert!(fresh.util_earnings().short >= stale.util_earnings().short);
    }

    /// A what-if mark applied AFTER the accrual replay changes only the
    /// pricing legs. Applying it before the replay would also reprice the
    /// elapsed utilization accrual (`accrue` scales the utilization legs
    /// by the mark), which is not what a what-if mark means.
    #[test]
    fn what_if_mark_leaves_the_accrual_replay_untouched() {
        let (market, accrual, maker) = golden_market_and_maker();
        let doubled_mark = Price::from_x96(market.mark.x96() * U256::from(2u8));

        let at_chain_mark = market.accrued(&accrual).unwrap();
        let chain = at_chain_mark.maker_equity(&maker).unwrap();
        let what_if = at_chain_mark
            .with_mark(doubled_mark)
            .maker_equity(&maker)
            .unwrap();

        assert_eq!(what_if.funding_owed(), chain.funding_owed());
        assert_eq!(what_if.util_earnings(), chain.util_earnings());
        assert_eq!(what_if.lp_fees(), chain.lp_fees());
        assert_ne!(
            what_if.unrealized_pnl(),
            chain.unrealized_pnl(),
            "the what-if mark must reprice valPnl"
        );

        // The wrong order (override before the replay) moves the
        // utilization legs — that is the regression this test pins.
        let accrued_at_doubled = MakerMarketSnapshot {
            mark: doubled_mark,
            ..market
        }
        .accrued(&accrual)
        .unwrap()
        .maker_equity(&maker)
        .unwrap();
        assert_ne!(
            accrued_at_doubled.util_earnings().short,
            chain.util_earnings().short
        );
    }

    /// `valPnl` is a SIGNED sum (`liquidityVal + delta0·mark/Q96 + delta1`),
    /// not `liquidityVal − (|delta0|·mark/Q96 + |delta1|)`. The golden vector
    /// has both delta legs negative, where the two formulas coincide — a
    /// mixed-sign delta tells them apart (here they differ by 2·|delta1| =
    /// 110 USD).
    #[test]
    fn val_pnl_is_a_signed_sum_over_mixed_sign_deltas() {
        let (market, accrual, mut maker) = golden_market_and_maker();
        let market = market.accrued(&accrual).unwrap();

        // With zero deltas, unrealized PnL is exactly the band's liquidity
        // value priced at the mark.
        maker.delta_perp = PerpDelta::ZERO;
        maker.delta_usd = UsdcDelta::ZERO;
        let liquidity_val = market.maker_equity(&maker).unwrap().unrealized_pnl().usdc();

        maker.delta_perp = PerpDelta::new(-30_000_000); // −30 perp
        maker.delta_usd = UsdcDelta::new(55_000_000); // +55 USD
        let b = market.maker_equity(&maker).unwrap();

        let mark = market.snapshot().mark.to_f64().unwrap();
        let expected = liquidity_val + (-30.0 * mark + 55.0);
        assert!(
            (b.unrealized_pnl().usdc() - expected).abs() < 1e-3,
            "unrealized {} expected {expected}",
            b.unrealized_pnl().usdc()
        );
    }

    #[test]
    fn out_of_range_ticks_surface_as_errors_not_panics() {
        let (market, accrual, mut maker) = golden_market_and_maker();
        maker.tick_upper = 1_000_000; // beyond the Uniswap tick domain
        assert!(
            market
                .accrued(&accrual)
                .unwrap()
                .maker_equity(&maker)
                .is_err()
        );
    }

    /// `makerCumlFunding` branch checks with hand-computed values. The
    /// golden vector only exercises the below-range branch (current tick
    /// under both band ticks); these pin the other placements against the
    /// contract's formulas:
    ///
    /// - `current >= tick`: the tick's checkpoint IS the below-side value;
    /// - `current < tick`: below-side = market cumulative − checkpoint.
    #[test]
    fn maker_cuml_funding_branches_match_the_contract() {
        let i = |v: i64| I256::try_from(v).unwrap();
        let f = |v: i64| Funding::from_x96(i(v));
        let d = |v: i64| FundingPerSqrtPrice::from_x96(i(v));
        let market_at = |tick: i32| MakerMarketSnapshot {
            block: BlockContext::default(),
            funding: f(1000),
            funding_div_sqrt_p: d(500),
            util_earnings: PerSide::default(),
            tick,
            sqrt_price: SqrtPrice::from_x96(Q96),
            mark: Price::from_x96(Q96),
            liquidation_fee: Ratio::ZERO,
        };
        let (_, _, mut maker) = golden_market_and_maker();
        maker.tick_lower = 0;
        maker.tick_upper = 100;
        maker.tick_lower_funding = TickFunding {
            cuml_funding_opp: f(30),
            cuml_funding_div_sqrt_p_opp: d(7),
        };
        maker.tick_upper_funding = TickFunding {
            cuml_funding_opp: f(20),
            cuml_funding_div_sqrt_p_opp: d(3),
        };

        // Above range: both checkpoints are already below-side values.
        // (below, within, divWithin) = (Lo, Uo − Lo, Ud − Ld).
        assert_eq!(
            market_at(150).maker_cuml_funding(&maker).unwrap(),
            (f(30), f(-10), d(-4))
        );
        // Inside range: the upper tick flips to (F − Uo, D − Ud).
        assert_eq!(
            market_at(50).maker_cuml_funding(&maker).unwrap(),
            (f(30), f(1000 - 20 - 30), d(500 - 3 - 7))
        );
        // Below range: both flip — within collapses to checkpoint deltas.
        assert_eq!(
            market_at(-50).maker_cuml_funding(&maker).unwrap(),
            (f(1000 - 30), f(10), d(4))
        );
    }

    #[test]
    fn mis_ordered_ticks_are_rejected() {
        let (market, accrual, mut maker) = golden_market_and_maker();
        std::mem::swap(&mut maker.tick_lower, &mut maker.tick_upper);
        assert!(matches!(
            market.accrued(&accrual).unwrap().maker_equity(&maker),
            Err(ValidationError::InvalidTickRange { .. })
        ));
    }

    /// A position checkpoint AHEAD of the market cumulative is mutually
    /// inconsistent state (a stale-replica read, or corrupt inputs). The
    /// unsigned subtraction must surface it as an error — ruint's `Sub`
    /// wraps in release, which would fabricate an astronomical earnings
    /// delta instead.
    #[test]
    fn checkpoint_ahead_of_market_cumulative_is_an_error_not_a_number() {
        let (market, accrual, mut maker) = golden_market_and_maker();
        let market = market.accrued(&accrual).unwrap();
        maker.last_util_earnings.long =
            Earnings::from_x96(market.snapshot().util_earnings.long.x96() + U256::from(1u8));
        let err = market.maker_equity(&maker).unwrap_err();
        assert!(matches!(err, ValidationError::Overflow { .. }), "{err}");
    }

    /// The construction invariant must hold through serde: a round-trip
    /// preserves the value, and an out-of-bound component is rejected at
    /// deserialization instead of poisoning the derived sums.
    #[test]
    fn breakdown_deserialization_enforces_the_component_bound() {
        let (market, accrual, maker) = golden_market_and_maker();
        let market = market.accrued(&accrual).unwrap();
        let b = market.maker_equity(&maker).unwrap();

        let json = serde_json::to_string(&b).unwrap();
        let round_tripped: MakerEquityBreakdown = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, b);

        let out_of_bound = json.replace(
            &format!("\"margin\":{}", b.margin().atoms()),
            &format!("\"margin\":{}", i128::MAX),
        );
        assert_ne!(json, out_of_bound, "replacement must have applied");
        assert!(
            serde_json::from_str::<MakerEquityBreakdown>(&out_of_bound).is_err(),
            "an over-supply component must be rejected at construction"
        );

        let negative_value = json.replace(
            &format!("\"position_value\":{}", b.position_value().atoms()),
            "\"position_value\":-1",
        );
        assert_ne!(json, negative_value, "replacement must have applied");
        assert!(
            serde_json::from_str::<MakerEquityBreakdown>(&negative_value).is_err(),
            "posVal is a uint on chain"
        );
        // The ratio's own domain travels with the ratio: this breakdown no
        // longer checks it, `Ratio` does, wherever one is deserialised.
        let wide_ratio = json.replace(
            "\"liquidation_margin_ratio\":50000",
            "\"liquidation_margin_ratio\":16777216",
        );
        assert_ne!(json, wide_ratio, "replacement must have applied");
        assert!(
            serde_json::from_str::<MakerEquityBreakdown>(&wide_ratio).is_err(),
            "the ratio is a uint24 on chain"
        );
    }

    /// The component bound is the accounting-token supply, not a magic
    /// number.
    #[test]
    fn component_bound_is_the_accounting_supply() {
        assert_eq!(
            U256::from(MAX_COMPONENT_ATOMS),
            crate::constants::ACCOUNTING_TOKEN_SUPPLY
        );
    }

    #[test]
    fn fee_growth_inside_matches_uniswap_branches() {
        let fg = |v: u64| FeeGrowth::from_x128(U256::from(v));
        let (g, ol, ou) = (fg(1000), fg(100), fg(50));
        // In range: inside = global − outsideLower − outsideUpper.
        assert_eq!(fee_growth_inside1(g, ol, ou, -10, 10, 0), fg(850));
        // Below range: below = g − ol, above = ou → inside = ol − ou.
        assert_eq!(fee_growth_inside1(g, ol, ou, -10, 10, -20), fg(50));
        // Above range: below = ol, above = g − ou → inside = ou − ol (wraps).
        assert_eq!(
            fee_growth_inside1(g, ol, ou, -10, 10, 20),
            fg(50).since(fg(100))
        );
    }
}
