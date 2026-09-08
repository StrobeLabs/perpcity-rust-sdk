//! Event decoding for `Perp` and `Beacon` contracts.
//!
//! Decodes raw [`Log`] entries from WebSocket subscriptions into typed
//! [`MarketEvent`] values. Consumers get human-readable f64 values for USDC
//! amounts and prices without touching ABI encoding or Q96 math.
//!
//! The new contracts emit lean, per-market events: each `Perp` contract is a
//! single market, so the emitting log's `address` identifies the market (there
//! is no `perp_id`). Position events no longer carry inline price/open-interest
//! — those now arrive as dedicated [`MarketEvent::OpenInterestUpdated`] /
//! [`MarketEvent::RatesAndEmasRefreshed`] / [`MarketEvent::CapacityUpdated`]
//! events, so a consumer reconstructs live market state from the stream.
//!
//! Taker events carry a [`SwapResult`] whose
//! `delta` is a packed Uniswap V4 `BalanceDelta` (`int128 amount0` = perp,
//! `int128 amount1` = USD). Internal X96/X128 accounting trackers
//! (cumulatives, tick funding) are surfaced as raw on-chain integers to avoid
//! precision loss.
//!
//! Two events come from outside the `Perp`'s own event library: the
//! PoolManager's `ModifyLiquidity` (perp pools are vanilla V4 pools, so a
//! maker's liquidity change is logged by the PoolManager, `salt == posId`)
//! and the position NFT's ERC721 `Transfer`. The ERC721 `Transfer` shares
//! its `topic0` with ERC20 `Transfer`; an ERC20-shaped log (two indexed
//! fields, the value in data) fails the ERC721 decode and returns `None`.
//!
//! Admin/governance events (module setters, timelock, fee collection) are
//! intentionally not decoded — they return `None`.
//!
//! # Usage
//!
//! ```rust,no_run
//! use perpcity_sdk::feeds::events::{MarketEvent, decode_log};
//! # use alloy::rpc::types::Log;
//! # fn example(log: &Log) {
//! if let Some(event) = decode_log(log) {
//!     match event {
//!         MarketEvent::TakerOpened { pos_id, swap } => {
//!             println!("taker {pos_id} opened at {}", swap.amm_price);
//!         }
//!         MarketEvent::OpenInterestUpdated { long_oi, short_oi } => {
//!             println!("OI now {long_oi}/{short_oi}");
//!         }
//!         _ => {}
//!     }
//! }
//! # }
//! ```

use alloy::primitives::{Address, B256, I256, U256};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use serde::{Deserialize, Serialize};

use crate::contracts::{IBeacon, IPoolManagerState, Perp, PerpDeployedEvents, SwapResult};
use crate::convert::{price_x96_to_f64, scale_from_6dec, unpack_balance_delta};

/// Funding/utilization rates are scaled by 1e18 per day on-chain.
const WAD_F64: f64 = 1e18;

/// Decoded details of a taker swap, in human-readable units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SwapInfo {
    /// Perp token delta (positive = received, negative = paid).
    pub perp_delta: f64,
    /// USD delta (positive = received, negative = paid).
    pub usd_delta: f64,
    /// AMM price after the swap (human-readable).
    pub amm_price: f64,
    /// Total fee charged on the swap, in USDC.
    pub total_fee: f64,
    /// Portion paid to liquidity providers, in USDC.
    pub lp_fee: f64,
    /// Portion paid to the protocol, in USDC.
    pub protocol_fee: f64,
    /// Portion paid to the market creator, in USDC.
    pub creator_fee: f64,
    /// Portion paid into the insurance fund, in USDC.
    pub insurance_fee: f64,
}

/// Time-settled fees applied to a maker position, in human-readable units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MakerSettle {
    /// Funding settled (positive = paid by the position), in USDC.
    pub funding: f64,
    /// Long-side utilization fees, in USDC.
    pub long_util_fees: f64,
    /// Short-side utilization fees, in USDC.
    pub short_util_fees: f64,
    /// LP fees earned, in USDC.
    pub lp_fees: f64,
}

/// Raw cumulative funding/fee trackers (X96 / X128 fixed-point, on-chain units).
///
/// These are internal accounting values surfaced verbatim — convert downstream
/// only if needed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct CumulativesInfo {
    pub funding_x96: I256,
    pub funding_div_sqrt_p_x96: I256,
    pub long_util_payments_x96: U256,
    pub short_util_payments_x96: U256,
    pub long_util_earnings_x96: U256,
    pub short_util_earnings_x96: U256,
}

