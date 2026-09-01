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
//! from the Solidity and converted to f64 USD only at the boundary. The
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

use alloy::primitives::{B256, I256, U256, U512};

use crate::constants::{INTERVAL, Q96, WAD};
use crate::errors::ValidationError;
use crate::math::fixed_point::{mul_div, s_full_mul_div, u512_to_u256};
use crate::math::swap::{amount0_delta, amount1_delta};
use crate::math::tick::get_sqrt_ratio_at_tick;

/// One `TickInfo` from the Perp's tick funding mapping.
#[derive(Debug, Clone, Copy, Default)]
#[allow(missing_docs)] // raw chain inputs, named after the contract fields
pub struct TickFunding {
    pub cuml_funding_opp_x96: I256,
    pub cuml_funding_div_sqrt_p_opp_x96: I256,
}

/// Block-pinned market-wide inputs shared by every position's computation.
///
/// The funding/earnings cumulatives are stored on chain as of the market's
/// last touch; call [`Self::accrue`] to replay them to the snapshot's
/// timestamp before computing equities.
#[derive(Debug, Clone, Copy)]
pub struct MakerMarketSnapshot {
    /// Block containing all state in this snapshot.
    pub block_number: u64,
    /// Canonical block hash (zero when the pinned block header was
    /// unavailable and only the number is known).
    pub block_hash: B256,
    /// Block timestamp.
    pub block_timestamp: u64,
    /// Cumulative funding (X96), signed.
    pub funding_x96: I256,
    /// Cumulative funding divided by sqrt price (X96), signed.
    pub funding_div_sqrt_p_x96: I256,
    /// Cumulative long utilization earnings (X96).
    pub long_util_earnings_x96: U256,
    /// Cumulative short utilization earnings (X96).
    pub short_util_earnings_x96: U256,
    /// Current pool tick.
    pub current_tick: i32,
    /// Current AMM sqrt price (X96).
    pub sqrt_amm_price_x96: U256,
    /// Mark price (X96) — prices `valPnl` and the accrual replay.
    pub mark_price_x96: U256,
}

/// Raw rates + accrual context for replaying `accrue()` from `lastTouch` to
/// `now`: the on-chain cumulatives are only current as of the last touch.
#[derive(Debug, Clone, Copy)]
#[allow(missing_docs)] // raw chain inputs, named after the contract fields
pub struct AccrualInputs {
    pub funding_per_day_wad: i128,
    pub long_util_fee_per_day_wad: u64,
    pub short_util_fee_per_day_wad: u64,
    pub last_touch: u64,
    pub now: u64,
    pub oi_long: u128,
    pub oi_short: u128,
    pub cap_long: u128,
    pub cap_short: u128,
}

/// Per-position inputs: the position row, maker row, its band's tick funding
/// checkpoints, and the V4 fee-growth state for its liquidity position.
#[derive(Debug, Clone)]
#[allow(missing_docs)] // raw chain inputs, named after the contract fields
pub struct MakerState {
    pub margin_6dec: u128,
    pub delta_amount0: i128,
    pub delta_amount1: i128,
    pub last_cuml_funding_x96: I256,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: u128,
    pub last_long_util_earnings_x96: U256,
    pub last_short_util_earnings_x96: U256,
    pub cap_long_6dec: u128,
    pub cap_short_6dec: u128,
    pub last_below_x96: I256,
    pub last_within_x96: I256,
    pub last_div_sqrt_within_x96: I256,
    pub tick_lower_funding: TickFunding,
    pub tick_upper_funding: TickFunding,
    /// V4 fee growth of token1 inside the band, now and at the position's
    /// last checkpoint (X128).
    pub fee_growth_inside1_x128: U256,
    pub fee_growth_inside1_last_x128: U256,
}

