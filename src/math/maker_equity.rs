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
//! [`storage`](crate::math::storage)) lives in the client:
//! [`PerpClient::read_maker_equities`](crate::client::PerpClient::read_maker_equities).

use alloy::primitives::{I256, U256, U512};
use serde::{Deserialize, Serialize};

use crate::constants::{INTERVAL, Q96, WAD};
use crate::convert::scale_from_6dec;
use crate::errors::ValidationError;
use crate::math::BlockContext;
use crate::math::fixed_point::{Rounding, mul_div, s_full_mul_div, u512_to_u256};
use crate::math::liquidity::amounts_for_liquidity;
use crate::math::swap::amount0_delta;
use crate::math::tick::get_sqrt_ratio_at_tick;

/// One `TickInfo` from the Perp's tick funding mapping (`s.ticks[tick]`),
/// fields named after the contract's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickFunding {
    /// `TickInfo.cumlFundingOppX96`: cumulative funding checkpointed on the
    /// opposite side of the tick (X96), signed.
    pub cuml_funding_opp_x96: I256,
    /// `TickInfo.cumlFundingDivSqrtPOppX96`: cumulative funding divided by
    /// sqrt price, checkpointed on the opposite side of the tick (X96),
    /// signed.
    pub cuml_funding_div_sqrt_p_opp_x96: I256,
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
    /// Cumulative funding (X96), signed.
    pub funding_x96: I256,
    /// Cumulative funding divided by sqrt price (X96), signed.
    pub funding_div_sqrt_p_x96: I256,
    /// Cumulative long utilization earnings (X96).
    pub long_util_earnings_x96: U256,
    /// Cumulative short utilization earnings (X96).
    pub short_util_earnings_x96: U256,
    /// Current pool tick.
    pub tick: i32,
    /// Current AMM Q64.96 square-root price.
    pub sqrt_price_x96: U256,
    /// Mark price (X96) — prices `valPnl` and the accrual replay.
    pub mark_price_x96: U256,
}

/// Raw rates + accrual context for replaying `accrue()` from `lastTouch` to
/// `now`: the on-chain cumulatives are only current as of the last touch.
/// Fields named after the contract's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccrualInputs {
    /// `rates().fundingPerDay` (int88): daily funding rate scaled by 1e18.
    pub funding_per_day_wad: i128,
    /// `rates().longUtilFeePerDay`: daily long-utilization fee rate scaled
    /// by 1e18.
    pub long_util_fee_per_day_wad: u64,
    /// `rates().shortUtilFeePerDay`: daily short-utilization fee rate
    /// scaled by 1e18.
    pub short_util_fee_per_day_wad: u64,
    /// `rates().lastTouch`: timestamp the cumulatives were last advanced.
    pub last_touch: u64,
    /// Timestamp to accrue to — the snapshot block's timestamp.
    pub now: u64,
    /// `openInterest().long`, 6-decimal perp atoms.
    pub oi_long: u128,
    /// `openInterest().short`, 6-decimal perp atoms.
    pub oi_short: u128,
    /// `capacity().long`, 6-decimal perp atoms.
    pub cap_long: u128,
    /// `capacity().short`, 6-decimal perp atoms.
    pub cap_short: u128,
}

