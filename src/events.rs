//! The market's event vocabulary: what a `Perp` or `Beacon` emitted,
//! decoded once for either tense.
//!
//! Decodes a raw [`Log`] into a typed [`MarketEvent`], whatever transport
//! delivered it — [`crate::feeds`] streams the present over a WebSocket,
//! [`crate::history`] replays the past from log scans, and both speak
//! this vocabulary. Every quantity carries its unit as a type — USDC and
//! perp amounts as counts of atoms, prices as their Q96 word, rates as the
//! contract's WAD — so a consumer folds exact integers and converts once,
//! at the end, rather than being handed a rounded `f64` per event.
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
//! `int128 amount1` = USD), unpacked here into the two delta types.
//!
//! Some events come from outside the `Perp`'s own event library: the
//! PoolManager's `ModifyLiquidity` (perp pools are vanilla V4 pools, so a
//! maker's liquidity change is logged by the PoolManager, `salt == posId`),
//! its `Initialize` and `Swap`, which state the pool's exact price and tick,
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
//! match decode_log(log) {
//!     // A log this vocabulary covers.
//!     Ok(Some(MarketEvent::TakerOpened { pos_id, swap })) => {
//!         // The pool price, not the price the contract marks at.
//!         println!("taker {pos_id} opened at {:?}", swap.pool_price.to_f64());
//!     }
//!     Ok(Some(MarketEvent::OpenInterestUpdated { open_interest })) => {
//!         println!("OI now {}/{}", open_interest.long.atoms(), open_interest.short.atoms());
//!     }
//!     Ok(Some(_)) => {}
//!     // Not one of ours: an admin, ERC20 or pool-internal log. Skip it.
//!     Ok(None) => {}
//!     // One of ours that would not decode, so this market's history has a
//!     // gap here. Worth saying out loud rather than skipping.
//!     Err(e) => eprintln!("undecodable market log: {e}"),
//! }
//! # }
//! ```

#![doc = "\n\nThe design of this module: [`src/events/DESIGN.md`](https://github.com/StrobeLabs/perpcity-rust-sdk/blob/main/src/events/DESIGN.md)."]

use alloy::primitives::{Address, B256, I256, U256};
use alloy::rpc::types::Log;
use alloy::sol_types::SolEvent;
use serde::{Deserialize, Serialize};

use crate::contracts::{
    Cumulatives, IBeacon, IPoolManagerState, Perp, PerpDeployedEvents, PerpV022, SwapResult,
};
use crate::convert::unpack_balance_delta;
use crate::errors::ValidationError;
use crate::units::{
    Earnings, Funding, FundingPerSqrtPrice, FundingRate, LDelta, LUnits, PerSide, PerpAtoms,
    PerpDelta, Price, SqrtPrice, UsdcAtoms, UsdcDelta, UtilizationRate,
};

/// An ERC-20 `Transfer` indexes two of its three fields, so topic0 plus two.
/// The ERC-721 shape this module wants indexes all three and has four.
const ERC20_TRANSFER_TOPICS: usize = 3;

/// What a taker's swap did, in the units the chain settled it in.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SwapInfo {
    /// Perp token delta, as V4 signs it: positive received, negative paid.
    pub perp_delta: PerpDelta,
    /// USDC delta, as V4 signs it: positive received, negative paid.
    pub usd_delta: UsdcDelta,
    /// The *pool* price after the swap — not the price the contract marks
    /// at, which is the fair price of this, the index and the two EMAs
    /// ([`crate::math::pricing`]). A basis computed against this is not the
    /// basis the contract sees.
    pub pool_price: Price,
    /// Total fee charged on the swap. Signed because the contract's
    /// `totalFeeAmt` is, and the four shares below are not.
    pub total_fee: UsdcDelta,
    /// Share paid to liquidity providers.
    pub lp_fee: UsdcAtoms,
    /// Share paid to the protocol.
    pub protocol_fee: UsdcAtoms,
    /// Share paid to the market creator.
    pub creator_fee: UsdcAtoms,
    /// Share paid into the insurance fund.
    pub insurance_fee: UsdcAtoms,
}

impl From<Cumulatives> for CumulativesInfo {
    /// The contract's struct, whether an event carried it or a view
    /// returned it: one conversion, so the two tenses agree to the word.
    fn from(c: Cumulatives) -> Self {
        Self {
            funding: Funding::from_x96(c.fundingX96),
            funding_div_sqrt_p: FundingPerSqrtPrice::from_x96(c.fundingDivSqrtPX96),
            util_payments: PerSide::new(
                Earnings::from_x96(c.longUtilPaymentsX96),
                Earnings::from_x96(c.shortUtilPaymentsX96),
            ),
            util_earnings: PerSide::new(
                Earnings::from_x96(c.longUtilEarningsX96),
                Earnings::from_x96(c.shortUtilEarningsX96),
            ),
        }
    }
}

/// What a touch settled on a maker position, in the units the chain
/// settled it in.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MakerSettle {
    /// Funding settled, positive when the position **pays** — the same
    /// direction as [`MakerEquityBreakdown::funding_owed`](crate::MakerEquityBreakdown::funding_owed),
    /// so it is subtracted from the three below rather than added.
    pub funding: UsdcDelta,
    /// Utilization fees earned, per side.
    pub util_fees: PerSide<UsdcAtoms>,
    /// LP fees earned.
    pub lp_fees: UsdcAtoms,
}