/// What the contract would settle if the position were touched now, in USD.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MakerEquityBreakdown {
    /// Last-settled margin (`positions(id).margin`).
    pub margin: f64,
    /// Accrued funding since the last settle (positive = position pays).
    pub funding: f64,
    /// Accrued utilization earnings, long side.
    pub long_util_earnings: f64,
    /// Accrued utilization earnings, short side.
    pub short_util_earnings: f64,
    /// Uncollected V4 LP fees (donated taker fees).
    pub lp_fees: f64,
    /// `valPnl`: current band value minus the recorded deposit value, both
    /// priced at the mark.
    pub unrealized_pnl: f64,
}

impl MakerEquityBreakdown {
    /// Margin as the contract would settle it now.
    pub fn settled_margin(&self) -> f64 {
        self.margin - self.funding
            + self.long_util_earnings
            + self.short_util_earnings
            + self.lp_fees
    }

    /// Settled margin plus inventory PnL — the position's live equity.
    pub fn equity(&self) -> f64 {
        self.settled_margin() + self.unrealized_pnl
    }

    /// Accrued income alone (what the position earned since its last settle).
    pub fn accrued_income(&self) -> f64 {
        self.long_util_earnings + self.short_util_earnings + self.lp_fees - self.funding
    }
}

impl MakerMarketSnapshot {
    /// Replay `PerpLogic.accrue` from `lastTouch` to `now`, advancing the
    /// cumulatives in place. Mirrors the contract exactly, using
    /// [`Self::mark_price_x96`] for the utilization leg (the contract
    /// recomputes the mark inside `accrue`; passing the current mark keeps
    /// the replay within micro-dollars).
    pub fn accrue(&mut self, accrual: &AccrualInputs) -> Result<(), ValidationError> {
        let dt = accrual.now.saturating_sub(accrual.last_touch);
        if dt == 0 {
            return Ok(());
        }
        let dt_days = U256::from(dt) * Q96 / U256::from(INTERVAL);
        let funding_accrued = s_full_mul_div(
            I256::try_from(accrual.funding_per_day_wad).expect("i128 fits I256"),
            to_i256(dt_days)?,
            WAD,
            false,
        )?;
        self.funding_x96 += funding_accrued;
        self.funding_div_sqrt_p_x96 += s_full_mul_div(
            funding_accrued,
            I256::from_raw(Q96),
            self.sqrt_amm_price_x96,
            false,
        )?;

        let dt_days_mult_mark = mul_div(dt_days, self.mark_price_x96, Q96, false)?;
        let lu_accrued = mul_div(
            U256::from(accrual.long_util_fee_per_day_wad),
            dt_days_mult_mark,
            WAD,
            false,
        )?;
        let su_accrued = mul_div(
            U256::from(accrual.short_util_fee_per_day_wad),
            dt_days_mult_mark,
            WAD,
            false,
        )?;
        if accrual.cap_long != 0 {
            self.long_util_earnings_x96 += mul_div(
                lu_accrued,
                U256::from(accrual.oi_long),
                U256::from(accrual.cap_long),
                false,
            )?;
        }
        if accrual.cap_short != 0 {
            self.short_util_earnings_x96 += mul_div(
                su_accrued,
                U256::from(accrual.oi_short),
                U256::from(accrual.cap_short),
                false,
            )?;
        }
        Ok(())
    }

