//! Write operations: open, close, adjust positions, transfers, approvals.

use alloy::primitives::{Address, B256, Bytes, I256, U256};
use alloy::rpc::types::{Log as RpcLog, TransactionReceipt};
use alloy::sol_types::SolEvent;
use serde::{Deserialize, Serialize};

use crate::constants::{MIN_OPENING_MARGIN, TICK_SPACING};
use crate::contracts::{IERC20, Perp, Position};
use crate::convert::{scale_from_6dec, scale_to_6dec, unpack_balance_delta};
use crate::errors::{ContractError, Result, TransactionError, ValidationError};
use crate::feeds::{MarketEvent, decode_log};
use crate::hft::gas::{GasLimits, Urgency};
use crate::math::range::TickRange;
use crate::math::tick::{align_tick_down, align_tick_up, price_to_tick};
use crate::units::{LDelta, LUnits, PerpAtoms, PerpDelta, UsdcAtoms, UsdcDelta};

use super::market::{Book, validate_fee_recipient};
use super::{MAX_APPROVAL, PerpClient, i32_to_i24};

// ── Parameters ──────────────────────────────────────────────────────

/// Client-facing parameters for opening a taker (long/short) position.
///
/// The SDK scales both to 6 decimals at the call: the perp token is an
/// `AccountingToken` with six, the same as the USDC margin, not eighteen.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpenTakerParams {
    /// Margin in USDC (e.g. `100.0` for 100 USDC).
    pub margin: f64,
    /// Perp token delta: positive = long, negative = short.
    /// Magnitude is the notional size in perp token units.
    pub perp_delta: f64,
    /// Slippage protection: max amount of token1 (USDC) willing to pay. `0` = no limit.
    pub amt1_limit: u128,
}

/// A taker open in the chain's own units: the single submission path, with
/// no float between the caller's figure and the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExactOpenTakerParams {
    /// Margin to post; at least [`MIN_OPENING_MARGIN`] atoms.
    pub margin: UsdcAtoms,
    /// The exposure to take: positive long, negative short.
    pub perp_delta: PerpDelta,
    /// Directional USDC limit, as [`TakerQuote::amt1_limit`](crate::TakerQuote::amt1_limit)
    /// produces it: the most to pay on a buy, the least to receive on a sell.
    pub amt1_limit: UsdcAtoms,
}

/// Client-facing parameters for opening a maker (LP) position.
///
/// The SDK converts these to contract types automatically:
/// - `margin` → scaled to 6 decimals
/// - `price_lower` / `price_upper` → converted to ticks
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpenMakerParams {
    /// Margin in USDC (e.g. `1000.0`).
    pub margin: f64,
    /// Lower bound of the price range.
    pub price_lower: f64,
    /// Upper bound of the price range.
    pub price_upper: f64,
    /// Liquidity amount to provide.
    pub liquidity: LUnits,
    /// Maximum amount of token0 willing to deposit.
    pub max_amt0_in: u128,
    /// Maximum amount of token1 willing to deposit.
    pub max_amt1_in: u128,
}

/// Client-facing parameters for adjusting a taker position.
///
/// Combines margin adjustment and notional adjustment in a single call.
/// To close a position, pass `perp_delta` opposing the position's current delta.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdjustTakerParams {
    /// Position NFT token ID.
    pub pos_id: U256,
    /// Margin delta in USDC: positive to deposit, negative to withdraw.
    pub margin_delta: f64,
    /// Perp token delta: positive to go more long, negative to go more short.
    /// Set to zero for margin-only adjustments.
    pub perp_delta: f64,
    /// Slippage protection: max amount of token1 (USDC). `0` = no limit.
    pub amt1_limit: u128,
}

/// A taker adjustment in the chain's own units: the single submission path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExactAdjustTakerParams {
    /// Position NFT token ID.
    pub pos_id: U256,
    /// Margin to deposit (positive) or withdraw (negative).
    pub margin_delta: UsdcDelta,
    /// The change in exposure; zero for a margin-only adjustment.
    pub perp_delta: PerpDelta,
    /// Directional USDC limit, as [`TakerQuote::amt1_limit`](crate::TakerQuote::amt1_limit)
    /// produces it.
    pub amt1_limit: UsdcAtoms,
}

/// A maker open in the chain's own units: the band as the ticks the pool
/// stores, the depth to stand in it, and the deposit caps. The single
/// submission path; [`OpenMakerParams`] is scaled and aligned into one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExactOpenMakerParams {
    /// Margin to post; at least [`MIN_OPENING_MARGIN`] atoms.
    pub margin: UsdcAtoms,
    /// The band. Its ticks must sit on the pool's spacing, which
    /// [`TickRange`] does not enforce and the contract does.
    pub range: TickRange,
    /// The depth to stand in the band.
    pub liquidity: LUnits,
    /// The most of the market's token the open may deposit.
    pub max_amt0_in: PerpAtoms,
    /// The most USDC the open may deposit.
    pub max_amt1_in: UsdcAtoms,
}