/// Per-position inputs: the position row, maker row, its band's tick funding
/// checkpoints, and the V4 fee-growth state for its liquidity position.
/// Fields named after the contract's.
///
/// `tick_lower < tick_upper` is required; [`MakerMarketSnapshot::maker_equity`]
/// validates the ordering (and the Uniswap tick domain) before computing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MakerState {
    /// `positions(id).margin`: last-settled margin, 6-decimal USDC atoms.
    pub margin_6dec: u128,
    /// `positions(id).delta` amount0 (perp atoms), unpacked from the packed
    /// `BalanceDelta`. Negative = owed to the pool.
    pub delta_amount0: i128,
    /// `positions(id).delta` amount1 (USD atoms), unpacked from the packed
    /// `BalanceDelta`. Negative = owed to the pool.
    pub delta_amount1: i128,
    /// `positions(id).lastCumlFundingX96`: market funding cumulative at the
    /// position's last settle.
    pub last_cuml_funding_x96: I256,
    /// `makerDetails(id).tickLower`: band lower tick.
    pub tick_lower: i32,
    /// `makerDetails(id).tickUpper`: band upper tick.
    pub tick_upper: i32,
    /// `makerDetails(id).liquidity`: V4 liquidity in the band.
    pub liquidity: u128,
    /// `makerDetails(id).lastLongUtilEarningsX96`: long utilization
    /// earnings cumulative at the last settle.
    pub last_long_util_earnings_x96: U256,
    /// `makerDetails(id).lastShortUtilEarningsX96`: short utilization
    /// earnings cumulative at the last settle.
    pub last_short_util_earnings_x96: U256,
    /// `makerDetails(id).capacity.long`, 6-decimal perp atoms.
    pub cap_long_6dec: u128,
    /// `makerDetails(id).capacity.short`, 6-decimal perp atoms.
    pub cap_short_6dec: u128,
    /// `makerDetails(id).lastCumlFunding.belowX96`: below-band funding
    /// cumulative at the last settle.
    pub last_below_x96: I256,
    /// `makerDetails(id).lastCumlFunding.withinX96`: within-band funding
    /// cumulative at the last settle.
    pub last_within_x96: I256,
    /// `makerDetails(id).lastCumlFunding.divSqrtPriceWithinX96`:
    /// within-band funding/sqrtP cumulative at the last settle.
    pub last_div_sqrt_within_x96: I256,
    /// `ticks[tickLower]`: the lower tick's live funding checkpoints.
    pub tick_lower_funding: TickFunding,
    /// `ticks[tickUpper]`: the upper tick's live funding checkpoints.
    pub tick_upper_funding: TickFunding,
    /// V4 `feeGrowthInside1X128` of the band now (X128; wraps by design).
    pub fee_growth_inside1_x128: U256,
    /// V4 `feeGrowthInside1LastX128` at the position's last checkpoint
    /// (X128; wraps by design).
    pub fee_growth_inside1_last_x128: U256,
}

/// What the contract would settle if the position were touched now.
///
/// The primary representation is exact signed 6-decimal USDC atoms —
/// the integer units the contract settles in. The `*_usd` accessors
/// convert to `f64` at the display boundary.
///
/// Every component is bounded to ±2^124 atoms at construction (far beyond
/// the protocol's 2^120-atom accounting supply), so the derived sums
/// ([`Self::settled_margin_atoms`], [`Self::equity_atoms`],
/// [`Self::accrued_income_atoms`]) can never overflow `i128`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MakerEquityBreakdown {
    /// Last-settled margin (`positions(id).margin`), in atoms.
    pub margin_atoms: i128,
    /// Accrued funding atoms since the last settle (positive = position
    /// pays).
    pub funding_atoms: i128,
    /// Accrued utilization earnings atoms, long side.
    pub long_util_earnings_atoms: i128,
    /// Accrued utilization earnings atoms, short side.
    pub short_util_earnings_atoms: i128,
    /// Uncollected V4 LP fee atoms (donated taker fees).
    pub lp_fees_atoms: i128,
    /// `valPnl` atoms: current band value minus the recorded deposit value,
    /// both priced at the mark.
    pub unrealized_pnl_atoms: i128,
}

impl MakerEquityBreakdown {
    /// Last-settled margin in USD.
    pub fn margin_usd(&self) -> f64 {
        scale_from_6dec(self.margin_atoms)
    }

    /// Accrued funding in USD (positive = position pays).
    pub fn funding_usd(&self) -> f64 {
        scale_from_6dec(self.funding_atoms)
    }

    /// Accrued long utilization earnings in USD.
    pub fn long_util_earnings_usd(&self) -> f64 {
        scale_from_6dec(self.long_util_earnings_atoms)
    }

    /// Accrued short utilization earnings in USD.
    pub fn short_util_earnings_usd(&self) -> f64 {
        scale_from_6dec(self.short_util_earnings_atoms)
    }