/// The market's cumulative accumulators as the event carried them.
///
/// Levels, not growth: what a position owes or has earned is the difference
/// between one of these and the position's own checkpoint, which each type
/// takes with the rule that accumulator follows.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct CumulativesInfo {
    pub funding: Funding,
    pub funding_div_sqrt_p: FundingPerSqrtPrice,
    pub util_payments: PerSide<Earnings>,
    pub util_earnings: PerSide<Earnings>,
}

/// A decoded market event, every quantity carrying its unit as a type.
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
    /// `liquidation_fee` in USDC); the untailed shape from contracts that split
    /// liquidations into dedicated events decodes as `0.0` / `false`.
    MakerConverted {
        pos_id: U256,
        settle: MakerSettle,
        liquidation_fee: UsdcAtoms,
        is_liquidation: bool,
    },
    /// A maker closed. Tails as on [`Self::MakerConverted`].
    MakerClosed {
        pos_id: U256,
        settle: MakerSettle,
        liquidation_fee: UsdcAtoms,
        is_liquidation: bool,
    },
    MakerLiquidated {
        pos_id: U256,
        liquidity_amount: LUnits,
        liquidation_fee: UsdcAtoms,
    },
    MakerBackstopped {
        pos_id: U256,
        margin_in: UsdcAtoms,
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
        funding: UsdcDelta,
        util_fees: UsdcAtoms,
    },
    /// A taker closed. Build `58b42b7` unifies close and liquidation in
    /// the event (`is_liquidation`, with `liquidation_fee` in USDC); a
    /// `v0.2.2` market emits the close alone and the fee in the
    /// `TakerLiquidated` that follows, so from it both tails read as a
    /// voluntary close here.
    TakerClosed {
        pos_id: U256,
        swap: SwapInfo,
        funding: UsdcDelta,
        util_fees: UsdcAtoms,
        liquidation_fee: UsdcAtoms,
        is_liquidation: bool,
    },
    TakerLiquidated {
        pos_id: U256,
        perp_amount: PerpAtoms,
        liquidation_fee: UsdcAtoms,
    },
    TakerBackstopped {
        pos_id: U256,
        margin_in: UsdcAtoms,
        pos_recipient: Address,
        funding: UsdcDelta,
        util_fees: UsdcAtoms,
    },

    // ── Market state ─────────────────────────────────────────────────
    /// Available taker open-interest capacity supplied by makers, in perp
    /// tokens — the same units the contract checks `OpenInterest` against.
    CapacityUpdated {
        capacity: PerSide<PerpAtoms>,
    },
    /// Current taker open interest, per side.
    OpenInterestUpdated {
        open_interest: PerSide<PerpAtoms>,
    },
    /// Cumulative funding/fee trackers were accrued.
    CumulativesAccrued {
        cumulatives: CumulativesInfo,
    },
    /// Funding rate, utilization fees, and EMA prices were refreshed.
    RatesAndEmasRefreshed {
        /// Daily funding rate (positive = longs pay shorts).
        funding_per_day: FundingRate,
        /// Utilization fee per day, per side.
        util_fee_per_day: PerSide<UtilizationRate>,
        /// Unix timestamp of the accrual.
        last_touch: u64,
        /// The *pool* price's EMA — the `ammPrice` leg of the contract's
        /// fair price, not the mark itself.
        pool_price_ema: Price,
        /// The index's EMA, the fair price's other smoothed leg.
        index_ema: Price,
    },
    /// The active tick range was crossed during a swap.
    TicksCrossed {
        starting_tick: i32,
        ending_tick: i32,
        zero_for_one: bool,
    },
    /// A tick was initialized, with the funding checkpoints it starts from.
    TickInitialized {
        tick: i32,
        cuml_funding_opp: Funding,
        cuml_funding_div_sqrt_p_opp: FundingPerSqrtPrice,
    },
    /// A tick was deleted.
    TickDeleted {
        tick: i32,
    },

    // ── Solvency / insurance ─────────────────────────────────────────
    Donated {
        donor: Address,
        amount: UsdcAtoms,
        bad_debt: UsdcAtoms,
        insurance: UsdcAtoms,
    },
    BadDebtAccounted {
        bad_debt: UsdcAtoms,
        insurance_after: UsdcAtoms,
        bad_debt_after: UsdcAtoms,
    },
    LossSocialized {
        original_amount: UsdcAtoms,
        fee_charged: UsdcAtoms,
        bad_debt_after: UsdcAtoms,
    },
    MarginTransferred {
        margin_delta: UsdcDelta,
        total_margin: UsdcAtoms,
    },

    // ── Oracle ───────────────────────────────────────────────────────
    /// Index price updated (from the Beacon contract).
    IndexUpdated {
        index: Price,
    },

    // ── Pool (Uniswap V4 PoolManager) ────────────────────────────────
    /// The pool created, at its first exact price and tick. Emitted by the
    /// PoolManager, in the factory's creation transaction.
    PoolInitialized {
        pool_id: B256,
        sqrt_price: SqrtPrice,
        tick: i32,
    },
    /// A swap through the pool, with its exact price, active liquidity and
    /// tick after it: the state a later liquidity change's amounts are
    /// computed at. Emitted by the PoolManager; for a perp's own swap
    /// `sender` is the Perp, whose taker event carries the amounts.
    PoolSwapped {
        pool_id: B256,
        sender: Address,
        sqrt_price: SqrtPrice,
        liquidity: LUnits,
        tick: i32,
    },
    /// Liquidity added to (`liquidity_delta > 0`) or removed from a pool's
    /// tick range. Emitted by the PoolManager, not the Perp — it reaches a
    /// consumer only through a subscription to the PoolManager address.
    /// For a perp pool `sender` is the Perp and `salt` is the position id.
    ModifyLiquidity {
        pool_id: B256,
        sender: Address,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: LDelta,
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

    // ── Governance ───────────────────────────────────────────────────
    /// Governance swapped one of the market's six modules. The rules a
    /// market prices, funds, fees and bounds by are the module's, so a fold
    /// that rebuilds the market from its events reads these to know which
    /// rules were in force at each point.
    ModuleSet {
        module: ModuleKind,
        address: Address,
    },
}