/// A decoded market event with human-readable values where meaningful.
///
/// Each event originates from a single `Perp` market (the log's `address`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum MarketEvent {
    // ── Maker lifecycle ──────────────────────────────────────────────
    MakerOpened {
        pos_id: U256,
    },
    MakerAdjusted {
        pos_id: U256,
        settle: MakerSettle,
    },
    /// A maker converted to a taker. On the deployed contracts this is
    /// also how a maker liquidation surfaces (`is_liquidation`, with
    /// `liq_fee` in USDC); the untailed shape from contracts that split
    /// liquidations into dedicated events decodes as `0.0` / `false`.
    MakerConverted {
        pos_id: U256,
        settle: MakerSettle,
        liq_fee: f64,
        is_liquidation: bool,
    },
    /// A maker closed. Tails as on [`Self::MakerConverted`].
    MakerClosed {
        pos_id: U256,
        settle: MakerSettle,
        liq_fee: f64,
        is_liquidation: bool,
    },
    MakerLiquidated {
        pos_id: U256,
        liquidity_amount: u128,
        liq_fee: f64,
    },
    MakerBackstopped {
        pos_id: U256,
        margin_in: f64,
        pos_recipient: Address,
        settle: MakerSettle,
    },

    // ── Taker lifecycle ──────────────────────────────────────────────
    TakerOpened {
        pos_id: U256,
        swap: SwapInfo,
    },
    TakerAdjusted {
        pos_id: U256,
        swap: SwapInfo,
        funding: f64,
        util_fees: f64,
    },
    /// A taker closed; the deployed event unifies close and liquidation
    /// (`is_liquidation`, with `liq_fee` in USDC).
    TakerClosed {
        pos_id: U256,
        swap: SwapInfo,
        funding: f64,
        util_fees: f64,
        liq_fee: f64,
        is_liquidation: bool,
    },
    TakerLiquidated {
        pos_id: U256,
        perp_amount: u128,
        liq_fee: f64,
    },
    TakerBackstopped {
        pos_id: U256,
        margin_in: f64,
        pos_recipient: Address,
        funding: f64,
        util_fees: f64,
    },

    // ── Market state ─────────────────────────────────────────────────
    /// Available taker open-interest capacity supplied by makers, in perp
    /// tokens — the same units the contract checks `OpenInterest` against.
    CapacityUpdated {
        long: f64,
        short: f64,
    },
    /// Current taker open interest, in perp tokens (multiply by the mark
    /// price for USD).
    OpenInterestUpdated {
        long_oi: f64,
        short_oi: f64,
    },
    /// Cumulative funding/fee trackers were accrued.
    CumulativesAccrued {
        cumulatives: CumulativesInfo,
    },
    /// Funding rate, utilization fees, and EMA prices were refreshed.
    RatesAndEmasRefreshed {
        /// Daily funding rate (positive = longs pay shorts).
        funding_per_day: f64,
        /// Long-side utilization fee per day (fraction).
        long_util_fee_per_day: f64,
        /// Short-side utilization fee per day (fraction).
        short_util_fee_per_day: f64,
        /// Unix timestamp of the accrual.
        last_touch: u64,
        /// AMM price EMA (human-readable).
        amm_price_ema: f64,
        /// Index price EMA (human-readable).
        index_ema: f64,
    },
    /// The active tick range was crossed during a swap.
    TicksCrossed {
        starting_tick: i32,
        ending_tick: i32,
        zero_for_one: bool,
    },
    /// A tick was initialized (raw X96 funding trackers).
    TickInitialized {
        tick: i32,
        cuml_funding_opp_x96: I256,
        cuml_funding_div_sqrt_p_opp_x96: I256,
    },
    /// A tick was deleted.
    TickDeleted {
        tick: i32,
    },

    // ── Solvency / insurance ─────────────────────────────────────────
    Donated {
        donor: Address,
        amount: f64,
        bad_debt: f64,
        insurance: f64,
    },
    BadDebtAccounted {
        bad_debt: f64,
        insurance_after: f64,
        bad_debt_after: f64,
    },
    LossSocialized {
        original_amount: f64,
        fee_charged: f64,
        bad_debt_after: f64,
    },
    MarginTransferred {
        margin_delta: f64,
        total_margin: f64,
    },

    // ── Oracle ───────────────────────────────────────────────────────
    /// Index price updated (from the Beacon contract).
    IndexUpdated {
        index: f64,
    },

    // ── Pool (Uniswap V4 PoolManager) ────────────────────────────────
    /// Liquidity added to (`liquidity_delta > 0`) or removed from a pool's
    /// tick range. Emitted by the PoolManager, not the Perp — it reaches a
    /// consumer only through a subscription to the PoolManager address.
    /// For a perp pool `sender` is the Perp and `salt` is the position id.
    ModifyLiquidity {
        pool_id: B256,
        sender: Address,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
        salt: B256,
    },

    // ── Position NFT ─────────────────────────────────────────────────
    /// ERC721 transfer of a position NFT. A mint has
    /// `from == Address::ZERO`; a burn (full close, liquidation)
    /// `to == Address::ZERO`.
    PositionTransferred {
        from: Address,
        to: Address,
        pos_id: U256,
    },
}