    /// Uncollected V4 LP fees in USD.
    pub fn lp_fees_usd(&self) -> f64 {
        scale_from_6dec(self.lp_fees_atoms)
    }

    /// Unrealized inventory PnL in USD.
    pub fn unrealized_pnl_usd(&self) -> f64 {
        scale_from_6dec(self.unrealized_pnl_atoms)
    }

    /// Margin atoms as the contract would settle them now.
    pub fn settled_margin_atoms(&self) -> i128 {
        self.margin_atoms - self.funding_atoms
            + self.long_util_earnings_atoms
            + self.short_util_earnings_atoms
            + self.lp_fees_atoms
    }

    /// Margin as the contract would settle it now, in USD.
    pub fn settled_margin(&self) -> f64 {
        scale_from_6dec(self.settled_margin_atoms())
    }

    /// Settled margin plus inventory PnL — the position's live equity, in
    /// atoms.
    pub fn equity_atoms(&self) -> i128 {
        self.settled_margin_atoms() + self.unrealized_pnl_atoms
    }

    /// Settled margin plus inventory PnL — the position's live equity, in
    /// USD.
    pub fn equity(&self) -> f64 {
        scale_from_6dec(self.equity_atoms())
    }

    /// Accrued income atoms alone (what the position earned since its last
    /// settle).
    pub fn accrued_income_atoms(&self) -> i128 {
        self.long_util_earnings_atoms + self.short_util_earnings_atoms + self.lp_fees_atoms
            - self.funding_atoms
    }

    /// Accrued income alone, in USD.
    pub fn accrued_income(&self) -> f64 {
        scale_from_6dec(self.accrued_income_atoms())
    }
}