/// The six modules a market delegates to; what a [`MarketEvent::ModuleSet`]
/// names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModuleKind {
    /// The index source.
    Beacon,
    /// Swap, insurance, creator and utilization fee rates.
    Fees,
    /// The funding rate rule.
    Funding,
    /// Initial, liquidation and backstop margin ratios, per kind.
    MarginRatios,
    /// The price band a swap may move the pool within.
    PriceImpact,
    /// The fair price the contract marks at.
    Pricing,
}

/// Decode a raw Alloy [`Log`] into a [`MarketEvent`], if recognized.
///
/// `Ok(None)` is a log this vocabulary does not cover: an admin or
/// governance event, an ERC20 event, a pool-internal event. There is
/// nothing wrong with it and a caller skips it.
///
/// # Errors
///
/// [`ValidationError::DecodeFailed`] when the topic *is* one of ours and
/// the log still will not decode — the binding disagrees with the shape on
/// chain, or a value is too wide to hold. That is a gap in what a caller
/// can see, so it is an error rather than another `None`: the two used to
/// be indistinguishable, and a scan dropped the second kind silently.
pub fn decode_log(log: &Log) -> Result<Option<MarketEvent>, ValidationError> {
    let Some(topic0) = log.topic0().copied() else {
        return Ok(None);
    };

    // The chain below stays `Option`-shaped — `Some` for a topic this
    // vocabulary covers, `None` for one it does not — while every `?` in it
    // returns an error from this function. So the two outcomes the old
    // signature collapsed stay separate without an arm having to say so.
    //
    // ── Maker lifecycle ──────────────────────────────────────────────
    let event = if topic0 == Perp::MakerOpened::SIGNATURE_HASH {
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
            // Defaulted, not observed: this era splits liquidations into
            // `MakerLiquidated`, so the event carries no tails to read.
            liquidation_fee: UsdcAtoms::ZERO,
            is_liquidation: false,
        })
    } else if topic0 == Perp::MakerClosed::SIGNATURE_HASH {
        let d = decode_raw::<Perp::MakerClosed>(log)?;
        Some(MarketEvent::MakerClosed {
            pos_id: d.posId,
            settle: maker_settle(d.funding, d.longUtilFees, d.shortUtilFees, d.lpFees)?,
            // Defaulted, as on `MakerConverted` above.
            liquidation_fee: UsdcAtoms::ZERO,
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
            liquidation_fee: u256_usdc(d.liqFee, "settled liquidation fee")?,
            is_liquidation: d.isLiquidation,
        })
    } else if topic0 == PerpDeployedEvents::MakerClosed::SIGNATURE_HASH {
        let d = decode_raw::<PerpDeployedEvents::MakerClosed>(log)?;
        Some(MarketEvent::MakerClosed {
            pos_id: d.posId,
            settle: maker_settle(d.funding, d.longUtilFees, d.shortUtilFees, d.lpFees)?,
            liquidation_fee: u256_usdc(d.liqFee, "settled liquidation fee")?,
            is_liquidation: d.isLiquidation,
        })
    } else if topic0 == Perp::MakerLiquidated::SIGNATURE_HASH {
        let d = decode_raw::<Perp::MakerLiquidated>(log)?;
        Some(MarketEvent::MakerLiquidated {
            pos_id: d.posId,
            liquidity_amount: LUnits::new(d.liquidityAmount),
            liquidation_fee: u256_usdc(d.liqFee, "settled liquidation fee")?,
        })
    } else if topic0 == Perp::MakerBackstopped::SIGNATURE_HASH {
        let d = decode_raw::<Perp::MakerBackstopped>(log)?;
        Some(MarketEvent::MakerBackstopped {
            pos_id: d.posId,
            margin_in: UsdcAtoms::new(d.marginIn),
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
            funding: i256_usdc(d.funding, "settled funding")?,
            util_fees: u256_usdc(d.utilFees, "settled utilization fees")?,
        })
    } else if topic0 == Perp::TakerClosed::SIGNATURE_HASH {
        let d = decode_raw::<Perp::TakerClosed>(log)?;
        Some(MarketEvent::TakerClosed {
            pos_id: d.posId,
            swap: swap_info(&d.sr)?,
            funding: i256_usdc(d.funding, "settled funding")?,
            util_fees: u256_usdc(d.utilFees, "settled utilization fees")?,
            liquidation_fee: u256_usdc(d.liqFee, "settled liquidation fee")?,
            is_liquidation: d.isLiquidation,
        })
    } else if topic0 == PerpV022::TakerClosed::SIGNATURE_HASH {
        // Untailed shape: a v0.2.2 liquidation says so in the
        // `TakerLiquidated` log after this one, so the tails default here.
        let d = decode_raw::<PerpV022::TakerClosed>(log)?;
        Some(MarketEvent::TakerClosed {
            pos_id: d.posId,
            swap: swap_info(&d.sr)?,
            funding: i256_usdc(d.funding, "settled funding")?,
            util_fees: u256_usdc(d.utilFees, "settled utilization fees")?,
            liquidation_fee: UsdcAtoms::ZERO,
            is_liquidation: false,
        })
    } else if topic0 == Perp::TakerLiquidated::SIGNATURE_HASH {
        let d = decode_raw::<Perp::TakerLiquidated>(log)?;
        Some(MarketEvent::TakerLiquidated {
            pos_id: d.posId,
            perp_amount: PerpAtoms::new(d.perpAmount),
            liquidation_fee: u256_usdc(d.liqFee, "settled liquidation fee")?,
        })
    } else if topic0 == Perp::TakerBackstopped::SIGNATURE_HASH {
        let d = decode_raw::<Perp::TakerBackstopped>(log)?;
        Some(MarketEvent::TakerBackstopped {
            pos_id: d.posId,
            margin_in: UsdcAtoms::new(d.marginIn),
            pos_recipient: d.posRecipient,
            funding: i256_usdc(d.funding, "settled funding")?,
            util_fees: u256_usdc(d.utilFees, "settled utilization fees")?,
        })

    // ── Market state ─────────────────────────────────────────────────
    } else if topic0 == Perp::CapacityUpdated::SIGNATURE_HASH {
        let d = decode_raw::<Perp::CapacityUpdated>(log)?;
        Some(MarketEvent::CapacityUpdated {
            capacity: d.cap.into(),
        })
    } else if topic0 == Perp::OpenInterestUpdated::SIGNATURE_HASH {
        let d = decode_raw::<Perp::OpenInterestUpdated>(log)?;
        Some(MarketEvent::OpenInterestUpdated {
            open_interest: d.oi.into(),
        })
    } else if topic0 == Perp::CumulativesAccrued::SIGNATURE_HASH {
        let d = decode_raw::<Perp::CumulativesAccrued>(log)?;
        Some(MarketEvent::CumulativesAccrued {
            cumulatives: d.cumls.into(),
        })
    } else if topic0 == Perp::RatesAndEmasRefreshed::SIGNATURE_HASH {
        let d = decode_raw::<Perp::RatesAndEmasRefreshed>(log)?;
        Some(MarketEvent::RatesAndEmasRefreshed {
            funding_per_day: FundingRate::from_wad(narrow_i128(
                d.rates.fundingPerDay.to_string(),
                d.rates.fundingPerDay.try_into(),
                "funding per day",
            )?),
            util_fee_per_day: PerSide::new(
                UtilizationRate::from_wad(d.rates.longUtilFeePerDay),
                UtilizationRate::from_wad(d.rates.shortUtilFeePerDay),
            ),
            last_touch: d.rates.lastTouch.to::<u64>(),
            pool_price_ema: Price::from_x96(U256::from(d.emas.ammPrice)),
            index_ema: Price::from_x96(U256::from(d.emas.index)),
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
            cuml_funding_opp: Funding::from_x96(d.cumlFundingOppX96),
            cuml_funding_div_sqrt_p_opp: FundingPerSqrtPrice::from_x96(d.cumlFundingDivSqrtPOppX96),
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
            amount: UsdcAtoms::new(d.amount),
            bad_debt: UsdcAtoms::new(d.badDebt),
            insurance: UsdcAtoms::new(d.insurance.to::<u128>()),
        })
    } else if topic0 == Perp::BadDebtAccounted::SIGNATURE_HASH {
        let d = decode_raw::<Perp::BadDebtAccounted>(log)?;
        Some(MarketEvent::BadDebtAccounted {
            bad_debt: u256_usdc(d.badDebt, "bad debt")?,
            insurance_after: u256_usdc(d.insuranceAfter, "insurance fund")?,
            bad_debt_after: UsdcAtoms::new(d.badDebtAfter),
        })
    } else if topic0 == Perp::LossSocialized::SIGNATURE_HASH {
        let d = decode_raw::<Perp::LossSocialized>(log)?;
        Some(MarketEvent::LossSocialized {
            original_amount: u256_usdc(d.originalAmount, "original amount")?,
            fee_charged: u256_usdc(d.feeCharged, "fee charged")?,
            bad_debt_after: UsdcAtoms::new(d.badDebtAfter),
        })
    } else if topic0 == Perp::MarginTransferred::SIGNATURE_HASH {
        let d = decode_raw::<Perp::MarginTransferred>(log)?;
        Some(MarketEvent::MarginTransferred {
            margin_delta: UsdcDelta::new(d.marginDelta),
            total_margin: UsdcAtoms::new(d.totalMargin),
        })

    // ── Oracle ───────────────────────────────────────────────────────
    } else if topic0 == IBeacon::IndexUpdated::SIGNATURE_HASH {
        let d = decode_raw::<IBeacon::IndexUpdated>(log)?;
        Some(MarketEvent::IndexUpdated {
            index: Price::from_x96(d.index),
        })

    // ── Pool / position NFT ──────────────────────────────────────────
    } else if topic0 == IPoolManagerState::Initialize::SIGNATURE_HASH {
        let d = decode_raw::<IPoolManagerState::Initialize>(log)?;
        Some(MarketEvent::PoolInitialized {
            pool_id: d.id,
            sqrt_price: SqrtPrice::from_x96(U256::from(d.sqrtPriceX96)),
            tick: d.tick.as_i32(),
        })
    } else if topic0 == IPoolManagerState::Swap::SIGNATURE_HASH {
        let d = decode_raw::<IPoolManagerState::Swap>(log)?;
        Some(MarketEvent::PoolSwapped {
            pool_id: d.id,
            sender: d.sender,
            sqrt_price: SqrtPrice::from_x96(U256::from(d.sqrtPriceX96)),
            liquidity: LUnits::new(d.liquidity),
            tick: d.tick.as_i32(),
        })
    } else if topic0 == IPoolManagerState::ModifyLiquidity::SIGNATURE_HASH {
        let d = decode_raw::<IPoolManagerState::ModifyLiquidity>(log)?;
        Some(MarketEvent::ModifyLiquidity {
            pool_id: d.id,
            sender: d.sender,
            tick_lower: d.tickLower.as_i32(),
            tick_upper: d.tickUpper.as_i32(),
            liquidity_delta: LDelta::new(narrow_i128(
                d.liquidityDelta.to_string(),
                d.liquidityDelta.try_into(),
                "liquidity delta",
            )?),
            salt: d.salt,
        })
    } else if topic0 == Perp::Transfer::SIGNATURE_HASH {
        // ERC20 `Transfer` shares this topic0, so the topic alone does not say
        // whose event this is; the arity does. At the ERC20 arity it is someone
        // else's token moving and the failure to decode is not a gap, which is
        // the one place in this function that is true. At any other arity the
        // log claims to be the position NFT's, so a failure is a gap and an
        // error, like every other branch here.
        if log.topics().len() == ERC20_TRANSFER_TOPICS {
            None
        } else {
            let d = decode_raw::<Perp::Transfer>(log)?;
            Some(MarketEvent::PositionTransferred {
                from: d.from,
                to: d.to,
                pos_id: d.tokenId,
            })
        }

    // ── Governance ───────────────────────────────────────────────────
    } else if topic0 == Perp::SetBeacon::SIGNATURE_HASH {
        let d = decode_raw::<Perp::SetBeacon>(log)?;
        Some(module_set(ModuleKind::Beacon, d.beacon))
    } else if topic0 == Perp::SetFeesModule::SIGNATURE_HASH {
        let d = decode_raw::<Perp::SetFeesModule>(log)?;
        Some(module_set(ModuleKind::Fees, d.fees))
    } else if topic0 == Perp::SetFundingModule::SIGNATURE_HASH {
        let d = decode_raw::<Perp::SetFundingModule>(log)?;
        Some(module_set(ModuleKind::Funding, d.funding))
    } else if topic0 == Perp::SetMarginRatiosModule::SIGNATURE_HASH {
        let d = decode_raw::<Perp::SetMarginRatiosModule>(log)?;
        Some(module_set(ModuleKind::MarginRatios, d.marginRatios))
    } else if topic0 == Perp::SetPriceImpactModule::SIGNATURE_HASH {
        let d = decode_raw::<Perp::SetPriceImpactModule>(log)?;
        Some(module_set(ModuleKind::PriceImpact, d.priceImpact))
    } else if topic0 == Perp::SetPricingModule::SIGNATURE_HASH {
        let d = decode_raw::<Perp::SetPricingModule>(log)?;
        Some(module_set(ModuleKind::Pricing, d.pricing))
    } else {
        None
    };
    Ok(event)
}