/// Client-facing parameters for adjusting a maker (LP) position.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdjustMakerParams {
    /// Position NFT token ID.
    pub pos_id: U256,
    /// Margin delta in USDC: positive to deposit, negative to withdraw.
    pub margin_delta: f64,
    /// Liquidity delta: positive to add, negative to remove.
    pub liquidity_delta: LDelta,
    /// Max/min amount of token0 for slippage protection.
    pub amt0_limit: u128,
    /// Max/min amount of token1 for slippage protection.
    pub amt1_limit: u128,
}

// ── Results ─────────────────────────────────────────────────────────

/// Result of opening a taker or maker position.
///
/// `pos_id` is the minted position NFT id. For taker opens, `perp_delta` and
/// `usd_delta` are the realized swap amounts decoded from the `TakerOpened`
/// event (signed: positive = received, negative = paid). Maker opens emit no
/// swap, so both are zero.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpenResult {
    /// Transaction hash.
    pub tx_hash: B256,
    /// Minted position NFT token ID.
    pub pos_id: U256,
    /// Realized perp-token delta from the open swap; zero for makers, which
    /// emit no swap.
    pub perp_delta: PerpDelta,
    /// Realized USDC delta from the open swap; zero for makers.
    pub usd_delta: UsdcDelta,
}

/// Result of adjusting a taker position (margin, notional, or both).
///
/// `perp_delta` and `usd_delta` are the realized swap amounts decoded from the
/// `TakerAdjusted` event — or `TakerClosed`, when the adjust reverses the full
/// delta and closes the position (signed: positive = received, negative =
/// paid). Both are zero for a margin-only adjust, which performs no swap.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdjustTakerResult {
    /// Transaction hash.
    pub tx_hash: B256,
    /// Realized perp-token delta from the adjust swap; zero if margin-only.
    pub perp_delta: PerpDelta,
    /// Realized USDC delta from the adjust swap; zero if margin-only.
    pub usd_delta: UsdcDelta,
}

/// Result of adjusting a maker position (margin, liquidity, or both).
///
/// Events are parameterless — read position state via view functions if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdjustMakerResult {
    /// Transaction hash.
    pub tx_hash: B256,
}

/// Extract the minted token ID from an ERC721 `Transfer(address(0), to, tokenId)` event.
///
/// The Perp contract inherits ERC721 and mints a position NFT on open.
/// The standard Transfer event carries the token ID.
fn parse_minted_token_id(receipt: &TransactionReceipt) -> std::result::Result<U256, ContractError> {
    // ERC721 Transfer event: Transfer(address indexed from, address indexed to, uint256 indexed tokenId)
    // topic0 = keccak256("Transfer(address,address,uint256)")
    let transfer_topic = IERC20::Transfer::SIGNATURE_HASH;
    for log in receipt.inner.logs() {
        let topics = log.topics();
        if topics.len() >= 4 && topics[0] == transfer_topic && topics[1] == B256::ZERO
        // from = address(0) means mint
        {
            // tokenId is topic[3] (indexed)
            return Ok(U256::from_be_bytes(topics[3].0));
        }
    }
    Err(ContractError::EventNotFound {
        event_name: "ERC721 Transfer (mint)".into(),
    })
}

/// Extract the realized swap `(perp_delta, usd_delta)` from a taker
/// open/adjust/close receipt.
///
/// Reuses the market feed's [`decode_log`] to find the `TakerOpened` /
/// `TakerAdjusted` / `TakerClosed` event and reads its decoded `SwapInfo`
/// (already unpacked from the `BalanceDelta`). Every taker
/// open/adjust/close emits one of these — a margin-only adjust still emits a
/// `TakerAdjusted` with a zero-delta swap — so on the taker paths `None` means
/// a decode/ABI failure, which the caller surfaces as an error rather than a
/// zero fill. (Maker opens emit no taker swap, but they don't call this.)
fn parse_taker_swap(receipt: &TransactionReceipt) -> Option<(PerpDelta, UsdcDelta)> {
    for log in receipt.inner.logs() {
        // An undecodable log is no different here from an unrecognised one:
        // the caller's `ok_or` turns a receipt with no readable swap into
        // `EventNotFound`, which says the same thing with the position's
        // context attached.
        if let Ok(Some(
            MarketEvent::TakerOpened { swap, .. }
            | MarketEvent::TakerAdjusted { swap, .. }
            | MarketEvent::TakerClosed { swap, .. },
        )) = decode_log(log)
        {
            return Some((swap.perp_delta, swap.usd_delta));
        }
    }
    None
}

