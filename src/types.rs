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
    /// Perp contract address (one per market).
    pub perp: Address,
    /// USDC token address.
    pub usdc: Address,
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

/// Live market data from a multicall snapshot.
///
/// Pure market state — no static config. Returned alongside [`PerpData`]
/// from [`PerpClient::get_perp_snapshot`](crate::PerpClient::get_perp_snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PerpSnapshot {
    /// Current mark price (TWAP) in human-readable units.
    pub mark_price: f64,
    /// Oracle index price from the beacon contract.
    pub index_price: f64,
    /// Daily funding rate (positive = longs pay shorts).
    pub funding_rate_daily: f64,
    /// Taker open interest.
    pub open_interest: OpenInterest,
}

/// Client-facing parameters for opening a taker (long/short) position.
///
/// The SDK converts these to contract types automatically:
/// - `margin` → scaled to 6 decimals
/// - `perp_delta` → scaled to 18 decimals (positive = long, negative = short)
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

/// Client-facing parameters for adjusting a maker (LP) position.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdjustMakerParams {
    /// Position NFT token ID.
    pub pos_id: U256,
    /// Margin delta in USDC: positive to deposit, negative to withdraw.
    pub margin_delta: f64,
    /// Liquidity delta: positive to add, negative to remove.
    pub liquidity_delta: i128,
    /// Max/min amount of token0 for slippage protection.
    pub amt0_limit: u128,
    /// Max/min amount of token1 for slippage protection.
    pub amt1_limit: u128,
}

// ── Result types ────────────────────────────────────────────────────

/// Result of opening a taker or maker position.
///
/// The new contracts emit parameterless events (`TakerOpened`, `MakerOpened`),
/// so detailed position data must be read via view functions after confirmation.
/// The `pos_id` comes from the function return value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpenResult {
    /// Transaction hash.
    pub tx_hash: B256,
    /// Minted position NFT token ID.
    pub pos_id: U256,
}

/// Result of adjusting a taker position (margin, notional, or both).
///
/// Events are parameterless — read position state via view functions if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdjustTakerResult {
    /// Transaction hash.
    pub tx_hash: B256,
}

/// Result of adjusting a maker position (margin, liquidity, or both).
///
/// Events are parameterless — read position state via view functions if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdjustMakerResult {
    /// Transaction hash.
    pub tx_hash: B256,
}

/// A single point on a price impact curve.
///
/// Describes the market impact of a trade at a specific size: what price
/// the trader would get, and how far that deviates from the current mark.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PriceImpactPoint {
    /// Trade size in USD that was simulated.
    pub size: f64,
    /// Perp token delta from the simulated swap.
    pub perp_delta: f64,
    /// USD delta from the simulated swap.
    pub usd_delta: f64,
    /// Effective execution price (|usd_delta / perp_delta|).
    pub effective_price: f64,
    /// Price impact in basis points relative to the mark price.
    pub impact_bps: f64,
}

impl PriceImpactPoint {
    /// Compute a price impact point from swap deltas and a reference mark price.
    ///
    /// Returns `None` if `perp_delta` is effectively zero (no swap occurred).
    pub fn from_swap(size: f64, perp_delta: f64, usd_delta: f64, mark_price: f64) -> Option<Self> {
        if perp_delta.abs() < f64::EPSILON {
            return None;
        }
        let effective_price = (usd_delta / perp_delta).abs();
        let impact_bps = ((effective_price - mark_price) / mark_price).abs() * 10_000.0;
        Some(Self {
            size,
            perp_delta,
            usd_delta,
            effective_price,
            impact_bps,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{B256, U256};

    #[test]
    fn open_result_serde_roundtrip() {
        let result = OpenResult {
            tx_hash: B256::ZERO,
            pos_id: U256::from(42),
        };
        let json = serde_json::to_string(&result).unwrap();
        let recovered: OpenResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, recovered);
    }

    #[test]
    fn adjust_taker_result_serde_roundtrip() {
        let result = AdjustTakerResult {
            tx_hash: B256::ZERO,
        };
        let json = serde_json::to_string(&result).unwrap();
        let recovered: AdjustTakerResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, recovered);
    }

    #[test]
    fn deployments_serde_roundtrip() {
        let deployments = Deployments {
            perp: Address::ZERO,
            usdc: Address::ZERO,
        };
        let json = serde_json::to_string(&deployments).unwrap();
        let recovered: Deployments = serde_json::from_str(&json).unwrap();
        assert_eq!(deployments, recovered);
    }

    #[test]
    fn price_impact_point_basic() {
        // Short trade: sell 100 perp at mark=50, get effective price 49.5 (1% worse)
        let point = PriceImpactPoint::from_swap(100.0, -100.0, 4950.0, 50.0).unwrap();
        assert_eq!(point.size, 100.0);
        assert!((point.effective_price - 49.5).abs() < 0.001);
        assert!((point.impact_bps - 100.0).abs() < 0.1); // 1% = 100 bps
    }

    #[test]
    fn price_impact_point_long() {
        // Long trade: buy 2 perp at mark=50, pay 101 USD (effective price 50.5)
        let point = PriceImpactPoint::from_swap(101.0, 2.0, -101.0, 50.0).unwrap();
        assert!((point.effective_price - 50.5).abs() < 0.001);
        assert!((point.impact_bps - 100.0).abs() < 0.1); // 1% = 100 bps
    }

    #[test]
    fn price_impact_point_zero_impact() {
        // Perfect execution at mark price
        let point = PriceImpactPoint::from_swap(50.0, -1.0, 50.0, 50.0).unwrap();
        assert!((point.impact_bps).abs() < 0.001);
    }

    #[test]
    fn price_impact_point_zero_perp_delta() {
        // No swap occurred — returns None
        assert!(PriceImpactPoint::from_swap(100.0, 0.0, 0.0, 50.0).is_none());
    }
}
