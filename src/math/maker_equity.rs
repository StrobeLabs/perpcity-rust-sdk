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
//! `math::storage`) lives in the client:
//! [`PerpClient::get_maker_equities`](crate::client::PerpClient::get_maker_equities).

use alloy::primitives::{I256, U256, U512};
use serde::{Deserialize, Serialize};

use crate::constants::{ACCOUNTING_TOKEN_SUPPLY, INTERVAL, Q96, WAD};
use crate::convert::scale_from_6dec;
use crate::errors::ValidationError;
use crate::math::BlockContext;
use crate::math::fixed_point::{
    Rounding, add_i, add_u, mul_div, s_full_mul_div, sub_i, sub_u, to_i256, u512_to_u256,
};
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
    /// Timestamp to accrue to — the snapshot block's timestamp, never a
    /// wall clock (a local clock ahead of the chain fabricates accrual;
    /// one behind erases it).
    pub accrue_to: u64,
    /// `openInterest().long`, 6-decimal perp atoms.
    pub oi_long_atoms: u128,
    /// `openInterest().short`, 6-decimal perp atoms.
    pub oi_short_atoms: u128,
    /// `capacity().long`, 6-decimal perp atoms.
    pub cap_long_atoms: u128,
    /// `capacity().short`, 6-decimal perp atoms.
    pub cap_short_atoms: u128,
}

/// Per-position inputs: the position row, maker row, its band's tick funding
/// checkpoints, and the V4 fee-growth state for its liquidity position.
/// Fields named after the contract's.
///
/// `tick_lower < tick_upper` is required; [`AccruedMakerSnapshot::maker_equity`]
/// validates the ordering (and the Uniswap tick domain) before computing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MakerState {
    /// `positions(id).margin`: last-settled margin, 6-decimal USDC atoms.
    pub margin_atoms: u128,
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
    pub cap_long_atoms: u128,
    /// `makerDetails(id).capacity.short`, 6-decimal perp atoms.
    pub cap_short_atoms: u128,
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
/// the integer units the contract settles in. The `*_usd()` accessors
/// convert to `f64` at the display boundary.
///
/// Every component is bounded to ±[`MAX_COMPONENT_ATOMS`] (the protocol's
/// accounting-token supply) at construction — including deserialization,
/// which rejects out-of-bound values — so the derived sums
/// ([`Self::settled_margin_atoms`], [`Self::equity_atoms`],
/// [`Self::accrued_income_atoms`]) can never overflow `i128`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawMakerEquityBreakdown")]
pub struct MakerEquityBreakdown {
    margin_atoms: i128,
    funding_owed_atoms: i128,
    long_util_earnings_atoms: i128,
    short_util_earnings_atoms: i128,
    lp_fees_atoms: i128,
    unrealized_pnl_atoms: i128,
}

/// Deserialization shadow of [`MakerEquityBreakdown`]: identical fields,
/// no invariant. Values enter the real type only through the validating
/// `TryFrom`.
#[derive(Deserialize)]
struct RawMakerEquityBreakdown {
    margin_atoms: i128,
    funding_owed_atoms: i128,
    long_util_earnings_atoms: i128,
    short_util_earnings_atoms: i128,
    lp_fees_atoms: i128,
    unrealized_pnl_atoms: i128,
}

impl TryFrom<RawMakerEquityBreakdown> for MakerEquityBreakdown {
    type Error = ValidationError;

    fn try_from(raw: RawMakerEquityBreakdown) -> Result<Self, ValidationError> {
        let bounded = |v: i128, context: &'static str| {
            if v.unsigned_abs() <= MAX_COMPONENT_ATOMS {
                Ok(v)
            } else {
                Err(ValidationError::Overflow {
                    context: context.into(),
                })
            }
        };
        Ok(Self {
            margin_atoms: bounded(raw.margin_atoms, "deserialized margin")?,
            funding_owed_atoms: bounded(raw.funding_owed_atoms, "deserialized funding")?,
            long_util_earnings_atoms: bounded(
                raw.long_util_earnings_atoms,
                "deserialized long utilization earnings",
            )?,
            short_util_earnings_atoms: bounded(
                raw.short_util_earnings_atoms,
                "deserialized short utilization earnings",
            )?,
            lp_fees_atoms: bounded(raw.lp_fees_atoms, "deserialized LP fees")?,
            unrealized_pnl_atoms: bounded(raw.unrealized_pnl_atoms, "deserialized unrealized PnL")?,
        })
    }
}

