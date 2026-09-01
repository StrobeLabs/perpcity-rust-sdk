//! Live maker equity: what the contract would settle for an OPEN maker
//! position if it were touched now.
//!
//! Ports the deployed-era `PerpLogic` maker settlement math (commit
//! `83d90aea` of perpcity-contracts — the fee/funding math is unchanged
//! through the deployed window `#165..#168`) so the fleet can compute, for an
//! OPEN maker position, exactly what the contract would settle if it were
//! touched now:
//!
//! - accrued range funding (`makerCumlFunding` + `makerFeesAccrued`),
//! - utilization earnings (capacity × earnings-checkpoint delta),
//! - uncollected Uniswap V4 LP fees (donated taker fees, read from the
//!   PoolManager's fee-growth accounting),
//! - inventory PnL (`valPnl`) and the resulting equity.
//!
//! All arithmetic is X96/X128 integer math (`U256`/`I256`), transcribed
//! 1:1 from the Solidity, converted to f64 USD only at the boundary. The
//! whole computation is validated end-to-end against a real on-chain
//! settle: the golden test reproduces the `MakerConverted` event of the
//! CHINA-PC pos-54 liquidation from pre-liquidation chain state.
//!
//! Two reads have no contract getter and go through raw storage slots, both
//! verified against live chain data:
//! - `s.ticks[tick]` on the Perp: mapping at slot 6 (`PerpStorage` starts at
//!   slot 3; `positions`/`makers`/`takers` precede `ticks`). Cross-checked by
//!   the SDK's production-verified `emas` slot 11.
//! - The V4 `PoolManager` pool state at `keccak(poolId, 6)`: global fee
//!   growth (+2), per-tick fee growth outside (ticks mapping at +4, member
//!   +2), and per-position fee growth checkpoints (positions mapping at +6,
//!   member +2). Only `token1` (USDC) fee growth matters — the pool is
//!   fee-0 and taker fees arrive as `donate`s in USDC.

use crate::client::PerpClient;
use crate::contracts::Perp;
use crate::errors::Result as SdkResult;
use crate::math::tick::get_sqrt_ratio_at_tick;
use alloy::primitives::{B256, I256, U256, U512, keccak256};
use alloy::providers::Provider;

const Q96_SHIFT: usize = 96;
const Q128_SHIFT: usize = 128;
const ONE_DAY: u64 = 86_400;
const WAD: u64 = 1_000_000_000_000_000_000;
/// `PerpStorage.ticks` mapping slot (base 3 + field index 3).
const PERP_TICKS_SLOT: u64 = 6;
/// `PoolManager._pools` mapping slot.
const POOL_MANAGER_POOLS_SLOT: u64 = 6;

fn q96() -> U256 {
    U256::from(1u8) << Q96_SHIFT
}

/// `floor(a × b / d)` in 512-bit intermediate precision (`FullMath.mulDiv`).
fn full_mul_div(a: U256, b: U256, d: U256) -> U256 {
    let wide = U512::from(a) * U512::from(b) / U512::from(d);
    U256::from(wide)
}

/// The contract's `SignedFixedPointMathLib.sFullMulDiv`: magnitude-truncated
/// signed mul-div, with the (quirky, sign-independent) `+1` when `roundUp`
/// is set and the division has a remainder. Ported faithfully.
fn s_full_mul_div(a: I256, b: I256, d: U256, round_up: bool) -> I256 {
    let (ua, ub) = (a.unsigned_abs(), b.unsigned_abs());
    let negative = (a.is_negative() && b > I256::ZERO) || (a > I256::ZERO && b.is_negative());
    let abs_result = I256::try_from(full_mul_div(ua, ub, d)).expect("mul-div fits i256");
    let mut result = if negative { -abs_result } else { abs_result };
    if round_up {
        let rem = U512::from(ua) * U512::from(ub) % U512::from(d);
        if rem != U512::ZERO {
            result += I256::ONE;
        }
    }
    result
}