    /// Compute the full settle preview for one maker position.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError::InvalidTickRange`] for out-of-range ticks
    /// and [`ValidationError::Overflow`] if an intermediate exceeds its
    /// integer domain — both indicate corrupt inputs, since chain-consistent
    /// state stays in range.
    pub fn maker_equity(
        &self,
        maker: &MakerState,
    ) -> Result<MakerEquityBreakdown, ValidationError> {
        let sqrt_l = get_sqrt_ratio_at_tick(maker.tick_lower)?;
        let sqrt_u = get_sqrt_ratio_at_tick(maker.tick_upper)?;
        let (mf_below, mf_within, mf_div_sqrt_within) = self.maker_cuml_funding(maker);

        // ── makerFeesAccrued ────────────────────────────────────────────
        let base_funding = s_full_mul_div(
            I256::try_from(maker.delta_amount0).expect("i128 fits I256"),
            self.funding_x96 - maker.last_cuml_funding_x96,
            Q96,
            true,
        )?;
        let perp_below = to_i256(amount0_delta(sqrt_l, sqrt_u, maker.liquidity, false)?)?;
        let funding_below = s_full_mul_div(perp_below, mf_below - maker.last_below_x96, Q96, true)?;
        let div_amm = mf_div_sqrt_within - maker.last_div_sqrt_within_x96;
        let d_within = mf_within - maker.last_within_x96;
        let div_upper = s_full_mul_div(d_within, I256::from_raw(Q96), sqrt_u, false)?;
        let funding_within = s_full_mul_div(
            I256::try_from(maker.liquidity).expect("u128 fits I256"),
            div_amm - div_upper,
            Q96,
            true,
        )?;
        let funding = base_funding + funding_below + funding_within;

        let long_util = mul_div(
            U256::from(maker.cap_long_6dec),
            self.long_util_earnings_x96 - maker.last_long_util_earnings_x96,
            Q96,
            false,
        )?;
        let short_util = mul_div(
            U256::from(maker.cap_short_6dec),
            self.short_util_earnings_x96 - maker.last_short_util_earnings_x96,
            Q96,
            false,
        )?;

        // ── V4 LP fees: liquidity × Δ feeGrowthInside1 / 2^128 ──────────
        // Fee growth deltas wrap by design in Uniswap; wrapping_sub matches.
        let fee_growth_delta = maker
            .fee_growth_inside1_x128
            .wrapping_sub(maker.fee_growth_inside1_last_x128);
        let lp_fees =
            u512_to_u256((U512::from(maker.liquidity) * U512::from(fee_growth_delta)) >> 128)?;

        // ── valPnl (maker overload) ─────────────────────────────────────
        let (perps, usd) =
            amounts_for_liquidity(self.sqrt_amm_price_x96, sqrt_l, sqrt_u, maker.liquidity)?;
        let liquidity_val = mul_div(perps, self.mark_price_x96, Q96, false)? + usd;
        let pos_val = mul_div(
            U256::from(maker.delta_amount0.unsigned_abs()),
            self.mark_price_x96,
            Q96,
            false,
        )? + U256::from(maker.delta_amount1.unsigned_abs());
        let unrealized = to_i256(liquidity_val)? - to_i256(pos_val)?;

        Ok(MakerEquityBreakdown {
            margin: maker.margin_6dec as f64 / 1e6,
            funding: usd6_signed(funding),
            long_util_earnings: usd6(long_util),
            short_util_earnings: usd6(short_util),
            lp_fees: usd6(lp_fees),
            unrealized_pnl: usd6_signed(unrealized),
        })
    }

