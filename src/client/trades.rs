//! Write operations: open, close, adjust positions, transfers, approvals.

use alloy::primitives::{Address, B256, Bytes, I256, U256};

use crate::constants::TICK_SPACING;
use crate::contracts::{IERC20, PerpManager};
use crate::convert::{leverage_to_margin_ratio, scale_to_6dec};
use crate::errors::{Result, ValidationError};
use crate::hft::gas::{GasLimits, Urgency};
use crate::math::tick::{align_tick_down, align_tick_up, price_to_tick};
use crate::types::{
    AdjustMarginParams, AdjustMarginResult, AdjustNotionalParams, AdjustNotionalResult,
    CloseParams, CloseResult, OpenMakerParams, OpenResult, OpenTakerParams,
};

use super::{
    MAX_APPROVAL, PerpClient, i32_to_i24, parse_adjust_result, parse_close_result,
    parse_margin_result, parse_open_result, u32_to_u24,
};

impl PerpClient {
    // ── Position operations ──────────────────────────────────────────

    /// Open a taker (long/short) position.
    ///
    /// Returns an [`OpenResult`] with the position ID and entry deltas
    /// parsed from the `PositionOpened` event.
    pub async fn open_taker(
        &self,
        perp_id: B256,
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
        let margin_ratio = leverage_to_margin_ratio(params.leverage)?;

        let wire_params = PerpManager::OpenTakerPositionParams {
            holder: self.address,
            isLong: params.is_long,
            margin: margin_scaled as u128,
            marginRatio: u32_to_u24(margin_ratio),
            unspecifiedAmountLimit: params.unspecified_amount_limit,
        };

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let calldata = contract
            .openTakerPos(perp_id, wire_params)
            .calldata()
            .clone();

        tracing::debug!(%perp_id, margin = params.margin, leverage = params.leverage, is_long = params.is_long, ?urgency, "opening taker position");

        let receipt = self
            .tx(self.deployments.perp_manager, calldata)
            .with_urgency(urgency)
            .send()
            .await?;

        let result = parse_open_result(&receipt)?;
        tracing::debug!(%perp_id, pos_id = %result.pos_id, perp_delta = result.perp_delta, usd_delta = result.usd_delta, "taker position opened");
        Ok(result)
    }

    /// Open a maker (LP) position within a price range.
    ///
    /// Converts `price_lower`/`price_upper` to aligned ticks internally.
    /// Returns an [`OpenResult`] with the position ID and entry deltas.
    pub async fn open_maker(
        &self,
        perp_id: B256,
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

        // Liquidity must fit in u120 on-chain
        let liquidity: u128 = params.liquidity;
        let max_u120: u128 = (1u128 << 120) - 1;
        if liquidity > max_u120 {
            return Err(ValidationError::Overflow {
                context: format!("liquidity {} exceeds uint120 max", liquidity),
            }
            .into());
        }

        let wire_params = PerpManager::OpenMakerPositionParams {
            holder: self.address,
            margin: margin_scaled as u128,
            liquidity: alloy::primitives::Uint::<120, 2>::from(liquidity),
            tickLower: i32_to_i24(tick_lower),
            tickUpper: i32_to_i24(tick_upper),
            maxAmt0In: params.max_amt0_in,
            maxAmt1In: params.max_amt1_in,
        };

        tracing::debug!(%perp_id, margin = params.margin, tick_lower, tick_upper, ?urgency, "opening maker position");

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let calldata = contract
            .openMakerPos(perp_id, wire_params)
            .calldata()
            .clone();

        let receipt = self
            .tx(self.deployments.perp_manager, calldata)
            .with_urgency(urgency)
            .send()
            .await?;

        let result = parse_open_result(&receipt)?;
        tracing::debug!(%perp_id, pos_id = %result.pos_id, perp_delta = result.perp_delta, usd_delta = result.usd_delta, "maker position opened");
        Ok(result)
    }