fn module_set(module: ModuleKind, address: Address) -> MarketEvent {
    MarketEvent::ModuleSet { module, address }
}

/// Decode a typed event from a raw log's topics + data.
///
/// The topic already said which event this is, so a failure here is a log
/// whose shape disagrees with the binding — an era we decode wrongly, or a
/// value we cannot hold — and the error names the signature it was read as.
pub(crate) fn decode_raw<E: SolEvent>(log: &Log) -> Result<E, ValidationError> {
    E::decode_raw_log(
        log.inner.data.topics().iter().copied(),
        log.inner.data.data.as_ref(),
    )
    .map_err(|e| ValidationError::DecodeFailed {
        context: format!("{}: {e}", E::SIGNATURE),
    })
}

/// Build a [`SwapInfo`] from a contract [`SwapResult`].
fn swap_info(sr: &SwapResult) -> Result<SwapInfo, ValidationError> {
    let (perp, usd) = unpack_balance_delta(sr.delta);
    Ok(SwapInfo {
        perp_delta: PerpDelta::new(perp),
        usd_delta: UsdcDelta::new(usd),
        pool_price: Price::from_x96(sr.ammPrice),
        total_fee: i256_usdc(sr.totalFeeAmt, "swap total fee")?,
        lp_fee: u256_usdc(sr.lpFeeAmt, "swap LP fee")?,
        protocol_fee: u256_usdc(sr.protocolFeeAmt, "swap protocol fee")?,
        creator_fee: u256_usdc(sr.creatorFeeAmt, "swap creator fee")?,
        insurance_fee: u256_usdc(sr.insuranceFeeAmt, "swap insurance fee")?,
    })
}

