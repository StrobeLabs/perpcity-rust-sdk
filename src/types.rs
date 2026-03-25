//! Client-facing types for the PerpCity SDK.
//!
//! These types use `f64` for human-readable values (prices, USDC amounts,
//! leverage) and Alloy's [`Address`] / [`B256`] for on-chain identifiers.
//! They are the public API surface — users construct these, and the SDK
//! converts them to wire-format contract types internally.
//!
//! Following the [hyperliquid-rust-sdk] pattern: client types are **not**
//! serializable. Serialization belongs on wire types only.

use alloy::primitives::{Address, B256, U256};

/// Deployed contract addresses for a PerpCity instance.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseParams {
    /// Minimum amount of token0 to receive (slippage protection).
    pub min_amt0_out: u128,
    /// Minimum amount of token1 to receive (slippage protection).
    pub min_amt1_out: u128,
    /// Maximum amount of token1 willing to pay.
    pub max_amt1_in: u128,
}

/// Result of opening a position (taker or maker).
///
/// Contains the entry deltas parsed from the `PositionOpened` event so
/// callers can construct position tracking data without a follow-up RPC read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpenResult {
    /// Minted position NFT token ID.
    pub pos_id: U256,
    /// Whether this is a maker position.
    pub is_maker: bool,
    /// Perp token delta (signed). Positive = long, negative = short.
    pub perp_delta: f64,
    /// USD delta (signed).
    pub usd_delta: f64,
}

/// Result of closing a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseResult {
    /// Transaction hash.
    pub tx_hash: B256,
    /// If the close was partial, the remaining position's NFT token ID.
    /// `None` means the position was fully closed.
    pub remaining_position_id: Option<U256>,
}

/// Result of a swap simulation via `quoteSwap`.
///
/// All values are human-readable (USDC as f64, perp delta as f64).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwapQuote {
    /// Perp token delta (positive = received, negative = spent).
    pub perp_delta: f64,
    /// USD delta (positive = received, negative = spent).
    pub usd_delta: f64,
}

/// Result of simulating a taker position open via `quoteOpenTakerPosition`.
///
/// All values are human-readable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpenTakerQuote {
    /// Perp token delta (positive = long exposure, negative = short).
    pub perp_delta: f64,
    /// USD delta (positive = received, negative = spent).
    pub usd_delta: f64,
}

/// Result of simulating a maker position open via `quoteOpenMakerPosition`.
///
/// All values are human-readable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpenMakerQuote {
    /// Perp token delta.
    pub perp_delta: f64,
    /// USD delta.
    pub usd_delta: f64,
}