/// Read the realized swap from a taker adjust or close receipt.
///
/// Every taker adjust emits a decodable event — `TakerAdjusted` (a
/// margin-only adjust carries a zero-delta swap) or `TakerClosed` on a full
/// close. A missing one signals an ABI/decode problem, so fail loudly rather
/// than recording a bogus zero fill.
fn adjust_taker_result(receipt: &TransactionReceipt) -> Result<AdjustTakerResult> {
    let (perp_delta, usd_delta) =
        parse_taker_swap(receipt).ok_or(ContractError::EventNotFound {
            event_name: "TakerAdjusted/TakerClosed".into(),
        })?;
    Ok(AdjustTakerResult {
        tx_hash: receipt.transaction_hash,
        perp_delta,
        usd_delta,
    })
}

/// The perp delta that closes a position: its on-chain perp amount, reversed.
fn closing_perp_delta(position: &Position) -> PerpDelta {
    let (perp_atoms, _usd_atoms) = unpack_balance_delta(position.delta);
    PerpDelta::new(-perp_atoms)
}

/// Refuse a margin below the protocol's opening minimum.
fn check_opening_margin(margin: UsdcAtoms) -> std::result::Result<(), ValidationError> {
    if margin.atoms() < u128::from(MIN_OPENING_MARGIN) {
        return Err(ValidationError::InvalidMargin {
            reason: format!("margin must be at least {MIN_OPENING_MARGIN} atoms"),
        });
    }
    Ok(())
}

/// Whether these logs carry the `TakerClosed` event for `pos_id`.
///
/// A close whose swap leaves any perp delta mines as `TakerAdjusted`
/// instead, with the position still open.
fn closes_taker(logs: &[RpcLog], pos_id: U256) -> bool {
    logs.iter().any(|log| {
        matches!(
            decode_log(log),
            Ok(Some(MarketEvent::TakerClosed { pos_id: closed, .. })) if closed == pos_id
        )
    })
}

/// Scale and validate position margin against the protocol's opening minimum.
///
/// Checks the scaled margin parameter against [`MIN_OPENING_MARGIN`] and returns
/// [`ValidationError::InvalidMargin`] when it is below the protocol minimum.
fn scale_opening_margin(margin: f64) -> std::result::Result<UsdcAtoms, ValidationError> {
    let scaled = scale_to_6dec(margin)?;
    if scaled < i128::from(MIN_OPENING_MARGIN) {
        let minimum = scale_from_6dec(i128::from(MIN_OPENING_MARGIN));
        return Err(ValidationError::InvalidMargin {
            reason: format!("margin must be at least {minimum} USDC, got {margin}"),
        });
    }
    // At least the minimum, so the cast to unsigned cannot wrap.
    Ok(UsdcAtoms::new(scaled as u128))
}

impl PerpClient {
    // ── Position operations ──────────────────────────────────────────

    /// Open a taker (long/short) position.
    ///
    /// Scales the human-readable parameters to wire units and delegates to
    /// [`Self::open_taker_exact`], which is the single submission path.
    /// Returns an [`OpenResult`] with the transaction hash and position ID.
    pub async fn open_taker(
        &self,
        params: &OpenTakerParams,
        urgency: Urgency,
    ) -> Result<OpenResult> {
        let exact = ExactOpenTakerParams {
            margin: scale_opening_margin(params.margin)?,
            // The perp token (V4 `currency0`) is an AccountingToken with 6
            // decimals — the same scaling as USD margin, not 1e18.
            perp_delta: PerpDelta::try_from(params.perp_delta)?,
            amt1_limit: UsdcAtoms::new(params.amt1_limit),
        };
        self.open_taker_exact(&exact, urgency).await
    }

    /// Open a taker position without converting through floating point.
    pub async fn open_taker_exact(
        &self,
        params: &ExactOpenTakerParams,
        urgency: Urgency,
    ) -> Result<OpenResult> {
        check_opening_margin(params.margin)?;
        let wire_params = crate::contracts::OpenTakerParams {
            holder: self.address,
            margin: params.margin.atoms(),
            perpDelta: I256::try_from(params.perp_delta.atoms()).expect("i128 fits I256"),
            amt1Limit: U256::from(params.amt1_limit.atoms()),
        };
        let contract = Perp::new(self.market.perp(), self.chain().provider());

        tracing::debug!(
            margin_atoms = params.margin.atoms(),
            perp_delta_atoms = params.perp_delta.atoms(),
            ?urgency,
            "opening taker position"
        );

        let receipt = self
            .tx(
                self.market.perp(),
                contract.openTaker(wire_params).calldata().clone(),
            )
            .with_urgency(urgency)
            .send()
            .await?;
        let pos_id = parse_minted_token_id(&receipt)?;
        // A taker open always emits a decodable `TakerOpened`; a missing one
        // signals an ABI/decode problem, so fail loudly rather than recording a
        // bogus zero fill.
        let (perp_delta, usd_delta) =
            parse_taker_swap(&receipt).ok_or(ContractError::EventNotFound {
                event_name: "TakerOpened".into(),
            })?;
        tracing::debug!(pos_id = %pos_id, "taker position opened");
        Ok(OpenResult {
            tx_hash: receipt.transaction_hash,
            pos_id,
            perp_delta,
            usd_delta,
        })
    }