/// Uniswap `getAmount0ForLiquidity` (token0 owed above the current price).
fn amount0_for_liquidity(sqrt_a: U256, sqrt_b: U256, liquidity: u128) -> U256 {
    let (sa, sb) = if sqrt_a <= sqrt_b {
        (sqrt_a, sqrt_b)
    } else {
        (sqrt_b, sqrt_a)
    };
    let numerator = U256::from(liquidity) << Q96_SHIFT;
    full_mul_div(numerator, sb - sa, sb) / sa
}

/// Uniswap `getAmountsForLiquidity` with the price clamped into the range.
fn amounts_for_liquidity(
    sqrt_p: U256,
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: u128,
) -> (U256, U256) {
    let (sa, sb) = if sqrt_a <= sqrt_b {
        (sqrt_a, sqrt_b)
    } else {
        (sqrt_b, sqrt_a)
    };
    let sp = sqrt_p.clamp(sa, sb);
    let amount0 = amount0_for_liquidity(sp, sb, liquidity);
    let amount1 = full_mul_div(U256::from(liquidity), sp - sa, q96());
    (amount0, amount1)
}

/// One `TickInfo` from the Perp's tick funding mapping.
#[derive(Debug, Clone, Copy, Default)]
#[allow(missing_docs)] // raw chain inputs, named after the contract fields
pub struct TickFunding {
    pub cuml_funding_opp_x96: I256,
    pub cuml_funding_div_sqrt_p_opp_x96: I256,
}