impl MakerMarketSnapshot {
    /// Replay `PerpLogic.accrue` from `lastTouch` to `now`, returning the
    /// snapshot with its cumulatives advanced. Mirrors the contract exactly,
    /// using [`Self::mark_price_x96`] for the utilization leg (the contract
    /// recomputes the mark inside `accrue`; passing the current mark keeps
    /// the replay within micro-dollars).
    ///
    /// Consumes `self`: the replay is not idempotent (each application adds
    /// another rate × dt), so the un-accrued snapshot is given up rather
    /// than left around to be accrued twice.
    pub fn accrued(mut self, accrual: &AccrualInputs) -> Result<Self, ValidationError> {
        let dt = accrual.now.saturating_sub(accrual.last_touch);
        if dt == 0 {
            return Ok(self);
        }
        let dt_days = U256::from(dt)
            .checked_mul(Q96)
            .ok_or(ValidationError::Overflow {
                context: "accrual dt in days".into(),
            })?
            / U256::from(INTERVAL);
        let funding_accrued = s_full_mul_div(
            I256::try_from(accrual.funding_per_day_wad).expect("i128 fits I256"),
            to_i256(dt_days)?,
            WAD,
            Rounding::TowardZero,
        )?;
        self.funding_x96 = add_i(
            self.funding_x96,
            funding_accrued,
            "accrued funding cumulative",
        )?;
        self.funding_div_sqrt_p_x96 = add_i(
            self.funding_div_sqrt_p_x96,
            s_full_mul_div(
                funding_accrued,
                I256::from_raw(Q96),
                self.sqrt_price_x96,
                Rounding::TowardZero,
            )?,
            "accrued funding/sqrtP cumulative",
        )?;

        let dt_days_mult_mark = mul_div(dt_days, self.mark_price_x96, Q96, Rounding::TowardZero)?;
        let lu_accrued = mul_div(
            U256::from(accrual.long_util_fee_per_day_wad),
            dt_days_mult_mark,
            WAD,
            Rounding::TowardZero,
        )?;
        let su_accrued = mul_div(
            U256::from(accrual.short_util_fee_per_day_wad),
            dt_days_mult_mark,
            WAD,
            Rounding::TowardZero,
        )?;
        if accrual.cap_long != 0 {
            self.long_util_earnings_x96 = add_u(
                self.long_util_earnings_x96,
                mul_div(
                    lu_accrued,
                    U256::from(accrual.oi_long),
                    U256::from(accrual.cap_long),
                    Rounding::TowardZero,
                )?,
                "accrued long utilization cumulative",
            )?;
        }
        if accrual.cap_short != 0 {
            self.short_util_earnings_x96 = add_u(
                self.short_util_earnings_x96,
                mul_div(
                    su_accrued,
                    U256::from(accrual.oi_short),
                    U256::from(accrual.cap_short),
                    Rounding::TowardZero,
                )?,
                "accrued short utilization cumulative",
            )?;
        }
        Ok(self)
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
        let (mf_below, mf_within, mf_div_sqrt_within) = self.maker_cuml_funding(maker)?;

        // ── makerFeesAccrued ────────────────────────────────────────────
        let base_funding = s_full_mul_div(
            I256::try_from(maker.delta_amount0).expect("i128 fits I256"),
            sub_i(
                self.funding_x96,
                maker.last_cuml_funding_x96,
                "funding cumulative delta",
            )?,
            Q96,
            Rounding::Up,
        )?;
        let perp_below = to_i256(amount0_delta(
            sqrt_l,
            sqrt_u,
            maker.liquidity,
            Rounding::TowardZero,
        )?)?;
        let funding_below = s_full_mul_div(
            perp_below,
            sub_i(mf_below, maker.last_below_x96, "below-band funding delta")?,
            Q96,
            Rounding::Up,
        )?;
        let div_amm = sub_i(
            mf_div_sqrt_within,
            maker.last_div_sqrt_within_x96,
            "within-band funding/sqrtP delta",
        )?;
        let d_within = sub_i(
            mf_within,
            maker.last_within_x96,
            "within-band funding delta",
        )?;
        let div_upper =
            s_full_mul_div(d_within, I256::from_raw(Q96), sqrt_u, Rounding::TowardZero)?;
        let funding_within = s_full_mul_div(
            I256::try_from(maker.liquidity).expect("u128 fits I256"),
            sub_i(div_amm, div_upper, "within-band funding components")?,
            Q96,
            Rounding::Up,
        )?;
        let funding = add_i(
            add_i(base_funding, funding_below, "accrued funding")?,
            funding_within,
            "accrued funding",
        )?;

        let long_util = mul_div(
            U256::from(maker.cap_long_6dec),
            sub_u(
                self.long_util_earnings_x96,
                maker.last_long_util_earnings_x96,
                "long utilization checkpoint ahead of market cumulative",
            )?,
            Q96,
            Rounding::TowardZero,
        )?;
        let short_util = mul_div(
            U256::from(maker.cap_short_6dec),
            sub_u(
                self.short_util_earnings_x96,
                maker.last_short_util_earnings_x96,
                "short utilization checkpoint ahead of market cumulative",
            )?,
            Q96,
            Rounding::TowardZero,
        )?;

        // ── V4 LP fees: liquidity × Δ feeGrowthInside1 / 2^128 ──────────
        // Fee growth deltas wrap by design in Uniswap; wrapping_sub matches.
        let fee_growth_delta = maker
            .fee_growth_inside1_x128
            .wrapping_sub(maker.fee_growth_inside1_last_x128);
        let lp_fees =
            u512_to_u256((U512::from(maker.liquidity) * U512::from(fee_growth_delta)) >> 128)?;

        // ── valPnl (maker overload) ─────────────────────────────────────
        // A SIGNED sum, per `PerpLogic.valPnl` (`perpcity-contracts@4bbe554f`):
        //   unrealizedPnl = liquidityVal.toInt256()
        //       + delta.amount0().sFullMulDiv(markP.toInt256(), Q96, false)
        //       + delta.amount1();
        // (For an open maker both deltas are usually negative — owed to the
        // pool — but a mixed-sign delta must not collapse to magnitudes.)
        let (perps, usd) =
            amounts_for_liquidity(self.sqrt_price_x96, sqrt_l, sqrt_u, maker.liquidity)?;
        let liquidity_val = add_u(
            mul_div(perps, self.mark_price_x96, Q96, Rounding::TowardZero)?,
            usd,
            "band liquidity value",
        )?;
        let residual_val = add_i(
            s_full_mul_div(
                I256::try_from(maker.delta_amount0).expect("i128 fits I256"),
                to_i256(self.mark_price_x96)?,
                Q96,
                Rounding::TowardZero,
            )?,
            I256::try_from(maker.delta_amount1).expect("i128 fits I256"),
            "deposit residual value",
        )?;
        let unrealized = add_i(to_i256(liquidity_val)?, residual_val, "unrealized PnL")?;

        Ok(MakerEquityBreakdown {
            margin_atoms: atoms(to_i256(U256::from(maker.margin_6dec))?, "margin")?,
            funding_atoms: atoms(funding, "accrued funding")?,
            long_util_earnings_atoms: atoms(to_i256(long_util)?, "long utilization earnings")?,
            short_util_earnings_atoms: atoms(to_i256(short_util)?, "short utilization earnings")?,
            lp_fees_atoms: atoms(to_i256(lp_fees)?, "LP fees")?,
            unrealized_pnl_atoms: atoms(unrealized, "unrealized PnL")?,
        })
    }