/// Build a [`MakerSettle`] from the raw funding/fee fields of a maker event.
fn maker_settle(
    funding: I256,
    long_util_fees: U256,
    short_util_fees: U256,
    lp_fees: U256,
) -> Result<MakerSettle, ValidationError> {
    Ok(MakerSettle {
        funding: i256_usdc(funding, "settled funding")?,
        util_fees: PerSide::new(
            u256_usdc(long_util_fees, "settled long utilization fees")?,
            u256_usdc(short_util_fees, "settled short utilization fees")?,
        ),
        lp_fees: u256_usdc(lp_fees, "settled LP fees")?,
    })
}

/// A signed USDC word, narrowed to the width the SDK holds a delta in.
fn i256_usdc(v: I256, what: &str) -> Result<UsdcDelta, ValidationError> {
    Ok(UsdcDelta::new(narrow_i128(
        v.to_string(),
        v.try_into(),
        what,
    )?))
}

/// An unsigned USDC word, narrowed to the width the contracts store it in.
fn u256_usdc(v: U256, what: &str) -> Result<UsdcAtoms, ValidationError> {
    Ok(UsdcAtoms::new(narrow_u128(
        v.to_string(),
        v.try_into(),
        what,
    )?))
}

/// A contract word narrowed to the width the SDK holds an amount in, naming
/// the field when it does not fit: a value this wide is a log the decoder
/// recognised and cannot represent, which is an error rather than a gap.
fn narrow_i128<E>(
    shown: String,
    narrowed: Result<i128, E>,
    what: &str,
) -> Result<i128, ValidationError> {
    narrowed.map_err(|_| ValidationError::DecodeFailed {
        context: format!("{what} {shown} exceeds i128"),
    })
}

