//! Write operations: open, close, adjust positions, transfers, approvals.

use alloy::primitives::{Address, B256, Bytes, I256, U256};
use alloy::sol_types::SolEvent;

use crate::constants::TICK_SPACING;
use crate::contracts::{IERC20, Perp};
use crate::convert::scale_to_6dec;
use crate::errors::{ContractError, Result, ValidationError};
use crate::hft::gas::{GasLimits, Urgency};
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
        if topics.len() >= 4
            && topics[0] == transfer_topic
            && topics[1] == B256::ZERO // from = address(0) means mint
        {
            // tokenId is topic[3] (indexed)
            return Ok(U256::from_be_bytes(topics[3].0));
        }
    }
    Err(ContractError::EventNotFound {
        event_name: "ERC721 Transfer (mint)".into(),
    })
}

/// Scale an f64 perp delta to 18-decimal I256.
fn scale_perp_delta(delta: f64) -> Result<I256> {
    let scaled = (delta * 1e18) as i128;
    I256::try_from(scaled).map_err(|_| {
        ValidationError::Overflow {
            context: format!("perp_delta {} overflows I256", delta),
        }
        .into()
    })
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
        let margin_scaled = scale_to_6dec(params.margin)?;
        if margin_scaled <= 0 {
            return Err(ValidationError::InvalidMargin {
                reason: format!("margin must be positive, got {}", params.margin),
            }
            .into());
        }

        let perp_delta = scale_perp_delta(params.perp_delta)?;

        let wire_params = crate::contracts::OpenTakerParams {
            holder: self.address,
            margin: margin_scaled as u128,
            perpDelta: perp_delta,
            amt1Limit: U256::from(params.amt1_limit),
        };

        let contract = Perp::new(self.deployments.perp, &self.provider);
        let calldata = contract.openTaker(wire_params).calldata().clone();

        tracing::debug!(margin = params.margin, perp_delta = params.perp_delta, ?urgency, "opening taker position");

        let receipt = self
            .tx(self.deployments.perp, calldata)
            .with_urgency(urgency)
            .send()
            .await?;

        let pos_id = parse_minted_token_id(&receipt)?;
        let result = OpenResult {
            tx_hash: receipt.transaction_hash,
            pos_id,
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
        let margin_scaled = scale_to_6dec(params.margin)?;
        if margin_scaled <= 0 {
            return Err(ValidationError::InvalidMargin {
                reason: format!("margin must be positive, got {}", params.margin),
            }
            .into());
        }

        let tick_lower = align_tick_down(price_to_tick(params.price_lower)?, TICK_SPACING);
        let tick_upper = align_tick_up(price_to_tick(params.price_upper)?, TICK_SPACING);

        if tick_lower >= tick_upper {
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

        tracing::debug!(margin = params.margin, tick_lower, tick_upper, ?urgency, "opening maker position");

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
            marginDelta: margin_delta as i128,
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
        Ok(AdjustTakerResult {
            tx_hash: receipt.transaction_hash,
        })
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
            marginDelta: margin_delta as i128,
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
        let receipt = self
            .tx(to, Bytes::new())
            .with_value(amount_wei)
            .with_gas_limit(GasLimits::ETH_TRANSFER)
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