    /// `PerpLogic.makerCumlFunding`: assemble the band's cumulative funding
    /// (below / within / within-div-sqrtP) from the two ticks' opposite-side
    /// checkpoints, branching on which side of each tick the current tick is.
    fn maker_cuml_funding(
        &self,
        maker: &MakerState,
    ) -> Result<(I256, I256, I256), ValidationError> {
        let lower = &maker.tick_lower_funding;
        let upper = &maker.tick_upper_funding;

        let (below, div_below_lower) = if self.tick >= maker.tick_lower {
            (
                lower.cuml_funding_opp_x96,
                lower.cuml_funding_div_sqrt_p_opp_x96,
            )
        } else {
            (
                sub_i(
                    self.funding_x96,
                    lower.cuml_funding_opp_x96,
                    "lower-tick funding checkpoint",
                )?,
                sub_i(
                    self.funding_div_sqrt_p_x96,
                    lower.cuml_funding_div_sqrt_p_opp_x96,
                    "lower-tick funding/sqrtP checkpoint",
                )?,
            )
        };
        let (below_upper, div_below_upper) = if self.tick >= maker.tick_upper {
            (
                upper.cuml_funding_opp_x96,
                upper.cuml_funding_div_sqrt_p_opp_x96,
            )
        } else {
            (
                sub_i(
                    self.funding_x96,
                    upper.cuml_funding_opp_x96,
                    "upper-tick funding checkpoint",
                )?,
                sub_i(
                    self.funding_div_sqrt_p_x96,
                    upper.cuml_funding_div_sqrt_p_opp_x96,
                    "upper-tick funding/sqrtP checkpoint",
                )?,
            )
        };
        Ok((
            below,
            sub_i(below_upper, below, "within-band cumulative funding")?,
            sub_i(
                div_below_upper,
                div_below_lower,
                "within-band cumulative funding/sqrtP",
            )?,
        ))
    }
}

/// Compute `feeGrowthInside1X128` for a band from the global growth and the
/// two ticks' `feeGrowthOutside1X128`, per Uniswap's `getFeeGrowthInside`.
/// Fee growth arithmetic wraps by design.
pub fn fee_growth_inside1(
    global_x128: U256,
    outside_lower_x128: U256,
    outside_upper_x128: U256,
    tick_lower: i32,
    tick_upper: i32,
    current_tick: i32,
) -> U256 {
    let below = if current_tick >= tick_lower {
        outside_lower_x128
    } else {
        global_x128.wrapping_sub(outside_lower_x128)
    };
    let above = if current_tick < tick_upper {
        outside_upper_x128
    } else {
        global_x128.wrapping_sub(outside_upper_x128)
    };
    global_x128.wrapping_sub(below).wrapping_sub(above)
}

fn to_i256(v: U256) -> Result<I256, ValidationError> {
    I256::try_from(v).map_err(|_| ValidationError::Overflow {
        context: "value exceeds I256".into(),
    })
}