    /// Open a maker (LP) position within a price range.
    ///
    /// Scales the margin and widens the prices to the ticks on the pool's
    /// spacing that enclose them, then delegates to
    /// [`Self::open_maker_exact`], which is the single submission path.
    pub async fn open_maker(
        &self,
        params: &OpenMakerParams,
        urgency: Urgency,
    ) -> Result<OpenResult> {
        let tick_lower = align_tick_down(price_to_tick(params.price_lower)?, TICK_SPACING);
        let tick_upper = align_tick_up(price_to_tick(params.price_upper)?, TICK_SPACING);
        let exact = ExactOpenMakerParams {
            margin: scale_opening_margin(params.margin)?,
            range: TickRange::new(tick_lower, tick_upper)?,
            liquidity: params.liquidity,
            max_amt0_in: PerpAtoms::new(params.max_amt0_in),
            max_amt1_in: UsdcAtoms::new(params.max_amt1_in),
        };
        self.open_maker_exact(&exact, urgency).await
    }

    /// Open a maker position on a band already expressed as the pool's
    /// ticks, without converting through floating point.
    pub async fn open_maker_exact(
        &self,
        params: &ExactOpenMakerParams,
        urgency: Urgency,
    ) -> Result<OpenResult> {
        check_opening_margin(params.margin)?;
        let wire_params = crate::contracts::OpenMakerParams {
            holder: self.address,
            margin: params.margin.atoms(),
            tickLower: i32_to_i24(params.range.lower()),
            tickUpper: i32_to_i24(params.range.upper()),
            liquidity: params.liquidity.units(),
            maxAmt0In: U256::from(params.max_amt0_in.atoms()),
            maxAmt1In: U256::from(params.max_amt1_in.atoms()),
        };

        tracing::debug!(
            margin_atoms = params.margin.atoms(),
            tick_lower = params.range.lower(),
            tick_upper = params.range.upper(),
            liquidity = params.liquidity.units(),
            ?urgency,
            "opening maker position"
        );

        let contract = Perp::new(self.market.perp(), self.chain().provider());
        let calldata = contract.openMaker(wire_params).calldata().clone();

        let receipt = self
            .tx(self.market.perp(), calldata)
            .with_urgency(urgency)
            .send()
            .await?;

        let pos_id = parse_minted_token_id(&receipt)?;
        let result = OpenResult {
            tx_hash: receipt.transaction_hash,
            pos_id,
            // Maker opens emit no taker swap (`MakerOpened` carries no deltas).
            perp_delta: PerpDelta::ZERO,
            usd_delta: UsdcDelta::ZERO,
        };
        tracing::debug!(pos_id = %result.pos_id, "maker position opened");
        Ok(result)
    }

    /// Adjust a taker position (margin, notional, or both).
    ///
    /// To close a position, pass `perp_delta` opposing the position's current delta.
    ///
    /// Scales the human-readable parameters to wire units and delegates to
    /// [`Self::adjust_taker_exact`], which is the single submission path.
    pub async fn adjust_taker(
        &self,
        params: &AdjustTakerParams,
        urgency: Urgency,
    ) -> Result<AdjustTakerResult> {
        let exact = ExactAdjustTakerParams {
            pos_id: params.pos_id,
            margin_delta: UsdcDelta::try_from(params.margin_delta)?,
            perp_delta: PerpDelta::try_from(params.perp_delta)?,
            amt1_limit: UsdcAtoms::new(params.amt1_limit),
        };
        self.adjust_taker_exact(&exact, urgency).await
    }

    /// Adjust a taker position without converting through floating point.
    pub async fn adjust_taker_exact(
        &self,
        params: &ExactAdjustTakerParams,
        urgency: Urgency,
    ) -> Result<AdjustTakerResult> {
        let receipt = self.send_adjust_taker(params, urgency).await?;
        adjust_taker_result(&receipt)
    }

    /// Broadcast `adjustTaker` and wait for its receipt.
    async fn send_adjust_taker(
        &self,
        params: &ExactAdjustTakerParams,
        urgency: Urgency,
    ) -> Result<TransactionReceipt> {
        let wire_params = crate::contracts::AdjustTakerParams {
            posId: params.pos_id,
            marginDelta: params.margin_delta.atoms(),
            perpDelta: I256::try_from(params.perp_delta.atoms()).expect("i128 fits I256"),
            amt1Limit: U256::from(params.amt1_limit.atoms()),
        };
        let contract = Perp::new(self.market.perp(), self.chain().provider());

        tracing::debug!(
            pos_id = %params.pos_id,
            margin_delta_atoms = params.margin_delta.atoms(),
            perp_delta_atoms = params.perp_delta.atoms(),
            ?urgency,
            "adjusting taker position"
        );

        let receipt = self
            .tx(
                self.market.perp(),
                contract.adjustTaker(wire_params).calldata().clone(),
            )
            .with_urgency(urgency)
            .send()
            .await?;

        tracing::debug!(pos_id = %params.pos_id, "taker position adjusted");
        Ok(receipt)
    }