/// Market-wide inputs shared by every position's computation, with the
/// cumulatives already replayed to `now` (see [`accrue_cumulatives`]).
#[derive(Debug, Clone, Copy)]
#[allow(missing_docs)] // raw chain inputs, named after the contract fields
pub struct MarketState {
    pub funding_x96: I256,
    pub funding_div_sqrt_p_x96: I256,
    pub long_util_earnings_x96: U256,
    pub short_util_earnings_x96: U256,
    pub current_tick: i32,
    pub sqrt_amm_price_x96: U256,
    /// Mark price in X96 — used for `valPnl` (and by the accrual replay).
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

/// Replay `PerpLogic.accrue` from `lastTouch` to `now`, advancing the
/// cumulatives in place. Mirrors the contract exactly, using the supplied
/// mark for the utilization leg (the contract recomputes mark inside
/// `accrue`; passing the current mark keeps the replay within micro-dollars).
pub fn accrue_cumulatives(market: &mut MarketState, accrual: &AccrualInputs) {
    let dt = accrual.now.saturating_sub(accrual.last_touch);
    if dt == 0 {
        return;
    }
    let dt_days = U256::from(dt) * q96() / U256::from(ONE_DAY);
    let funding_accrued = s_full_mul_div(
        I256::try_from(accrual.funding_per_day_wad).expect("rate fits"),
        I256::try_from(dt_days).expect("dt_days fits"),
        U256::from(WAD),
        false,
    );
    market.funding_x96 += funding_accrued;
    market.funding_div_sqrt_p_x96 += s_full_mul_div(
        funding_accrued,
        I256::try_from(q96()).expect("q96 fits"),
        market.sqrt_amm_price_x96,
        false,
    );

    let dt_days_mult_mark = full_mul_div(dt_days, market.mark_price_x96, q96());
    let lu_accrued = full_mul_div(
        U256::from(accrual.long_util_fee_per_day_wad),
        dt_days_mult_mark,
        U256::from(WAD),
    );
    let su_accrued = full_mul_div(
        U256::from(accrual.short_util_fee_per_day_wad),
        dt_days_mult_mark,
        U256::from(WAD),
    );
    if accrual.cap_long != 0 {
        market.long_util_earnings_x96 += full_mul_div(
            lu_accrued,
            U256::from(accrual.oi_long),
            U256::from(accrual.cap_long),
        );
    }
    if accrual.cap_short != 0 {
        market.short_util_earnings_x96 += full_mul_div(
            su_accrued,
            U256::from(accrual.oi_short),
            U256::from(accrual.cap_short),
        );
    }
}

/// `PerpLogic.makerCumlFunding`: assemble the band's cumulative funding
/// (below / within / within-div-sqrtP) from the two ticks' opposite-side
/// checkpoints, branching on which side of each tick the current tick is.
fn maker_cuml_funding(market: &MarketState, maker: &MakerState) -> (I256, I256, I256) {
    let lower = &maker.tick_lower_funding;
    let upper = &maker.tick_upper_funding;

    let (below, div_below_lower) = if market.current_tick >= maker.tick_lower {
        (
            lower.cuml_funding_opp_x96,
            lower.cuml_funding_div_sqrt_p_opp_x96,
        )
    } else {
        (
            market.funding_x96 - lower.cuml_funding_opp_x96,
            market.funding_div_sqrt_p_x96 - lower.cuml_funding_div_sqrt_p_opp_x96,
        )
    };
    let (below_upper, div_below_upper) = if market.current_tick >= maker.tick_upper {
        (
            upper.cuml_funding_opp_x96,
            upper.cuml_funding_div_sqrt_p_opp_x96,
        )
    } else {
        (
            market.funding_x96 - upper.cuml_funding_opp_x96,
            market.funding_div_sqrt_p_x96 - upper.cuml_funding_div_sqrt_p_opp_x96,
        )
    };
    (
        below,
        below_upper - below,
        div_below_upper - div_below_lower,
    )
}

/// Compute the full settle preview for one maker position.
pub fn compute_maker_equity(market: &MarketState, maker: &MakerState) -> MakerEquityBreakdown {
    let sqrt_l = get_sqrt_ratio_at_tick(maker.tick_lower).expect("valid tick");
    let sqrt_u = get_sqrt_ratio_at_tick(maker.tick_upper).expect("valid tick");
    let (mf_below, mf_within, mf_div_sqrt_within) = maker_cuml_funding(market, maker);

    // ── makerFeesAccrued ────────────────────────────────────────────
    let base_funding = s_full_mul_div(
        I256::try_from(maker.delta_amount0).expect("amount0 fits"),
        market.funding_x96 - maker.last_cuml_funding_x96,
        q96(),
        true,
    );
    let perp_below =
        I256::try_from(amount0_for_liquidity(sqrt_l, sqrt_u, maker.liquidity)).expect("fits");
    let funding_below = s_full_mul_div(perp_below, mf_below - maker.last_below_x96, q96(), true);
    let div_amm = mf_div_sqrt_within - maker.last_div_sqrt_within_x96;
    let d_within = mf_within - maker.last_within_x96;
    let div_upper = s_full_mul_div(
        d_within,
        I256::try_from(q96()).expect("fits"),
        sqrt_u,
        false,
    );
    let funding_within = s_full_mul_div(
        I256::try_from(maker.liquidity).expect("liquidity fits"),
        div_amm - div_upper,
        q96(),
        true,
    );
    let funding = base_funding + funding_below + funding_within;

    let long_util = full_mul_div(
        U256::from(maker.cap_long_6dec),
        market.long_util_earnings_x96 - maker.last_long_util_earnings_x96,
        q96(),
    );
    let short_util = full_mul_div(
        U256::from(maker.cap_short_6dec),
        market.short_util_earnings_x96 - maker.last_short_util_earnings_x96,
        q96(),
    );

    // ── V4 LP fees: liquidity × Δ feeGrowthInside1 / 2^128 ──────────
    // Fee growth deltas wrap by design in Uniswap; wrapping_sub matches.
    let fee_growth_delta = maker
        .fee_growth_inside1_x128
        .wrapping_sub(maker.fee_growth_inside1_last_x128);
    let lp_fees = (U512::from(maker.liquidity) * U512::from(fee_growth_delta)) >> Q128_SHIFT;
    let lp_fees = U256::from(lp_fees);

    // ── valPnl (maker overload) ─────────────────────────────────────
    let (perps, usd) =
        amounts_for_liquidity(market.sqrt_amm_price_x96, sqrt_l, sqrt_u, maker.liquidity);
    let liquidity_val = full_mul_div(perps, market.mark_price_x96, q96()) + usd;
    let pos_val = full_mul_div(
        U256::from(maker.delta_amount0.unsigned_abs()),
        market.mark_price_x96,
        q96(),
    ) + U256::from(maker.delta_amount1.unsigned_abs());
    let unrealized =
        I256::try_from(liquidity_val).expect("fits") - I256::try_from(pos_val).expect("fits");

    let usd6 =
        |v: U256| u64::try_from(v.min(U256::from(u64::MAX))).unwrap_or(u64::MAX) as f64 / 1e6;
    let usd6_i = |v: I256| {
        let neg = v.is_negative();
        let m = usd6(v.unsigned_abs());
        if neg { -m } else { m }
    };
    MakerEquityBreakdown {
        margin: maker.margin_6dec as f64 / 1e6,
        funding: usd6_i(funding),
        long_util_earnings: usd6(long_util),
        short_util_earnings: usd6(short_util),
        lp_fees: usd6(lp_fees),
        unrealized_pnl: usd6_i(unrealized),
    }
}

// ── Storage-slot helpers ────────────────────────────────────────────

pub(crate) fn mapping_slot(key: B256, slot: U256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(key.as_slice());
    buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

pub(crate) fn signed_key(tick: i32) -> B256 {
    B256::from(I256::try_from(tick).expect("tick fits").into_raw())
}

/// Slot of `s.ticks[tick]` on the Perp contract.
pub fn perp_tick_slot(tick: i32) -> U256 {
    mapping_slot(signed_key(tick), U256::from(PERP_TICKS_SLOT))
}

/// Base slot of the pool's `Pool.State` inside the PoolManager.
pub fn pool_state_slot(pool_id: B256) -> U256 {
    mapping_slot(pool_id, U256::from(POOL_MANAGER_POOLS_SLOT))
}

/// Slot of `state.ticks[tick]` inside the pool state (fee growth at +2).
pub fn v4_tick_slot(pool_id: B256, tick: i32) -> U256 {
    mapping_slot(signed_key(tick), pool_state_slot(pool_id) + U256::from(4u8))
}

/// Slot of the V4 position keyed by `(owner, tickLower, tickUpper, salt)`.
pub fn v4_position_slot(
    pool_id: B256,
    owner: alloy::primitives::Address,
    tick_lower: i32,
    tick_upper: i32,
    salt: B256,
) -> U256 {
    let mut packed = Vec::with_capacity(20 + 3 + 3 + 32);
    packed.extend_from_slice(owner.as_slice());
    packed.extend_from_slice(&tick_lower.to_be_bytes()[1..]);
    packed.extend_from_slice(&tick_upper.to_be_bytes()[1..]);
    packed.extend_from_slice(salt.as_slice());
    let key = keccak256(&packed);
    mapping_slot(key, pool_state_slot(pool_id) + U256::from(6u8))
}

/// Compute `feeGrowthInside1X128` for a band from the global growth and the
/// two ticks' `feeGrowthOutside1X128`, per Uniswap's `getFeeGrowthInside`.
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

// ── Live reader ─────────────────────────────────────────────────────

fn f64_to_x96(value: f64) -> U256 {
    if value <= 0.0 {
        return U256::ZERO;
    }
    // Two-step conversion keeps the full f64 mantissa: value × 2^48 fits in
    // u128 for any real price, then shift the rest.
    let hi = (value * (1u64 << 48) as f64) as u128;
    U256::from(hi) << (Q96_SHIFT - 48)
}

/// Read everything needed and compute the equity breakdown for each maker
/// position in `pos_ids`, all pinned to one block. Non-maker ids (zero
/// liquidity — takers or deleted positions) are omitted from the result.
///
/// `mark_price` is the caller's current mark (snapshot / market-data cache);
/// it prices `valPnl` and the accrual replay.
pub async fn read_maker_equities(
    client: &PerpClient,
    pos_ids: &[U256],
    mark_price: f64,
) -> SdkResult<Vec<(U256, MakerEquityBreakdown)>> {
    if pos_ids.is_empty() {
        return Ok(Vec::new());
    }
    let provider = client.provider();
    let deployments = client.deployments();
    let perp = Perp::new(deployments.perp, provider);

    // Pin a few blocks back: on load-balanced RPC endpoints the newest
    // block's state may not be materialized on every replica yet, and
    // Arbitrum produces ~4 blocks/s so the lag is under two seconds.
    let block_number = provider.get_block_number().await?.saturating_sub(8);
    let block_id = alloy::eips::BlockId::from(block_number);
    let now = provider
        .get_block_by_number(block_number.into())
        .await?
        .map(|b| b.header.timestamp)
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        });