// Chain-derived values must never wrap silently: alloy's `Signed` only
// debug-asserts on overflow and ruint's `Sub` wraps in release, so every
// add/sub on snapshot inputs goes through these checked helpers. An `Err`
// means corrupt or mutually inconsistent inputs (e.g. a position checkpoint
// ahead of the market cumulative), not a value to propagate.

fn add_i(a: I256, b: I256, context: &'static str) -> Result<I256, ValidationError> {
    a.checked_add(b).ok_or(ValidationError::Overflow {
        context: context.into(),
    })
}

fn sub_i(a: I256, b: I256, context: &'static str) -> Result<I256, ValidationError> {
    a.checked_sub(b).ok_or(ValidationError::Overflow {
        context: context.into(),
    })
}

fn add_u(a: U256, b: U256, context: &'static str) -> Result<U256, ValidationError> {
    a.checked_add(b).ok_or(ValidationError::Overflow {
        context: context.into(),
    })
}

fn sub_u(a: U256, b: U256, context: &'static str) -> Result<U256, ValidationError> {
    a.checked_sub(b).ok_or(ValidationError::Overflow {
        context: context.into(),
    })
}

/// Maximum settle-component magnitude accepted into a breakdown: 2^124
/// atoms, comfortably past the protocol's 2^120-atom accounting supply.
/// Bounding here makes the breakdown's i128 component sums provably
/// overflow-free.
const MAX_COMPONENT_ATOMS: i128 = 1 << 124;

/// Narrow a settle component to 6-decimal atoms, erroring — never
/// saturating — when the value exceeds [`MAX_COMPONENT_ATOMS`]. Chain-
/// consistent state stays far inside the bound; exceeding it means corrupt
/// inputs.
fn atoms(v: I256, context: &'static str) -> Result<i128, ValidationError> {
    i128::try_from(v)
        .ok()
        .filter(|a| a.unsigned_abs() <= MAX_COMPONENT_ATOMS as u128)
        .ok_or(ValidationError::Overflow {
            context: context.into(),
        })
}

#[cfg(test)]
mod tests {
    use alloy::primitives::B256;

    use super::*;

    /// Golden vector: CHINA-PC (`0x796f…8ed0`) position 54, chain state at
    /// block 500612175 — the block before its liquidation. The liquidation's
    /// `MakerConverted` event settled at timestamp 1788260191 with funding
    /// 209.633223, longUtil 0.432735, shortUtil 24.630722, lpFees 7.722360.
    /// The accrual replay uses the AMM price as the mark (the contract
    /// recomputes mark inside `accrue`), which costs a few micro-dollars on
    /// the utilization legs over the 5775s replay window.
    fn golden_market_and_maker() -> (MakerMarketSnapshot, AccrualInputs, MakerState) {
        let i = |s: &str| I256::from_dec_str(s).unwrap();
        let u = |s: &str| U256::from_str_radix(s, 10).unwrap();
        let market = MakerMarketSnapshot {
            block: BlockContext {
                number: 500612175,
                hash: B256::ZERO,
                timestamp: 1788260191,
            },
            funding_x96: i("-5817301051923220663714693204286"),
            funding_div_sqrt_p_x96: i("-1332253658657311045256648214058"),
            long_util_earnings_x96: u("361206840527920163630096383165"),
            short_util_earnings_x96: u("512938731932611361114843741066"),
            tick: 28543,
            sqrt_price_x96: u("330115084885190701587787251116"),
            mark_price_x96: u("1375470108235016714305503507110"),
        };
        let accrual = AccrualInputs {
            funding_per_day_wad: 840374978539967329,
            long_util_fee_per_day_wad: 10000000000000000,
            short_util_fee_per_day_wad: 10000000000000000,
            last_touch: 1788254416,
            now: 1788260191,
            oi_long: 2587247,
            oi_short: 175795732,
            cap_long: 303811186,
            cap_short: 223153047,
        };
        let maker = MakerState {
            margin_6dec: 143730198,
            delta_amount0: -134328,
            delta_amount1: -137992489,
            last_cuml_funding_x96: i("-10162710870332004796583430787875"),
            tick_lower: 33810,
            tick_upper: 34710,
            liquidity: 570282387,
            last_long_util_earnings_x96: u("105980308075601242205274025040"),
            last_short_util_earnings_x96: u("79412639757423009537924209956"),
            cap_long_6dec: 134327,
            cap_short_6dec: 4493830,
            last_below_x96: i("-10162710870332004796583430787875"),
            last_within_x96: I256::ZERO,
            last_div_sqrt_within_x96: I256::ZERO,
            tick_lower_funding: TickFunding {
                cuml_funding_opp_x96: i("2413781515094096341489935830192"),
                cuml_funding_div_sqrt_p_opp_x96: i("440051787484224301957495580026"),
            },
            tick_upper_funding: TickFunding::default(),
            fee_growth_inside1_x128: fee_growth_inside1(
                u("28998515790711655837734081581084912609"),
                u("4607862979514044838473387691959359354"),
                U256::ZERO,
                33810,
                34710,
                28543,
            ),
            fee_growth_inside1_last_x128: U256::ZERO,
        };
        (market, accrual, maker)
    }