    /// Close a taker position at market.
    ///
    /// Reads the position's perp delta on-chain and reverses it exactly. The
    /// contract closes a taker only when its remaining perp delta is zero,
    /// to the atom: it then pays the position's equity (margin plus realized
    /// PnL, less fees and funding) to the caller and burns the NFT. A delta
    /// that is off by one atom leaves the position open with all its margin,
    /// which is why the delta comes from the chain and not from the caller.
    ///
    /// Fails with [`TransactionError::TakerNotClosed`] when the close mines
    /// as an adjust instead: the position changed between the read and the
    /// block the close landed in. The error is transient; a retry reads the
    /// delta again.
    ///
    /// This is a market close: slippage is unconstrained. The `amt1` limit is
    /// set to the no-op sentinel for the swap direction — selling (reversing a
    /// long) floors the USD received at `0`; buying (reversing a short) caps
    /// the USD paid at `u128::MAX`. For a protected close, call
    /// [`Self::adjust_taker_exact`] with an explicit `amt1_limit` and the
    /// position's exact delta.
    pub async fn close_taker(&self, pos_id: U256, urgency: Urgency) -> Result<AdjustTakerResult> {
        let position = self.market.get_position(pos_id).await?;
        let perp_delta = closing_perp_delta(&position);
        let params = ExactAdjustTakerParams {
            pos_id,
            margin_delta: UsdcDelta::ZERO,
            perp_delta,
            // Buying back a short pays USDC and takes no cap; selling off a
            // long receives it and accepts any amount.
            amt1_limit: UsdcAtoms::new(if perp_delta.atoms() > 0 { u128::MAX } else { 0 }),
        };
        let receipt = self.send_adjust_taker(&params, urgency).await?;
        if !closes_taker(receipt.inner.logs(), pos_id) {
            return Err(TransactionError::TakerNotClosed {
                tx_hash: receipt.transaction_hash,
                pos_id,
            }
            .into());
        }
        adjust_taker_result(&receipt)
    }

    /// Adjust a maker position (margin, liquidity, or both).
    pub async fn adjust_maker(
        &self,
        params: &AdjustMakerParams,
        urgency: Urgency,
    ) -> Result<AdjustMakerResult> {
        let margin_delta = scale_to_6dec(params.margin_delta)?;

        let wire_params = crate::contracts::AdjustMakerParams {
            posId: params.pos_id,
            marginDelta: margin_delta,
            liquidityDelta: params.liquidity_delta.units(),
            amt0Limit: U256::from(params.amt0_limit),
            amt1Limit: U256::from(params.amt1_limit),
        };

        tracing::debug!(
            pos_id = %params.pos_id,
            margin_delta = params.margin_delta,
            liquidity_delta = params.liquidity_delta.units(),
            ?urgency,
            "adjusting maker position"
        );

        let contract = Perp::new(self.market.perp(), self.chain().provider());
        let calldata = contract.adjustMaker(wire_params).calldata().clone();

        let receipt = self
            .tx(self.market.perp(), calldata)
            .with_urgency(urgency)
            .send()
            .await?;

        tracing::debug!(pos_id = %params.pos_id, "maker position adjusted");
        Ok(AdjustMakerResult {
            tx_hash: receipt.transaction_hash,
        })
    }

    /// Close a maker position by removing its full liquidity.
    ///
    /// Removing all liquidity drives the position to zero, which the contract
    /// settles automatically: it returns the position's tokens/equity to the
    /// caller and burns the position NFT.
    ///
    /// `current_liquidity` must be the position's full current liquidity,
    /// typically from locally tracked state.
    ///
    /// This is a market close: the `amt0`/`amt1` minimums are set to `0`
    /// (accept any output). For a protected close, call [`Self::adjust_maker`]
    /// directly with explicit limits.
    pub async fn close_maker(
        &self,
        pos_id: U256,
        current_liquidity: LUnits,
        urgency: Urgency,
    ) -> Result<AdjustMakerResult> {
        let liquidity_delta = current_liquidity.negated()?;
        self.adjust_maker(
            &AdjustMakerParams {
                pos_id,
                margin_delta: 0.0,
                liquidity_delta,
                amt0_limit: 0,
                amt1_limit: 0,
            },
            urgency,
        )
        .await
    }

    // ── Liquidations (permissionless) ───────────────────────────────

