//! Client-facing types for the PerpCity SDK.
//!
//! These types use `f64` for human-readable values (prices, USDC amounts,
//! leverage) and Alloy's [`Address`] / [`B256`] for on-chain identifiers.
//! They are the public API surface — users construct these, and the SDK
//! converts them to wire-format contract types internally. The `Exact*`
//! variants carry wire units (atoms) directly for callers that must not
//! round-trip through `f64`.
//!
//! Everything here is inert data: no invariant beyond its field types, no
//! arithmetic. A type that carries an invariant or behavior lives with it
//! in its domain module — the taker quoting types in [`crate::math::swap`],
//! a validated [`TickRange`](crate::math::range::TickRange) in
//! [`crate::math::range`], capacity in [`crate::math::capacity`].
//!
//! All types implement [`Serialize`] and
//! [`Deserialize`] for logging, dashboards, persistence,
//! and inter-process communication.

#![doc = "\n\nThe design of this module: [`src/types/DESIGN.md`](https://github.com/StrobeLabs/perpcity-rust-sdk/blob/main/src/types/DESIGN.md)."]

use std::fmt;

use alloy::primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};

use crate::math::BlockContext;
use crate::math::pricing::Emas;
use crate::units::{LDelta, LUnits};

/// The addresses every market on a chain shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainDeployments {
    /// The collateral token.
    pub usdc: Address,
    /// The Uniswap V4 `PoolManager` every market's pool lives in (see
    /// `ARBITRUM_POOL_MANAGER` / `ARBITRUM_SEPOLIA_POOL_MANAGER`).
    pub pool_manager: Address,
}

/// A market's configuration: what deployment fixed and what governance
/// sets, read once per market.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketConfig {
    /// The market's `Perp` contract address (the market identifier).
    pub perp: Address,
    /// Tick spacing for the underlying Uniswap V4 pool.
    pub tick_spacing: i32,
    /// `EMA_WINDOW()`, in seconds: the time constant the contract smooths
    /// the pool price and the index with; a deployment immutable, and what
    /// [`Emas::advanced`] takes.
    pub ema_window: u64,
    /// Pool (AMM spot) price in human-readable units (e.g. `1.05`) — not
    /// the contract's mark, which is the fair price
    /// ([`crate::math::pricing`]).
    pub pool_price: f64,
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

/// One side's margin-ratio thresholds, as fractions of position value.
///
/// Built from the module's 1e6-scaled `uint24` values by [`Self::from_e6`];
/// the `*_e6` getters recover them exactly.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MarginRatioTriple {
    /// Minimum equity over value to open or increase a position.
    pub init: f64,
    /// Equity over value below which the position is liquidatable.
    pub liquidation: f64,
    /// Equity over value below which the position can be backstopped.
    pub backstop: f64,
}

/// `MarginRatioTriple` fractions are the on-chain e6 values over this.
const RATIO_E6_F64: f64 = 1_000_000.0;

impl MarginRatioTriple {
    /// Build from the module's 1e6-scaled values (`1_000_000` = 100%).
    pub fn from_e6(init_e6: u32, liquidation_e6: u32, backstop_e6: u32) -> Self {
        Self {
            init: init_e6 as f64 / RATIO_E6_F64,
            liquidation: liquidation_e6 as f64 / RATIO_E6_F64,
            backstop: backstop_e6 as f64 / RATIO_E6_F64,
        }
    }

    /// `init` as the on-chain 1e6-scaled value.
    pub fn init_e6(&self) -> u32 {
        to_e6(self.init)
    }

    /// `liquidation` as the on-chain 1e6-scaled value — the
    /// `liq_margin_ratio_e6` a position opened now stores.
    pub fn liquidation_e6(&self) -> u32 {
        to_e6(self.liquidation)
    }

    /// `backstop` as the on-chain 1e6-scaled value.
    pub fn backstop_e6(&self) -> u32 {
        to_e6(self.backstop)
    }
}

/// Recover a `uint24` e6 value from its fraction. Exact: the fraction came
/// from an integer below 2^24 divided by 1e6, so the product rounds back
/// to that integer.
fn to_e6(ratio: f64) -> u32 {
    (ratio * RATIO_E6_F64).round() as u32
}

