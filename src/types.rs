//! Client-facing types for the PerpCity SDK.
//!
//! These types use `f64` for human-readable values (prices, USDC amounts,
//! leverage) and Alloy's [`Address`] / [`B256`] for on-chain identifiers.
//! They are the public API surface — users construct these, and the SDK
//! converts them to wire-format contract types internally.
//!
//! All types implement [`Serialize`] and
//! [`Deserialize`] for logging, dashboards, persistence,
//! and inter-process communication.

use alloy::primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};

/// Deployed contract addresses for a PerpCity instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployments {
    /// PerpManager proxy address.
    pub perp_manager: Address,
    /// USDC token address.
    pub usdc: Address,
    /// Fees module address (if registered).
    pub fees_module: Option<Address>,
    /// Margin ratios module address (if registered).
    pub margin_ratios_module: Option<Address>,
    /// Lockup period module address (if registered).
    pub lockup_period_module: Option<Address>,
    /// Sqrt-price impact limit module address (if registered).
    pub sqrt_price_impact_limit_module: Option<Address>,
}

/// Metadata about a perpetual market.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerpData {
    /// Unique perp identifier (`PoolId` / `bytes32` on-chain).
    pub id: B256,
    /// Tick spacing for the underlying Uniswap V4 pool.
    pub tick_spacing: i32,
    /// Current mark price in human-readable units (e.g. `1.05`).
    pub mark: f64,
    /// Beacon contract address.
    pub beacon: Address,
    /// Leverage and margin constraints.
    pub bounds: Bounds,
    /// Fee structure.
    pub fees: Fees,
}

/// Leverage and margin constraints for a perpetual market.
///
/// All values are human-readable: leverage as a multiplier (e.g. `10.0`),
/// margin in USDC (e.g. `5.0`), and ratios as fractions (e.g. `0.005`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    /// Minimum margin to open a position, in USDC (e.g. `5.0`).
    pub min_margin: f64,
    /// Minimum taker leverage (e.g. `1.0`).
    pub min_taker_leverage: f64,
    /// Maximum taker leverage (e.g. `100.0`).
    pub max_taker_leverage: f64,
    /// Margin ratio at which taker liquidation occurs, as a fraction
    /// (e.g. `0.005` = 0.5%).
    pub liquidation_taker_ratio: f64,
}

/// Fee percentages for a perpetual market, expressed as fractions of 1.
///
/// For example, `0.001` means 0.1% (which is `1_000` on-chain at 1e6 scale).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fees {
    /// Fee paid to the perp creator.
    pub creator_fee: f64,
    /// Fee that goes to the insurance fund.
    pub insurance_fee: f64,
    /// Fee earned by liquidity providers.
    pub lp_fee: f64,
    /// Fee charged on liquidations.
    pub liquidation_fee: f64,
}

/// Real-time position metrics, typically from a `quoteClosePosition` call.
///
/// All USDC values are human-readable (e.g. `12.50` not `12_500_000`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LiveDetails {
    /// Unrealized PnL in USDC.
    pub pnl: f64,
    /// Accumulated funding payment in USDC (positive = received).
    pub funding_payment: f64,
    /// Current effective margin in USDC.
    pub effective_margin: f64,
    /// Whether this position would be liquidated at the current price.
    pub is_liquidatable: bool,
}

/// Taker open interest for a perp market, in USDC.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpenInterest {
    /// Total long open interest in USDC.
    pub long_oi: f64,
    /// Total short open interest in USDC.
    pub short_oi: f64,
}

/// Client-facing parameters for opening a taker (long/short) position.
///
/// The SDK converts these to contract types automatically:
/// - `margin` → scaled to 6 decimals
/// - `leverage` → converted to margin ratio via `1e6 / leverage`
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpenTakerParams {
    /// `true` for long, `false` for short.
    pub is_long: bool,
    /// Margin in USDC (e.g. `100.0` for 100 USDC).
    pub margin: f64,
    /// Leverage multiplier (e.g. `10.0` for 10×).
    pub leverage: f64,
    /// Slippage protection: max unspecified token amount. `0` = no limit.
    pub unspecified_amount_limit: u128,
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
    pub liquidity: u128,
    /// Maximum amount of token0 willing to deposit.
    pub max_amt0_in: u128,
    /// Maximum amount of token1 willing to deposit.
    pub max_amt1_in: u128,
}

/// Client-facing parameters for closing a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseParams {
    /// Minimum amount of token0 to receive (slippage protection).
    pub min_amt0_out: u128,
    /// Minimum amount of token1 to receive (slippage protection).
    pub min_amt1_out: u128,
    /// Maximum amount of token1 willing to pay.
    pub max_amt1_in: u128,
}

/// Client-facing parameters for adjusting a position's notional exposure.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdjustNotionalParams {
    /// USD delta: positive to increase notional, negative to decrease.
    pub usd_delta: f64,
    /// Maximum perp token amount for slippage protection. `u128::MAX` = no limit.
    pub perp_limit: u128,
}