    /// Check whether a maker `pos_id` is liquidatable right now, via
    /// `eth_call` — the batch/scanner probe.
    ///
    /// Use this to sweep candidate ids WITHOUT racing: the contract is the
    /// health oracle, and `Ok` means the liquidation would execute. When a
    /// position looks liquidatable and latency matters, call
    /// [`Self::liquidate_maker`] directly instead of chaining
    /// simulate-then-send — the send runs its own preflight, so the extra
    /// serial `eth_call` only costs time in the race.
    ///
    /// A contract revert surfaces as
    /// [`TransactionError::SimulationReverted`];
    /// triage it typed via
    /// [`TransactionError::is_revert`](crate::errors::TransactionError::is_revert):
    ///
    /// - `err.is_revert::<Perp::NotLiquidatable>()` — healthy right now;
    ///   retry the id later.
    /// - `err.is_revert::<Perp::NonMakerPosition>()` — a taker or burned
    ///   id on the maker path; drop it (or route it to the taker twin).
    /// - other reverts (a utilization gate from capacity pinned under live
    ///   OI, …) — inspect `error_name`.
    /// - an empty revert or out-of-gas inside the pinned limit
    ///   ([`SimulationFailed`](crate::errors::TransactionError::SimulationFailed),
    ///   not transient) — the node's answer; retrying reproduces it.
    /// - transport failures
    ///   ([`GasUnavailable`](crate::errors::TransactionError::GasUnavailable),
    ///   transient) — keep liquidating.
    ///
    /// The simulation runs from this client's address and is capped at
    /// [`GasLimits::LIQUIDATE`], exactly like the send.
    pub async fn simulate_liquidate_maker(
        &self,
        pos_id: U256,
        fee_recipient: Address,
    ) -> Result<()> {
        self.market
            .simulate_liquidate_maker(self.address, pos_id, fee_recipient)
            .await
    }

    /// Liquidate an unhealthy maker position (always the full position on
    /// the deployed contracts). The liquidation fee goes to `fee_recipient`.
    ///
    /// Safe to call directly in the liquidation race — no prior
    /// [`Self::simulate_liquidate_maker`] needed: every send preflights at
    /// the pinned limit before broadcast, so a position that turns out
    /// healthy (or was already liquidated by a competitor) surfaces as a
    /// decoded
    /// [`TransactionError::SimulationReverted`]
    /// without burning gas. Reserve the simulate twin for scanning.
    ///
    /// Sends with the fixed [`GasLimits::LIQUIDATE`] bound instead of
    /// estimating: Arbitrum liquidations have gone out-of-gas where the gas
    /// estimate passed.
    pub async fn liquidate_maker(
        &self,
        pos_id: U256,
        fee_recipient: Address,
        urgency: Urgency,
    ) -> Result<TransactionReceipt> {
        self.send_liquidation(Book::Maker, pos_id, fee_recipient, urgency)
            .await
    }

    /// Check whether a taker `pos_id` is liquidatable right now, via
    /// `eth_call` — the batch/scanner probe for the taker book.
    ///
    /// Identical contract semantics to
    /// [`Self::simulate_liquidate_maker`], including the typed-revert
    /// triage via
    /// [`TransactionError::is_revert`](crate::errors::TransactionError::is_revert)
    /// — except the "wrong book" revert here is `Perp::NonTakerPosition`
    /// (drop the id, or route it to the maker twin). As on the maker side,
    /// prefer calling [`Self::liquidate_taker`] directly when racing; this
    /// probe is for sweeps.
    pub async fn simulate_liquidate_taker(
        &self,
        pos_id: U256,
        fee_recipient: Address,
    ) -> Result<()> {
        self.market
            .simulate_liquidate_taker(self.address, pos_id, fee_recipient)
            .await
    }

    /// Liquidate an unhealthy taker position (always the full position on
    /// the deployed contracts). The liquidation fee goes to `fee_recipient`.
    ///
    /// Safe to call directly in the race, exactly like
    /// [`Self::liquidate_maker`]: the send preflights at the pinned
    /// [`GasLimits::LIQUIDATE`] bound, so a would-be revert decodes into
    /// [`TransactionError::SimulationReverted`]
    /// instead of burning gas on-chain.
    pub async fn liquidate_taker(
        &self,
        pos_id: U256,
        fee_recipient: Address,
        urgency: Urgency,
    ) -> Result<TransactionReceipt> {
        self.send_liquidation(Book::Taker, pos_id, fee_recipient, urgency)
            .await
    }