/// The market's `IMarginRatios` module: maker and taker thresholds.
///
/// These are the module's CURRENT values, applied to positions opened from
/// now on; an open position keeps the liquidation ratio stored on it at
/// open (`positions(id).liqMarginRatio`, surfaced by
/// [`MakerEquityBreakdown::liq_margin_ratio_e6`](crate::MakerEquityBreakdown::liq_margin_ratio_e6)).
/// Read with [`MarketReader::get_margin_ratios`](crate::MarketReader::get_margin_ratios).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MarginRatios {
    /// Maker (LP) thresholds.
    pub maker: MarginRatioTriple,
    /// Taker thresholds.
    pub taker: MarginRatioTriple,
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

/// A taker direction: a long gains when the price rises, a short when it
/// falls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    /// Long exposure (positive perp delta).
    Long,
    /// Short exposure (negative perp delta).
    Short,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Long => "long",
            Self::Short => "short",
        })
    }
}

/// Taker open interest for a perp market, in perp tokens (multiply by the
/// mark price for USD). The contract accumulates `|perp_delta|` per side.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpenInterest {
    /// Total long open interest in perp tokens.
    pub long_oi: f64,
    /// Total short open interest in perp tokens.
    pub short_oi: f64,
}

/// The market's own solvency books, in USDC: the contract's `SolvencyState`.
///
/// `total_margin` moves only when real USDC enters or leaves, so it is
/// the honest upper bound on what positions may collectively claim.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SolvencyState {
    /// Insolvency the contract has recognised and booked.
    pub bad_debt: f64,
    /// Margin the contract believes it holds.
    pub total_margin: f64,
}

/// The market's live state at the lagged snapshot block, in human units.
///
/// Pure market state — no configuration. Returned alongside
/// [`MarketConfig`] from
/// [`MarketReader::get_snapshot`](crate::MarketReader::get_snapshot).
/// What a live cache seeds from before it follows the feed: the prices,
/// the contract's mark, and the stored EMAs it needs to keep marking
/// between touches.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MarketSnapshot {
    /// The block every field was read at: the lagged snapshot block, with
    /// its hash, so further reads can pin to it.
    pub block: BlockContext,
    /// Pool (AMM spot) price in human-readable units — not a TWAP, and not
    /// the contract's mark, which is the fair price
    /// ([`crate::math::pricing`]).
    pub pool_price: f64,
    /// Oracle index price from the beacon contract.
    pub index_price: f64,
    /// The contract's mark at the block: the fair price of the pool price,
    /// the index and the EMAs advanced to the block's timestamp, exact in
    /// X96 and converted once. What every health check, `valPnl` and
    /// liquidation prices at, so the basis the contract sees is this
    /// against the index, not the pool price against it.
    pub mark: f64,
    /// The stored EMAs as of the market's last touch. A cache that follows
    /// the feed advances them to now against the prices it holds and marks
    /// with [`Emas::mark`]; `RatesAndEmasRefreshed` replaces them on every
    /// touch.
    pub emas: Emas,
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

/// Exact wire-unit parameters for latency-sensitive taker opens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExactOpenTakerParams {
    /// Margin in USDC atoms (six decimals).
    pub margin: u128,
    /// Signed perp atoms (six decimals).
    pub perp_delta: i128,
    /// Directional token1 limit produced by [`crate::TakerQuote::amt1_limit`].
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

/// Exact wire-unit parameters for latency-sensitive taker adjustments.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExactAdjustTakerParams {
    /// Position NFT token ID.
    pub pos_id: U256,
    /// Signed margin change in USDC atoms.
    pub margin_delta: i128,
    /// Signed perp atoms.
    pub perp_delta: i128,
    /// Directional token1 limit produced by [`crate::TakerQuote::amt1_limit`].
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
    pub liquidity_delta: LDelta,
    /// Max/min amount of token0 for slippage protection.
    pub amt0_limit: u128,
    /// Max/min amount of token1 for slippage protection.
    pub amt1_limit: u128,
}