/// Decode a raw Alloy [`Log`] into a [`MarketEvent`], if recognized.
///
/// Returns `None` for unrecognized events (admin/governance events, ERC20
/// events, pool-internal events, etc.).
///
/// # Errors
///
/// Returns `None` (not an error) if ABI decoding or value conversion fails.
/// This is intentional — a malformed log should not crash the event stream.
pub fn decode_log(log: &Log) -> Option<MarketEvent> {
    let topic0 = *log.topic0()?;

    // ── Maker lifecycle ──────────────────────────────────────────────
    if topic0 == Perp::MakerOpened::SIGNATURE_HASH {
        let d = decode_raw::<Perp::MakerOpened>(log)?;
        Some(MarketEvent::MakerOpened { pos_id: d.posId })
    } else if topic0 == Perp::MakerAdjusted::SIGNATURE_HASH {
        let d = decode_raw::<Perp::MakerAdjusted>(log)?;
        Some(MarketEvent::MakerAdjusted {
            pos_id: d.posId,
            settle: maker_settle(d.funding, d.longUtilFees, d.shortUtilFees, d.lpFees)?,
        })
    } else if topic0 == Perp::MakerConverted::SIGNATURE_HASH {
        // Untailed shape: these contracts split liquidations into
        // `MakerLiquidated`, so this event carries no tails.
        let d = decode_raw::<Perp::MakerConverted>(log)?;
        Some(MarketEvent::MakerConverted {
            pos_id: d.posId,
            settle: maker_settle(d.funding, d.longUtilFees, d.shortUtilFees, d.lpFees)?,
            liq_fee: 0.0,
            is_liquidation: false,
        })
    } else if topic0 == Perp::MakerClosed::SIGNATURE_HASH {
        let d = decode_raw::<Perp::MakerClosed>(log)?;
        Some(MarketEvent::MakerClosed {
            pos_id: d.posId,
            settle: maker_settle(d.funding, d.longUtilFees, d.shortUtilFees, d.lpFees)?,
            liq_fee: 0.0,
            is_liquidation: false,
        })

    // ── Deployed-era maker close/convert shapes ──────────────────────
    // The live Arbitrum perps emit maker closes with `liqFee`/
    // `isLiquidation` tails (there is no MakerLiquidated event on that
    // era), which changes topic0. Decode them into the same variants,
    // tails included — a maker liquidation is a convert with
    // `is_liquidation` set.
    } else if topic0 == PerpDeployedEvents::MakerConverted::SIGNATURE_HASH {
        let d = decode_raw::<PerpDeployedEvents::MakerConverted>(log)?;
        Some(MarketEvent::MakerConverted {
            pos_id: d.posId,
            settle: maker_settle(d.funding, d.longUtilFees, d.shortUtilFees, d.lpFees)?,
            liq_fee: u256_usdc(d.liqFee)?,
            is_liquidation: d.isLiquidation,
        })
    } else if topic0 == PerpDeployedEvents::MakerClosed::SIGNATURE_HASH {
        let d = decode_raw::<PerpDeployedEvents::MakerClosed>(log)?;
        Some(MarketEvent::MakerClosed {
            pos_id: d.posId,
            settle: maker_settle(d.funding, d.longUtilFees, d.shortUtilFees, d.lpFees)?,
            liq_fee: u256_usdc(d.liqFee)?,
            is_liquidation: d.isLiquidation,
        })
    } else if topic0 == Perp::MakerLiquidated::SIGNATURE_HASH {
        let d = decode_raw::<Perp::MakerLiquidated>(log)?;
        Some(MarketEvent::MakerLiquidated {
            pos_id: d.posId,
            liquidity_amount: d.liquidityAmount,
            liq_fee: u256_usdc(d.liqFee)?,
        })
    } else if topic0 == Perp::MakerBackstopped::SIGNATURE_HASH {
        let d = decode_raw::<Perp::MakerBackstopped>(log)?;
        Some(MarketEvent::MakerBackstopped {
            pos_id: d.posId,
            margin_in: scale_from_6dec(d.marginIn as i128),
            pos_recipient: d.posRecipient,
            settle: maker_settle(d.funding, d.longUtilFees, d.shortUtilFees, d.lpFees)?,
        })

    // ── Taker lifecycle ──────────────────────────────────────────────
    } else if topic0 == Perp::TakerOpened::SIGNATURE_HASH {
        let d = decode_raw::<Perp::TakerOpened>(log)?;
        Some(MarketEvent::TakerOpened {
            pos_id: d.posId,
            swap: swap_info(&d.sr)?,
        })
    } else if topic0 == Perp::TakerAdjusted::SIGNATURE_HASH {
        let d = decode_raw::<Perp::TakerAdjusted>(log)?;
        Some(MarketEvent::TakerAdjusted {
            pos_id: d.posId,
            swap: swap_info(&d.sr)?,
            funding: i256_usdc(d.funding)?,
            util_fees: u256_usdc(d.utilFees)?,
        })
    } else if topic0 == Perp::TakerClosed::SIGNATURE_HASH {
        let d = decode_raw::<Perp::TakerClosed>(log)?;
        Some(MarketEvent::TakerClosed {
            pos_id: d.posId,
            swap: swap_info(&d.sr)?,
            funding: i256_usdc(d.funding)?,
            util_fees: u256_usdc(d.utilFees)?,
            liq_fee: u256_usdc(d.liqFee)?,
            is_liquidation: d.isLiquidation,
        })
    } else if topic0 == Perp::TakerLiquidated::SIGNATURE_HASH {
        let d = decode_raw::<Perp::TakerLiquidated>(log)?;
        Some(MarketEvent::TakerLiquidated {
            pos_id: d.posId,
            perp_amount: d.perpAmount,
            liq_fee: u256_usdc(d.liqFee)?,
        })
    } else if topic0 == Perp::TakerBackstopped::SIGNATURE_HASH {
        let d = decode_raw::<Perp::TakerBackstopped>(log)?;
        Some(MarketEvent::TakerBackstopped {
            pos_id: d.posId,
            margin_in: scale_from_6dec(d.marginIn as i128),
            pos_recipient: d.posRecipient,
            funding: i256_usdc(d.funding)?,
            util_fees: u256_usdc(d.utilFees)?,
        })

    // ── Market state ─────────────────────────────────────────────────
    } else if topic0 == Perp::CapacityUpdated::SIGNATURE_HASH {
        let d = decode_raw::<Perp::CapacityUpdated>(log)?;
        Some(MarketEvent::CapacityUpdated {
            long: scale_from_6dec(d.cap.long as i128),
            short: scale_from_6dec(d.cap.short as i128),
        })
    } else if topic0 == Perp::OpenInterestUpdated::SIGNATURE_HASH {
        let d = decode_raw::<Perp::OpenInterestUpdated>(log)?;
        Some(MarketEvent::OpenInterestUpdated {
            long_oi: scale_from_6dec(d.oi.long as i128),
            short_oi: scale_from_6dec(d.oi.short as i128),
        })
    } else if topic0 == Perp::CumulativesAccrued::SIGNATURE_HASH {
        let d = decode_raw::<Perp::CumulativesAccrued>(log)?;
        let c = &d.cumls;
        Some(MarketEvent::CumulativesAccrued {
            cumulatives: CumulativesInfo {
                funding_x96: c.fundingX96,
                funding_div_sqrt_p_x96: c.fundingDivSqrtPX96,
                long_util_payments_x96: c.longUtilPaymentsX96,
                short_util_payments_x96: c.shortUtilPaymentsX96,
                long_util_earnings_x96: c.longUtilEarningsX96,
                short_util_earnings_x96: c.shortUtilEarningsX96,
            },
        })
    } else if topic0 == Perp::RatesAndEmasRefreshed::SIGNATURE_HASH {
        let d = decode_raw::<Perp::RatesAndEmasRefreshed>(log)?;
        Some(MarketEvent::RatesAndEmasRefreshed {
            funding_per_day: i128::try_from(d.rates.fundingPerDay).ok()? as f64 / WAD_F64,
            long_util_fee_per_day: d.rates.longUtilFeePerDay as f64 / WAD_F64,
            short_util_fee_per_day: d.rates.shortUtilFeePerDay as f64 / WAD_F64,
            last_touch: d.rates.lastTouch.to::<u64>(),
            amm_price_ema: price_x96_to_f64(U256::from(d.emas.ammPrice)).ok()?,
            index_ema: price_x96_to_f64(U256::from(d.emas.index)).ok()?,
        })
    } else if topic0 == Perp::TicksCrossed::SIGNATURE_HASH {
        let d = decode_raw::<Perp::TicksCrossed>(log)?;
        Some(MarketEvent::TicksCrossed {
            starting_tick: d.startingTick.as_i32(),
            ending_tick: d.endingTick.as_i32(),
            zero_for_one: d.zeroForOne,
        })
    } else if topic0 == Perp::TickInitialized::SIGNATURE_HASH {
        let d = decode_raw::<Perp::TickInitialized>(log)?;
        Some(MarketEvent::TickInitialized {
            tick: d.tick.as_i32(),
            cuml_funding_opp_x96: d.cumlFundingOppX96,
            cuml_funding_div_sqrt_p_opp_x96: d.cumlFundingDivSqrtPOppX96,
        })
    } else if topic0 == Perp::TickDeleted::SIGNATURE_HASH {
        let d = decode_raw::<Perp::TickDeleted>(log)?;
        Some(MarketEvent::TickDeleted {
            tick: d.tick.as_i32(),
        })

    // ── Solvency / insurance ─────────────────────────────────────────
    } else if topic0 == Perp::Donated::SIGNATURE_HASH {
        let d = decode_raw::<Perp::Donated>(log)?;
        Some(MarketEvent::Donated {
            donor: d.donor,
            amount: scale_from_6dec(d.amount as i128),
            bad_debt: scale_from_6dec(d.badDebt as i128),
            insurance: scale_from_6dec(d.insurance.to::<u128>() as i128),
        })
    } else if topic0 == Perp::BadDebtAccounted::SIGNATURE_HASH {
        let d = decode_raw::<Perp::BadDebtAccounted>(log)?;
        Some(MarketEvent::BadDebtAccounted {
            bad_debt: u256_usdc(d.badDebt)?,
            insurance_after: u256_usdc(d.insuranceAfter)?,
            bad_debt_after: scale_from_6dec(d.badDebtAfter as i128),
        })
    } else if topic0 == Perp::LossSocialized::SIGNATURE_HASH {
        let d = decode_raw::<Perp::LossSocialized>(log)?;
        Some(MarketEvent::LossSocialized {
            original_amount: u256_usdc(d.originalAmount)?,
            fee_charged: u256_usdc(d.feeCharged)?,
            bad_debt_after: scale_from_6dec(d.badDebtAfter as i128),
        })
    } else if topic0 == Perp::MarginTransferred::SIGNATURE_HASH {
        let d = decode_raw::<Perp::MarginTransferred>(log)?;
        Some(MarketEvent::MarginTransferred {
            margin_delta: scale_from_6dec(d.marginDelta),
            total_margin: scale_from_6dec(d.totalMargin as i128),
        })

    // ── Oracle ───────────────────────────────────────────────────────
    } else if topic0 == IBeacon::IndexUpdated::SIGNATURE_HASH {
        let d = decode_raw::<IBeacon::IndexUpdated>(log)?;
        Some(MarketEvent::IndexUpdated {
            index: price_x96_to_f64(d.index).ok()?,
        })

    // ── Pool / position NFT ──────────────────────────────────────────
    } else if topic0 == IPoolManagerState::ModifyLiquidity::SIGNATURE_HASH {
        let d = decode_raw::<IPoolManagerState::ModifyLiquidity>(log)?;
        Some(MarketEvent::ModifyLiquidity {
            pool_id: d.id,
            sender: d.sender,
            tick_lower: d.tickLower.as_i32(),
            tick_upper: d.tickUpper.as_i32(),
            liquidity_delta: i128::try_from(d.liquidityDelta).ok()?,
            salt: d.salt,
        })
    } else if topic0 == Perp::Transfer::SIGNATURE_HASH {
        // Same topic0 as ERC20 Transfer; the ERC721 shape needs three
        // indexed topics, so an ERC20 log fails here and returns None.
        let d = decode_raw::<Perp::Transfer>(log)?;
        Some(MarketEvent::PositionTransferred {
            from: d.from,
            to: d.to,
            pos_id: d.tokenId,
        })
    } else {
        None
    }
}