impl MakerEquityBreakdown {
    /// Last-settled margin (`positions(id).margin`), in atoms.
    pub fn margin_atoms(&self) -> i128 {
        self.margin_atoms
    }

    /// Funding owed since the last settle, in atoms. **Positive = the
    /// position pays** (it is subtracted when settling margin).
    pub fn funding_owed_atoms(&self) -> i128 {
        self.funding_owed_atoms
    }

    /// Accrued utilization earnings atoms, long side.
    pub fn long_util_earnings_atoms(&self) -> i128 {
        self.long_util_earnings_atoms
    }

    /// Accrued utilization earnings atoms, short side.
    pub fn short_util_earnings_atoms(&self) -> i128 {
        self.short_util_earnings_atoms
    }

    /// Uncollected V4 LP fee atoms (donated taker fees).
    pub fn lp_fees_atoms(&self) -> i128 {
        self.lp_fees_atoms
    }

    /// `valPnl` atoms: current band value minus the recorded deposit value,
    /// both priced at the mark.
    pub fn unrealized_pnl_atoms(&self) -> i128 {
        self.unrealized_pnl_atoms
    }

    /// Last-settled margin in USD.
    pub fn margin_usd(&self) -> f64 {
        scale_from_6dec(self.margin_atoms)
    }

    /// Funding owed since the last settle, in USD. **Positive = the
    /// position pays.**
    pub fn funding_owed_usd(&self) -> f64 {
        scale_from_6dec(self.funding_owed_atoms)
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
        self.margin_atoms - self.funding_owed_atoms
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
            - self.funding_owed_atoms
    }

    /// Accrued income alone, in USD.
    pub fn accrued_income(&self) -> f64 {
        scale_from_6dec(self.accrued_income_atoms())
    }
}