    /// Close a position (taker or maker).
    pub async fn close_position(
        &self,
        pos_id: U256,
        params: &CloseParams,
        urgency: Urgency,
    ) -> Result<CloseResult> {
        let wire_params = PerpManager::ClosePositionParams {
            posId: pos_id,
            minAmt0Out: params.min_amt0_out,
            minAmt1Out: params.min_amt1_out,
            maxAmt1In: params.max_amt1_in,
        };

        tracing::debug!(pos_id = %pos_id, ?urgency, "closing position");

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let calldata = contract.closePosition(wire_params).calldata().clone();

        let receipt = self
            .tx(self.deployments.perp_manager, calldata)
            .with_urgency(urgency)
            .send()
            .await?;

        let result = parse_close_result(&receipt, pos_id)?;
        tracing::debug!(pos_id = %pos_id, was_liquidated = result.was_liquidated, net_margin = result.net_margin, "position closed");
        Ok(result)
    }

    /// Adjust the notional exposure of a taker position.
    pub async fn adjust_notional(
        &self,
        pos_id: U256,
        params: &AdjustNotionalParams,
        urgency: Urgency,
    ) -> Result<AdjustNotionalResult> {
        let usd_delta_scaled = scale_to_6dec(params.usd_delta)?;

        let wire_params = PerpManager::AdjustNotionalParams {
            posId: pos_id,
            usdDelta: I256::try_from(usd_delta_scaled).map_err(|_| ValidationError::Overflow {
                context: format!("usd_delta {} overflows I256", usd_delta_scaled),
            })?,
            perpLimit: params.perp_limit,
        };

        tracing::debug!(pos_id = %pos_id, usd_delta = params.usd_delta, ?urgency, "adjusting notional");

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let calldata = contract.adjustNotional(wire_params).calldata().clone();

        let receipt = self
            .tx(self.deployments.perp_manager, calldata)
            .with_urgency(urgency)
            .send()
            .await?;

        let result = parse_adjust_result(&receipt)?;
        tracing::debug!(pos_id = %pos_id, new_perp_delta = result.new_perp_delta, "notional adjusted");
        Ok(result)
    }

    /// Add or remove margin from a position.
    pub async fn adjust_margin(
        &self,
        pos_id: U256,
        params: &AdjustMarginParams,
        urgency: Urgency,
    ) -> Result<AdjustMarginResult> {
        let delta_scaled = scale_to_6dec(params.margin_delta)?;

        let wire_params = PerpManager::AdjustMarginParams {
            posId: pos_id,
            marginDelta: I256::try_from(delta_scaled).map_err(|_| ValidationError::Overflow {
                context: format!("margin_delta {} overflows I256", delta_scaled),
            })?,
        };

        tracing::debug!(pos_id = %pos_id, margin_delta = params.margin_delta, ?urgency, "adjusting margin");

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let calldata = contract.adjustMargin(wire_params).calldata().clone();

        let receipt = self
            .tx(self.deployments.perp_manager, calldata)
            .with_urgency(urgency)
            .send()
            .await?;

        let result = parse_margin_result(&receipt)?;
        tracing::debug!(pos_id = %pos_id, new_margin = result.new_margin, "margin adjusted");
        Ok(result)
    }

    // ── Approval + transfers ────────────────────────────────────────

    /// Ensure USDC is approved for the PerpManager to spend.
    pub async fn ensure_approval(&self, min_amount: U256) -> Result<Option<B256>> {
        let usdc = IERC20::new(self.deployments.usdc, &self.provider);
        let allowance: U256 = usdc
            .allowance(self.address, self.deployments.perp_manager)
            .call()
            .await?;

        if allowance >= min_amount {
            tracing::debug!(allowance = %allowance, "USDC approval sufficient");
            return Ok(None);
        }

        tracing::debug!(allowance = %allowance, min_amount = %min_amount, "approving USDC");

        let calldata = usdc
            .approve(self.deployments.perp_manager, MAX_APPROVAL)
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