/// Decode a typed event from a raw log's topics + data.
fn decode_raw<E: SolEvent>(log: &Log) -> Option<E> {
    E::decode_raw_log(
        log.inner.data.topics().iter().copied(),
        log.inner.data.data.as_ref(),
    )
    .ok()
}

/// Build a [`SwapInfo`] from a contract [`SwapResult`].
fn swap_info(sr: &SwapResult) -> Option<SwapInfo> {
    let (perp, usd) = unpack_balance_delta(sr.delta);
    Some(SwapInfo {
        perp_delta: scale_from_6dec(perp),
        usd_delta: scale_from_6dec(usd),
        amm_price: price_x96_to_f64(sr.ammPrice).ok()?,
        total_fee: i256_usdc(sr.totalFeeAmt)?,
        lp_fee: u256_usdc(sr.lpFeeAmt)?,
        protocol_fee: u256_usdc(sr.protocolFeeAmt)?,
        creator_fee: u256_usdc(sr.creatorFeeAmt)?,
        insurance_fee: u256_usdc(sr.insuranceFeeAmt)?,
    })
}

/// Build a [`MakerSettle`] from the raw funding/fee fields of a maker event.
fn maker_settle(
    funding: I256,
    long_util_fees: U256,
    short_util_fees: U256,
    lp_fees: U256,
) -> Option<MakerSettle> {
    Some(MakerSettle {
        funding: i256_usdc(funding)?,
        long_util_fees: u256_usdc(long_util_fees)?,
        short_util_fees: u256_usdc(short_util_fees)?,
        lp_fees: u256_usdc(lp_fees)?,
    })
}

