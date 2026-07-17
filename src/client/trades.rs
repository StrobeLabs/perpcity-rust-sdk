//! Write operations: open, close, adjust positions, transfers, approvals.

use alloy::primitives::{Address, B256, Bytes, I256, U256};
use alloy::sol_types::SolEvent;

use crate::constants::{MAX_TICK, MIN_OPENING_MARGIN, MIN_TICK, TICK_SPACING};
use crate::contracts::{IERC20, Perp};
use crate::convert::{scale_from_6dec, scale_to_6dec};
use crate::errors::{ContractError, Result, ValidationError};
use crate::feeds::{MarketEvent, decode_log};
use crate::hft::gas::Urgency;
use crate::math::tick::{align_tick_down, align_tick_up, price_to_tick};
use crate::types::{
    AdjustMakerParams, AdjustMakerResult, AdjustTakerParams, AdjustTakerResult, OpenMakerParams,
    OpenResult, OpenTakerParams,
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

/// Scale an f64 perp delta to the accounting token's 6-decimal fixed point.
///
/// The perp token (Uniswap V4 `currency0`) is an `AccountingToken` with 6
/// decimals — the same scaling as USD margin — so `perp_delta` uses `scale_to_6dec`,
/// not 1e18.
fn scale_perp_delta(delta: f64) -> Result<I256> {
    let scaled = scale_to_6dec(delta)?;
    // An i128 always fits in I256.
    Ok(I256::try_from(scaled).expect("i128 fits in I256"))
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
    /// Returns an [`OpenResult`] with the transaction hash and position ID.
    pub async fn open_taker(
        &self,
        params: &OpenTakerParams,
        urgency: Urgency,
    ) -> Result<OpenResult> {
        let margin_scaled = scale_opening_margin(params.margin)?;

        let perp_delta = scale_perp_delta(params.perp_delta)?;

        let wire_params = crate::contracts::OpenTakerParams {
            holder: self.address,
            margin: margin_scaled as u128,
            perpDelta: perp_delta,
            amt1Limit: U256::from(params.amt1_limit),
        };

        let contract = Perp::new(self.deployments.perp, &self.provider);
        let calldata = contract.openTaker(wire_params).calldata().clone();

        tracing::debug!(
            margin = params.margin,
            perp_delta = params.perp_delta,
            ?urgency,
            "opening taker position"
        );

        let receipt = self
            .tx(self.deployments.perp, calldata)
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
        let result = OpenResult {
            tx_hash: receipt.transaction_hash,
            pos_id,
            perp_delta,
            usd_delta,
        };
        tracing::debug!(pos_id = %result.pos_id, "taker position opened");
        Ok(result)
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
    pub async fn adjust_taker(
        &self,
        params: &AdjustTakerParams,
        urgency: Urgency,
    ) -> Result<AdjustTakerResult> {
        let margin_delta = scale_to_6dec(params.margin_delta)?;
        let perp_delta = scale_perp_delta(params.perp_delta)?;

        let wire_params = crate::contracts::AdjustTakerParams {
            posId: params.pos_id,
            marginDelta: margin_delta,
            perpDelta: perp_delta,
            amt1Limit: U256::from(params.amt1_limit),
        };

        tracing::debug!(
            pos_id = %params.pos_id,
            margin_delta = params.margin_delta,
            perp_delta = params.perp_delta,
            ?urgency,
            "adjusting taker position"
        );

        let contract = Perp::new(self.deployments.perp, &self.provider);
        let calldata = contract.adjustTaker(wire_params).calldata().clone();

        let receipt = self
            .tx(self.deployments.perp, calldata)
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