impl MakerMarketSnapshot {
    /// Replay `PerpLogic.accrue` from `last_touch` to `accrue_to`,
    /// returning an [`AccruedMakerSnapshot`] with the cumulatives advanced.
    /// Mirrors the contract exactly, using [`Self::mark_price_x96`] for the
    /// utilization leg (the contract recomputes the mark inside `accrue`;
    /// passing the current mark keeps the replay within micro-dollars).
    ///
    /// Consumes `self` and returns a distinct type: the replay is not
    /// idempotent (each application adds another rate × dt), and equities
    /// computed from an un-accrued snapshot would silently be stale to the
    /// market's last touch — so [`AccruedMakerSnapshot::maker_equity`] is
    /// only reachable through this replay.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError::Overflow`] when a replayed cumulative
    /// exceeds its integer domain — chain-consistent rates over a sane dt
    /// stay far in range, so an error indicates corrupt inputs (e.g. a
    /// wall-clock timestamp fed as the accrual target against a stale
    /// `last_touch`).
    pub fn accrued(
        mut self,
        accrual: &AccrualInputs,
    ) -> Result<AccruedMakerSnapshot, ValidationError> {
        let dt = accrual.accrue_to.saturating_sub(accrual.last_touch);
        if dt == 0 {
            return Ok(AccruedMakerSnapshot(self));
        }
        let dt_days = U256::from(dt)
            .checked_mul(Q96)
            .ok_or(ValidationError::Overflow {
                context: "accrual dt in days".into(),
            })?
            / U256::from(INTERVAL);
        let funding_accrued = s_full_mul_div(
            I256::unchecked_from(accrual.funding_per_day_wad),
            to_i256(dt_days, "accrual dt in days")?,
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
        if accrual.cap_long_atoms != 0 {
            self.long_util_earnings_x96 = add_u(
                self.long_util_earnings_x96,
                mul_div(
                    lu_accrued,
                    U256::from(accrual.oi_long_atoms),
                    U256::from(accrual.cap_long_atoms),
                    Rounding::TowardZero,
                )?,
                "accrued long utilization cumulative",
            )?;
        }
        if accrual.cap_short_atoms != 0 {
            self.short_util_earnings_x96 = add_u(
                self.short_util_earnings_x96,
                mul_div(
                    su_accrued,
                    U256::from(accrual.oi_short_atoms),
                    U256::from(accrual.cap_short_atoms),
                    Rounding::TowardZero,
                )?,
                "accrued short utilization cumulative",
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

    /// Reprice this accrued snapshot at a what-if mark (exact X96).
    ///
    /// Only the pricing input changes: the accrual replay already ran at
    /// the mark the chain would have used, so the replayed funding and
    /// utilization cumulatives are untouched. The new mark prices
    /// `valPnl` (the band's liquidity value and the inventory legs) in
    /// every subsequent [`Self::maker_equity`].
    #[must_use]
    pub fn with_mark(mut self, mark_price_x96: U256) -> Self {
        self.0.mark_price_x96 = mark_price_x96;
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
            I256::unchecked_from(maker.delta_amount0),
            sub_i(
                self.0.funding_x96,
                maker.last_cuml_funding_x96,
                "funding cumulative delta",
            )?,
            Q96,
            Rounding::Up,
        )?;
        let perp_below = to_i256(
            amount0_delta(sqrt_l, sqrt_u, maker.liquidity, Rounding::TowardZero)?,
            "band perp amount",
        )?;
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
            I256::unchecked_from(maker.liquidity),
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
            U256::from(maker.cap_long_atoms),
            sub_u(
                self.0.long_util_earnings_x96,
                maker.last_long_util_earnings_x96,
                "long utilization checkpoint ahead of market cumulative",
            )?,
            Q96,
            Rounding::TowardZero,
        )?;
        let short_util = mul_div(
            U256::from(maker.cap_short_atoms),
            sub_u(
                self.0.short_util_earnings_x96,
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
            amounts_for_liquidity(self.0.sqrt_price_x96, sqrt_l, sqrt_u, maker.liquidity)?;
        let liquidity_val = add_u(
            mul_div(perps, self.0.mark_price_x96, Q96, Rounding::TowardZero)?,
            usd,
            "band liquidity value",
        )?;
        let residual_val = add_i(
            s_full_mul_div(
                I256::unchecked_from(maker.delta_amount0),
                to_i256(self.0.mark_price_x96, "mark price")?,
                Q96,
                Rounding::TowardZero,
            )?,
            I256::unchecked_from(maker.delta_amount1),
            "deposit residual value",
        )?;
        let unrealized = add_i(
            to_i256(liquidity_val, "band liquidity value")?,
            residual_val,
            "unrealized PnL",
        )?;

        Ok(MakerEquityBreakdown {
            margin_atoms: atoms(to_i256(U256::from(maker.margin_atoms), "margin")?, "margin")?,
            funding_owed_atoms: atoms(funding, "accrued funding")?,
            long_util_earnings_atoms: atoms(
                to_i256(long_util, "long utilization earnings")?,
                "long utilization earnings",
            )?,
            short_util_earnings_atoms: atoms(
                to_i256(short_util, "short utilization earnings")?,
                "short utilization earnings",
            )?,
            lp_fees_atoms: atoms(to_i256(lp_fees, "LP fees")?, "LP fees")?,
            unrealized_pnl_atoms: atoms(unrealized, "unrealized PnL")?,
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
pub(crate) fn fee_growth_inside1(
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
            accrue_to: 1788260191,
            oi_long_atoms: 2587247,
            oi_short_atoms: 175795732,
            cap_long_atoms: 303811186,
            cap_short_atoms: 223153047,
        };
        let maker = MakerState {
            margin_atoms: 143730198,
            delta_amount0: -134328,
            delta_amount1: -137992489,
            last_cuml_funding_x96: i("-10162710870332004796583430787875"),
            tick_lower: 33810,
            tick_upper: 34710,
            liquidity: 570282387,
            last_long_util_earnings_x96: u("105980308075601242205274025040"),
            last_short_util_earnings_x96: u("79412639757423009537924209956"),
            cap_long_atoms: 134327,
            cap_short_atoms: 4493830,
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
        let (market, accrual, maker) = golden_market_and_maker();
        let market = market.accrued(&accrual).unwrap();
        let b = market.maker_equity(&maker).unwrap();

        // The funding replay lands within one atom of the event's exact
        // 209_633_223 (the settle's own rounding happens at a different
        // cumulative granularity); the short-util leg is priced with the
        // caller's mark instead of the mark `accrue` recomputes, costing a
        // few atoms over the 5775s window. The unreplayed legs are exact.
        assert!(
            (b.funding_owed_atoms() - 209_633_223).abs() <= 1,
            "funding {}",
            b.funding_owed_atoms()
        );
        assert_eq!(b.long_util_earnings_atoms(), 432_735);
        assert!(
            (b.short_util_earnings_atoms() - 24_630_722).abs() <= 10,
            "short util {}",
            b.short_util_earnings_atoms()
        );
        assert_eq!(b.lp_fees_atoms(), 7_722_360, "lp {}", b.lp_fees_usd());

        // The position was fee-insolvent (the contracts#292 wedge): equity
        // deeply negative, dominated by accrued funding + inventory loss.
        assert_eq!(b.margin_atoms(), 143_730_198);
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
            fresh.funding_owed_atoms() > stale.funding_owed_atoms(),
            "funding accrues over dt"
        );
        assert!(
            fresh.funding_owed_atoms() - stale.funding_owed_atoms() < 5_000_000,
            "dt is ~1.6h"
        );
        // Utilization also accrues.
        assert!(fresh.short_util_earnings_atoms() >= stale.short_util_earnings_atoms());
    }

    /// A what-if mark applied AFTER the accrual replay changes only the
    /// pricing legs. Applying it before the replay would also reprice the
    /// elapsed utilization accrual (`accrue` scales the utilization legs
    /// by the mark), which is not what a what-if mark means.
    #[test]
    fn what_if_mark_leaves_the_accrual_replay_untouched() {
        let (market, accrual, maker) = golden_market_and_maker();
        let doubled_mark = market.mark_price_x96 * U256::from(2u8);

        let at_chain_mark = market.accrued(&accrual).unwrap();
        let chain = at_chain_mark.maker_equity(&maker).unwrap();
        let what_if = at_chain_mark
            .with_mark(doubled_mark)
            .maker_equity(&maker)
            .unwrap();

        assert_eq!(what_if.funding_owed_atoms(), chain.funding_owed_atoms());
        assert_eq!(
            what_if.long_util_earnings_atoms(),
            chain.long_util_earnings_atoms()
        );
        assert_eq!(
            what_if.short_util_earnings_atoms(),
            chain.short_util_earnings_atoms()
        );
        assert_eq!(what_if.lp_fees_atoms(), chain.lp_fees_atoms());
        assert_ne!(
            what_if.unrealized_pnl_atoms(),
            chain.unrealized_pnl_atoms(),
            "the what-if mark must reprice valPnl"
        );

        // The wrong order (override before the replay) moves the
        // utilization legs — that is the regression this test pins.
        let accrued_at_doubled = MakerMarketSnapshot {
            mark_price_x96: doubled_mark,
            ..market
        }
        .accrued(&accrual)
        .unwrap()
        .maker_equity(&maker)
        .unwrap();
        assert_ne!(
            accrued_at_doubled.short_util_earnings_atoms(),
            chain.short_util_earnings_atoms()
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
        maker.delta_amount0 = 0;
        maker.delta_amount1 = 0;
        let liquidity_val = market.maker_equity(&maker).unwrap().unrealized_pnl_usd();

        maker.delta_amount0 = -30_000_000; // −30 perp
        maker.delta_amount1 = 55_000_000; // +55 USD
        let b = market.maker_equity(&maker).unwrap();

        let mark = crate::convert::price_x96_to_f64(market.snapshot().mark_price_x96).unwrap();
        let expected = liquidity_val + (-30.0 * mark + 55.0);
        assert!(
            (b.unrealized_pnl_usd() - expected).abs() < 1e-3,
            "unrealized {} expected {expected}",
            b.unrealized_pnl_usd()
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
        maker.last_long_util_earnings_x96 =
            market.snapshot().long_util_earnings_x96 + U256::from(1u8);
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
            &format!("\"margin_atoms\":{}", b.margin_atoms()),
            &format!("\"margin_atoms\":{}", i128::MAX),
        );
        assert_ne!(json, out_of_bound, "replacement must have applied");
        assert!(
            serde_json::from_str::<MakerEquityBreakdown>(&out_of_bound).is_err(),
            "an over-supply component must be rejected at construction"
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