    let cumls = perp.cumulatives().block(block_id).call().await?;
    let rates = perp.rates().block(block_id).call().await?;
    let pool_state = perp.poolState().block(block_id).call().await?;
    let capacity = perp.capacity().block(block_id).call().await?;
    let oi = perp.openInterest().block(block_id).call().await?;
    let pool_id = perp.POOL_ID().block(block_id).call().await?;

    let mut market = MarketState {
        funding_x96: cumls.fundingX96,
        funding_div_sqrt_p_x96: cumls.fundingDivSqrtPX96,
        long_util_earnings_x96: cumls.longUtilEarningsX96,
        short_util_earnings_x96: cumls.shortUtilEarningsX96,
        current_tick: pool_state.tick.as_i32(),
        sqrt_amm_price_x96: U256::from(pool_state.sqrtPrice),
        mark_price_x96: f64_to_x96(mark_price),
    };
    accrue_cumulatives(
        &mut market,
        &AccrualInputs {
            funding_per_day_wad: i128::try_from(rates.fundingPerDay).unwrap_or(0),
            long_util_fee_per_day_wad: rates.longUtilFeePerDay,
            short_util_fee_per_day_wad: rates.shortUtilFeePerDay,
            last_touch: rates.lastTouch.to::<u64>(),
            now,
            oi_long: oi.long,
            oi_short: oi.short,
            cap_long: capacity.long,
            cap_short: capacity.short,
        },
    );