    #[test]
    fn golden_vector_reproduces_pos54_liquidation_settle() {
        let (mut market, accrual, maker) = golden_market_and_maker();
        market = market.accrued(&accrual).unwrap();
        let b = market.maker_equity(&maker).unwrap();

        // The funding replay lands within one atom of the event's exact
        // 209_633_223 (the settle's own rounding happens at a different
        // cumulative granularity); the short-util leg is priced with the
        // caller's mark instead of the mark `accrue` recomputes, costing a
        // few atoms over the 5775s window. The unreplayed legs are exact.
        assert!(
            (b.funding_atoms - 209_633_223).abs() <= 1,
            "funding {}",
            b.funding_atoms
        );
        assert_eq!(b.long_util_earnings_atoms, 432_735);
        assert!(
            (b.short_util_earnings_atoms - 24_630_722).abs() <= 10,
            "short util {}",
            b.short_util_earnings_atoms
        );
        assert_eq!(b.lp_fees_atoms, 7_722_360, "lp {}", b.lp_fees_usd());

        // The position was fee-insolvent (the contracts#292 wedge): equity
        // deeply negative, dominated by accrued funding + inventory loss.
        assert_eq!(b.margin_atoms, 143_730_198);
        assert!(
            b.equity() < -80.0 && b.equity() > -110.0,
            "equity {}",
            b.equity()
        );
        assert!(b.accrued_income_atoms() < 0);
    }

    /// Without the accrual replay the funding is stale to lastTouch — the
    /// replay must move it by the rate × dt amount, not by orders of
    /// magnitude.
    #[test]
    fn accrual_replay_moves_funding_forward() {
        let (market, accrual, maker) = golden_market_and_maker();
        let stale = market.maker_equity(&maker).unwrap();
        let market = market.accrued(&accrual).unwrap();
        let fresh = market.maker_equity(&maker).unwrap();
        assert!(
            fresh.funding_atoms > stale.funding_atoms,
            "funding accrues over dt"
        );
        assert!(
            fresh.funding_atoms - stale.funding_atoms < 5_000_000,
            "dt is ~1.6h"
        );
        // Utilization also accrues.
        assert!(fresh.short_util_earnings_atoms >= stale.short_util_earnings_atoms);
    }

    /// `valPnl` is a SIGNED sum (`liquidityVal + delta0·mark/Q96 + delta1`),
    /// not `liquidityVal − (|delta0|·mark/Q96 + |delta1|)`. The golden vector
    /// has both delta legs negative, where the two formulas coincide — a
    /// mixed-sign delta tells them apart (here they differ by 2·|delta1| =
    /// 110 USD).
    #[test]
    fn val_pnl_is_a_signed_sum_over_mixed_sign_deltas() {
        let (mut market, accrual, mut maker) = golden_market_and_maker();
        market = market.accrued(&accrual).unwrap();

        // With zero deltas, unrealized PnL is exactly the band's liquidity
        // value priced at the mark.
        maker.delta_amount0 = 0;
        maker.delta_amount1 = 0;
        let liquidity_val = market.maker_equity(&maker).unwrap().unrealized_pnl_usd();

        maker.delta_amount0 = -30_000_000; // −30 perp
        maker.delta_amount1 = 55_000_000; // +55 USD
        let b = market.maker_equity(&maker).unwrap();

        let mark = crate::convert::price_x96_to_f64(market.mark_price_x96).unwrap();
        let expected = liquidity_val + (-30.0 * mark + 55.0);
        assert!(
            (b.unrealized_pnl_usd() - expected).abs() < 1e-3,
            "unrealized {} expected {expected}",
            b.unrealized_pnl_usd()
        );
    }