/// The unsigned twin of [`narrow_i128`].
fn narrow_u128<E>(
    shown: String,
    narrowed: Result<u128, E>,
    what: &str,
) -> Result<u128, ValidationError> {
    narrowed.map_err(|_| ValidationError::DecodeFailed {
        context: format!("{what} {shown} exceeds u128"),
    })
}

/// An RPC log carrying `event`, emitted by `address`, as a receipt returns
/// it: the shape the decoders take, for tests.
#[cfg(test)]
pub(crate) fn rpc_log<E: SolEvent>(event: &E, address: Address) -> Log {
    Log {
        inner: alloy::primitives::Log {
            address,
            data: event.encode_log_data(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256, LogData, U256};
    use alloy::rpc::types::Log as RpcLog;

    use crate::constants::Q96;
    use crate::convert::pack_balance_delta;

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

        let log = rpc_log(&event, Address::ZERO);
        match decode_log(&log)
            .unwrap()
            .expect("should decode TakerOpened")
        {
            MarketEvent::TakerOpened { pos_id, swap } => {
                assert_eq!(pos_id, U256::from(42u64));
                assert_eq!(swap.perp_delta.atoms(), 100_000_000);
                assert_eq!(swap.usd_delta.atoms(), -100_000_000);
                assert_eq!(swap.pool_price.x96(), Q96);
                assert_eq!(swap.total_fee.atoms(), 1_000_000);
                // The design says the four shares account for the whole
                // fee. Nothing checked it until the counts could be added.
                assert_eq!(
                    swap.lp_fee + swap.protocol_fee + swap.creator_fee + swap.insurance_fee,
                    swap.total_fee.magnitude(),
                    "the four shares sum to the total"
                );
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

        let log = rpc_log(&event, Address::ZERO);
        match decode_log(&log)
            .unwrap()
            .expect("should decode TakerLiquidated")
        {
            MarketEvent::TakerLiquidated {
                pos_id,
                perp_amount,
                liquidation_fee,
            } => {
                assert_eq!(pos_id, U256::from(7u64));
                assert_eq!(perp_amount.atoms(), 50_000_000);
                assert_eq!(liquidation_fee.atoms(), 1_000_000);
            }
            _ => panic!("expected TakerLiquidated"),
        }
    }

    #[test]
    fn decode_maker_opened_event() {
        let event = Perp::MakerOpened {
            posId: U256::from(3u64),
        };
        let log = rpc_log(&event, Address::ZERO);
        match decode_log(&log)
            .unwrap()
            .expect("should decode MakerOpened")
        {
            MarketEvent::MakerOpened { pos_id } => assert_eq!(pos_id, U256::from(3u64)),
            _ => panic!("expected MakerOpened"),
        }
    }

    /// Each of the six module setters decodes to `ModuleSet` naming its
    /// module, so a fold can tell which rules were in force.
    #[test]
    fn decode_module_set_events() {
        let module = Address::repeat_byte(0x4D);
        let cases: [(LogData, ModuleKind); 6] = [
            (
                Perp::SetBeacon { beacon: module }.encode_log_data(),
                ModuleKind::Beacon,
            ),
            (
                Perp::SetFeesModule { fees: module }.encode_log_data(),
                ModuleKind::Fees,
            ),
            (
                Perp::SetFundingModule { funding: module }.encode_log_data(),
                ModuleKind::Funding,
            ),
            (
                Perp::SetMarginRatiosModule {
                    marginRatios: module,
                }
                .encode_log_data(),
                ModuleKind::MarginRatios,
            ),
            (
                Perp::SetPriceImpactModule {
                    priceImpact: module,
                }
                .encode_log_data(),
                ModuleKind::PriceImpact,
            ),
            (
                Perp::SetPricingModule { pricing: module }.encode_log_data(),
                ModuleKind::Pricing,
            ),
        ];
        for (data, expected) in cases {
            let log = RpcLog {
                inner: alloy::primitives::Log {
                    address: Address::ZERO,
                    data,
                },
                ..Default::default()
            };
            match decode_log(&log).unwrap().expect("a module setter decodes") {
                MarketEvent::ModuleSet {
                    module: kind,
                    address,
                } => {
                    assert_eq!(kind, expected);
                    assert_eq!(address, module);
                }
                other => panic!("expected ModuleSet, got {other:?}"),
            }
        }
    }

    /// Golden vector: a real `SetPricingModule` log from Arbitrum One
    /// (HORMUZ-COUNT-PERP `0x8ac0…7b6c`, block 477_478_424, tx
    /// `0xfd4168ead7a8…`, log 17). The signature hash cannot say which
    /// field is indexed; this log can: the module is the second topic and
    /// the data is empty, on the deployed build as in the declaration.
    #[test]
    fn decode_mainnet_set_pricing_module_golden_vector() {
        let log = RpcLog {
            inner: alloy::primitives::Log {
                address: alloy::primitives::address!("8ac0179073a9eb5aaee58e5ebe9882066b9e7b6c"),
                data: LogData::new_unchecked(
                    vec![
                        alloy::primitives::b256!(
                            "a3c68ccb672060124d2ccfc83677f8c033e5f93ec4eff04e73206a48129d9c28"
                        ),
                        alloy::primitives::b256!(
                            "000000000000000000000000ac7d819ba220fda0e59b05db61afbbe9ab852914"
                        ),
                    ],
                    alloy::primitives::Bytes::new(),
                ),
            },
            ..Default::default()
        };
        match decode_log(&log)
            .unwrap()
            .expect("should decode mainnet SetPricingModule")
        {
            MarketEvent::ModuleSet { module, address } => {
                assert_eq!(module, ModuleKind::Pricing);
                assert_eq!(
                    address,
                    alloy::primitives::address!("ac7d819ba220fda0e59b05db61afbbe9ab852914")
                );
            }
            other => panic!("expected ModuleSet, got {other:?}"),
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
        let log = rpc_log(&event, Address::ZERO);
        match decode_log(&log)
            .unwrap()
            .expect("should decode OpenInterestUpdated")
        {
            MarketEvent::OpenInterestUpdated { open_interest } => {
                assert_eq!(open_interest.long.atoms(), 2_000_000);
                assert_eq!(open_interest.short.atoms(), 1_000_000);
            }
            _ => panic!("expected OpenInterestUpdated"),
        }
    }

    #[test]
    fn decode_index_updated_event() {
        let event = IBeacon::IndexUpdated {
            index: Q96 * U256::from(100u64), // index = 100.0
        };

        let log = rpc_log(&event, Address::ZERO);
        match decode_log(&log)
            .unwrap()
            .expect("should decode IndexUpdated")
        {
            MarketEvent::IndexUpdated { index } => {
                assert_eq!(index.x96(), Q96 * U256::from(100u64));
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
        let log = rpc_log(&event, Address::ZERO);
        match decode_log(&log)
            .unwrap()
            .expect("should decode deployed-era MakerClosed")
        {
            MarketEvent::MakerClosed {
                pos_id,
                settle,
                liquidation_fee,
                is_liquidation,
            } => {
                assert_eq!(pos_id, U256::from(9u64));
                assert_eq!(settle.funding.atoms(), 2_000_000);
                assert_eq!(settle.util_fees.long.atoms(), 500_000);
                assert_eq!(settle.util_fees.short.atoms(), 250_000);
                assert_eq!(settle.lp_fees.atoms(), 1_500_000);
                assert_eq!(liquidation_fee.atoms(), 750_000);
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
        let log = rpc_log(&event, Address::ZERO);
        match decode_log(&log)
            .unwrap()
            .expect("should decode MakerClosed")
        {
            MarketEvent::MakerClosed {
                liquidation_fee,
                is_liquidation,
                ..
            } => {
                assert_eq!(liquidation_fee, UsdcAtoms::ZERO);
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
        let log = rpc_log(&event, Address::ZERO);
        match decode_log(&log)
            .unwrap()
            .expect("should decode TakerClosed")
        {
            MarketEvent::TakerClosed {
                pos_id,
                funding,
                util_fees,
                liquidation_fee,
                is_liquidation,
                ..
            } => {
                assert_eq!(pos_id, U256::from(5u64));
                assert_eq!(funding.atoms(), -250_000);
                assert_eq!(util_fees.atoms(), 10_000);
                assert_eq!(liquidation_fee.atoms(), 1_250_000);
                assert!(is_liquidation);
            }
            other => panic!("expected TakerClosed, got {other:?}"),
        }
    }

    /// The v0.2.2 `TakerClosed` carries no liquidation tails; it decodes to
    /// the same variant with both defaulted, as the untailed maker closes do.
    #[test]
    fn decode_v022_taker_closed_defaults_the_tails() {
        let event = PerpV022::TakerClosed {
            posId: U256::from(9u64),
            sr: SwapResult {
                delta: I256::ZERO,
                ammPrice: U256::from(1u8) << 96,
                totalFeeAmt: I256::ZERO,
                lpFeeAmt: U256::ZERO,
                protocolFeeAmt: U256::ZERO,
                creatorFeeAmt: U256::ZERO,
                insuranceFeeAmt: U256::ZERO,
            },
            funding: I256::try_from(-7_000i64).unwrap(),
            utilFees: U256::from(300u64),
        };
        let log = rpc_log(&event, Address::ZERO);
        assert_eq!(
            log.topic0().copied(),
            Some(alloy::primitives::b256!(
                "208f950e4dba30512aa9e643b25c9df8bdb616ee90bbff00f669a5d1d3d452f3"
            ))
        );
        match decode_log(&log)
            .unwrap()
            .expect("should decode the v0.2.2 TakerClosed")
        {
            MarketEvent::TakerClosed {
                pos_id,
                funding,
                util_fees,
                liquidation_fee,
                is_liquidation,
                ..
            } => {
                assert_eq!(pos_id, U256::from(9u64));
                assert_eq!(funding.atoms(), -7_000);
                assert_eq!(util_fees.atoms(), 300);
                assert_eq!(liquidation_fee.atoms(), 0);
                assert!(!is_liquidation);
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
        match decode_log(&log)
            .unwrap()
            .expect("should decode mainnet MakerConverted")
        {
            MarketEvent::MakerConverted {
                pos_id,
                settle,
                liquidation_fee,
                is_liquidation,
            } => {
                assert_eq!(pos_id, U256::from(54u64));
                // The exact atoms the log carried, not an f64 within an
                // epsilon of them: the whole point of the typed fields.
                assert_eq!(settle.funding.atoms(), 209_633_223);
                assert_eq!(settle.util_fees.long.atoms(), 432_735);
                assert_eq!(settle.util_fees.short.atoms(), 24_630_722);
                assert_eq!(settle.lp_fees.atoms(), 7_722_360);
                // The liquidation tails: liqFee 0x15696a = 1.403242 USDC.
                assert_eq!(liquidation_fee.atoms(), 1_403_242);
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
        let log = rpc_log(&event, Address::repeat_byte(0x36));
        match decode_log(&log)
            .unwrap()
            .expect("should decode ModifyLiquidity")
        {
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
                assert_eq!(liquidity_delta, LDelta::new(-570_282_387));
                assert_eq!(U256::from_be_bytes(salt.0), U256::from(54u64));
            }
            other => panic!("expected ModifyLiquidity, got {other:?}"),
        }
    }

    /// A `liquidityDelta` outside `i128` cannot come from a V4 pool, where
    /// liquidity is `uint128`. The topic is ours and the value is not one we
    /// can hold, which is the case this signature exists to separate: it is
    /// an error naming the field, not another `None` a scan would drop
    /// alongside every admin log it skips.
    #[test]
    fn a_known_log_we_cannot_represent_is_an_error() {
        let event = IPoolManagerState::ModifyLiquidity {
            id: B256::ZERO,
            sender: Address::ZERO,
            tickLower: alloy::primitives::Signed::<24, 1>::ZERO,
            tickUpper: alloy::primitives::Signed::<24, 1>::ZERO,
            liquidityDelta: I256::MAX,
            salt: B256::ZERO,
        };
        let err = decode_log(&rpc_log(&event, Address::ZERO)).unwrap_err();
        assert!(
            matches!(&err, ValidationError::DecodeFailed { context } if context.contains("liquidity delta")),
            "the error names the field that would not fit: {err}"
        );
    }

    #[test]
    fn decode_position_transferred_mint() {
        let holder = Address::repeat_byte(0x11);
        let event = Perp::Transfer {
            from: Address::ZERO,
            to: holder,
            tokenId: U256::from(77u64),
        };
        let log = rpc_log(&event, Address::ZERO);
        match decode_log(&log).unwrap().expect("should decode Transfer") {
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
        let log = rpc_log(&event, Address::ZERO);
        assert_eq!(log.topic0(), Some(&Perp::Transfer::SIGNATURE_HASH));
        // Not an error, even though the topic is one of ours: the topic is
        // *shared* with ERC20, so this is someone else's event rather than a
        // log of ours we cannot read.
        assert!(decode_log(&log).unwrap().is_none());
    }

    /// The ERC20 arity is the only one the shared topic excuses. A `Transfer`
    /// log at any other arity claims to be the position NFT's, so a failure to
    /// read it is a gap like any other.
    #[test]
    fn a_transfer_at_neither_arity_is_an_error() {
        let log = RpcLog {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(
                    vec![Perp::Transfer::SIGNATURE_HASH, B256::repeat_byte(0x11)],
                    vec![].into(),
                ),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };
        let err = decode_log(&log).expect_err("a Transfer we cannot read is a gap");
        assert!(
            matches!(&err, ValidationError::DecodeFailed { context } if context.contains("Transfer")),
            "the error names the signature it tried: {err}"
        );
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
        assert!(decode_log(&log).unwrap().is_none());
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
        assert!(decode_log(&log).unwrap().is_none());
    }
}
