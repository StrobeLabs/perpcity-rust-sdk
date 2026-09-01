//! Write operations: open, close, adjust positions, transfers, approvals.

use alloy::primitives::{Address, B256, Bytes, I256, U256};
use alloy::sol_types::{SolCall, SolEvent};

use crate::constants::{MAX_TICK, MIN_OPENING_MARGIN, MIN_TICK, TICK_SPACING};
use crate::contracts::{IERC20, Perp};
use crate::convert::{scale_from_6dec, scale_to_6dec};
use crate::errors::{ContractError, Result, ValidationError};
use crate::feeds::{MarketEvent, decode_log};
use crate::hft::gas::{GasLimits, Urgency};
use crate::math::tick::{align_tick_down, align_tick_up, price_to_tick};
use crate::types::{
    AdjustMakerParams, AdjustMakerResult, AdjustTakerParams, AdjustTakerResult,
    ExactAdjustTakerParams, ExactOpenTakerParams, OpenMakerParams, OpenResult, OpenTakerParams,
};

use super::{MAX_APPROVAL, PerpClient, i32_to_i24};

/// Extract the minted token ID from an ERC721 `Transfer(address(0), to, tokenId)` event.
///
/// The Perp contract inherits ERC721 and mints a position NFT on open.
/// The standard Transfer event carries the token ID.
fn parse_minted_token_id(
    receipt: &alloy::rpc::types::TransactionReceipt,
) -> std::result::Result<U256, ContractError> {
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
/// (already unpacked from the `BalanceDelta` and scaled to f64). Every taker
/// open/adjust/close emits one of these — a margin-only adjust still emits a
/// `TakerAdjusted` with a zero-delta swap — so on the taker paths `None` means
/// a decode/ABI failure, which the caller surfaces as an error rather than a
/// zero fill. (Maker opens emit no taker swap, but they don't call this.)
fn parse_taker_swap(receipt: &alloy::rpc::types::TransactionReceipt) -> Option<(f64, f64)> {
    for log in receipt.inner.logs() {
        if let Some(
            MarketEvent::TakerOpened { swap, .. }
            | MarketEvent::TakerAdjusted { swap, .. }
            | MarketEvent::TakerClosed { swap, .. },
        ) = decode_log(log)
        {
            return Some((swap.perp_delta, swap.usd_delta));
        }
    }
    None
}

/// Which side of the book a liquidation targets. The two contract entry
/// points are twins; only the encoded call differs.
#[derive(Debug, Clone, Copy)]
enum Side {
    Maker,
    Taker,
}

impl Side {
    fn liquidation_calldata(self, pos_id: U256, fee_recipient: Address) -> Bytes {
        match self {
            Self::Maker => Perp::liquidateMakerCall {
                posId: pos_id,
                liquidationFeeRecipient: fee_recipient,
            }
            .abi_encode()
            .into(),
            Self::Taker => Perp::liquidateTakerCall {
                posId: pos_id,
                liquidationFeeRecipient: fee_recipient,
            }
            .abi_encode()
            .into(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Maker => "maker",
            Self::Taker => "taker",
        }
    }
}

/// Reject `Address::ZERO` as a liquidation fee recipient: the contract
/// transfers the fee wherever it is told, so the zero address silently
/// burns the caller's liquidation reward. Always a caller bug.
fn validate_fee_recipient(fee_recipient: Address) -> std::result::Result<(), ValidationError> {
    if fee_recipient == Address::ZERO {
        return Err(ValidationError::InvalidConfig {
            reason: "liquidation fee_recipient must not be the zero address \
                     (the fee would be burned)"
                .into(),
        });
    }
    Ok(())
}

/// Scale and validate position margin against the protocol's opening minimum.
///
/// Checks the scaled margin parameter against [`MIN_OPENING_MARGIN`] and returns
/// [`ValidationError::InvalidMargin`] when it is below the protocol minimum.
fn scale_opening_margin(margin: f64) -> std::result::Result<i128, ValidationError> {
    let scaled = scale_to_6dec(margin)?;
    if scaled < i128::from(MIN_OPENING_MARGIN) {
        let minimum = scale_from_6dec(i128::from(MIN_OPENING_MARGIN));
        return Err(ValidationError::InvalidMargin {
            reason: format!("margin must be at least {minimum} USDC, got {margin}"),
        });
    }
    Ok(scaled)
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
            // scale_opening_margin returns >= MIN_OPENING_MARGIN, so the cast
            // to unsigned cannot wrap.
            margin: scale_opening_margin(params.margin)? as u128,
            // The perp token (V4 `currency0`) is an AccountingToken with 6
            // decimals — the same scaling as USD margin, not 1e18.
            perp_delta: scale_to_6dec(params.perp_delta)?,
            amt1_limit: params.amt1_limit,
        };
        self.open_taker_exact(&exact, urgency).await
    }

    /// Open a taker position without converting through floating point.
    pub async fn open_taker_exact(
        &self,
        params: &ExactOpenTakerParams,
        urgency: Urgency,
    ) -> Result<OpenResult> {
        if params.margin < u128::from(MIN_OPENING_MARGIN) {
            return Err(ValidationError::InvalidMargin {
                reason: format!("margin must be at least {MIN_OPENING_MARGIN} atoms"),
            }
            .into());
        }
        let wire_params = crate::contracts::OpenTakerParams {
            holder: self.address,
            margin: params.margin,
            perpDelta: I256::try_from(params.perp_delta).expect("i128 fits I256"),
            amt1Limit: U256::from(params.amt1_limit),
        };
        let contract = Perp::new(self.deployments.perp, &self.provider);

        tracing::debug!(
            margin_atoms = params.margin,
            perp_delta_atoms = params.perp_delta,
            ?urgency,
            "opening taker position"
        );

        let receipt = self
            .tx(
                self.deployments.perp,
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
    /// Converts `price_lower`/`price_upper` to aligned ticks internally.
    /// Returns an [`OpenResult`] with the transaction hash and position ID.
    pub async fn open_maker(
        &self,
        params: &OpenMakerParams,
        urgency: Urgency,
    ) -> Result<OpenResult> {
        let margin_scaled = scale_opening_margin(params.margin)?;

        let tick_lower = align_tick_down(price_to_tick(params.price_lower)?, TICK_SPACING);
        let tick_upper = align_tick_up(price_to_tick(params.price_upper)?, TICK_SPACING);

        if tick_lower < MIN_TICK || tick_upper > MAX_TICK || tick_lower >= tick_upper {
            return Err(ValidationError::InvalidTickRange {
                lower: tick_lower,
                upper: tick_upper,
            }
            .into());
        }

        let wire_params = crate::contracts::OpenMakerParams {
            holder: self.address,
            margin: margin_scaled as u128,
            tickLower: i32_to_i24(tick_lower),
            tickUpper: i32_to_i24(tick_upper),
            liquidity: params.liquidity,
            maxAmt0In: U256::from(params.max_amt0_in),
            maxAmt1In: U256::from(params.max_amt1_in),
        };

        tracing::debug!(
            margin = params.margin,
            tick_lower,
            tick_upper,
            ?urgency,
            "opening maker position"
        );

        let contract = Perp::new(self.deployments.perp, &self.provider);
        let calldata = contract.openMaker(wire_params).calldata().clone();

        let receipt = self
            .tx(self.deployments.perp, calldata)
            .with_urgency(urgency)
            .send()
            .await?;

        let pos_id = parse_minted_token_id(&receipt)?;
        let result = OpenResult {
            tx_hash: receipt.transaction_hash,
            pos_id,
            // Maker opens emit no taker swap (`MakerOpened` carries no deltas).
            perp_delta: 0.0,
            usd_delta: 0.0,
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
            margin_delta: scale_to_6dec(params.margin_delta)?,
            perp_delta: scale_to_6dec(params.perp_delta)?,
            amt1_limit: params.amt1_limit,
        };
        self.adjust_taker_exact(&exact, urgency).await
    }

    /// Adjust a taker position without converting through floating point.
    pub async fn adjust_taker_exact(
        &self,
        params: &ExactAdjustTakerParams,
        urgency: Urgency,
    ) -> Result<AdjustTakerResult> {
        let wire_params = crate::contracts::AdjustTakerParams {
            posId: params.pos_id,
            marginDelta: params.margin_delta,
            perpDelta: I256::try_from(params.perp_delta).expect("i128 fits I256"),
            amt1Limit: U256::from(params.amt1_limit),
        };
        let contract = Perp::new(self.deployments.perp, &self.provider);

        tracing::debug!(
            pos_id = %params.pos_id,
            margin_delta_atoms = params.margin_delta,
            perp_delta_atoms = params.perp_delta,
            ?urgency,
            "adjusting taker position"
        );

        let receipt = self
            .tx(
                self.deployments.perp,
                contract.adjustTaker(wire_params).calldata().clone(),
            )
            .with_urgency(urgency)
            .send()
            .await?;

        tracing::debug!(pos_id = %params.pos_id, "taker position adjusted");
        // Every taker adjust emits a decodable event — `TakerAdjusted` (a
        // margin-only adjust carries a zero-delta swap) or `TakerClosed` on a
        // full close. A missing one signals an ABI/decode problem, so fail
        // loudly rather than recording a bogus zero fill.
        let (perp_delta, usd_delta) =
            parse_taker_swap(&receipt).ok_or(ContractError::EventNotFound {
                event_name: "TakerAdjusted/TakerClosed".into(),
            })?;
        Ok(AdjustTakerResult {
            tx_hash: receipt.transaction_hash,
            perp_delta,
            usd_delta,
        })
    }

    /// Close a taker position by reversing its full perp delta.
    ///
    /// Reversing the entire delta drives the position's notional to exactly
    /// zero, which the contract settles automatically: it returns the
    /// position's equity (remaining margin + realized PnL) to the caller and
    /// burns the position NFT. No separate margin withdrawal is required.
    ///
    /// `current_perp_delta` must be the position's **full** signed delta
    /// (positive = long, negative = short), typically from locally tracked
    /// state. If it does not match the on-chain delta exactly, the position
    /// will not fully close (and the contract may revert).
    ///
    /// This is a market close: slippage is unconstrained. The `amt1` limit is
    /// set to the no-op sentinel for the swap direction — selling (reversing a
    /// long) floors the USD received at `0`; buying (reversing a short) caps
    /// the USD paid at `u128::MAX`. For a protected close, call
    /// [`Self::adjust_taker`] directly with an explicit `amt1_limit`.
    pub async fn close_taker(
        &self,
        pos_id: U256,
        current_perp_delta: f64,
        urgency: Urgency,
    ) -> Result<AdjustTakerResult> {
        let perp_delta = -current_perp_delta;
        self.adjust_taker(
            &AdjustTakerParams {
                pos_id,
                margin_delta: 0.0,
                perp_delta,
                amt1_limit: if perp_delta > 0.0 { u128::MAX } else { 0 },
            },
            urgency,
        )
        .await
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
            liquidityDelta: params.liquidity_delta,
            amt0Limit: U256::from(params.amt0_limit),
            amt1Limit: U256::from(params.amt1_limit),
        };

        tracing::debug!(
            pos_id = %params.pos_id,
            margin_delta = params.margin_delta,
            liquidity_delta = params.liquidity_delta,
            ?urgency,
            "adjusting maker position"
        );

        let contract = Perp::new(self.deployments.perp, &self.provider);
        let calldata = contract.adjustMaker(wire_params).calldata().clone();

        let receipt = self
            .tx(self.deployments.perp, calldata)
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
        current_liquidity: u128,
        urgency: Urgency,
    ) -> Result<AdjustMakerResult> {
        let liquidity_delta = i128::try_from(current_liquidity).map(|l| -l).map_err(|_| {
            ValidationError::Overflow {
                context: format!("liquidity {current_liquidity} exceeds i128::MAX"),
            }
        })?;
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
    /// [`TransactionError::SimulationReverted`](crate::errors::TransactionError::SimulationReverted);
    /// triage it typed via
    /// [`TransactionError::is_revert`](crate::errors::TransactionError::is_revert):
    ///
    /// - `err.is_revert::<Perp::NotLiquidatable>()` — healthy right now;
    ///   retry the id later.
    /// - `err.is_revert::<Perp::NonMakerPosition>()` — a taker or burned
    ///   id on the maker path; drop it (or route it to the taker twin).
    /// - other reverts (a utilization gate from capacity pinned under live
    ///   OI, …) — inspect `error_name`.
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
        self.simulate_liquidation(Side::Maker, pos_id, fee_recipient)
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
    /// [`TransactionError::SimulationReverted`](crate::errors::TransactionError::SimulationReverted)
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
    ) -> Result<alloy::rpc::types::TransactionReceipt> {
        self.send_liquidation(Side::Maker, pos_id, fee_recipient, urgency)
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
        self.simulate_liquidation(Side::Taker, pos_id, fee_recipient)
            .await
    }

    /// Liquidate an unhealthy taker position (always the full position on
    /// the deployed contracts). The liquidation fee goes to `fee_recipient`.
    ///
    /// Safe to call directly in the race, exactly like
    /// [`Self::liquidate_maker`]: the send preflights at the pinned
    /// [`GasLimits::LIQUIDATE`] bound, so a would-be revert decodes into
    /// [`TransactionError::SimulationReverted`](crate::errors::TransactionError::SimulationReverted)
    /// instead of burning gas on-chain.
    pub async fn liquidate_taker(
        &self,
        pos_id: U256,
        fee_recipient: Address,
        urgency: Urgency,
    ) -> Result<alloy::rpc::types::TransactionReceipt> {
        self.send_liquidation(Side::Taker, pos_id, fee_recipient, urgency)
            .await
    }

    /// Shared `eth_call` health probe behind the four public liquidation
    /// methods: validates the fee recipient, encodes the side's call, and
    /// preflights at the pinned [`GasLimits::LIQUIDATE`] cap.
    async fn simulate_liquidation(
        &self,
        side: Side,
        pos_id: U256,
        fee_recipient: Address,
    ) -> Result<()> {
        validate_fee_recipient(fee_recipient)?;
        let calldata = side.liquidation_calldata(pos_id, fee_recipient);
        self.preflight_call(
            self.deployments.perp,
            &calldata,
            0,
            Some(GasLimits::LIQUIDATE),
        )
        .await?;
        Ok(())
    }

    /// Shared send path behind the public liquidation methods: fixed
    /// [`GasLimits::LIQUIDATE`] bound, preflighted at that limit before
    /// broadcast.
    async fn send_liquidation(
        &self,
        side: Side,
        pos_id: U256,
        fee_recipient: Address,
        urgency: Urgency,
    ) -> Result<alloy::rpc::types::TransactionReceipt> {
        validate_fee_recipient(fee_recipient)?;
        let calldata = side.liquidation_calldata(pos_id, fee_recipient);

        tracing::debug!(
            pos_id = %pos_id,
            %fee_recipient,
            side = side.label(),
            ?urgency,
            "liquidating position"
        );

        let receipt = self
            .tx(self.deployments.perp, calldata)
            .with_gas_limit(GasLimits::LIQUIDATE)
            .with_urgency(urgency)
            .send()
            .await?;
        tracing::debug!(
            pos_id = %pos_id,
            tx_hash = %receipt.transaction_hash,
            side = side.label(),
            "position liquidated"
        );
        Ok(receipt)
    }

    // ── Approval + transfers ────────────────────────────────────────

    /// Ensure USDC is approved for the Perp contract to spend.
    pub async fn ensure_approval(&self, min_amount: U256) -> Result<Option<B256>> {
        let usdc = IERC20::new(self.deployments.usdc, &self.provider);
        let allowance: U256 = usdc
            .allowance(self.address, self.deployments.perp)
            .call()
            .await?;

        if allowance >= min_amount {
            tracing::debug!(allowance = %allowance, "USDC approval sufficient");
            return Ok(None);
        }

        tracing::debug!(allowance = %allowance, min_amount = %min_amount, "approving USDC");

        let calldata = usdc
            .approve(self.deployments.perp, MAX_APPROVAL)
            .calldata()
            .clone();

        let receipt = self.tx(self.deployments.usdc, calldata).send().await?;

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
        let usdc = IERC20::new(self.deployments.usdc, &self.provider);
        let scaled = U256::from(scale_to_6dec(amount)? as u128);
        let calldata = usdc.transfer(to, scaled).calldata().clone();
        let receipt = self
            .tx(self.deployments.usdc, calldata)
            .with_urgency(urgency)
            .send()
            .await?;
        tracing::debug!(tx_hash = %receipt.transaction_hash, "USDC transferred");
        Ok(receipt.transaction_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::scale_opening_margin;

    #[test]
    fn opening_margin_enforces_protocol_minimum() {
        assert!(scale_opening_margin(4.999_999).is_err());
        assert_eq!(scale_opening_margin(5.0).unwrap(), 5_000_000);
        assert_eq!(scale_opening_margin(5.000_001).unwrap(), 5_000_001);
    }
}