    /// Shared send path behind the public liquidation methods: fixed
    /// [`GasLimits::LIQUIDATE`] bound, preflighted at that limit before
    /// broadcast.
    async fn send_liquidation(
        &self,
        book: Book,
        pos_id: U256,
        fee_recipient: Address,
        urgency: Urgency,
    ) -> Result<TransactionReceipt> {
        validate_fee_recipient(fee_recipient)?;
        let calldata = book.liquidation_calldata(pos_id, fee_recipient);

        tracing::debug!(
            pos_id = %pos_id,
            %fee_recipient,
            book = book.label(),
            ?urgency,
            "liquidating position"
        );

        let receipt = self
            .tx(self.market.perp(), calldata)
            .with_gas_limit(GasLimits::LIQUIDATE)
            .with_urgency(urgency)
            .send()
            .await?;
        tracing::debug!(
            pos_id = %pos_id,
            tx_hash = %receipt.transaction_hash,
            book = book.label(),
            "position liquidated"
        );
        Ok(receipt)
    }

    // ── Approval + transfers ────────────────────────────────────────

    /// Ensure USDC is approved for the Perp contract to spend.
    pub async fn ensure_approval(&self, min_amount: U256) -> Result<Option<B256>> {
        let usdc = IERC20::new(self.chain().deployments().usdc, self.chain().provider());
        let allowance: U256 = usdc
            .allowance(self.address, self.market.perp())
            .call()
            .await?;

        if allowance >= min_amount {
            tracing::debug!(allowance = %allowance, "USDC approval sufficient");
            return Ok(None);
        }

        tracing::debug!(allowance = %allowance, min_amount = %min_amount, "approving USDC");

        let calldata = usdc
            .approve(self.market.perp(), MAX_APPROVAL)
            .calldata()
            .clone();

        let receipt = self
            .tx(self.chain().deployments().usdc, calldata)
            .send()
            .await?;

        tracing::debug!(tx_hash = %receipt.transaction_hash, "USDC approved");
        Ok(Some(receipt.transaction_hash))
    }

    /// Transfer ETH to an address.
    pub async fn transfer_eth(
        &self,
        to: Address,
        amount_wei: u128,
        urgency: Urgency,
    ) -> Result<B256> {
        tracing::debug!(%to, amount_wei, ?urgency, "transferring ETH");
        // Estimate gas rather than hardcoding 21_000: Arbitrum's intrinsic gas
        // includes an L1 data component, so a fixed 21_000 is rejected as
        // "intrinsic gas too low".
        let receipt = self
            .tx(to, Bytes::new())
            .with_value(amount_wei)
            .with_urgency(urgency)
            .send()
            .await?;
        tracing::debug!(tx_hash = %receipt.transaction_hash, "ETH transferred");
        Ok(receipt.transaction_hash)
    }

    /// Transfer USDC to an address. `amount` is in human units (e.g. 100.0 = 100 USDC).
    pub async fn transfer_usdc(&self, to: Address, amount: f64, urgency: Urgency) -> Result<B256> {
        tracing::debug!(%to, amount, ?urgency, "transferring USDC");
        let usdc = IERC20::new(self.chain().deployments().usdc, self.chain().provider());
        let scaled = U256::from(scale_to_6dec(amount)? as u128);
        let calldata = usdc.transfer(to, scaled).calldata().clone();
        let receipt = self
            .tx(self.chain().deployments().usdc, calldata)
            .with_urgency(urgency)
            .send()
            .await?;
        tracing::debug!(tx_hash = %receipt.transaction_hash, "USDC transferred");
        Ok(receipt.transaction_hash)
    }

    /// Transfer an open position to another address.
    ///
    /// A position is an ERC721 token that owns its margin and its whole
    /// accrual history, so the transfer hands over the live position — the
    /// contract settles nothing, withdraws nothing, and the recipient
    /// adjusts or closes it exactly as the sender could have. Use it to move
    /// a position between wallets you control (handing a hand-seeded book to
    /// the account that will manage it); the receiving account must be able
    /// to call the Perp, so an EOA or a contract that can.
    ///
    /// Ownership is checked before sending: the contract's own
    /// `TransferFromIncorrectOwner` revert costs a broadcast to discover,
    /// and calling this for a position the client does not own is always a
    /// caller bug.
    pub async fn transfer_position(
        &self,
        to: Address,
        pos_id: U256,
        urgency: Urgency,
    ) -> Result<B256> {
        tracing::debug!(%to, %pos_id, ?urgency, "transferring position");
        if to == Address::ZERO {
            return Err(ValidationError::InvalidConfig {
                reason: "position recipient must not be the zero address \
                         (the position and its margin would be unrecoverable)"
                    .into(),
            }
            .into());
        }
        let from = self.address();
        if to == from {
            return Err(ValidationError::InvalidConfig {
                reason: "position recipient must differ from the sender".into(),
            }
            .into());
        }

        let contract = Perp::new(self.market.perp(), self.chain().provider());
        let owner = contract.ownerOf(pos_id).call().await?;
        if owner != from {
            return Err(ContractError::PositionNotOwned {
                pos_id,
                owner,
                caller: from,
            }
            .into());
        }

        let calldata = contract
            .safeTransferFrom(from, to, pos_id)
            .calldata()
            .clone();
        let receipt = self
            .tx(self.market.perp(), calldata)
            .with_urgency(urgency)
            .send()
            .await?;
        tracing::debug!(
            tx_hash = %receipt.transaction_hash,
            %pos_id, %to,
            "position transferred"
        );
        Ok(receipt.transaction_hash)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, I256, U256, Uint};