    /// `PerpLogic.makerCumlFunding`: assemble the band's cumulative funding
    /// (below / within / within-div-sqrtP) from the two ticks' opposite-side
    /// checkpoints, branching on which side of each tick the current tick is.
    fn maker_cuml_funding(&self, maker: &MakerState) -> (I256, I256, I256) {
        let lower = &maker.tick_lower_funding;
        let upper = &maker.tick_upper_funding;

        let (below, div_below_lower) = if self.current_tick >= maker.tick_lower {
            (
                lower.cuml_funding_opp_x96,
                lower.cuml_funding_div_sqrt_p_opp_x96,
            )
        } else {
            (
                self.funding_x96 - lower.cuml_funding_opp_x96,
                self.funding_div_sqrt_p_x96 - lower.cuml_funding_div_sqrt_p_opp_x96,
            )
        };
        let (below_upper, div_below_upper) = if self.current_tick >= maker.tick_upper {
            (
                upper.cuml_funding_opp_x96,
                upper.cuml_funding_div_sqrt_p_opp_x96,
            )
        } else {
            (
                self.funding_x96 - upper.cuml_funding_opp_x96,
                self.funding_div_sqrt_p_x96 - upper.cuml_funding_div_sqrt_p_opp_x96,
            )
        };
        (
            below,
            below_upper - below,
            div_below_upper - div_below_lower,
        )
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

/// Uniswap `getAmountsForLiquidity` with the price clamped into the range.
fn amounts_for_liquidity(
    sqrt_p: U256,
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: u128,
) -> Result<(U256, U256), ValidationError> {
    let (sa, sb) = if sqrt_a <= sqrt_b {
        (sqrt_a, sqrt_b)
    } else {
        (sqrt_b, sqrt_a)
    };
    let sp = sqrt_p.clamp(sa, sb);
    let amount0 = amount0_delta(sp, sb, liquidity, false)?;
    let amount1 = amount1_delta(sa, sp, liquidity, false)?;
    Ok((amount0, amount1))
}

fn to_i256(v: U256) -> Result<I256, ValidationError> {
    I256::try_from(v).map_err(|_| ValidationError::Overflow {
        context: "value exceeds I256".into(),
    })
}

/// 6-decimal USD atoms to f64, saturating at `u64::MAX` atoms.
fn usd6(v: U256) -> f64 {
    v.min(U256::from(u64::MAX)).to::<u64>() as f64 / 1e6
}

fn usd6_signed(v: I256) -> f64 {
    let magnitude = usd6(v.unsigned_abs());
    if v.is_negative() {
        -magnitude
    } else {
        magnitude
    }
}

#[cfg(test)]
mod tests {
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
            block_number: 500612175,
            block_hash: B256::ZERO,
            block_timestamp: 1788260191,
            funding_x96: i("-5817301051923220663714693204286"),
            funding_div_sqrt_p_x96: i("-1332253658657311045256648214058"),
            long_util_earnings_x96: u("361206840527920163630096383165"),
            short_util_earnings_x96: u("512938731932611361114843741066"),
            current_tick: 28543,
            sqrt_amm_price_x96: u("330115084885190701587787251116"),
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
        market.accrue(&accrual).unwrap();
        let b = market.maker_equity(&maker).unwrap();

        assert!(
            (b.funding - 209.633223).abs() < 1e-4,
            "funding {}",
            b.funding
        );
        assert!(
            (b.long_util_earnings - 0.432735).abs() < 1e-4,
            "long util {}",
            b.long_util_earnings
        );
        assert!(
            (b.short_util_earnings - 24.630722).abs() < 1e-3,
            "short util {}",
            b.short_util_earnings
        );
        assert!((b.lp_fees - 7.722360).abs() < 1e-5, "lp {}", b.lp_fees);

        // The position was fee-insolvent (the contracts#292 wedge): equity
        // deeply negative, dominated by accrued funding + inventory loss.
        assert!((b.margin - 143.730198).abs() < 1e-9);
        assert!(
            b.equity() < -80.0 && b.equity() > -110.0,
            "equity {}",
            b.equity()
        );
        assert!(b.accrued_income() < 0.0);
    }

    /// Without the accrual replay the funding is stale to lastTouch — the
    /// replay must move it by the rate × dt amount, not by orders of
    /// magnitude.
    #[test]
    fn accrual_replay_moves_funding_forward() {
        let (mut market, accrual, maker) = golden_market_and_maker();
        let stale = market.maker_equity(&maker).unwrap();
        market.accrue(&accrual).unwrap();
        let fresh = market.maker_equity(&maker).unwrap();
        assert!(fresh.funding > stale.funding, "funding accrues over dt");
        assert!((fresh.funding - stale.funding) < 5.0, "dt is ~1.6h");
        // Utilization also accrues.
        assert!(fresh.short_util_earnings >= stale.short_util_earnings);
    }

    #[test]
    fn out_of_range_ticks_surface_as_errors_not_panics() {
        let (market, _, mut maker) = golden_market_and_maker();
        maker.tick_upper = 1_000_000; // beyond the Uniswap tick domain
        assert!(market.maker_equity(&maker).is_err());
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