/// Convert a signed 6-decimal USDC value to f64.
fn i256_usdc(v: I256) -> Option<f64> {
    Some(scale_from_6dec(i128::try_from(v).ok()?))
}

/// Convert an unsigned 6-decimal USDC value to f64.
fn u256_usdc(v: U256) -> Option<f64> {
    Some(scale_from_6dec(i128::try_from(v).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256, LogData, U256};
    use alloy::rpc::types::Log as RpcLog;

    use crate::constants::{Q96, Q96_PRECISION};

    /// Build a synthetic RPC Log from an event that implements SolEvent.
    fn make_log<E: SolEvent>(event: &E, address: Address) -> RpcLog {
        let log_data = event.encode_log_data();
        RpcLog {
            inner: alloy::primitives::Log {
                address,
                data: log_data,
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    /// Pack two int128 amounts into a Uniswap V4 `BalanceDelta` (`int256`).
    fn pack_balance_delta(amount0: i128, amount1: i128) -> I256 {
        let mut bytes = [0u8; 32];
        bytes[0..16].copy_from_slice(&amount0.to_be_bytes());
        bytes[16..32].copy_from_slice(&amount1.to_be_bytes());
        I256::from_be_bytes(bytes)
    }

    #[test]
    fn balance_delta_roundtrips() {
        for (a0, a1) in [(0i128, 0i128), (100, -100), (-1, 1), (i128::MAX, i128::MIN)] {
            let (g0, g1) = unpack_balance_delta(pack_balance_delta(a0, a1));
            assert_eq!((g0, g1), (a0, a1));
        }
    }

    #[test]
    fn decode_taker_opened_event() {
        let event = Perp::TakerOpened {
            posId: U256::from(42u64),
            sr: SwapResult {
                delta: pack_balance_delta(100_000_000, -100_000_000),
                ammPrice: Q96, // price = 1.0
                totalFeeAmt: I256::try_from(1_000_000i64).unwrap(),
                lpFeeAmt: U256::from(700_000u64),
                protocolFeeAmt: U256::from(100_000u64),
                creatorFeeAmt: U256::from(100_000u64),
                insuranceFeeAmt: U256::from(100_000u64),
            },
        };

        let log = make_log(&event, Address::ZERO);
        match decode_log(&log).expect("should decode TakerOpened") {
            MarketEvent::TakerOpened { pos_id, swap } => {
                assert_eq!(pos_id, U256::from(42u64));
                assert!((swap.perp_delta - 100.0).abs() < 1e-9);
                assert!((swap.usd_delta - (-100.0)).abs() < 1e-9);
                assert!((swap.amm_price - 1.0).abs() < Q96_PRECISION);
                assert!((swap.total_fee - 1.0).abs() < 1e-9);
            }
            _ => panic!("expected TakerOpened"),
        }
    }

    #[test]
    fn decode_taker_liquidated_event() {
        let event = Perp::TakerLiquidated {
            posId: U256::from(7u64),
            perpAmount: 50_000_000u128,
            liqFee: U256::from(1_000_000u64),
        };

        let log = make_log(&event, Address::ZERO);
        match decode_log(&log).expect("should decode TakerLiquidated") {
            MarketEvent::TakerLiquidated {
                pos_id,
                perp_amount,
                liq_fee,
            } => {
                assert_eq!(pos_id, U256::from(7u64));
                assert_eq!(perp_amount, 50_000_000u128);
                assert!((liq_fee - 1.0).abs() < 1e-9);
            }
            _ => panic!("expected TakerLiquidated"),
        }
    }

    #[test]
    fn decode_maker_opened_event() {
        let event = Perp::MakerOpened {
            posId: U256::from(3u64),
        };
        let log = make_log(&event, Address::ZERO);
        match decode_log(&log).expect("should decode MakerOpened") {
            MarketEvent::MakerOpened { pos_id } => assert_eq!(pos_id, U256::from(3u64)),
            _ => panic!("expected MakerOpened"),
        }
    }

    #[test]
    fn decode_open_interest_updated_event() {
        let event = Perp::OpenInterestUpdated {
            oi: crate::contracts::OpenInterest {
                long: 2_000_000u128,
                short: 1_000_000u128,
            },
        };
        let log = make_log(&event, Address::ZERO);
        match decode_log(&log).expect("should decode OpenInterestUpdated") {
            MarketEvent::OpenInterestUpdated { long_oi, short_oi } => {
                assert!((long_oi - 2.0).abs() < 1e-9);
                assert!((short_oi - 1.0).abs() < 1e-9);
            }
            _ => panic!("expected OpenInterestUpdated"),
        }
    }

    #[test]
    fn decode_index_updated_event() {
        let event = IBeacon::IndexUpdated {
            index: Q96 * U256::from(100u64), // index = 100.0
        };

        let log = make_log(&event, Address::ZERO);
        match decode_log(&log).expect("should decode IndexUpdated") {
            MarketEvent::IndexUpdated { index } => {
                assert!((index - 100.0).abs() < Q96_PRECISION);
            }
            _ => panic!("expected IndexUpdated"),
        }
    }

    #[test]
    fn decode_deployed_era_maker_closed_event() {
        let event = PerpDeployedEvents::MakerClosed {
            posId: U256::from(9u64),
            funding: I256::try_from(2_000_000i64).unwrap(),
            longUtilFees: U256::from(500_000u64),
            shortUtilFees: U256::from(250_000u64),
            lpFees: U256::from(1_500_000u64),
            liqFee: U256::from(750_000u64),
            isLiquidation: true,
        };
        let log = make_log(&event, Address::ZERO);
        match decode_log(&log).expect("should decode deployed-era MakerClosed") {
            MarketEvent::MakerClosed {
                pos_id,
                settle,
                liq_fee,
                is_liquidation,
            } => {
                assert_eq!(pos_id, U256::from(9u64));
                assert!((settle.funding - 2.0).abs() < 1e-9);
                assert!((settle.long_util_fees - 0.5).abs() < 1e-9);
                assert!((settle.short_util_fees - 0.25).abs() < 1e-9);
                assert!((settle.lp_fees - 1.5).abs() < 1e-9);
                assert!((liq_fee - 0.75).abs() < 1e-9);
                assert!(is_liquidation);
            }
            other => panic!("expected MakerClosed, got {other:?}"),
        }
    }

    /// The untailed shape has no tails; they decode as no liquidation.
    #[test]
    fn decode_untailed_maker_closed_has_no_liquidation_tail() {
        let event = Perp::MakerClosed {
            posId: U256::from(9u64),
            funding: I256::ZERO,
            longUtilFees: U256::ZERO,
            shortUtilFees: U256::ZERO,
            lpFees: U256::ZERO,
        };
        let log = make_log(&event, Address::ZERO);
        match decode_log(&log).expect("should decode MakerClosed") {
            MarketEvent::MakerClosed {
                liq_fee,
                is_liquidation,
                ..
            } => {
                assert_eq!(liq_fee, 0.0);
                assert!(!is_liquidation);
            }
            other => panic!("expected MakerClosed, got {other:?}"),
        }
    }

    #[test]
    fn decode_taker_closed_carries_liquidation_tail() {
        let event = Perp::TakerClosed {
            posId: U256::from(5u64),
            sr: SwapResult {
                delta: pack_balance_delta(-100_000_000, 100_000_000),
                ammPrice: Q96,
                totalFeeAmt: I256::ZERO,
                lpFeeAmt: U256::ZERO,
                protocolFeeAmt: U256::ZERO,
                creatorFeeAmt: U256::ZERO,
                insuranceFeeAmt: U256::ZERO,
            },
            funding: I256::try_from(-250_000i64).unwrap(),
            utilFees: U256::from(10_000u64),
            liqFee: U256::from(1_250_000u64),
            isLiquidation: true,
        };
        let log = make_log(&event, Address::ZERO);
        match decode_log(&log).expect("should decode TakerClosed") {
            MarketEvent::TakerClosed {
                pos_id,
                funding,
                util_fees,
                liq_fee,
                is_liquidation,
                ..
            } => {
                assert_eq!(pos_id, U256::from(5u64));
                assert!((funding + 0.25).abs() < 1e-9);
                assert!((util_fees - 0.01).abs() < 1e-9);
                assert!((liq_fee - 1.25).abs() < 1e-9);
                assert!(is_liquidation);
            }
            other => panic!("expected TakerClosed, got {other:?}"),
        }
    }

    /// Golden vector: a real `MakerConverted` log from Arbitrum mainnet
    /// (CHINA-PC perp `0x796f…8ed0`, maker liquidation of position 54, tx
    /// `0x4d1fa289fbbe…`, 2026-09-01). Locks the decoder to the shape the
    /// deployed contracts actually emit.
    #[test]
    fn decode_deployed_era_maker_converted_golden_vector() {
        let topic0 = alloy::primitives::b256!(
            "8d8df09df1280157a012f3f883267724105b6d76650a4f9ff07413e4741711e8"
        );
        let data = alloy::hex::decode(concat!(
            "0000000000000000000000000000000000000000000000000000000000000036",
            "000000000000000000000000000000000000000000000000000000000c7ebfc7",
            "0000000000000000000000000000000000000000000000000000000000069a5f",
            "000000000000000000000000000000000000000000000000000000000177d5c2",
            "000000000000000000000000000000000000000000000000000000000075d578",
            "000000000000000000000000000000000000000000000000000000000015696a",
            "0000000000000000000000000000000000000000000000000000000000000001",
        ))
        .unwrap();
        let log = RpcLog {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(vec![topic0], data.into()),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };
        match decode_log(&log).expect("should decode mainnet MakerConverted") {
            MarketEvent::MakerConverted {
                pos_id,
                settle,
                liq_fee,
                is_liquidation,
            } => {
                assert_eq!(pos_id, U256::from(54u64));
                assert!((settle.funding - 209.633223).abs() < 1e-9);
                assert!((settle.long_util_fees - 0.432735).abs() < 1e-9);
                assert!((settle.short_util_fees - 24.630722).abs() < 1e-9);
                assert!((settle.lp_fees - 7.722360).abs() < 1e-9);
                // The liquidation tails: liqFee 0x15696a = 1.403242 USDC.
                assert!((liq_fee - 1.403242).abs() < 1e-9);
                assert!(is_liquidation);
            }
            other => panic!("expected MakerConverted, got {other:?}"),
        }
    }

    #[test]
    fn decode_modify_liquidity_event() {
        let pool_id = B256::repeat_byte(0xAB);
        let perp = Address::repeat_byte(0xCD);
        let event = IPoolManagerState::ModifyLiquidity {
            id: pool_id,
            sender: perp,
            tickLower: alloy::primitives::Signed::<24, 1>::try_from(-60).unwrap(),
            tickUpper: alloy::primitives::Signed::<24, 1>::try_from(120).unwrap(),
            liquidityDelta: I256::try_from(-570_282_387i64).unwrap(),
            salt: B256::from(U256::from(54u64)),
        };
        let log = make_log(&event, Address::repeat_byte(0x36));
        match decode_log(&log).expect("should decode ModifyLiquidity") {
            MarketEvent::ModifyLiquidity {
                pool_id: id,
                sender,
                tick_lower,
                tick_upper,
                liquidity_delta,
                salt,
            } => {
                assert_eq!(id, pool_id);
                assert_eq!(sender, perp);
                assert_eq!((tick_lower, tick_upper), (-60, 120));
                assert_eq!(liquidity_delta, -570_282_387);
                assert_eq!(U256::from_be_bytes(salt.0), U256::from(54u64));
            }
            other => panic!("expected ModifyLiquidity, got {other:?}"),
        }
    }

    /// A `liquidityDelta` outside `i128` cannot come from a V4 pool
    /// (liquidity is `uint128`); like the other decoders, the malformed
    /// log yields `None` rather than a panic.
    #[test]
    fn modify_liquidity_delta_overflow_returns_none() {
        let event = IPoolManagerState::ModifyLiquidity {
            id: B256::ZERO,
            sender: Address::ZERO,
            tickLower: alloy::primitives::Signed::<24, 1>::ZERO,
            tickUpper: alloy::primitives::Signed::<24, 1>::ZERO,
            liquidityDelta: I256::MAX,
            salt: B256::ZERO,
        };
        assert!(decode_log(&make_log(&event, Address::ZERO)).is_none());
    }

    #[test]
    fn decode_position_transferred_mint() {
        let holder = Address::repeat_byte(0x11);
        let event = Perp::Transfer {
            from: Address::ZERO,
            to: holder,
            tokenId: U256::from(77u64),
        };
        let log = make_log(&event, Address::ZERO);
        match decode_log(&log).expect("should decode Transfer") {
            MarketEvent::PositionTransferred { from, to, pos_id } => {
                assert_eq!(from, Address::ZERO);
                assert_eq!(to, holder);
                assert_eq!(pos_id, U256::from(77u64));
            }
            other => panic!("expected PositionTransferred, got {other:?}"),
        }
    }

    /// ERC20 `Transfer` has the same topic0 but only two indexed fields
    /// (the value is in data); it must not decode as a position transfer.
    #[test]
    fn erc20_shaped_transfer_returns_none() {
        let event = crate::contracts::IERC20::Transfer {
            from: Address::repeat_byte(0x11),
            to: Address::repeat_byte(0x22),
            value: U256::from(1_000_000u64),
        };
        let log = make_log(&event, Address::ZERO);
        assert_eq!(log.topic0(), Some(&Perp::Transfer::SIGNATURE_HASH));
        assert!(decode_log(&log).is_none());
    }

    #[test]
    fn unrecognized_event_returns_none() {
        let log = RpcLog {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(vec![B256::repeat_byte(0xFF)], vec![].into()),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };
        assert!(decode_log(&log).is_none());
    }

    #[test]
    fn empty_log_returns_none() {
        let log = RpcLog {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(vec![], vec![].into()),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };
        assert!(decode_log(&log).is_none());
    }
}