// ── Result types ────────────────────────────────────────────────────

/// Result of opening a taker or maker position.
///
/// `pos_id` is the minted position NFT id. For taker opens, `perp_delta` and
/// `usd_delta` are the realized swap amounts decoded from the `TakerOpened`
/// event (signed: positive = received, negative = paid). Maker opens emit no
/// swap, so both are `0.0`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpenResult {
    /// Transaction hash.
    pub tx_hash: B256,
    /// Minted position NFT token ID.
    pub pos_id: U256,
    /// Realized perp-token delta from the open swap (taker only; `0.0` for makers).
    pub perp_delta: f64,
    /// Realized USD delta from the open swap (taker only; `0.0` for makers).
    pub usd_delta: f64,
}

/// Result of adjusting a taker position (margin, notional, or both).
///
/// `perp_delta` and `usd_delta` are the realized swap amounts decoded from the
/// `TakerAdjusted` event — or `TakerClosed`, when the adjust reverses the full
/// delta and closes the position (signed: positive = received, negative =
/// paid). Both are `0.0` for a margin-only adjust, which performs no swap.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdjustTakerResult {
    /// Transaction hash.
    pub tx_hash: B256,
    /// Realized perp-token delta from the adjust swap (`0.0` if margin-only).
    pub perp_delta: f64,
    /// Realized USD delta from the adjust swap (`0.0` if margin-only).
    pub usd_delta: f64,
}

/// Result of adjusting a maker position (margin, liquidity, or both).
///
/// Events are parameterless — read position state via view functions if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdjustMakerResult {
    /// Transaction hash.
    pub tx_hash: B256,
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
            perp_delta: 0.0681,
            usd_delta: -500.0,
        };
        let json = serde_json::to_string(&result).unwrap();
        let recovered: OpenResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, recovered);
    }

    #[test]
    fn adjust_taker_result_serde_roundtrip() {
        let result = AdjustTakerResult {
            tx_hash: B256::ZERO,
            perp_delta: -0.0681,
            usd_delta: 499.5,
        };
        let json = serde_json::to_string(&result).unwrap();
        let recovered: AdjustTakerResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, recovered);
    }

    #[test]
    fn chain_deployments_serde_roundtrip() {
        let deployments = ChainDeployments {
            usdc: Address::ZERO,
            pool_manager: Address::ZERO,
        };
        let json = serde_json::to_string(&deployments).unwrap();
        let recovered: ChainDeployments = serde_json::from_str(&json).unwrap();
        assert_eq!(deployments, recovered);
    }

    /// The deployed HORMUZ-TRAFFIC module values (2026-09-07): maker
    /// 1.0 / 0.9 / 0.8, taker 0.1 / 0.05 / 0.02 — and the e6 getters
    /// recover every `uint24` exactly.
    #[test]
    fn margin_ratio_triple_e6_roundtrip() {
        let maker = MarginRatioTriple::from_e6(1_000_000, 900_000, 800_000);
        assert_eq!(
            (maker.init, maker.liquidation, maker.backstop),
            (1.0, 0.9, 0.8)
        );
        let taker = MarginRatioTriple::from_e6(100_000, 50_000, 20_000);
        assert_eq!(
            (taker.init, taker.liquidation, taker.backstop),
            (0.1, 0.05, 0.02)
        );
        assert_eq!(
            (maker.init_e6(), maker.liquidation_e6(), maker.backstop_e6()),
            (1_000_000, 900_000, 800_000)
        );
        assert_eq!(
            (taker.init_e6(), taker.liquidation_e6(), taker.backstop_e6()),
            (100_000, 50_000, 20_000)
        );
        for e6 in [0u32, 1, 3, 333_333, 999_999, (1 << 24) - 1] {
            assert_eq!(MarginRatioTriple::from_e6(e6, e6, e6).init_e6(), e6);
        }

        let ratios = MarginRatios { maker, taker };
        let json = serde_json::to_string(&ratios).unwrap();
        let recovered: MarginRatios = serde_json::from_str(&json).unwrap();
        assert_eq!(ratios, recovered);
    }
}