/// Client-facing parameters for adjusting a position's margin.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdjustMarginParams {
    /// Margin delta in USDC: positive to deposit, negative to withdraw.
    pub margin_delta: f64,
}

// ── Result types ────────────────────────────────────────────────────

/// Result of opening a position (taker or maker).
///
/// Parsed from the `PositionOpened` event in the transaction receipt.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpenResult {
    /// Minted position NFT token ID.
    pub pos_id: U256,
    /// Whether this is a maker position.
    pub is_maker: bool,
    /// Perp token delta (signed). Positive = long, negative = short.
    pub perp_delta: f64,
    /// USD delta (signed).
    pub usd_delta: f64,
    /// Lower tick of the position's price range.
    pub tick_lower: i32,
    /// Upper tick of the position's price range.
    pub tick_upper: i32,
}

/// Result of adjusting a position's notional size.
///
/// Parsed from the `NotionalAdjusted` event in the transaction receipt.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdjustNotionalResult {
    /// New cumulative perp delta after adjustment (signed).
    pub new_perp_delta: f64,
    /// Perp delta from this specific swap (signed).
    pub swap_perp_delta: f64,
    /// USD delta from this specific swap (signed).
    pub swap_usd_delta: f64,
    /// Funding settled during this adjustment.
    pub funding: f64,
    /// Utilization fee charged.
    pub utilization_fee: f64,
    /// Auto-deleveraging amount.
    pub adl: f64,
    /// Trading fees charged.
    pub trading_fees: f64,
}

/// Result of adjusting a position's margin.
///
/// Parsed from the `MarginAdjusted` event in the transaction receipt.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdjustMarginResult {
    /// New margin after adjustment.
    pub new_margin: f64,
}

/// Result of closing a position.
///
/// Parsed from the `PositionClosed` event in the transaction receipt.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CloseResult {
    /// Transaction hash.
    pub tx_hash: B256,
    /// Whether this was a maker position.
    pub was_maker: bool,
    /// Whether the position was liquidated.
    pub was_liquidated: bool,
    /// If the close was partial, the remaining position's NFT token ID.
    /// `None` means the position was fully closed.
    pub remaining_position_id: Option<U256>,
    /// Perp delta at exit (signed).
    pub exit_perp_delta: f64,
    /// USD delta at exit (signed).
    pub exit_usd_delta: f64,
    /// Net USD delta after settlement.
    pub net_usd_delta: f64,
    /// Funding settled at close.
    pub funding: f64,
    /// Utilization fee charged.
    pub utilization_fee: f64,
    /// Auto-deleveraging amount.
    pub adl: f64,
    /// Liquidation fee (zero if not liquidated).
    pub liquidation_fee: f64,
    /// Net margin returned.
    pub net_margin: f64,
}

/// Result of a swap simulation via `quoteSwap`.
///
/// All values are human-readable (USDC as f64, perp delta as f64).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SwapQuote {
    /// Perp token delta (positive = received, negative = spent).
    pub perp_delta: f64,
    /// USD delta (positive = received, negative = spent).
    pub usd_delta: f64,
}

/// Result of simulating a taker position open via `quoteOpenTakerPosition`.
///
/// All values are human-readable.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpenTakerQuote {
    /// Perp token delta (positive = long exposure, negative = short).
    pub perp_delta: f64,
    /// USD delta (positive = received, negative = spent).
    pub usd_delta: f64,
}

/// Result of simulating a maker position open via `quoteOpenMakerPosition`.
///
/// All values are human-readable.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpenMakerQuote {
    /// Perp token delta.
    pub perp_delta: f64,
    /// USD delta.
    pub usd_delta: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{B256, U256};

    #[test]
    fn open_result_serde_roundtrip() {
        let result = OpenResult {
            pos_id: U256::from(42),
            is_maker: false,
            perp_delta: -1234.567,
            usd_delta: 98765.43,
            tick_lower: -69090,
            tick_upper: 69090,
        };
        let json = serde_json::to_string(&result).unwrap();
        let recovered: OpenResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, recovered);
    }

    #[test]
    fn close_result_serde_roundtrip() {
        let result = CloseResult {
            tx_hash: B256::ZERO,
            was_maker: false,
            was_liquidated: false,
            remaining_position_id: None,
            exit_perp_delta: -100.0,
            exit_usd_delta: 200.0,
            net_usd_delta: 195.0,
            funding: -0.5,
            utilization_fee: 0.1,
            adl: 0.0,
            liquidation_fee: 0.0,
            net_margin: 150.0,
        };
        let json = serde_json::to_string(&result).unwrap();
        let recovered: CloseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, recovered);
    }

    #[test]
    fn deployments_serde_roundtrip() {
        let deployments = Deployments {
            perp_manager: Address::ZERO,
            usdc: Address::ZERO,
            fees_module: None,
            margin_ratios_module: None,
            lockup_period_module: None,
            sqrt_price_impact_limit_module: None,
        };
        let json = serde_json::to_string(&deployments).unwrap();
        let recovered: Deployments = serde_json::from_str(&json).unwrap();
        assert_eq!(deployments, recovered);
    }
}