    #[test]
    fn out_of_range_ticks_surface_as_errors_not_panics() {
        let (market, _, mut maker) = golden_market_and_maker();
        maker.tick_upper = 1_000_000; // beyond the Uniswap tick domain
        assert!(market.maker_equity(&maker).is_err());
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
        let market_at = |tick: i32| MakerMarketSnapshot {
            block: BlockContext::default(),
            funding_x96: i(1000),
            funding_div_sqrt_p_x96: i(500),
            long_util_earnings_x96: U256::ZERO,
            short_util_earnings_x96: U256::ZERO,
            tick,
            sqrt_price_x96: Q96,
            mark_price_x96: Q96,
        };
        let (_, _, mut maker) = golden_market_and_maker();
        maker.tick_lower = 0;
        maker.tick_upper = 100;
        maker.tick_lower_funding = TickFunding {
            cuml_funding_opp_x96: i(30),
            cuml_funding_div_sqrt_p_opp_x96: i(7),
        };
        maker.tick_upper_funding = TickFunding {
            cuml_funding_opp_x96: i(20),
            cuml_funding_div_sqrt_p_opp_x96: i(3),
        };

        // Above range: both checkpoints are already below-side values.
        // (below, within, divWithin) = (Lo, Uo − Lo, Ud − Ld).
        assert_eq!(
            market_at(150).maker_cuml_funding(&maker).unwrap(),
            (i(30), i(-10), i(-4))
        );
        // Inside range: the upper tick flips to (F − Uo, D − Ud).
        assert_eq!(
            market_at(50).maker_cuml_funding(&maker).unwrap(),
            (i(30), i(1000 - 20 - 30), i(500 - 3 - 7))
        );
        // Below range: both flip — within collapses to checkpoint deltas.
        assert_eq!(
            market_at(-50).maker_cuml_funding(&maker).unwrap(),
            (i(1000 - 30), i(10), i(4))
        );
    }

    #[test]
    fn mis_ordered_ticks_are_rejected() {
        let (market, _, mut maker) = golden_market_and_maker();
        std::mem::swap(&mut maker.tick_lower, &mut maker.tick_upper);
        assert!(matches!(
            market.maker_equity(&maker),
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
        let (mut market, accrual, mut maker) = golden_market_and_maker();
        market = market.accrued(&accrual).unwrap();
        maker.last_long_util_earnings_x96 = market.long_util_earnings_x96 + U256::from(1u8);
        let err = market.maker_equity(&maker).unwrap_err();
        assert!(matches!(err, ValidationError::Overflow { .. }), "{err}");
    }

    #[test]
    fn fee_growth_inside_matches_uniswap_branches() {
        let g = U256::from(1000u64);
        let ol = U256::from(100u64);
        let ou = U256::from(50u64);
        // In range: inside = global − outsideLower − outsideUpper.
        assert_eq!(
            fee_growth_inside1(g, ol, ou, -10, 10, 0),
            U256::from(850u64)
        );
        // Below range: below = g − ol, above = ou → inside = ol − ou.
        assert_eq!(
            fee_growth_inside1(g, ol, ou, -10, 10, -20),
            U256::from(50u64)
        );
        // Above range: below = ol, above = g − ou → inside = ou − ol (wraps).
        assert_eq!(
            fee_growth_inside1(g, ol, ou, -10, 10, 20),
            U256::from(50u64).wrapping_sub(U256::from(100u64))
        );
    }
}