    use super::{
        AdjustTakerResult, OpenResult, PerpDelta, UsdcAtoms, UsdcDelta, closes_taker,
        closing_perp_delta, scale_opening_margin,
    };
    use crate::constants::Q96;
    use crate::contracts::{Perp, Position, SwapResult};
    use crate::convert::pack_balance_delta;
    use crate::events::rpc_log;

    #[test]
    fn open_result_serde_roundtrip() {
        let result = OpenResult {
            tx_hash: B256::ZERO,
            pos_id: U256::from(42),
            perp_delta: PerpDelta::new(68_100),
            usd_delta: UsdcDelta::new(-500_000_000),
        };
        let json = serde_json::to_string(&result).unwrap();
        let recovered: OpenResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, recovered);
    }

    #[test]
    fn adjust_taker_result_serde_roundtrip() {
        let result = AdjustTakerResult {
            tx_hash: B256::ZERO,
            perp_delta: PerpDelta::new(-68_100),
            usd_delta: UsdcDelta::new(499_500_000),
        };
        let json = serde_json::to_string(&result).unwrap();
        let recovered: AdjustTakerResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, recovered);
    }

    fn position(perp_atoms: i128, usd_atoms: i128) -> Position {
        Position {
            delta: pack_balance_delta(perp_atoms, usd_atoms),
            margin: 3_936_538_407,
            liqMarginRatio: Uint::ZERO,
            backstopMarginRatio: Uint::ZERO,
            lastCumlFundingX96: I256::ZERO,
        }
    }

    fn swap(perp_atoms: i128) -> SwapResult {
        SwapResult {
            delta: pack_balance_delta(perp_atoms, -perp_atoms * 46),
            ammPrice: Q96,
            totalFeeAmt: I256::ZERO,
            lpFeeAmt: U256::ZERO,
            protocolFeeAmt: U256::ZERO,
            creatorFeeAmt: U256::ZERO,
            insuranceFeeAmt: U256::ZERO,
        }
    }

    fn closed(pos_id: u64) -> Perp::TakerClosed {
        Perp::TakerClosed {
            posId: U256::from(pos_id),
            sr: swap(-85_908_380),
            funding: I256::ZERO,
            utilFees: U256::ZERO,
            liqFee: U256::ZERO,
            isLiquidation: false,
        }
    }

    fn adjusted(pos_id: u64) -> Perp::TakerAdjusted {
        Perp::TakerAdjusted {
            posId: U256::from(pos_id),
            sr: swap(-85_908_381),
            funding: I256::ZERO,
            utilFees: U256::ZERO,
        }
    }

    /// The close reverses the perp amount exactly, whatever the USD leg.
    #[test]
    fn the_closing_delta_reverses_the_perp_amount_to_the_atom() {
        assert_eq!(
            closing_perp_delta(&position(85_908_380, -3_998_080_483)),
            PerpDelta::new(-85_908_380)
        );
        assert_eq!(
            closing_perp_delta(&position(-12_020_548, 797_292_655)),
            PerpDelta::new(12_020_548)
        );
        // What an f64 close one atom long leaves behind: one atom short.
        assert_eq!(
            closing_perp_delta(&position(-1, -1_393_793)),
            PerpDelta::new(1)
        );
    }

    /// A close is only a close when the contract says `TakerClosed` for
    /// that position; a swap that left any delta mines as `TakerAdjusted`.
    #[test]
    fn only_a_taker_closed_event_for_the_position_is_a_close() {
        let pos_id = U256::from(1837u64);
        let perp = Address::repeat_byte(0x11);

        assert!(closes_taker(&[rpc_log(&closed(1837), perp)], pos_id));
        assert!(!closes_taker(&[rpc_log(&adjusted(1837), perp)], pos_id));
        assert!(
            !closes_taker(&[rpc_log(&closed(1791), perp)], pos_id),
            "another position's close does not close this one"
        );
        assert!(!closes_taker(&[], pos_id));
    }

    #[test]
    fn opening_margin_enforces_protocol_minimum() {
        assert!(scale_opening_margin(4.999_999).is_err());
        assert_eq!(
            scale_opening_margin(5.0).unwrap(),
            UsdcAtoms::new(5_000_000)
        );
        assert_eq!(
            scale_opening_margin(5.000_001).unwrap(),
            UsdcAtoms::new(5_000_001)
        );
    }
}