    let fg1_global = provider
        .get_storage_at(
            deployments.pool_manager,
            pool_state_slot(pool_id) + U256::from(2u8),
        )
        .block_id(block_id)
        .await?;

    let read_slot = |addr: alloy::primitives::Address, slot: U256| {
        provider.get_storage_at(addr, slot).block_id(block_id)
    };
    let as_i256 = |v: U256| I256::from_raw(v);

    let mut out = Vec::with_capacity(pos_ids.len());
    for &pos_id in pos_ids {
        let pos = perp.positions(pos_id).block(block_id).call().await?;
        let details = perp.makerDetails(pos_id).block(block_id).call().await?;
        if details.liquidity == 0 {
            continue;
        }
        let (tick_lower, tick_upper) = (details.tickLower.as_i32(), details.tickUpper.as_i32());

        let tl_slot = perp_tick_slot(tick_lower);
        let tu_slot = perp_tick_slot(tick_upper);
        let tick_lower_funding = TickFunding {
            cuml_funding_opp_x96: as_i256(read_slot(deployments.perp, tl_slot).await?),
            cuml_funding_div_sqrt_p_opp_x96: as_i256(
                read_slot(deployments.perp, tl_slot + U256::ONE).await?,
            ),
        };
        let tick_upper_funding = TickFunding {
            cuml_funding_opp_x96: as_i256(read_slot(deployments.perp, tu_slot).await?),
            cuml_funding_div_sqrt_p_opp_x96: as_i256(
                read_slot(deployments.perp, tu_slot + U256::ONE).await?,
            ),
        };

        let fg1_out_lower = read_slot(
            deployments.pool_manager,
            v4_tick_slot(pool_id, tick_lower) + U256::from(2u8),
        )
        .await?;
        let fg1_out_upper = read_slot(
            deployments.pool_manager,
            v4_tick_slot(pool_id, tick_upper) + U256::from(2u8),
        )
        .await?;
        let position_slot = v4_position_slot(
            pool_id,
            deployments.perp,
            tick_lower,
            tick_upper,
            B256::from(pos_id),
        );
        let fg1_inside_last =
            read_slot(deployments.pool_manager, position_slot + U256::from(2u8)).await?;

        let (amount0, amount1) = crate::convert::unpack_balance_delta(pos.delta);
        let maker = MakerState {
            margin_6dec: pos.margin,
            delta_amount0: amount0,
            delta_amount1: amount1,
            last_cuml_funding_x96: pos.lastCumlFundingX96,
            tick_lower,
            tick_upper,
            liquidity: details.liquidity,
            last_long_util_earnings_x96: details.lastLongUtilEarningsX96,
            last_short_util_earnings_x96: details.lastShortUtilEarningsX96,
            cap_long_6dec: details.capacity.long,
            cap_short_6dec: details.capacity.short,
            last_below_x96: details.lastCumlFunding.belowX96,
            last_within_x96: details.lastCumlFunding.withinX96,
            last_div_sqrt_within_x96: details.lastCumlFunding.divSqrtPriceWithinX96,
            tick_lower_funding,
            tick_upper_funding,
            fee_growth_inside1_x128: fee_growth_inside1(
                fg1_global,
                fg1_out_lower,
                fg1_out_upper,
                tick_lower,
                tick_upper,
                market.current_tick,
            ),
            fee_growth_inside1_last_x128: fg1_inside_last,
        };
        out.push((pos_id, compute_maker_equity(&market, &maker)));
    }
    Ok(out)
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
    fn golden_market_and_maker() -> (MarketState, AccrualInputs, MakerState) {
        let i = |s: &str| I256::from_dec_str(s).unwrap();
        let u = |s: &str| U256::from_str_radix(s, 10).unwrap();
        let market = MarketState {
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
        accrue_cumulatives(&mut market, &accrual);
        let b = compute_maker_equity(&market, &maker);

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
        let stale = compute_maker_equity(&market, &maker);
        accrue_cumulatives(&mut market, &accrual);
        let fresh = compute_maker_equity(&market, &maker);
        assert!(fresh.funding > stale.funding, "funding accrues over dt");
        assert!((fresh.funding - stale.funding) < 5.0, "dt is ~1.6h");
        // Utilization also accrues.
        assert!(fresh.short_util_earnings >= stale.short_util_earnings);
    }

    #[test]
    fn s_full_mul_div_matches_contract_semantics() {
        let q = U256::from(100u8);
        let big = |v: i64| I256::try_from(v).unwrap();
        assert_eq!(s_full_mul_div(big(7), big(10), q, false), big(0));
        assert_eq!(s_full_mul_div(big(7), big(10), q, true), big(1));
        assert_eq!(s_full_mul_div(big(-7), big(10), q, false), big(0));
        // The contract's roundUp adds +1 regardless of sign.
        assert_eq!(s_full_mul_div(big(-7), big(10), q, true), big(1));
        assert_eq!(s_full_mul_div(big(-70), big(10), q, false), big(-7));
        assert_eq!(s_full_mul_div(big(-70), big(10), q, true), big(-7));
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

    #[test]
    fn v4_position_slot_packs_signed_ticks() {
        // Just shape checks: negative ticks must pack as 3-byte two's
        // complement, and different salts must land on different slots.
        let pool = B256::repeat_byte(1);
        let owner = alloy::primitives::Address::repeat_byte(2);
        let a = v4_position_slot(pool, owner, -60, 60, B256::from(U256::from(1u8)));
        let b = v4_position_slot(pool, owner, -60, 60, B256::from(U256::from(2u8)));
        assert_ne!(a, b);
    }
}
